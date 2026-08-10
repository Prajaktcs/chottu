//! Seeded stock-research A/B helper: current panel vs prior Opus panel.
//!
//! Usage:
//!   cargo run -p finance-advisor --bin research_ab
//!   cargo run -p finance-advisor --bin research_ab -- --targets "ASTS, RKLB, IONQ"
//!
//! Writes artifacts under `evals/research-ab/<run_id>/`.
//! Prefer `research_bench` for gold-metric scorer comparison.

use anyhow::{Context, Result};
use chotu_common::{config_path, load_config, InvestmentPhilosophy};
use finance_advisor::{
    ScoreReport, ScoredCandidate, StageDraft, StockResearcher, UniverseEntry,
    DEFAULT_JUDGE_MODEL,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_TARGETS: &str = "ASTS, RKLB, IONQ, SOUN, JOBY, ACHR, LUNR, RDW";

const ARM_A_PANEL: &[&str] = &[
    "openai/gpt-5.6-sol",
    "qwen/qwen3.8-max",
    "moonshotai/kimi-k3",
];
const ARM_B_PANEL: &[&str] = &[
    "openai/gpt-5.6-sol",
    "anthropic/claude-opus-5",
    "moonshotai/kimi-k3",
];

struct ArmSpec {
    id: &'static str,
    label: &'static str,
    panel: &'static [&'static str],
    judge: &'static str,
}

const ARMS: &[ArmSpec] = &[
    ArmSpec {
        id: "a-baseline",
        label: "A baseline (Sol + Qwen + Kimi, judge Kimi)",
        panel: ARM_A_PANEL,
        judge: DEFAULT_JUDGE_MODEL,
    },
    ArmSpec {
        id: "b-opus",
        label: "B prior panel (Sol + Opus + Kimi, judge Kimi)",
        panel: ARM_B_PANEL,
        judge: DEFAULT_JUDGE_MODEL,
    },
];

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let targets = parse_targets(env::args().skip(1).collect())?;
    let philosophy = load_philosophy();
    let run_id = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let out_root = PathBuf::from("evals/research-ab").join(&run_id);
    fs::create_dir_all(&out_root)
        .with_context(|| format!("create output dir {}", out_root.display()))?;

    println!("research_ab run_id={run_id}");
    println!("targets={targets}");
    println!("output={}", out_root.display());
    println!(
        "OPENROUTER_API_KEY={}",
        if env::var("OPENROUTER_API_KEY").is_ok() {
            "set"
        } else {
            "MISSING"
        }
    );
    println!(
        "FINNHUB_API_KEY={}",
        if env::var("FINNHUB_API_KEY").is_ok() {
            "set"
        } else {
            "unset (model-estimated caps)"
        }
    );

    let mut arm_artifacts: Vec<(&ArmSpec, finance_advisor::ResearchRunArtifacts, u128)> =
        Vec::new();

    for arm in ARMS {
        println!("\n=== Running {} ===", arm.label);
        let panel: Vec<String> = arm.panel.iter().map(|s| (*s).to_string()).collect();
        let researcher = StockResearcher::with_models(panel, arm.judge.to_string());
        if !researcher.is_configured() {
            anyhow::bail!(
                "OPENROUTER_API_KEY is not set. Add it to `.env` or the environment, then re-run."
            );
        }

        let started = Instant::now();
        let artifacts = researcher
            .perform_research_with_artifacts(Some(&targets), Some(&philosophy), None)
            .await
            .with_context(|| format!("arm {} research failed", arm.id))?;
        let elapsed_ms = started.elapsed().as_millis();

        let arm_dir = out_root.join(arm.id);
        write_arm_artifacts(&arm_dir, &artifacts)?;
        println!(
            "arm {} done in {:.1}s — universe={} scorers={} → {}",
            arm.id,
            elapsed_ms as f64 / 1000.0,
            artifacts.universe.len(),
            artifacts.score_drafts.len(),
            arm_dir.display()
        );
        arm_artifacts.push((arm, artifacts, elapsed_ms));
    }

    let summary = build_summary(&targets, &philosophy, &arm_artifacts);
    let summary_path = out_root.join("summary.md");
    fs::write(&summary_path, &summary)
        .with_context(|| format!("write {}", summary_path.display()))?;
    println!("\nWrote {}", summary_path.display());
    println!("\n--- summary preview ---\n{summary}");
    Ok(())
}

fn parse_targets(args: Vec<String>) -> Result<String> {
    let mut targets = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--targets" => {
                i += 1;
                let value = args
                    .get(i)
                    .context("--targets requires a comma-separated value")?;
                targets = Some(value.clone());
            }
            "-h" | "--help" => {
                println!(
                    "Usage: research_ab [--targets \"T1, T2, ...\"]\nDefault targets: {DEFAULT_TARGETS}"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }
    Ok(targets.unwrap_or_else(|| DEFAULT_TARGETS.to_string()))
}

