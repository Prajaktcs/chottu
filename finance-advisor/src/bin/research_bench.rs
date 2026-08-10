//! Repeatable local research-model bench against `evals/research/fixtures.json`.
//!
//! Run on your laptop (with `OPENROUTER_API_KEY`), not on cloud agents:
//!
//! ```bash
//! cargo test -p finance-advisor bench::
//! cargo run -p finance-advisor --bin research_bench -- --dry-run
//! cargo run -p finance-advisor --bin research_bench -- \
//!   --baseline openai/gpt-5.6-sol \
//!   --candidate qwen/qwen3.8-max \
//!   --trials 2
//! ```

use anyhow::{Context, Result};
use chotu_common::{config_path, load_config, InvestmentPhilosophy};
use finance_advisor::{
    compare_scorers, evaluate_scorer, format_metrics_table, parse_score_draft, BenchComparison,
    ResearchFixture, ScorerMetrics, StageDraft, StockResearcher, DEFAULT_JUDGE_MODEL,
    DEFAULT_PANEL_MODELS,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug)]
struct Args {
    fixtures: PathBuf,
    baseline: String,
    candidate: String,
    companions: Vec<String>,
    judge: String,
    trials: usize,
    top_k: usize,
    with_judge: bool,
    dry_run: bool,
    out_dir: PathBuf,
}

impl Args {
    fn parse(argv: Vec<String>) -> Result<Self> {
        let mut fixtures = PathBuf::from("evals/research/fixtures.json");
        let mut baseline = "openai/gpt-5.6-sol".to_string();
        let mut candidate = "qwen/qwen3.8-max".to_string();
        // Score-only default: evaluate the slot model alone against gold (cheapest, clearest).
        let mut companions: Vec<String> = Vec::new();
        let mut judge = DEFAULT_JUDGE_MODEL.to_string();
        let mut trials = 1usize;
        let mut top_k = 3usize;
        let mut with_judge = false;
        let mut dry_run = false;
        let mut out_dir = PathBuf::from("evals/research/results");

        let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--fixtures" => {
                    i += 1;
                    fixtures = PathBuf::from(argv.get(i).context("--fixtures requires a path")?);
                }
                "--baseline" => {
                    i += 1;
                    baseline = argv.get(i).context("--baseline requires a model")?.clone();
                }
                "--candidate" => {
                    i += 1;
                    candidate = argv.get(i).context("--candidate requires a model")?.clone();
                }
                "--companions" => {
                    i += 1;
                    let raw = argv.get(i).context("--companions requires csv models")?;
                    companions = raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "--judge" => {
                    i += 1;
                    judge = argv.get(i).context("--judge requires a model")?.clone();
                }
                "--trials" => {
                    i += 1;
                    trials = argv
                        .get(i)
                        .context("--trials requires an integer")?
                        .parse()
                        .context("invalid --trials")?;
                }
                "--top-k" => {
                    i += 1;
                    top_k = argv
                        .get(i)
                        .context("--top-k requires an integer")?
                        .parse()
                        .context("invalid --top-k")?;
                }
                "--with-judge" => with_judge = true,
                "--dry-run" => dry_run = true,
                "--out-dir" => {
                    i += 1;
                    out_dir = PathBuf::from(argv.get(i).context("--out-dir requires a path")?);
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument: {other}"),
            }
            i += 1;
        }

        if trials == 0 {
            anyhow::bail!("--trials must be >= 1");
        }
        if with_judge && companions.is_empty() {
            anyhow::bail!(
                "--with-judge needs a multi-model panel; pass --companions e.g. \
                 openai/gpt-5.6-sol,moonshotai/kimi-k3"
            );
        }

        Ok(Self {
            fixtures,
            baseline,
            candidate,
            companions,
            judge,
            trials,
            top_k,
            with_judge,
            dry_run,
            out_dir,
        })
    }
}

