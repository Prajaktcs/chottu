# Research model bench (local)

Repeatable comparison of OpenRouter scorers against a **checked-in gold universe** of known stocks.

**Run this on your laptop**, not on cloud agents. Cloud VMs usually lack your `OPENROUTER_API_KEY`, and frontier score calls are billed to your account.

## Why not local Ollama?

Local triage models (`qwen3.5:9b`) are too weak for a definitive hundred-bagger screen. The bench still runs *locally as a process*; it calls OpenRouter only for the models under test.

## Quick start

```bash
# Offline: metrics unit tests + fixture validation
cargo test -p finance-advisor bench::
cargo run -p finance-advisor --bin research_bench -- --dry-run

# Live Sol vs Qwen (score-only, 2 trials) — needs OPENROUTER_API_KEY in .env
cargo run -p finance-advisor --bin research_bench -- \
  --baseline openai/gpt-5.6-sol \
  --candidate qwen/qwen3.8-max \
  --trials 2
```

Results: `evals/research/results/<run_id>/summary.md`.

## Gold fixture

[`fixtures.json`](fixtures.json) labels each name:

| Role | Meaning | Scored? |
| ------ | --------- | --------- |
| `clear_pass` | Mega-cap / wrong sleeve — expect Pass/Low | yes |
| `clear_interest` | Mandate-aligned speculative small/micro | yes |
| `contested` | Ambiguous — qualitative only | no (excluded from gold pairwise) |

Primary metrics (higher better):

1. **Pairwise interest > pass** — clear_interest fit_score beats clear_pass
2. **Pass label accuracy** — clear_pass gets Pass/Low
3. **Interest in top-k** — clear_interest names appear in top-k by fit

Composite = `0.5*pairwise + 0.3*pass_labels + 0.2*top_k`.

## Future models

Same command, new `--candidate`:

```bash
cargo run -p finance-advisor --bin research_bench -- \
  --baseline openai/gpt-5.6-sol \
  --candidate some-org/new-frontier-model \
  --trials 2
```

Optional full panel + judge (more expensive):

```bash
cargo run -p finance-advisor --bin research_bench -- \
  --baseline openai/gpt-5.6-sol \
  --candidate qwen/qwen3.8-max \
  --companions moonshotai/kimi-k3 \
  --with-judge \
  --trials 1
```

## Decision rule

Change `DEFAULT_PANEL_MODELS` only if the candidate wins composite **and** pairwise is not worse. On near-ties, prefer the cheaper model. Commit interesting `results/<run_id>/summary.md` when you make a swap.
