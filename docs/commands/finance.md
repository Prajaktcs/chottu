# Finance commands

Ledger summaries, category budgets, portfolio net worth, and multi-model stock research.

Ledger rows come from email triage (Streamer) and `~/chotu_drop/` imports (Janitor). Portfolio holdings sync from dropped statements.

---

## `/monthly [YYYY-MM]`

```text
/monthly
/monthly 2026-07
```

Spend summary for the month in your `config.yaml` base currency. Includes budget progress when `spend_budgets` (or Telegram overrides) exist.

Plain text: `monthly spend`.

---

## `/budget`

| Command | Effect |
| :--- | :--- |
| `/budget` | This month’s category progress |
| `/budget set Food 800` | Override / set a category cap |
| `/budget clear Entertainment` | Remove a Telegram override |

YAML `spend_budgets` are the baseline; Telegram sets merge on top. Mid-month alerts at **80%** and **100%** fan out to linked DMs (+ optional `TELEGRAM_CHAT_ID`).

Plain text: `how's food budget`.

---

## `/networth`

Invested net worth from `portfolio_holdings` (live quotes via Yahoo when holdings exist). **Cash balances are not tracked yet** — the email ledger is spend history, not account balances.

**Looks like**

```text
🔍 Fetching live quotes via Yahoo Finance...

💰 Project Chotu Net Worth Summary (CAD)

• 💵 Liquid Cash: not tracked yet …
• 📈 Stock Portfolio: …
━━━━━━━━━━━━━━━━━━━━━━━━
✨ Invested Net Worth: …
```

Empty holdings → $0 invested with a hint to drop a portfolio statement.

Plain text: `net worth`.

Optional target allocation buckets live in `config.yaml` (see example file).

---

## `/research [companies]`

Shared-universe research via **OpenRouter** (not Gemini):

1. Propose (skipped if you pass seed companies)
2. Finnhub market-cap filter (optional key)
3. Score panel (configurable models)
4. Judge synthesizes shortlist
5. Report saved under the brain/research path

```text
/research
/research Apple, Nvidia
```

**Looks like (progress stream)**

```text
🔎 [1/…] Proposing universe…
📏 [2/…] Filtering by market cap…
🧪 [3/…] Scoring (model …)…
🧠 [4/…] Judge synthesizing shortlist…
💾 [5/…] Saving report…
<final shortlist / rationale>
```

Without `OPENROUTER_API_KEY`:

```text
❌ Stock research requires OPENROUTER_API_KEY in .env. Gemini is not used for /research.
```

Model overrides and laptop-only benches: root README + `evals/research/README.md`.