fn print_help() {
    println!(
        "\
research_bench — local repeatable research model comparison

Run on your machine with OPENROUTER_API_KEY in `.env`. Not intended for cloud agents.

Usage:
  cargo run -p finance-advisor --bin research_bench -- [options]

Options:
  --fixtures PATH     Gold fixture (default: evals/research/fixtures.json)
  --baseline MODEL    Current panel scorer (default: openai/gpt-5.6-sol)
  --candidate MODEL   Challenger scorer (default: qwen/qwen3.8-max)
  --companions CSV    Extra panel models (default: none — slot-only score bench)
  --judge MODEL       Judge model when --with-judge (default: {DEFAULT_JUDGE_MODEL})
  --trials N          Repeat each arm N times and average (default: 1)
  --top-k N           Interest-in-top-k metric (default: 3)
  --with-judge        Full panel + judge (requires --companions; more expensive)
  --dry-run           Validate fixture + print plan; no API calls
  --out-dir PATH      Results root (default: evals/research/results)

Typical local Sol vs Qwen (score-only, 2 trials):
  cargo run -p finance-advisor --bin research_bench -- \\
    --baseline openai/gpt-5.6-sol \\
    --candidate qwen/qwen3.8-max \\
    --trials 2
"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = Args::parse(env::args().skip(1).collect())?;
    let fixture = ResearchFixture::load(&args.fixtures)?;
    let philosophy = load_config(config_path())
        .investment_philosophy
        .unwrap_or_else(InvestmentPhilosophy::default);

    println!("research_bench (local)");
    println!("  fixtures: {}", args.fixtures.display());
    println!("  names: {}", fixture.names.len());
    println!("  targets: {}", fixture.targets_csv());
    println!("  baseline: {}", args.baseline);
    println!("  candidate: {}", args.candidate);
    println!(
        "  companions: {}",
        if args.companions.is_empty() {
            "(none — slot-only)".into()
        } else {
            args.companions.join(", ")
        }
    );
    println!(
        "  mode: {}",
        if args.with_judge {
            "full panel + judge"
        } else {
            "score-only (recommended)"
        }
    );
    println!("  trials: {}", args.trials);
    println!(
        "  production panel default: {}",
        DEFAULT_PANEL_MODELS.join(", ")
    );

    if args.dry_run {
        println!("\nDry run OK — fixture validated. No OpenRouter calls made.");
        println!("On your laptop:");
        println!(
            "  cargo run -p finance-advisor --bin research_bench -- \\\n    --baseline {} \\\n    --candidate {} \\\n    --trials {}",
            args.baseline,
            args.candidate,
            args.trials.max(2)
        );
        return Ok(());
    }

    if env::var("OPENROUTER_API_KEY").is_err() {
        anyhow::bail!(
            "OPENROUTER_API_KEY is not set. Add it to your local `.env` and re-run on your machine."
        );
    }

    let run_id = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let out_root = args.out_dir.join(&run_id);
    fs::create_dir_all(&out_root)?;

    let mut baseline_trial_metrics = Vec::new();
    let mut candidate_trial_metrics = Vec::new();

    for trial in 1..=args.trials {
        println!(
            "\n=== Trial {trial}/{} — baseline ({}) ===",
            args.trials, args.baseline
        );
        let b_metrics = run_arm(
            &out_root.join(format!("trial{trial}-baseline")),
            &fixture,
            &philosophy,
            &args.baseline,
            &args.companions,
            &args.judge,
            args.with_judge,
            args.top_k,
        )
        .await?;
        baseline_trial_metrics.push(b_metrics);

        println!(
            "\n=== Trial {trial}/{} — candidate ({}) ===",
            args.trials, args.candidate
        );
        let c_metrics = run_arm(
            &out_root.join(format!("trial{trial}-candidate")),
            &fixture,
            &philosophy,
            &args.candidate,
            &args.companions,
            &args.judge,
            args.with_judge,
            args.top_k,
        )
        .await?;
        candidate_trial_metrics.push(c_metrics);
    }

    let baseline_avg = average_metrics(&args.baseline, &baseline_trial_metrics);
    let candidate_avg = average_metrics(&args.candidate, &candidate_trial_metrics);
    let comparison = compare_scorers(baseline_avg, candidate_avg);

    let summary = render_summary(
        &run_id,
        &args,
        &fixture,
        &baseline_trial_metrics,
        &candidate_trial_metrics,
        &comparison,
    );
    let summary_path = out_root.join("summary.md");
    fs::write(&summary_path, &summary)?;
    fs::write(
        out_root.join("comparison.json"),
        serde_json::to_string_pretty(&comparison)?,
    )?;

    println!("\n{summary}");
    println!("Wrote {}", summary_path.display());
    if comparison.candidate_wins {
        println!("Verdict lean: candidate may replace baseline panel slot (review summary).");
    } else {
        println!("Verdict lean: keep baseline panel slot (review summary).");
    }
    Ok(())
}

async fn run_arm(
    arm_dir: &Path,
    fixture: &ResearchFixture,
    philosophy: &InvestmentPhilosophy,
    slot_model: &str,
    companions: &[String],
    judge: &str,
    with_judge: bool,
    top_k: usize,
) -> Result<ScorerMetrics> {
    fs::create_dir_all(arm_dir)?;
    let mut panel = vec![slot_model.to_string()];
    panel.extend(companions.iter().cloned());

    let researcher = StockResearcher::with_models(panel.clone(), judge.to_string());
    if !researcher.is_configured() {
        anyhow::bail!("OpenRouter client not configured");
    }

    let started = Instant::now();
    let targets = fixture.targets_csv();
    let artifacts = if with_judge {
        researcher
            .perform_research_with_artifacts(Some(&targets), Some(philosophy), None)
            .await
            .context("research arm failed")?
    } else {
        researcher
            .perform_score_only_with_artifacts(&targets, Some(philosophy))
            .await
            .context("score-only arm failed")?
    };
    let elapsed_ms = started.elapsed().as_millis();

    fs::write(arm_dir.join("synthesis.md"), &artifacts.synthesis)?;
    for draft in &artifacts.score_drafts {
        let slug = draft
            .model_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();
        fs::write(arm_dir.join(format!("score-{slug}.json")), &draft.raw_json)?;
    }

    let slot_draft = find_draft(&artifacts.score_drafts, slot_model)
        .with_context(|| format!("missing score draft for slot model {slot_model}"))?;
    let report = parse_score_draft(&slot_draft.raw_json)
        .with_context(|| format!("parse scores for {slot_model}"))?;
    let metrics = evaluate_scorer(slot_model, fixture, &report, top_k);
    fs::write(
        arm_dir.join("metrics.json"),
        serde_json::to_string_pretty(&metrics)?,
    )?;

    println!(
        "  slot={} composite={:.3} pairwise={:.1}% elapsed={:.1}s panel={}",
        slot_model,
        metrics.composite,
        100.0 * metrics.pairwise_order_accuracy,
        elapsed_ms as f64 / 1000.0,
        panel.join(", ")
    );
    Ok(metrics)
}

fn find_draft<'a>(drafts: &'a [StageDraft], model_id: &str) -> Option<&'a StageDraft> {
    drafts.iter().find(|d| d.model_id == model_id).or_else(|| {
        let short = model_id.rsplit('/').next().unwrap_or(model_id);
        drafts.iter().find(|d| {
            d.model_id == short || d.model_id.rsplit('/').next() == Some(short)
        })
    })
}

