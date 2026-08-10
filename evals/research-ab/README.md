# Research panel A/B — Sol vs Qwen3.8-Max

Seeded stock-research comparison that keeps Opus + Kimi + Kimi-judge fixed and swaps only the expensive OpenAI scorer for Qwen3.8-Max.

## Run

Requires `OPENROUTER_API_KEY` (optional `FINNHUB_API_KEY` for cap enrichment):

```bash
cargo run -p finance-advisor --bin research_ab
# or custom seed:
cargo run -p finance-advisor --bin research_ab -- --targets "ASTS, RKLB, IONQ, SOUN"
```

Artifacts land in `evals/research-ab/<run_id>/`:

- `a-baseline/` — Sol + Opus + Kimi, judge Kimi
- `b-qwen/` — Qwen3.8-Max + Opus + Kimi, judge Kimi
- `summary.md` — universe, score tables, top-3 overlap

Default seed: `ASTS, RKLB, IONQ, SOUN, JOBY, ACHR, LUNR, RDW`.

## Decision rule

Change `DEFAULT_PANEL_MODELS` in `finance-advisor` only if arm B is clearly as good or better on shortlist quality / philosophy fit. Single-pass runs are directional (LLM variance).
