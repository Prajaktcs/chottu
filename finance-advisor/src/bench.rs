//! Repeatable research-model evaluation against a checked-in gold fixture.
//!
//! Metrics are computed locally from structured score drafts. Live OpenRouter
//! calls stay in the `research_bench` binary — run that on a machine with keys.

use crate::{normalize_ticker, Conviction, ScoredCandidate, ScoreReport, UniverseEntry};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureRole {
    ClearPass,
    ClearInterest,
    Contested,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureName {
    pub ticker: String,
    pub company: String,
    pub role: FixtureRole,
    /// Acceptable conviction labels for this name (PascalCase: Pass, Low, …).
    pub expected_conviction: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchFixture {
    pub version: u32,
    pub description: String,
    #[serde(default)]
    pub philosophy_note: String,
    pub names: Vec<FixtureName>,
}

impl ResearchFixture {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, anyhow::Error> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read fixture {}", path.display()))?;
        let fix: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse fixture {}", path.display()))?;
        fix.validate()?;
        Ok(fix)
    }

    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.names.is_empty() {
            anyhow::bail!("fixture has no names");
        }
        let mut seen = HashSet::new();
        for n in &self.names {
            let t = normalize_ticker(&n.ticker);
            if t.is_empty() {
                anyhow::bail!("fixture entry has empty ticker");
            }
            if !seen.insert(t) {
                anyhow::bail!("duplicate fixture ticker: {}", n.ticker);
            }
            if n.expected_conviction.is_empty() {
                anyhow::bail!("{} has empty expected_conviction", n.ticker);
            }
        }
        Ok(())
    }

    pub fn targets_csv(&self) -> String {
        self.names
            .iter()
            .map(|n| n.ticker.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn by_ticker(&self) -> HashMap<String, &FixtureName> {
        self.names
            .iter()
            .map(|n| (normalize_ticker(&n.ticker), n))
            .collect()
    }

    pub fn universe_entries(&self) -> Vec<UniverseEntry> {
        self.names
            .iter()
            .map(|n| UniverseEntry {
                ticker: normalize_ticker(&n.ticker),
                company: n.company.clone(),
                exchange: None,
                market_cap_band: None,
                market_cap_m: None,
                proposed_by: vec!["fixture".into()],
                one_line_why: Some(n.notes.clone()),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScorerMetrics {
    pub model_id: String,
    /// Fraction of clear_pass names whose conviction is in expected_conviction.
    pub pass_label_accuracy: f64,
    pub pass_label_n: usize,
    /// Fraction of clear_interest names whose conviction is in expected_conviction.
    pub interest_label_accuracy: f64,
    pub interest_label_n: usize,
    /// Among (clear_interest, clear_pass) pairs, fraction where interest fit_score > pass.
    pub pairwise_order_accuracy: f64,
    pub pairwise_n: usize,
    /// Fraction of clear_interest names in the model's top-k by fit_score.
    pub interest_in_top_k: f64,
    pub top_k: usize,
    pub mean_fit_clear_pass: Option<f64>,
    pub mean_fit_clear_interest: Option<f64>,
    /// Higher is better composite in \[0, 1\] (equal weight on available signals).
    pub composite: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchComparison {
    pub baseline: ScorerMetrics,
    pub candidate: ScorerMetrics,
    pub candidate_wins: bool,
    pub delta_composite: f64,
    pub notes: Vec<String>,
}

fn conviction_label(c: &Conviction) -> &'static str {
    match c {
        Conviction::High => "High",
        Conviction::Medium => "Medium",
        Conviction::Low => "Low",
        Conviction::Pass => "Pass",
    }
}

fn score_map(scores: &[ScoredCandidate]) -> BTreeMap<String, &ScoredCandidate> {
    scores
        .iter()
        .map(|s| (normalize_ticker(&s.ticker), s))
        .collect()
}

fn mean(vals: &[f64]) -> Option<f64> {
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

/// Evaluate one model's structured scores against the gold fixture.
pub fn evaluate_scorer(
    model_id: &str,
    fixture: &ResearchFixture,
    report: &ScoreReport,
    top_k: usize,
) -> ScorerMetrics {
    let by = fixture.by_ticker();
    let scores = score_map(&report.scores);

    let mut pass_ok = 0usize;
    let mut pass_n = 0usize;
    let mut interest_ok = 0usize;
    let mut interest_n = 0usize;
    let mut pass_fits = Vec::new();
    let mut interest_fits = Vec::new();
    let mut interest_tickers = Vec::new();
    let mut pass_tickers = Vec::new();

    for (ticker, name) in &by {
        let Some(sc) = scores.get(ticker) else {
            continue;
        };
        let label = conviction_label(&sc.conviction);
        let in_expected = name
            .expected_conviction
            .iter()
            .any(|e| e.eq_ignore_ascii_case(label));
        match name.role {
            FixtureRole::ClearPass => {
                pass_n += 1;
                if in_expected {
                    pass_ok += 1;
                }
                pass_fits.push(sc.fit_score as f64);
                pass_tickers.push(ticker.clone());
            }
            FixtureRole::ClearInterest => {
                interest_n += 1;
                if in_expected {
                    interest_ok += 1;
                }
                interest_fits.push(sc.fit_score as f64);
                interest_tickers.push(ticker.clone());
            }
            FixtureRole::Contested => {}
        }
    }

    let mut pair_ok = 0usize;
    let mut pair_n = 0usize;
    for it in &interest_tickers {
        let Some(is) = scores.get(it) else { continue };
        for pt in &pass_tickers {
            let Some(ps) = scores.get(pt) else { continue };
            pair_n += 1;
            if is.fit_score > ps.fit_score {
                pair_ok += 1;
            }
        }
    }

    let mut ranked: Vec<_> = report.scores.iter().collect();
    ranked.sort_by(|a, b| {
        b.fit_score
            .cmp(&a.fit_score)
            .then_with(|| a.ticker.cmp(&b.ticker))
    });
    let k = top_k.max(1);
    let top: HashSet<String> = ranked
        .iter()
        .take(k)
        .map(|s| normalize_ticker(&s.ticker))
        .collect();
    let interest_hits = interest_tickers.iter().filter(|t| top.contains(*t)).count();
    let interest_in_top_k = if interest_n == 0 {
        0.0
    } else {
        interest_hits as f64 / interest_n as f64
    };

    let pass_label_accuracy = if pass_n == 0 {
        0.0
    } else {
        pass_ok as f64 / pass_n as f64
    };
    let interest_label_accuracy = if interest_n == 0 {
        0.0
    } else {
        interest_ok as f64 / interest_n as f64
    };
    let pairwise_order_accuracy = if pair_n == 0 {
        0.0
    } else {
        pair_ok as f64 / pair_n as f64
    };

    // Composite: pairwise ordering is the strongest gold signal; then pass labels; then top-k.
    let composite = 0.5 * pairwise_order_accuracy
        + 0.3 * pass_label_accuracy
        + 0.2 * interest_in_top_k;

    ScorerMetrics {
        model_id: model_id.to_string(),
        pass_label_accuracy,
        pass_label_n: pass_n,
        interest_label_accuracy,
        interest_label_n: interest_n,
        pairwise_order_accuracy,
        pairwise_n: pair_n,
        interest_in_top_k,
        top_k: k,
        mean_fit_clear_pass: mean(&pass_fits),
        mean_fit_clear_interest: mean(&interest_fits),
        composite,
    }
}

pub fn compare_scorers(
    baseline: ScorerMetrics,
    candidate: ScorerMetrics,
) -> BenchComparison {
    let delta = candidate.composite - baseline.composite;
    let mut notes = Vec::new();
    if candidate.pairwise_order_accuracy + 1e-9 < baseline.pairwise_order_accuracy {
        notes.push("Candidate weaker on interest>pass pairwise ordering.".into());
    }
    if candidate.pass_label_accuracy + 1e-9 < baseline.pass_label_accuracy {
        notes.push("Candidate weaker at labeling clear-pass names Pass/Low.".into());
    }
    if delta > 0.02 {
        notes.push("Candidate composite ahead — consider swapping the panel slot.".into());
    } else if delta < -0.02 {
        notes.push("Baseline composite ahead — keep current panel scorer.".into());
    } else {
        notes.push("Composites within 0.02 — treat as inconclusive; prefer cheaper model if tied.".into());
    }

    BenchComparison {
        candidate_wins: delta > 0.02
            || (delta.abs() <= 0.02
                && candidate.pairwise_order_accuracy >= baseline.pairwise_order_accuracy),
        delta_composite: delta,
        baseline,
        candidate,
        notes,
    }
}

pub fn parse_score_draft(raw_json: &str) -> Result<ScoreReport, serde_json::Error> {
    serde_json::from_str(raw_json)
}

pub fn format_metrics_table(m: &ScorerMetrics) -> String {
    format!(
        "| metric | value |\n|---|---:|\n\
         | composite | {:.3} |\n\
         | pairwise interest>pass | {:.1}% (n={}) |\n\
         | pass label accuracy | {:.1}% (n={}) |\n\
         | interest label accuracy | {:.1}% (n={}) |\n\
         | interest in top-{} | {:.1}% |\n\
         | mean fit clear_pass | {} |\n\
         | mean fit clear_interest | {} |\n",
        m.composite,
        100.0 * m.pairwise_order_accuracy,
        m.pairwise_n,
        100.0 * m.pass_label_accuracy,
        m.pass_label_n,
        100.0 * m.interest_label_accuracy,
        m.interest_label_n,
        m.top_k,
        100.0 * m.interest_in_top_k,
        m.mean_fit_clear_pass
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "—".into()),
        m.mean_fit_clear_interest
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "—".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Conviction;

    fn fixture() -> ResearchFixture {
        ResearchFixture {
            version: 1,
            description: "test".into(),
            philosophy_note: String::new(),
            names: vec![
                FixtureName {
                    ticker: "AAPL".into(),
                    company: "Apple".into(),
                    role: FixtureRole::ClearPass,
                    expected_conviction: vec!["Pass".into(), "Low".into()],
                    notes: String::new(),
                },
                FixtureName {
                    ticker: "ASTS".into(),
                    company: "AST".into(),
                    role: FixtureRole::ClearInterest,
                    expected_conviction: vec!["High".into(), "Medium".into(), "Low".into()],
                    notes: String::new(),
                },
                FixtureName {
                    ticker: "SOUN".into(),
                    company: "SoundHound".into(),
                    role: FixtureRole::Contested,
                    expected_conviction: vec!["High".into(), "Pass".into()],
                    notes: String::new(),
                },
            ],
        }
    }

    fn scored(ticker: &str, fit: i32, c: Conviction) -> ScoredCandidate {
        ScoredCandidate {
            ticker: ticker.into(),
            company: ticker.into(),
            fit_score: fit,
            conviction: c,
            thesis: "t".into(),
            catalysts: vec![],
            risks: vec![],
            hundred_bagger_plausible: fit >= 7,
            pass_reason: None,
        }
    }

    #[test]
    fn good_scorer_beats_bad_scorer() {
        let fix = fixture();
        let good = ScoreReport {
            scores: vec![
                scored("AAPL", 2, Conviction::Pass),
                scored("ASTS", 9, Conviction::High),
                scored("SOUN", 5, Conviction::Medium),
            ],
        };
        let bad = ScoreReport {
            scores: vec![
                scored("AAPL", 9, Conviction::High),
                scored("ASTS", 3, Conviction::Pass),
                scored("SOUN", 5, Conviction::Medium),
            ],
        };
        let g = evaluate_scorer("good", &fix, &good, 1);
        let b = evaluate_scorer("bad", &fix, &bad, 1);
        assert!(g.pairwise_order_accuracy > b.pairwise_order_accuracy);
        assert!(g.pass_label_accuracy > b.pass_label_accuracy);
        assert!(g.composite > b.composite);
        let cmp = compare_scorers(b, g);
        assert!(cmp.candidate_wins);
    }

    #[test]
    fn fixture_targets_csv_stable() {
        let fix = fixture();
        assert_eq!(fix.targets_csv(), "AAPL, ASTS, SOUN");
        fix.validate().unwrap();
    }
}
