# Research model bench

- **Run id:** `20260809-203705`
- **Fixture:** `evals/research/fixtures.json`
- **Universe:** AAPL, MSFT, BRK.B, ASTS, RKLB, LUNR, RDW, SOUN, IONQ, JOBY
- **Baseline slot:** `openai/gpt-5.6-sol`
- **Candidate slot:** `qwen/qwen3.8-max`
- **Companions:** _none (slot-only)_
- **Mode:** score-only
- **Trials:** 2

Gold universe for hundred-bagger / micro-small-cap philosophy. Used by research_bench for repeatable model comparison. Labels are relative expectations, not investment advice.

## Baseline (avg)

| metric | value |
|---|---:|
| composite | 0.950 |
| pairwise interest>pass | 100.0% (n=8) |
| pass label accuracy | 100.0% (n=2) |
| interest label accuracy | 87.5% (n=4) |
| interest in top-3 | 75.0% |
| mean fit clear_pass | 1.50 |
| mean fit clear_interest | 6.25 |

## Candidate (avg)

| metric | value |
|---|---:|
| composite | 0.925 |
| pairwise interest>pass | 100.0% (n=8) |
| pass label accuracy | 100.0% (n=2) |
| interest label accuracy | 87.5% (n=4) |
| interest in top-3 | 62.5% |
| mean fit clear_pass | 1.00 |
| mean fit clear_interest | 4.00 |

## Comparison

- **Δ composite (candidate − baseline):** `-0.025`
- **Lean swap to candidate?** no
- Baseline composite ahead — keep current panel scorer.

### Per-trial composites

| trial | baseline | candidate |
|---:|---:|---:|
| 1 | 0.950 | 0.900 |
| 2 | 0.950 | 0.950 |

## Decision rule

Change `DEFAULT_PANEL_MODELS` only if the candidate wins on composite **and** pairwise interest>pass is not worse. Prefer the cheaper model on near-ties. Re-run with `--trials 2` (or more) before swapping production defaults.
