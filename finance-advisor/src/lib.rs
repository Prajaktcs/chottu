use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use anyhow::Context;
use chotu_common::{
    AppConfig, CapBand, FinancialLedgerEntry, FinnhubClient, InvestmentPhilosophy,
    OpenRouterClient, TargetAllocation,
};
use futures::future::join_all;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PANEL_MODELS: &[&str] = &[
    "openai/gpt-5.6-sol",
    "anthropic/claude-opus-5",
    "moonshotai/kimi-k3",
];
pub const DEFAULT_JUDGE_MODEL: &str = "moonshotai/kimi-k3";
pub const MAX_UNIVERSE_SIZE: usize = 12;
pub const MAX_PROPOSALS_PER_MODEL: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MarketCapBand {
    Micro,
    Small,
    Mid,
    Large,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum Conviction {
    High,
    Medium,
    Low,
    Pass,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProposeCandidate {
    pub ticker: String,
    pub company: String,
    pub one_line_why: String,
    pub market_cap_band: MarketCapBand,
    pub exchange: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProposeList {
    pub proposals: Vec<ProposeCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScoredCandidate {
    pub ticker: String,
    pub company: String,
    pub fit_score: i32,
    pub conviction: Conviction,
    pub thesis: String,
    pub catalysts: Vec<String>,
    pub risks: Vec<String>,
    pub hundred_bagger_plausible: bool,
    #[serde(default)]
    pub pass_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScoreReport {
    pub scores: Vec<ScoredCandidate>,
}

#[derive(Debug, Clone)]
pub struct StageDraft {
    pub model_id: String,
    pub raw_json: String,
}

#[derive(Debug, Clone)]
pub struct UniverseEntry {
    pub ticker: String,
    pub company: String,
    pub exchange: Option<String>,
    pub market_cap_band: Option<MarketCapBand>,
    /// Finnhub market cap in USD millions when enriched.
    pub market_cap_m: Option<f64>,
    pub proposed_by: Vec<String>,
    pub one_line_why: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DroppedUniverseEntry {
    pub ticker: String,
    pub company: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ResearchRunArtifacts {
    pub synthesis: String,
    pub universe: Vec<UniverseEntry>,
    pub propose_drafts: Vec<StageDraft>,
    pub score_drafts: Vec<StageDraft>,
    pub dropped: Vec<DroppedUniverseEntry>,
    pub finnhub_used: bool,
}

/// Stage updates for UX (Telegram, logs, etc.).
#[derive(Debug, Clone)]
pub enum ResearchProgress {
    /// Discovery run: propose → universe → score → judge (4 stages).
    /// Seeded run: universe → score → judge (3 stages).
    Started {
        total_stages: u32,
        seeded: bool,
        panel: String,
        judge: String,
    },
    Proposing {
        stage: u32,
        total_stages: u32,
        model_count: usize,
    },
    UniverseReady {
        stage: u32,
        total_stages: u32,
        tickers: Vec<String>,
        from_propose: bool,
        finnhub_filtered: bool,
        dropped_count: usize,
        lookup_misses: usize,
    },
    Scoring {
        stage: u32,
        total_stages: u32,
        universe_size: usize,
        model_count: usize,
    },
    ScoringDone {
        stage: u32,
        total_stages: u32,
        succeeded: usize,
        failed: usize,
    },
    Judging {
        stage: u32,
        total_stages: u32,
        judge: String,
    },
    Saving {
        stage: u32,
        total_stages: u32,
    },
}

async fn emit_progress(
    tx: Option<&tokio::sync::mpsc::Sender<ResearchProgress>>,
    event: ResearchProgress,
) {
    if let Some(tx) = tx {
        let _ = tx.send(event).await;
    }
}

#[derive(Debug, Clone)]
pub struct StockResearcher {
    client: Option<OpenRouterClient>,
    panel_models: Vec<String>,
    judge_model: String,
}

impl StockResearcher {
    pub fn new(api_key: String) -> Self {
        let client = OpenRouterClient::new(&api_key).ok();
        Self {
            client,
            panel_models: panel_models_from_env(),
            judge_model: judge_model_from_env(),
        }
    }

    /// Prefer `OPENROUTER_API_KEY`; returns a researcher that errors clearly if unset.
    pub fn from_env() -> Self {
        let client = OpenRouterClient::from_env().ok();
        Self {
            client,
            panel_models: panel_models_from_env(),
            judge_model: judge_model_from_env(),
        }
    }

    /// Build with an explicit panel + judge (e.g. A/B harnesses). Uses `OPENROUTER_API_KEY`.
    pub fn with_models(panel_models: Vec<String>, judge_model: String) -> Self {
        let client = OpenRouterClient::from_env().ok();
        Self {
            client,
            panel_models,
            judge_model,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.client.is_some()
    }

    pub fn panel_models(&self) -> &[String] {
        &self.panel_models
    }

    pub fn judge_model(&self) -> &str {
        &self.judge_model
    }

    pub fn panel_display_names(&self) -> String {
        self.panel_models
            .iter()
            .map(|m| short_model_label(m))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Run shared-universe research: propose (or seed) → score → judge.
    pub async fn perform_research(
        &self,
        targets: Option<&str>,
        philosophy: Option<&InvestmentPhilosophy>,
    ) -> Result<String, anyhow::Error> {
        let artifacts = self
            .perform_research_with_artifacts(targets, philosophy, None)
            .await?;
        Ok(artifacts.synthesis)
    }

    /// Full pipeline with stage drafts for disk persistence.
    pub async fn perform_research_with_artifacts(
        &self,
        targets: Option<&str>,
        philosophy: Option<&InvestmentPhilosophy>,
        progress: Option<tokio::sync::mpsc::Sender<ResearchProgress>>,
    ) -> Result<ResearchRunArtifacts, anyhow::Error> {
        let client = self.client.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "OPENROUTER_API_KEY is not set. Add it to `.env` to enable multi-model stock research."
            )
        })?;

        let default_philosophy = InvestmentPhilosophy::default();
        let p = philosophy.unwrap_or(&default_philosophy);
        let seeded = matches!(targets, Some(t) if !t.trim().is_empty());
        let total_stages: u32 = if seeded { 3 } else { 4 };
        let progress = progress.as_ref();
        let finnhub = FinnhubClient::from_env().ok();
        if finnhub.is_none() {
            eprintln!(
                "Finance Advisor: FINNHUB_API_KEY not set — using model-estimated market-cap bands."
            );
        }

        emit_progress(
            progress,
            ResearchProgress::Started {
                total_stages,
                seeded,
                panel: self.panel_display_names(),
                judge: short_model_label(&self.judge_model),
            },
        )
        .await;

        let (mut universe, propose_drafts, mut stage) = if seeded {
            let t = targets.unwrap();
            let universe = universe_from_targets(t)?;
            (universe, Vec::new(), 1u32)
        } else {
            emit_progress(
                progress,
                ResearchProgress::Proposing {
                    stage: 1,
                    total_stages,
                    model_count: self.panel_models.len(),
                },
            )
            .await;
            let (drafts, lists) = self.run_propose_stage(client, p).await?;
            // When Finnhub is available it is the authority; otherwise keep model Micro/Small gate.
            let universe = build_shared_universe(&lists, finnhub.is_none())?;
            (universe, drafts, 2u32)
        };

        let enrich = if let Some(ref fh) = finnhub {
            enrich_universe_with_finnhub(&mut universe, fh, !seeded).await?
        } else {
            EnrichResult::default()
        };

        emit_progress(
            progress,
            ResearchProgress::UniverseReady {
                stage,
                total_stages,
                tickers: universe.iter().map(|e| e.ticker.clone()).collect(),
                from_propose: !seeded,
                finnhub_filtered: finnhub.is_some(),
                dropped_count: enrich.dropped.len(),
                lookup_misses: enrich.lookup_misses,
            },
        )
        .await;

        if universe.is_empty() {
            return Err(anyhow::anyhow!(
                "Shared universe is empty after filtering; cannot score. {}",
                if finnhub.is_some() {
                    "All proposals failed the Finnhub micro/small gate or profile lookup."
                } else {
                    "Check model proposals / market-cap bands."
                }
            ));
        }

        stage += 1;
        emit_progress(
            progress,
            ResearchProgress::Scoring {
                stage,
                total_stages,
                universe_size: universe.len(),
                model_count: self.panel_models.len(),
            },
        )
        .await;

        let (score_drafts, score_failures) = self.run_score_stage(client, p, &universe).await?;
        emit_progress(
            progress,
            ResearchProgress::ScoringDone {
                stage,
                total_stages,
                succeeded: score_drafts.len(),
                failed: score_failures.len(),
            },
        )
        .await;

        if score_drafts.len() < 2 {
            return Err(anyhow::anyhow!(
                "Stock research needs at least 2 successful scorers; got {}. Failures: {}",
                score_drafts.len(),
                if score_failures.is_empty() {
                    "none".to_string()
                } else {
                    score_failures.join("; ")
                }
            ));
        }

        stage += 1;
        emit_progress(
            progress,
            ResearchProgress::Judging {
                stage,
                total_stages,
                judge: short_model_label(&self.judge_model),
            },
        )
        .await;

        let synthesis = self
            .run_judge(client, p, &universe, &score_drafts)
            .await
            .context("Judge synthesis failed")?;

        let mut full = synthesis;
        full.push_str("\n\n---\n_Universe:_ ");
        full.push_str(
            &universe
                .iter()
                .map(|e| e.ticker.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        full.push_str("\n_Scorers:_ ");
        full.push_str(
            &score_drafts
                .iter()
                .map(|s| short_model_label(&s.model_id))
                .collect::<Vec<_>>()
                .join(", "),
        );
        full.push_str(" · _Judge:_ ");
        full.push_str(&short_model_label(&self.judge_model));
        if !score_failures.is_empty() {
            full.push_str("\n_Failed scorers:_ ");
            full.push_str(&score_failures.join("; "));
        }

        Ok(ResearchRunArtifacts {
            synthesis: full,
            universe,
            propose_drafts,
            score_drafts,
            dropped: enrich.dropped,
            finnhub_used: finnhub.is_some(),
        })
    }

    /// Backward-compatible alias used by older call sites.
    pub async fn perform_research_with_panel(
        &self,
        targets: Option<&str>,
        philosophy: Option<&InvestmentPhilosophy>,
    ) -> Result<(String, Vec<StageDraft>), anyhow::Error> {
        let artifacts = self
            .perform_research_with_artifacts(targets, philosophy, None)
            .await?;
        Ok((artifacts.synthesis, artifacts.score_drafts))
    }

    async fn run_propose_stage(
        &self,
        client: &OpenRouterClient,
        philosophy: &InvestmentPhilosophy,
    ) -> Result<(Vec<StageDraft>, Vec<(String, ProposeList)>), anyhow::Error> {
        let (system_prompt, user_prompt) = build_propose_prompts(philosophy);
        let futures = self.panel_models.iter().map(|model| {
            let system_prompt = system_prompt.clone();
            let user_prompt = user_prompt.clone();
            let model = model.clone();
            async move {
                let result = client
                    .generate_structured::<ProposeList>(&model, &system_prompt, &user_prompt)
                    .await;
                (model, result)
            }
        });

        let outcomes = join_all(futures).await;
        let mut drafts = Vec::new();
        let mut lists = Vec::new();
        let mut failures = Vec::new();

        for (model, result) in outcomes {
            match result {
                Ok(mut list) => {
                    if list.proposals.len() > MAX_PROPOSALS_PER_MODEL {
                        list.proposals.truncate(MAX_PROPOSALS_PER_MODEL);
                    }
                    let raw_json = serde_json::to_string_pretty(&list)
                        .unwrap_or_else(|_| format!("{:?}", list));
                    drafts.push(StageDraft {
                        model_id: model.clone(),
                        raw_json,
                    });
                    lists.push((model, list));
                }
                Err(e) => {
                    eprintln!("Finance Advisor: propose model {} failed: {:?}", model, e);
                    failures.push(format!("{}: {}", model, e));
                }
            }
        }

        if lists.is_empty() {
            return Err(anyhow::anyhow!(
                "All proposers failed; cannot build shared universe. Failures: {}",
                failures.join("; ")
            ));
        }

        Ok((drafts, lists))
    }

    async fn run_score_stage(
        &self,
        client: &OpenRouterClient,
        philosophy: &InvestmentPhilosophy,
        universe: &[UniverseEntry],
    ) -> Result<(Vec<StageDraft>, Vec<String>), anyhow::Error> {
        let (system_prompt, user_prompt) = build_score_prompts(philosophy, universe);
        let futures = self.panel_models.iter().map(|model| {
            let system_prompt = system_prompt.clone();
            let user_prompt = user_prompt.clone();
            let model = model.clone();
            async move {
                let result = client
                    .generate_structured::<ScoreReport>(&model, &system_prompt, &user_prompt)
                    .await;
                (model, result)
            }
        });

        let outcomes = join_all(futures).await;
        let mut drafts = Vec::new();
        let mut failures = Vec::new();

        for (model, result) in outcomes {
            match result {
                Ok(report) => {
                    if let Err(reason) = validate_score_report(universe, &report) {
                        eprintln!(
                            "Finance Advisor: score model {} rejected: {}",
                            model, reason
                        );
                        failures.push(format!("{}: {}", model, reason));
                        continue;
                    }
                    let raw_json = serde_json::to_string_pretty(&report)
                        .unwrap_or_else(|_| format!("{:?}", report));
                    drafts.push(StageDraft {
                        model_id: model,
                        raw_json,
                    });
                }
                Err(e) => {
                    eprintln!("Finance Advisor: score model {} failed: {:?}", model, e);
                    failures.push(format!("{}: {}", model, e));
                }
            }
        }

        Ok((drafts, failures))
    }

    async fn run_judge(
        &self,
        client: &OpenRouterClient,
        philosophy: &InvestmentPhilosophy,
        universe: &[UniverseEntry],
        score_drafts: &[StageDraft],
    ) -> Result<String, anyhow::Error> {
        let focus_areas_str = philosophy
            .focus_areas
            .iter()
            .enumerate()
            .map(|(i, area)| format!("{}. {}", i + 1, area))
            .collect::<Vec<_>>()
            .join(", ");

        let system_prompt = format!(
            "You are a skeptical equity research editor. Multiple analyst models scored the SAME \
             shared ticker universe for an investment philosophy specializing in {}. Focus areas: {}. \
             Produce a clean Telegram-ready Markdown report with:\n\
             1. Ranked shortlist of 2–3 picks (ONLY tickers from the shared universe — never invent tickers)\n\
             2. For each pick: thesis, catalysts, risks, which scorers supported it\n\
             3. Explicit disagreements between scorers on the same names\n\
             4. A kill list of universe candidates you reject and why\n\
             5. Overall confidence (low/medium/high) with a one-line rationale\n\
             Prefer disagreement and skepticism over false consensus. Do not pad with fluff.",
            philosophy.description, focus_areas_str
        );

        let mut user_prompt = String::from("Shared universe:\n");
        for entry in universe {
            user_prompt.push_str(&format!(
                "- {} ({}) proposed_by=[{}] band={:?} exchange={:?} why={}\n",
                entry.ticker,
                entry.company,
                entry.proposed_by.join(", "),
                entry.market_cap_band,
                entry.exchange,
                entry.one_line_why.as_deref().unwrap_or("-")
            ));
        }
        user_prompt.push_str("\nScorer reports (JSON):\n\n");
        for (i, draft) in score_drafts.iter().enumerate() {
            user_prompt.push_str(&format!(
                "### Scorer {} — {}\n```json\n{}\n```\n\n",
                i + 1,
                draft.model_id,
                draft.raw_json
            ));
        }
        user_prompt.push_str(
            "Synthesize the final Markdown research memo for the household investment chat.",
        );

        client
            .generate_prompt(&self.judge_model, &system_prompt, &user_prompt)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

fn panel_models_from_env() -> Vec<String> {
    match std::env::var("RESEARCH_PANEL_MODELS") {
        Ok(raw) if !raw.trim().is_empty() => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_PANEL_MODELS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    }
}

fn judge_model_from_env() -> String {
    std::env::var("RESEARCH_JUDGE_MODEL").unwrap_or_else(|_| DEFAULT_JUDGE_MODEL.to_string())
}

fn short_model_label(model_id: &str) -> String {
    model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .to_string()
}

fn sanitize_model_slug(model_id: &str) -> String {
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

/// Normalize tickers: trim, uppercase; keep letters, digits, `.`, `-`.
pub fn normalize_ticker(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('$')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

pub fn universe_from_targets(targets: &str) -> Result<Vec<UniverseEntry>, anyhow::Error> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for part in targets.split(',') {
        let label = part.trim();
        if label.is_empty() {
            continue;
        }
        let ticker = normalize_ticker(label);
        if ticker.is_empty() || !seen.insert(ticker.clone()) {
            continue;
        }
        entries.push(UniverseEntry {
            ticker: ticker.clone(),
            company: label.to_string(),
            exchange: None,
            market_cap_band: None,
            market_cap_m: None,
            proposed_by: vec!["user".to_string()],
            one_line_why: Some("Seeded via /research arguments".to_string()),
        });
        if entries.len() >= MAX_UNIVERSE_SIZE {
            break;
        }
    }

    if entries.is_empty() {
        return Err(anyhow::anyhow!(
            "Could not parse any tickers/names from research targets: {}",
            targets
        ));
    }
    Ok(entries)
}

/// Union proposes with optional Mid/Large drop, dedupe, round-robin cap.
/// When `apply_model_band_filter` is false (Finnhub will filter), Mid/Large model guesses are kept.
pub fn build_shared_universe(
    proposes: &[(String, ProposeList)],
    apply_model_band_filter: bool,
) -> Result<Vec<UniverseEntry>, anyhow::Error> {
    // Per-proposer queues of eligible candidates (already truncated to max 4 upstream).
    let mut queues: Vec<(String, Vec<ProposeCandidate>)> = proposes
        .iter()
        .map(|(model, list)| {
            let eligible: Vec<_> = list
                .proposals
                .iter()
                .filter(|c| {
                    !apply_model_band_filter
                        || matches!(
                            c.market_cap_band,
                            MarketCapBand::Micro | MarketCapBand::Small
                        )
                })
                .cloned()
                .collect();
            (model.clone(), eligible)
        })
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut by_ticker: HashMap<String, UniverseEntry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    // Round-robin so one model cannot flood the list.
    loop {
        if order.len() >= MAX_UNIVERSE_SIZE {
            break;
        }
        let mut progressed = false;
        for (model, queue) in queues.iter_mut() {
            if order.len() >= MAX_UNIVERSE_SIZE {
                break;
            }
            while let Some(cand) = queue.first().cloned() {
                queue.remove(0);
                let ticker = normalize_ticker(&cand.ticker);
                if ticker.is_empty() {
                    continue;
                }
                progressed = true;
                if let Some(existing) = by_ticker.get_mut(&ticker) {
                    if !existing.proposed_by.contains(model) {
                        existing.proposed_by.push(model.clone());
                    }
                } else if seen.insert(ticker.clone()) {
                    order.push(ticker.clone());
                    by_ticker.insert(
                        ticker.clone(),
                        UniverseEntry {
                            ticker,
                            company: cand.company,
                            exchange: Some(cand.exchange),
                            market_cap_band: Some(cand.market_cap_band),
                            market_cap_m: None,
                            proposed_by: vec![model.clone()],
                            one_line_why: Some(cand.one_line_why),
                        },
                    );
                }
                break;
            }
        }
        if !progressed {
            break;
        }
    }

    let universe: Vec<_> = order
        .into_iter()
        .filter_map(|t| by_ticker.remove(&t))
        .collect();

    if universe.is_empty() {
        return Err(anyhow::anyhow!(
            "Shared universe empty after deduping proposals{}.",
            if apply_model_band_filter {
                " (model Mid/Large dropped)"
            } else {
                ""
            }
        ));
    }
    Ok(universe)
}

#[derive(Debug, Default)]
pub struct EnrichResult {
    pub dropped: Vec<DroppedUniverseEntry>,
    pub lookup_misses: usize,
}

fn cap_band_to_market(band: CapBand) -> MarketCapBand {
    match band {
        CapBand::Micro => MarketCapBand::Micro,
        CapBand::Small => MarketCapBand::Small,
        CapBand::Mid => MarketCapBand::Mid,
        CapBand::Large => MarketCapBand::Large,
    }
}

/// Enrich universe via Finnhub profile2. Discovery drops Mid/Large/unresolved; seeded keeps all.
pub async fn enrich_universe_with_finnhub(
    universe: &mut Vec<UniverseEntry>,
    client: &FinnhubClient,
    discovery: bool,
) -> Result<EnrichResult, anyhow::Error> {
    let tickers: Vec<String> = universe.iter().map(|e| e.ticker.clone()).collect();
    let lookups = client.lookup_profiles(&tickers).await;
    let mut by_ticker: HashMap<String, chotu_common::CompanyProfile> = HashMap::new();
    let mut lookup_misses = 0usize;

    for (ticker, result) in lookups {
        match result {
            Ok(profile) => {
                by_ticker.insert(normalize_ticker(&ticker), profile);
            }
            Err(e) => {
                eprintln!("Finance Advisor: Finnhub lookup failed for {ticker}: {e}");
                lookup_misses += 1;
            }
        }
    }

    let mut result = apply_finnhub_profiles(universe, &by_ticker, discovery);
    result.lookup_misses = lookup_misses;
    Ok(result)
}

/// Pure enrich/filter step (unit-testable without HTTP).
pub fn apply_finnhub_profiles(
    universe: &mut Vec<UniverseEntry>,
    by_ticker: &HashMap<String, chotu_common::CompanyProfile>,
    discovery: bool,
) -> EnrichResult {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();

    for entry in universe.drain(..) {
        let key = normalize_ticker(&entry.ticker);
        match by_ticker.get(&key) {
            Some(profile) => {
                let band = cap_band_to_market(profile.cap_band);
                let mut enriched = entry;
                enriched.company = profile.name.clone();
                enriched.exchange = profile.exchange.clone();
                enriched.market_cap_band = Some(band);
                enriched.market_cap_m = Some(profile.market_cap_m);
                if !profile.symbol.is_empty() {
                    enriched.ticker = normalize_ticker(&profile.symbol);
                }

                if discovery && !matches!(band, MarketCapBand::Micro | MarketCapBand::Small) {
                    dropped.push(DroppedUniverseEntry {
                        ticker: enriched.ticker,
                        company: enriched.company,
                        reason: format!(
                            "Finnhub band {:?} (cap ${:.1}M) outside micro/small mandate",
                            band, profile.market_cap_m
                        ),
                    });
                } else {
                    kept.push(enriched);
                }
            }
            None => {
                if discovery {
                    dropped.push(DroppedUniverseEntry {
                        ticker: entry.ticker.clone(),
                        company: entry.company.clone(),
                        reason: "Finnhub profile not found".to_string(),
                    });
                } else {
                    kept.push(entry);
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for entry in kept {
        if seen.insert(entry.ticker.clone()) {
            deduped.push(entry);
        }
    }
    *universe = deduped;

    EnrichResult {
        dropped,
        lookup_misses: 0,
    }
}

/// Ensure a score report covers exactly the shared universe (no missing / invented tickers).
pub fn validate_score_report(
    universe: &[UniverseEntry],
    report: &ScoreReport,
) -> Result<(), String> {
    let expected: HashSet<String> = universe.iter().map(|e| e.ticker.clone()).collect();
    let mut seen: HashSet<String> = HashSet::new();

    for score in &report.scores {
        let ticker = normalize_ticker(&score.ticker);
        if ticker.is_empty() {
            return Err("score entry has empty ticker".into());
        }
        if !expected.contains(&ticker) {
            return Err(format!("invented ticker not in universe: {ticker}"));
        }
        if !seen.insert(ticker.clone()) {
            return Err(format!("duplicate ticker in score report: {ticker}"));
        }
    }

    let missing: Vec<_> = expected.difference(&seen).cloned().collect();
    if !missing.is_empty() {
        return Err(format!("missing universe tickers: {}", missing.join(", ")));
    }
    Ok(())
}

fn focus_areas_str(philosophy: &InvestmentPhilosophy) -> String {
    philosophy
        .focus_areas
        .iter()
        .enumerate()
        .map(|(i, area)| format!("{}. {}", i + 1, area))
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_propose_prompts(philosophy: &InvestmentPhilosophy) -> (String, String) {
    let system_prompt = format!(
        "You are an equity idea generator specializing in {}. Focus on: {}. \
         Propose 3–4 micro-cap or small-cap ticker ideas only. Do NOT write a full thesis. \
         For each name provide ticker, company, one_line_why, market_cap_band \
         (Micro|Small|Mid|Large — estimate), and exchange. Prefer Micro/Small; avoid Mid/Large.",
        philosophy.description,
        focus_areas_str(philosophy)
    );
    let user_prompt = format!(
        "Propose up to {} distinct public companies that could fit this hundred-bagger / \
         micro-small-cap mandate. Output structured proposals only.",
        MAX_PROPOSALS_PER_MODEL
    );
    (system_prompt, user_prompt)
}

fn build_score_prompts(
    philosophy: &InvestmentPhilosophy,
    universe: &[UniverseEntry],
) -> (String, String) {
    let system_prompt = format!(
        "You are a professional equity research analyst specializing in {}. Focus on: {}. \
         Score EVERY name in the provided shared universe. Do NOT add new tickers. \
         Be willing to mark conviction Pass or set hundred_bagger_plausible=false when warranted. \
         Provide fit_score 1-10, conviction (High|Medium|Low|Pass), thesis, catalysts, risks, \
         hundred_bagger_plausible, and optional pass_reason.",
        philosophy.description,
        focus_areas_str(philosophy)
    );

    let mut user_prompt = String::from("Score this shared universe (same list for every analyst):\n\n");
    for entry in universe {
        let cap = entry
            .market_cap_m
            .map(|m| format!("${m:.1}M"))
            .unwrap_or_else(|| "unknown".into());
        user_prompt.push_str(&format!(
            "- ticker={} company={} exchange={} band={:?} market_cap={} note={}\n",
            entry.ticker,
            entry.company,
            entry.exchange.as_deref().unwrap_or("unknown"),
            entry.market_cap_band,
            cap,
            entry.one_line_why.as_deref().unwrap_or("-")
        ));
    }
    user_prompt.push_str(
        "\nReturn one score object per universe ticker. Do not invent additional tickers.",
    );
    (system_prompt, user_prompt)
}

fn format_universe_markdown(
    universe: &[UniverseEntry],
    dropped: &[DroppedUniverseEntry],
    finnhub_used: bool,
) -> String {
    let mut out = String::from("# Shared research universe\n\n");
    out.push_str(&format!(
        "_Finnhub enrichment: {}_\n\n",
        if finnhub_used { "yes" } else { "no (model bands only)" }
    ));
    for (i, entry) in universe.iter().enumerate() {
        let cap = entry
            .market_cap_m
            .map(|m| format!("${m:.1}M"))
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{}. **{}** — {}\n   - proposed_by: {}\n   - band: {:?}\n   - market_cap: {}\n   - exchange: {}\n   - why: {}\n\n",
            i + 1,
            entry.ticker,
            entry.company,
            entry.proposed_by.join(", "),
            entry.market_cap_band,
            cap,
            entry.exchange.as_deref().unwrap_or("-"),
            entry.one_line_why.as_deref().unwrap_or("-")
        ));
    }
    if !dropped.is_empty() {
        out.push_str("## Dropped by Finnhub filter\n\n");
        for d in dropped {
            out.push_str(&format!(
                "- **{}** ({}) — {}\n",
                d.ticker, d.company, d.reason
            ));
        }
        out.push('\n');
    }
    out
}

/// Runs shared-universe research, writes markdown files, logs to sqlite.
pub async fn run_stock_research(
    pool: &sqlx::SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
) -> Result<String, anyhow::Error> {
    run_stock_research_with_progress(pool, researcher, philosophy, targets, None).await
}

/// Same as [`run_stock_research`], with optional stage progress events.
pub async fn run_stock_research_with_progress(
    pool: &sqlx::SqlitePool,
    researcher: &StockResearcher,
    philosophy: Option<&InvestmentPhilosophy>,
    targets: Option<&str>,
    progress: Option<tokio::sync::mpsc::Sender<ResearchProgress>>,
) -> Result<String, anyhow::Error> {
    let artifacts = researcher
        .perform_research_with_artifacts(targets, philosophy, progress.clone())
        .await?;

    let total_stages: u32 = if matches!(targets, Some(t) if !t.trim().is_empty()) {
        3
    } else {
        4
    };
    emit_progress(
        progress.as_ref(),
        ResearchProgress::Saving {
            stage: total_stages,
            total_stages,
        },
    )
    .await;

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() != 3 {
        return Err(anyhow::anyhow!(
            "Invalid date format for stock research save: {}",
            date_str
        ));
    }
    let year = parts[0];
    let month = parts[1];

    let brain_dir_str =
        std::env::var("CHOTU_BRAIN_DIR").unwrap_or_else(|_| "~/chotu_brain".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/user".to_string());
    let brain_path = PathBuf::from(brain_dir_str.replace("~", &home));
    let target_dir = brain_path.join("Research").join(year).join(month);
    tokio::fs::create_dir_all(&target_dir)
        .await
        .context("Failed to create target stock research directory")?;

    let file_path = target_dir.join(format!("{}-stocks.md", date_str));
    tokio::fs::write(&file_path, &artifacts.synthesis)
        .await
        .with_context(|| format!("Failed to write stock research file to {:?}", file_path))?;

    let universe_path = target_dir.join(format!("{}-stocks-universe.md", date_str));
    if let Err(e) =
        tokio::fs::write(
            &universe_path,
            format_universe_markdown(
                &artifacts.universe,
                &artifacts.dropped,
                artifacts.finnhub_used,
            ),
        )
        .await
    {
        eprintln!(
            "Finance Advisor: failed to write universe file {:?}: {:?}",
            universe_path, e
        );
    }

    for draft in &artifacts.propose_drafts {
        let name = format!(
            "{}-stocks-propose-{}.md",
            date_str,
            sanitize_model_slug(&draft.model_id)
        );
        let path = target_dir.join(&name);
        let body = format!(
            "# Propose draft — {}\n\n```json\n{}\n```\n",
            draft.model_id, draft.raw_json
        );
        if let Err(e) = tokio::fs::write(&path, &body).await {
            eprintln!(
                "Finance Advisor: failed to write propose draft {:?}: {:?}",
                path, e
            );
        }
    }

    for draft in &artifacts.score_drafts {
        let name = format!(
            "{}-stocks-score-{}.md",
            date_str,
            sanitize_model_slug(&draft.model_id)
        );
        let path = target_dir.join(&name);
        let body = format!(
            "# Score draft — {}\n\n```json\n{}\n```\n",
            draft.model_id, draft.raw_json
        );
        if let Err(e) = tokio::fs::write(&path, &body).await {
            eprintln!(
                "Finance Advisor: failed to write score draft {:?}: {:?}",
                path, e
            );
        }
    }

    let tickers = artifacts
        .universe
        .iter()
        .map(|e| e.ticker.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let path_str = file_path.to_string_lossy().to_string();
    let log_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    sqlx::query(
        "INSERT INTO stock_research_log (id, timestamp, tickers_analyzed, summary_file_path) VALUES (?, ?, ?, ?)"
    )
    .bind(&log_id)
    .bind(now)
    .bind(&tickers)
    .bind(&path_str)
    .execute(pool)
    .await
    .context("Failed to insert stock research log to database")?;

    Ok(artifacts.synthesis)
}

pub fn extract_between<'a>(t: &'a str, start_delim: &str, end_delim: &str) -> Vec<&'a str> {
    let mut results = Vec::new();
    let mut temp = t;
    while let Some(start_idx) = temp.find(start_delim) {
        let start_pos = start_idx + start_delim.len();
        let remaining = &temp[start_pos..];
        if let Some(end_idx) = remaining.find(end_delim) {
            let inside = &remaining[..end_idx];
            results.push(inside);
            temp = &remaining[end_idx + end_delim.len()..];
        } else {
            break;
        }
    }
    results
}

pub fn extract_tickers(text: &str) -> String {
    use std::collections::BTreeSet;
    let mut tickers = BTreeSet::new();

    let is_valid_ticker = |w: &str| {
        let clean: String = w.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        !clean.is_empty() && clean.len() <= 5 && clean.chars().all(|c| c.is_ascii_uppercase())
    };

    for line in text.lines() {
        let lower = line.to_lowercase();
        if let Some(idx) = lower.find("ticker:") {
            let rest = &line[idx + 7..];
            for word in rest.split(|c: char| {
                c.is_whitespace()
                    || c == ':'
                    || c == '$'
                    || c == '('
                    || c == ')'
                    || c == ','
                    || c == '*'
                    || c == '`'
                    || c == '.'
            }) {
                let clean: String = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
                if is_valid_ticker(&clean) {
                    tickers.insert(clean);
                }
            }
        }
    }

    for inside in extract_between(text, "(", ")") {
        for part in inside.split(|c: char| c.is_whitespace() || c == ':') {
            let clean: String = part.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    for inside in extract_between(text, "[", "]") {
        for part in inside.split(|c: char| c.is_whitespace() || c == ':') {
            let clean: String = part.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    for inside in extract_between(text, "**", "**") {
        let clean: String = inside.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        if is_valid_ticker(&clean) {
            tickers.insert(clean);
        }
    }

    for word in
        text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '.' || c == ')')
    {
        if word.starts_with('$') {
            let clean: String = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if is_valid_ticker(&clean) {
                tickers.insert(clean);
            }
        }
    }

    let result: Vec<String> = tickers.into_iter().collect();
    result.join(",")
}

fn match_transaction(merchant: &str, category: &str, ticker: &str) -> bool {
    let merchant_upper = merchant.to_uppercase();
    let category_upper = category.to_uppercase();
    let ticker_upper = ticker.to_uppercase();

    if ticker_upper == "MICRO-CAP PICKS" {
        return merchant_upper.contains("QUESTRADE")
            || merchant_upper.contains("QUESTTRADE")
            || merchant_upper.contains("WEALTHSIMPLE")
            || merchant_upper.contains("ROBINHOOD")
            || category_upper == "INVESTMENT";
    }

    if let Some(idx) = merchant_upper.find(&ticker_upper) {
        let before_ok =
            idx == 0 || !merchant_upper.chars().nth(idx - 1).unwrap_or(' ').is_alphanumeric();
        let after_ok = idx + ticker_upper.len() == merchant_upper.len()
            || !merchant_upper
                .chars()
                .nth(idx + ticker_upper.len())
                .unwrap_or(' ')
                .is_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }

    false
}

pub fn check_allocation_status(
    target_month: &str,
    allocation: &TargetAllocation,
    entries: &[FinancialLedgerEntry],
    config: &AppConfig,
    rates: &std::collections::HashMap<String, f64>,
) -> String {
    let base = config.currency();
    let mut msg = String::new();
    msg.push_str(&format!(
        "🎯 *Savings & Target Allocation Tracking: {}* ({})\n\n",
        target_month, base
    ));

    let mut total_actual_buys = 0.0;

    for bucket in &allocation.buckets {
        let mut actual_bucket_buy = 0.0;
        let mut holdings_lines = Vec::new();

        for holding in &bucket.holdings {
            let mut actual_holding_buy = 0.0;
            for entry in entries {
                if entry.amount < 0.0
                    && match_transaction(&entry.merchant, &entry.category, &holding.ticker)
                {
                    let converted =
                        config.convert_to_base(entry.amount.abs(), &entry.currency, rates);
                    actual_holding_buy += converted;
                }
            }
            actual_bucket_buy += actual_holding_buy;
            total_actual_buys += actual_holding_buy;

            let holding_percent = if holding.amount > 0.0 {
                (actual_holding_buy / holding.amount) * 100.0
            } else {
                0.0
            };

            let status_icon = if actual_holding_buy >= holding.amount {
                "✅"
            } else if actual_holding_buy > 0.0 {
                "⚠️"
            } else {
                "❌"
            };

            holdings_lines.push(format!(
                "  - *{}*: ${:.2} / ${:.2} {} ({:.1}% - {})",
                holding.ticker,
                actual_holding_buy,
                holding.amount,
                base,
                holding_percent,
                status_icon
            ));
        }

        let bucket_percent = if bucket.monthly_buy > 0.0 {
            (actual_bucket_buy / bucket.monthly_buy) * 100.0
        } else {
            0.0
        };

        let bucket_status_icon = if actual_bucket_buy >= bucket.monthly_buy {
            "✅"
        } else if actual_bucket_buy > 0.0 {
            "⚠️"
        } else {
            "❌"
        };

        msg.push_str(&format!(
            "• *{}* (Target: ${:.2} | Actual: ${:.2} {} | {:.1}% - {})\n",
            bucket.name,
            bucket.monthly_buy,
            actual_bucket_buy,
            base,
            bucket_percent,
            bucket_status_icon
        ));
        for line in holdings_lines {
            msg.push_str(&line);
            msg.push_str("\n");
        }
        msg.push_str("\n");
    }

    let overall_percent = if allocation.monthly_budget > 0.0 {
        (total_actual_buys / allocation.monthly_budget) * 100.0
    } else {
        0.0
    };

    let overall_status = if total_actual_buys >= allocation.monthly_budget {
        "On Track ✅"
    } else if total_actual_buys > 0.0 {
        "Partially Funded ⚠️"
    } else {
        "Not Funded ❌"
    };

    msg.push_str("━━━━━━━━━━━━━━━━━━━━━━━━\n");
    msg.push_str(&format!(
        "✨ *Overall Savings Budget:* ${:.2} / ${:.2} {} ({:.1}% - {})\n",
        total_actual_buys, allocation.monthly_budget, base, overall_percent, overall_status
    ));

    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use chotu_common::{AllocationBucket, BucketHolding};

    fn cand(ticker: &str, band: MarketCapBand) -> ProposeCandidate {
        ProposeCandidate {
            ticker: ticker.to_string(),
            company: format!("{} Co", ticker),
            one_line_why: "test".to_string(),
            market_cap_band: band,
            exchange: "NASDAQ".to_string(),
        }
    }

    #[test]
    fn test_normalize_ticker() {
        assert_eq!(normalize_ticker(" $png "), "PNG");
        assert_eq!(normalize_ticker("jdg.l"), "JDG.L");
    }

    #[test]
    fn test_universe_from_targets() {
        let u = universe_from_targets("Apple, Nvidia, apple").unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].proposed_by, vec!["user".to_string()]);
    }

    #[test]
    fn test_build_shared_universe_filters_and_round_robin() {
        let proposes = vec![
            (
                "model-a".to_string(),
                ProposeList {
                    proposals: vec![
                        cand("AAA", MarketCapBand::Micro),
                        cand("BBB", MarketCapBand::Small),
                        cand("BIG", MarketCapBand::Large),
                        cand("CCC", MarketCapBand::Micro),
                    ],
                },
            ),
            (
                "model-b".to_string(),
                ProposeList {
                    proposals: vec![
                        cand("DDD", MarketCapBand::Small),
                        cand("AAA", MarketCapBand::Micro), // duplicate
                        cand("EEE", MarketCapBand::Mid),   // dropped
                    ],
                },
            ),
            (
                "model-c".to_string(),
                ProposeList {
                    proposals: vec![cand("FFF", MarketCapBand::Micro)],
                },
            ),
        ];

        let universe = build_shared_universe(&proposes, true).unwrap();
        let tickers: Vec<_> = universe.iter().map(|e| e.ticker.as_str()).collect();
        // Round-robin: A, B, C, then A, B, ...
        assert_eq!(tickers, vec!["AAA", "DDD", "FFF", "BBB", "CCC"]);
        assert!(!tickers.contains(&"BIG"));
        assert!(!tickers.contains(&"EEE"));
        let aaa = universe.iter().find(|e| e.ticker == "AAA").unwrap();
        assert!(aaa.proposed_by.contains(&"model-a".to_string()));
        assert!(aaa.proposed_by.contains(&"model-b".to_string()));
    }

    #[test]
    fn test_build_shared_universe_keeps_mid_without_model_filter() {
        let proposes = vec![(
            "m1".to_string(),
            ProposeList {
                proposals: vec![
                    cand("MIC", MarketCapBand::Micro),
                    cand("MID", MarketCapBand::Mid),
                ],
            },
        )];
        let universe = build_shared_universe(&proposes, false).unwrap();
        let tickers: Vec<_> = universe.iter().map(|e| e.ticker.as_str()).collect();
        assert!(tickers.contains(&"MIC"));
        assert!(tickers.contains(&"MID"));
    }

    #[test]
    fn test_apply_finnhub_profiles_discovery_vs_seeded() {
        use chotu_common::{CapBand, CompanyProfile};

        let mut universe = vec![
            UniverseEntry {
                ticker: "AAA".into(),
                company: "A".into(),
                exchange: None,
                market_cap_band: Some(MarketCapBand::Micro),
                market_cap_m: None,
                proposed_by: vec!["m1".into()],
                one_line_why: None,
            },
            UniverseEntry {
                ticker: "BIG".into(),
                company: "BigCo".into(),
                exchange: None,
                market_cap_band: Some(MarketCapBand::Micro),
                market_cap_m: None,
                proposed_by: vec!["m1".into()],
                one_line_why: None,
            },
            UniverseEntry {
                ticker: "MISS".into(),
                company: "Missing".into(),
                exchange: None,
                market_cap_band: None,
                market_cap_m: None,
                proposed_by: vec!["m1".into()],
                one_line_why: None,
            },
        ];

        let mut profiles = HashMap::new();
        profiles.insert(
            "AAA".into(),
            CompanyProfile {
                symbol: "AAA".into(),
                name: "Alpha Micro".into(),
                exchange: Some("NASDAQ".into()),
                market_cap_m: 120.0,
                finnhub_industry: None,
                cap_band: CapBand::Micro,
            },
        );
        profiles.insert(
            "BIG".into(),
            CompanyProfile {
                symbol: "BIG".into(),
                name: "Big Corp".into(),
                exchange: Some("NYSE".into()),
                market_cap_m: 50_000.0,
                finnhub_industry: None,
                cap_band: CapBand::Large,
            },
        );

        let discovery = apply_finnhub_profiles(&mut universe, &profiles, true);
        assert_eq!(universe.len(), 1);
        assert_eq!(universe[0].ticker, "AAA");
        assert_eq!(universe[0].company, "Alpha Micro");
        assert_eq!(universe[0].market_cap_m, Some(120.0));
        assert_eq!(discovery.dropped.len(), 2);

        let mut seeded = vec![
            UniverseEntry {
                ticker: "BIG".into(),
                company: "BigCo".into(),
                exchange: None,
                market_cap_band: None,
                market_cap_m: None,
                proposed_by: vec!["user".into()],
                one_line_why: None,
            },
            UniverseEntry {
                ticker: "MISS".into(),
                company: "Missing".into(),
                exchange: None,
                market_cap_band: None,
                market_cap_m: None,
                proposed_by: vec!["user".into()],
                one_line_why: None,
            },
        ];
        let seeded_result = apply_finnhub_profiles(&mut seeded, &profiles, false);
        assert_eq!(seeded.len(), 2);
        assert!(seeded.iter().any(|e| e.ticker == "BIG" && e.market_cap_m == Some(50_000.0)));
        assert!(seeded.iter().any(|e| e.ticker == "MISS"));
        assert!(seeded_result.dropped.is_empty());
    }

    #[test]
    fn test_build_shared_universe_caps_at_12() {
        let mut proposals = Vec::new();
        for i in 0..20 {
            proposals.push(cand(&format!("T{i:02}"), MarketCapBand::Micro));
        }
        let proposes = vec![
            (
                "m1".to_string(),
                ProposeList {
                    proposals: proposals[..4].to_vec(),
                },
            ),
            (
                "m2".to_string(),
                ProposeList {
                    proposals: proposals[4..8].to_vec(),
                },
            ),
            (
                "m3".to_string(),
                ProposeList {
                    proposals: proposals[8..12].to_vec(),
                },
            ),
        ];
        // Only 12 unique micro names across 3 models x 4 — exactly at cap.
        let universe = build_shared_universe(&proposes, true).unwrap();
        assert_eq!(universe.len(), 12);
    }

    #[test]
    fn test_validate_score_report() {
        let universe = vec![
            UniverseEntry {
                ticker: "AAA".to_string(),
                company: "A".to_string(),
                exchange: None,
                market_cap_band: Some(MarketCapBand::Micro),
                market_cap_m: None,
                proposed_by: vec!["m1".into()],
                one_line_why: None,
            },
            UniverseEntry {
                ticker: "BBB".to_string(),
                company: "B".to_string(),
                exchange: None,
                market_cap_band: Some(MarketCapBand::Small),
                market_cap_m: None,
                proposed_by: vec!["m1".into()],
                one_line_why: None,
            },
        ];

        let ok = ScoreReport {
            scores: vec![
                ScoredCandidate {
                    ticker: "aaa".into(),
                    company: "A".into(),
                    fit_score: 8,
                    conviction: Conviction::High,
                    thesis: "t".into(),
                    catalysts: vec![],
                    risks: vec![],
                    hundred_bagger_plausible: true,
                    pass_reason: None,
                },
                ScoredCandidate {
                    ticker: "BBB".into(),
                    company: "B".into(),
                    fit_score: 3,
                    conviction: Conviction::Pass,
                    thesis: "t".into(),
                    catalysts: vec![],
                    risks: vec![],
                    hundred_bagger_plausible: false,
                    pass_reason: Some("no".into()),
                },
            ],
        };
        assert!(validate_score_report(&universe, &ok).is_ok());

        let invented = ScoreReport {
            scores: vec![
                ok.scores[0].clone(),
                ok.scores[1].clone(),
                ScoredCandidate {
                    ticker: "ZZZ".into(),
                    company: "Z".into(),
                    fit_score: 1,
                    conviction: Conviction::Pass,
                    thesis: "t".into(),
                    catalysts: vec![],
                    risks: vec![],
                    hundred_bagger_plausible: false,
                    pass_reason: None,
                },
            ],
        };
        assert!(validate_score_report(&universe, &invented)
            .unwrap_err()
            .contains("invented"));

        let missing = ScoreReport {
            scores: vec![ok.scores[0].clone()],
        };
        assert!(validate_score_report(&universe, &missing)
            .unwrap_err()
            .contains("missing"));
    }

    #[test]
    fn test_extract_tickers() {
        let sample = "\
### Stock Analysis Report
We present **Palantir Technologies** (Ticker: PLTR) as our first candidate.
The second candidate is $MELI, which has high margins.
Also check out Sea Limited (SE).
Some noise like Ticker: NOT_A_TICKER is ignored because it is too long.";
        let result = extract_tickers(sample);
        assert!(result.contains("PLTR"));
        assert!(result.contains("MELI"));
        assert!(result.contains("SE"));
        assert!(!result.contains("NOT_A_TICKER"));
    }

    #[test]
    fn test_sanitize_model_slug() {
        assert_eq!(
            sanitize_model_slug("openai/gpt-5.6-sol"),
            "openai-gpt-5-6-sol"
        );
    }

    #[test]
    fn test_check_allocation_status() {
        let allocation = TargetAllocation {
            monthly_budget: 1000.0,
            buckets: vec![AllocationBucket {
                name: "Core Equities".to_string(),
                weight_percent: 100.0,
                monthly_buy: 1000.0,
                holdings: vec![
                    BucketHolding {
                        ticker: "VFV".to_string(),
                        amount: 600.0,
                    },
                    BucketHolding {
                        ticker: "QQC".to_string(),
                        amount: 400.0,
                    },
                ],
            }],
        };

        let entries = vec![
            FinancialLedgerEntry {
                id: "1".to_string(),
                timestamp: chrono::Utc::now(),
                amount: -600.0,
                currency: "USD".to_string(),
                institution: "Questrade".to_string(),
                merchant: "VFV - Vanguard S&P 500 ETF: Bought shares".to_string(),
                category: "Uncategorized".to_string(),
                source_type: "BATCH_DROP".to_string(),
            },
            FinancialLedgerEntry {
                id: "2".to_string(),
                timestamp: chrono::Utc::now(),
                amount: -150.0,
                currency: "USD".to_string(),
                institution: "Questrade".to_string(),
                merchant: "QQC Bought".to_string(),
                category: "Uncategorized".to_string(),
                source_type: "BATCH_DROP".to_string(),
            },
        ];

        let config = AppConfig::default();
        let rates = std::collections::HashMap::new();
        let report = check_allocation_status("2026-06", &allocation, &entries, &config, &rates);
        assert!(report.contains("VFV"));
        assert!(report.contains("$600.00 / $600.00"));
        assert!(report.contains("QQC"));
        assert!(report.contains("$150.00 / $400.00"));
        assert!(report.contains("Core Equities"));
        assert!(report.contains("Overall Savings Budget"));
    }
}