fn load_philosophy() -> InvestmentPhilosophy {
    let cfg = load_config(config_path());
    cfg.investment_philosophy
        .unwrap_or_else(InvestmentPhilosophy::default)
}

fn write_arm_artifacts(
    arm_dir: &Path,
    artifacts: &finance_advisor::ResearchRunArtifacts,
) -> Result<()> {
    fs::create_dir_all(arm_dir)?;
    fs::write(arm_dir.join("synthesis.md"), &artifacts.synthesis)?;

    let universe_json = serde_json::json!({
        "finnhub_used": artifacts.finnhub_used,
        "tickers": artifacts.universe.iter().map(|u| {
            serde_json::json!({
                "ticker": u.ticker,
                "company": u.company,
                "exchange": u.exchange,
                "market_cap_band": u.market_cap_band,
                "market_cap_m": u.market_cap_m,
            })
        }).collect::<Vec<_>>(),
        "dropped": artifacts.dropped.iter().map(|d| {
            serde_json::json!({
                "ticker": d.ticker,
                "company": d.company,
                "reason": d.reason,
            })
        }).collect::<Vec<_>>(),
    });
    fs::write(
        arm_dir.join("universe.json"),
        serde_json::to_string_pretty(&universe_json)?,
    )?;

    for draft in &artifacts.score_drafts {
        let slug = sanitize_slug(&draft.model_id);
        fs::write(arm_dir.join(format!("score-{slug}.json")), &draft.raw_json)?;
    }
    Ok(())
}

