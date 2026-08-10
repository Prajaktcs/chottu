# Legacy one-off A/B helper

Prefer the durable local bench:

→ **[`evals/research/README.md`](../research/README.md)** (`research_bench`)

```bash
cargo run -p finance-advisor --bin research_bench -- --dry-run
cargo run -p finance-advisor --bin research_bench -- \
  --baseline openai/gpt-5.6-sol \
  --candidate qwen/qwen3.8-max \
  --trials 2
```

`research_ab` remains as a full-panel qualitative runner if you want side-by-side syntheses without gold metrics.