fn average_metrics(model_id: &str, trials: &[ScorerMetrics]) -> ScorerMetrics {
    let n = trials.len().max(1) as f64;
    let sum = |f: fn(&ScorerMetrics) -> f64| trials.iter().map(f).sum::<f64>() / n;
    let mean_opt = |f: fn(&ScorerMetrics) -> Option<f64>| {
        let vals: Vec<f64> = trials.iter().filter_map(f).collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };
    ScorerMetrics {
        model_id: model_id.to_string(),
        pass_label_accuracy: sum(|m| m.pass_label_accuracy),
        pass_label_n: trials.first().map(|m| m.pass_label_n).unwrap_or(0),
        interest_label_accuracy: sum(|m| m.interest_label_accuracy),
        interest_label_n: trials.first().map(|m| m.interest_label_n).unwrap_or(0),
        pairwise_order_accuracy: sum(|m| m.pairwise_order_accuracy),
        pairwise_n: trials.first().map(|m| m.pairwise_n).unwrap_or(0),
        interest_in_top_k: sum(|m| m.interest_in_top_k),
        top_k: trials.first().map(|m| m.top_k).unwrap_or(3),
        mean_fit_clear_pass: mean_opt(|m| m.mean_fit_clear_pass),
        mean_fit_clear_interest: mean_opt(|m| m.mean_fit_clear_interest),
        composite: sum(|m| m.composite),
    }
}