fn sanitize_slug(model_id: &str) -> String {
    model_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn short_label(model_id: &str) -> String {
    model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .to_string()
}

fn parse_scores(drafts: &[StageDraft]) -> Vec<(String, Vec<ScoredCandidate>)> {
    let mut out = Vec::new();
    for draft in drafts {
        match serde_json::from_str::<ScoreReport>(&draft.raw_json) {
            Ok(report) => out.push((draft.model_id.clone(), report.scores)),
            Err(e) => eprintln!(
                "research_ab: could not parse scores for {}: {e}",
                draft.model_id
            ),
        }
    }
    out
}

fn mean_fit_by_ticker(parsed: &[(String, Vec<ScoredCandidate>)]) -> BTreeMap<String, f64> {
    let mut sums: HashMap<String, (i64, i32)> = HashMap::new();
    for (_, scores) in parsed {
        for s in scores {
            let entry = sums.entry(s.ticker.to_ascii_uppercase()).or_insert((0, 0));
            entry.0 += s.fit_score as i64;
            entry.1 += 1;
        }
    }
    sums.into_iter()
        .map(|(t, (sum, n))| (t, sum as f64 / n as f64))
        .collect()
}

fn shortlist_from_means(means: &BTreeMap<String, f64>, n: usize) -> Vec<String> {
    let mut pairs: Vec<_> = means.iter().collect();
    pairs.sort_by(|a, b| {
        b.1.partial_cmp(a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    pairs.into_iter().take(n).map(|(t, _)| t.clone()).collect()
}

fn tickers_mentioned(synthesis: &str, universe: &[UniverseEntry]) -> Vec<String> {
    let upper = synthesis.to_ascii_uppercase();
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for u in universe {
        let t = u.ticker.to_ascii_uppercase();
        // Word-ish match: ticker bounded by non-alphanumeric (or edges).
        let bytes = upper.as_bytes();
        let tb = t.as_bytes();
        let mut start = 0;
        while let Some(rel) = upper[start..].find(&t) {
            let i = start + rel;
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = i + tb.len();
            let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
            if before_ok && after_ok && seen.insert(t.clone()) {
                found.push(t.clone());
                break;
            }
            start = i + 1;
        }
    }
    found
}

fn build_summary(
    targets: &str,
    philosophy: &InvestmentPhilosophy,
    arms: &[(&ArmSpec, finance_advisor::ResearchRunArtifacts, u128)],
) -> String {
    let mut md = String::new();
    md.push_str("# Research panel A/B — Sol vs Qwen3.8-Max\n\n");
    md.push_str(&format!(
        "- **Seed targets:** `{targets}`\n- **Judge (both arms):** `{DEFAULT_JUDGE_MODEL}`\n"
    ));
    md.push_str(&format!(
        "- **Philosophy:** {}\n",
        philosophy.description
    ));
    md.push_str(
        "- **Note:** Single-pass seeded run; LLM variance means this is directional, not definitive.\n\n",
    );

    if let Some((_, arts, _)) = arms.first() {
        md.push_str("## Universe\n\n");
        md.push_str("| Ticker | Company | Cap (USD M) | Band |\n|---|---|---:|---|\n");
        for u in &arts.universe {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                u.ticker,
                u.company.replace('|', "/"),
                u.market_cap_m
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "—".into()),
                u.market_cap_band
                    .map(|b| format!("{b:?}"))
                    .unwrap_or_else(|| "—".into()),
            ));
        }
        md.push('\n');
    }

    for (arm, arts, elapsed_ms) in arms {
        md.push_str(&format!("## {}\n\n", arm.label));
        md.push_str(&format!(
            "- Panel: `{}`\n- Elapsed: {:.1}s\n- Scorers succeeded: {}\n- Finnhub: {}\n\n",
            arm.panel.join(", "),
            *elapsed_ms as f64 / 1000.0,
            arts.score_drafts.len(),
            arts.finnhub_used,
        ));

        let parsed = parse_scores(&arts.score_drafts);
        let means = mean_fit_by_ticker(&parsed);
        let score_shortlist = shortlist_from_means(&means, 3);
        let synth_tickers = tickers_mentioned(&arts.synthesis, &arts.universe);

        md.push_str(&format!(
            "- **Mean-fit top 3:** {}\n- **Tickers mentioned in synthesis (order found):** {}\n\n",
            score_shortlist.join(", "),
            if synth_tickers.is_empty() {
                "—".into()
            } else {
                synth_tickers.join(", ")
            }
        ));

        md.push_str("### Score table (fit_score)\n\n");
        let mut tickers: Vec<String> = arts.universe.iter().map(|u| u.ticker.clone()).collect();
        tickers.sort();
        md.push_str("| Ticker |");
        for (model, _) in &parsed {
            md.push_str(&format!(" {} |", short_label(model)));
        }
        md.push_str(" Mean |\n|---|");
        for _ in &parsed {
            md.push_str("---:|");
        }
        md.push_str("---:|\n");

        for t in &tickers {
            md.push_str(&format!("| {t} |"));
            for (_, scores) in &parsed {
                let cell = scores
                    .iter()
                    .find(|s| s.ticker.eq_ignore_ascii_case(t))
                    .map(|s| format!("{}", s.fit_score))
                    .unwrap_or_else(|| "—".into());
                md.push_str(&format!(" {cell} |"));
            }
            let mean = means
                .get(&t.to_ascii_uppercase())
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".into());
            md.push_str(&format!(" {mean} |\n"));
        }
        md.push('\n');
    }

    if arms.len() >= 2 {
        let (a_arm, a_arts, _) = &arms[0];
        let (b_arm, b_arts, _) = &arms[1];
        let a_means = mean_fit_by_ticker(&parse_scores(&a_arts.score_drafts));
        let b_means = mean_fit_by_ticker(&parse_scores(&b_arts.score_drafts));
        let a_top = shortlist_from_means(&a_means, 3);
        let b_top = shortlist_from_means(&b_means, 3);
        let a_set: BTreeSet<_> = a_top.iter().cloned().collect();
        let b_set: BTreeSet<_> = b_top.iter().cloned().collect();
        let overlap: Vec<_> = a_set.intersection(&b_set).cloned().collect();
        let only_a: Vec<_> = a_set.difference(&b_set).cloned().collect();
        let only_b: Vec<_> = b_set.difference(&a_set).cloned().collect();

        md.push_str("## Agreement / disagreement\n\n");
        md.push_str(&format!(
            "- **{} mean-fit top 3:** {}\n- **{} mean-fit top 3:** {}\n",
            a_arm.id,
            a_top.join(", "),
            b_arm.id,
            b_top.join(", "),
        ));
        md.push_str(&format!(
            "- **Overlap:** {}\n- **Only A:** {}\n- **Only B:** {}\n\n",
            if overlap.is_empty() {
                "none".into()
            } else {
                overlap.join(", ")
            },
            if only_a.is_empty() {
                "none".into()
            } else {
                only_a.join(", ")
            },
            if only_b.is_empty() {
                "none".into()
            } else {
                only_b.join(", ")
            },
        ));

        md.push_str("### Mean fit delta (B − A)\n\n");
        md.push_str("| Ticker | A mean | B mean | Δ |\n|---|---:|---:|---:|\n");
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(a_means.keys().cloned());
        all.extend(b_means.keys().cloned());
        for t in all {
            let a = a_means.get(&t).copied();
            let b = b_means.get(&t).copied();
            let delta = match (a, b) {
                (Some(av), Some(bv)) => format!("{:.1}", bv - av),
                _ => "—".into(),
            };
            md.push_str(&format!(
                "| {t} | {} | {} | {delta} |\n",
                a.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".into()),
                b.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".into()),
            ));
        }
        md.push('\n');
    }

    md.push_str("## Manual review checklist\n\n");
    md.push_str(
        "- Shortlist overlap / rank stability vs philosophy (moat, unit economics, 100x narrative)\n",
    );
    md.push_str("- Hallucinated tickers or broken structured scores\n");
    md.push_str("- Relative cost: Sol ~$5/$30 vs Qwen3.8-Max ~$2/$6 per 1M tokens\n");
    md.push_str("- Change `DEFAULT_PANEL_MODELS` only if B is clearly as good or better\n");
    md
}