fn render_summary(
    run_id: &str,
    args: &Args,
    fixture: &ResearchFixture,
    baseline_trials: &[ScorerMetrics],
    candidate_trials: &[ScorerMetrics],
    comparison: &BenchComparison,
) -> String {
    let mut md = String::new();
    md.push_str("# Research model bench\n\n");
    md.push_str(&format!("- **Run id:** `{run_id}`\n"));
    md.push_str(&format!("- **Fixture:** `{}`\n", args.fixtures.display()));
    md.push_str(&format!("- **Universe:** {}\n", fixture.targets_csv()));
    md.push_str(&format!("- **Baseline slot:** `{}`\n", args.baseline));
    md.push_str(&format!("- **Candidate slot:** `{}`\n", args.candidate));
    md.push_str(&format!(
        "- **Companions:** {}\n",
        if args.companions.is_empty() {
            "_none (slot-only)_".into()
        } else {
            format!("`{}`", args.companions.join(", "))
        }
    ));
    md.push_str(&format!(
        "- **Mode:** {}\n",
        if args.with_judge {
            "full panel + judge"
        } else {
            "score-only"
        }
    ));
    md.push_str(&format!("- **Trials:** {}\n\n", args.trials));
    md.push_str(&format!("{}\n\n", fixture.description));

    md.push_str("## Baseline (avg)\n\n");
    md.push_str(&format_metrics_table(&comparison.baseline));
    md.push('\n');
    md.push_str("## Candidate (avg)\n\n");
    md.push_str(&format_metrics_table(&comparison.candidate));
    md.push('\n');

    md.push_str("## Comparison\n\n");
    md.push_str(&format!(
        "- **Δ composite (candidate − baseline):** `{:.3}`\n",
        comparison.delta_composite
    ));
    md.push_str(&format!(
        "- **Lean swap to candidate?** {}\n",
        if comparison.candidate_wins {
            "yes"
        } else {
            "no"
        }
    ));
    for note in &comparison.notes {
        md.push_str(&format!("- {note}\n"));
    }
    md.push('\n');

    if baseline_trials.len() > 1 {
        md.push_str("### Per-trial composites\n\n");
        md.push_str("| trial | baseline | candidate |\n|---:|---:|---:|\n");
        for (i, (b, c)) in baseline_trials.iter().zip(candidate_trials).enumerate() {
            md.push_str(&format!(
                "| {} | {:.3} | {:.3} |\n",
                i + 1,
                b.composite,
                c.composite
            ));
        }
        md.push('\n');
    }

    md.push_str("## Decision rule\n\n");
    md.push_str(
        "Change `DEFAULT_PANEL_MODELS` only if the candidate wins on composite **and** \
         pairwise interest>pass is not worse. Prefer the cheaper model on near-ties. \
         Re-run with `--trials 2` (or more) before swapping production defaults.\n",
    );
    md
}
