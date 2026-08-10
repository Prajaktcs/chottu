# Project Chotu: Local-First Household Agent

Project Chotu is a self-hosted, local-first multi-agent system built in Rust. A supervisor runs specialized agents concurrently — email triage, document ingestion, family health coaching, finance/research, and a Telegram coordinator — so one household can automate daily workflows without sending private life to a cloud “assistant.”

It started as a personal finance helper (“chotu” ≈ agile apprentice in Mumbai slang) and now covers family nutrition, calendar/tasks from email, queryable personal memory, morning briefs, evening reflection, and investment research.

## AI involvement (transparency)

This project was built with **heavy AI coding assistance** (primarily Cursor). Rough split:

| Who | Owns |
| :--- | :--- |
| **Human** | Product goals, architecture, integrations (OAuth, IMAP, Telegram, Google Health, etc.), security/privacy posture, reviewing diffs, and deciding what ships |
| **AI** | Large share of implementation drafts, refactors, tests, docs, and iterative bug-fix scaffolding — always under human direction |

Nothing here runs or deploys without human review. Treat the codebase as **human-directed, AI-assisted**, not AI-autonomous. The in-app LLMs (Ollama / Gemini) are runtime features of the household agent; they are separate from how the source was written.

---

## What It Can Do

### Email intelligence (Streamer)
- Idle on Gmail via IMAP + OAuth2 and classify every message with a local Ollama model into one of nine categories:
  - **LEDGER_STREAM** — purchases, transfers, refunds, points redemptions → financial ledger
  - **FINANCIAL_BILL** — upcoming bills / due dates → bill tracking + calendar
  - **ACTION_ITEM** — requests and commitments → tasks (complete / snooze / reassign)
  - **TRAVEL_ITINERARY** — flights, hotels, trip logistics → travel dates on calendar
  - **STATEMENT_DOCUMENT** — statements, pay stubs, tax docs (often with PDFs)
  - **NEWSLETTER** — subscribed digests → stored for memory / recall
  - **PERSONAL_REFERENCE** — recipes, notes, saved articles → memory corpus
  - **ARCHIVE** — important non-transactional notices (security, “bill paid”, etc.)
  - **TRASH** — promo / spam → staged under an `AI-Trash` label for review
- Learns from feedback: reply `unactionable` to a task reminder to suppress similar emails next time.
- Dual compute tier: local Ollama for triage; Gemini for multimodal / long-context; OpenRouter for multi-model stock research.

### Document drop (Janitor)
- Watches `~/chotu_drop/` for CSVs and PDFs (bank exports, statements).
- Parses CSVs natively (no LLM); escalates multi-page / scanned PDFs to Gemini.
- Deduplicates ledger rows, archives processed files, and keeps the SQLite store tidy.

### Family health & nutrition (Health Coach)
- Multi-member household profiles (`config.yaml`: adults/kids, optional per-member nutrition + fitness goals).
- Two-way **Google Health** sync: pull daily calories/macros/activity/exercises; push Telegram-logged meals back per linked member. Evening sync ~8:45 local; **late steps sync ~11:00 PM ET** (override with `HEALTH_LATE_SYNC_HOUR` / `HEALTH_LATE_SYNC_TZ`) with a private nudge toward the daily step goal (config `nutrition_goals.steps`, default 10 000).
- Log food by text (`/food`), plain-language chat (“log 2 eggs for praj”), or **photo**:
  - Barcode → [Open Food Facts](https://world.openfoodfacts.org/)
  - Package / plated meal → Gemini vision
- Adjust, undo, or clear today’s food; overnight scheduled sync merges Telegram meals with Google Health instead of overwriting.
- Daily `/status` and multi-day `/trends` with goal progress when goals are configured; each member report ends with a short local-Ollama coach tip grounded in metrics, exercises, and long-horizon `fitness_goals` (e.g. beach body by a target date).
- Weekly training plans via `/plan` (Ollama, stored per member/week); morning brief includes today’s session and days-to-target.

### Tasks, calendar & morning brief (Coordinator)
- Tasks from email action items **or** Telegram (`/tasks add`, “remind me to…”); manage via `/tasks` (list, complete, snooze, reassign, reopen).
- Timed reminders: one Telegram ping when a task’s `due_at` is reached (assignee’s linked DM, else household targets).
- **Per-person Telegram DMs**: each adult runs `/link <member_id>` in a private chat; food/tasks without a member id default to that person. Once any member is linked, unknown chats are rejected (`/chat` and `/link` still work for setup).
- Per-member **Google Calendar** OAuth: action items, bill due dates, and travel dates can be auto-scheduled.
- **Calendar agenda** (`/cal [today|tomorrow|week]` or “what's today?”): family-merged timeline with timed-event conflict detection.
- **Morning brief** (manual `/brief` or scheduled ~7:00 local): today’s calendar, open tasks, bills due, yesterday’s nutrition vs goals, and training (outcome countdown + today’s planned session) — fans out to all linked adult DMs (`TELEGRAM_CHAT_ID` remains an optional shared fallback).
- **Evening reflection**: scheduled or `/reflect` — grounds a journal prompt in the day’s ledger + health data; replies saved under `~/chotu_brain/Journal/`.

### Queryable memory (RAG)
- Local embeddings (`nomic-embed-text` via Ollama) over journals, newsletter digests, personal references, and tasks.
- Ask in natural language (`/memory …` or “what was that recipe I saved”); answers prefer local Ollama, Gemini only as fallback.
- `/memory reindex` rebuilds the embedding index.

### Finance & investing (Finance Advisor + ledger)
- Continuous ledger from email receipts and drop-folder imports.
- `/monthly` spend summaries; `/networth` from cash + holdings (FX-aware when rates are available).
- Category **spend budgets** (`spend_budgets` in `config.yaml` or `/budget set`): progress via `/budget`, appended on `/monthly`, and mid-month Telegram pushes at 80% / 100%.
- Portfolio positions sync from dropped statements; optional target allocation buckets in `config.yaml`.
- `/research` — OpenRouter shared-universe research: multi-propose → Finnhub market-cap filter → score (GPT-5.6 Sol + Opus 5 + Kimi K3) → Kimi K3 judge. Optional company args seed the universe and skip propose.

### Natural-language Telegram UX
- Slash commands for everything above, **plus** free-text intent routing (“what's today”, “remind me to call the dentist tomorrow 3pm”, “morning brief”, “open tasks”, “net worth”, “trends last 14 days”, “sync health”, …).
- Unclear messages get a short clarifying question instead of dumping the full command list.
- Interactive `/login` for Google Health, Gmail, and Calendar (local OAuth callback; tokens written to `.env`).

### Privacy & platform posture
- **Local-first**: SQLite (`chotu.db`), local Ollama for triage/memory/reflection; cloud only where multimodal (Gemini) or multi-model research (OpenRouter) needs it.
- **Private goals & health**: `config.yaml` and `.env` are gitignored. Put real `nutrition_goals` / `fitness_goals` / constraints only in your local `config.yaml` — not in the public example. Linked personal Telegram DMs receive **only that member’s** nutrition, training plan, coach tips, sync details, and trends; other adults’ fitness goals are not fan‑out to the household.
- **Future medical records** (planned): private local ingest only; never committed or shared across family DMs; coach may use user-confirmed constraints, not diagnoses.
- **Zero-`unsafe` Rust** workspace policy (see `ARCHITECTURE.md`).
- Runs as a Cargo workspace supervisor (`make run`) or via Docker on non-macOS hosts.

---

## Agents at a Glance

| Agent | Role |
| :--- | :--- |
| **Streamer** | Live Gmail IMAP triage → ledger, tasks, bills, travel, digests, trash |
| **Janitor** | `~/chotu_drop/` CSV/PDF ingestion + ledger hygiene |
| **Health Coach** | Scheduled Google Health sync, nutrition/activity summaries, weekly `/plan`, outcome-aware coaching |
| **Coordinator** | Telegram bot, morning brief, evening reflection, OAuth login |
| **Finance Advisor** | Portfolio/net-worth helpers + stock research |
| **chotu-evals** | Golden-set evals for classifier / prompt regressions |

Shared library: `chotu-common` (DB, OAuth, LLM clients, calendar, memory, family config).

---

## Features (quick list)

- Google Health two-way nutrition sync (per family member)
- Telegram OAuth onboarding: `/login health <member>`, `/login gmail`, `/login calendar <member>`
- Per-person Telegram DMs via `/link <member_id>` (food/tasks default to linked member)
- Gmail IMAP streamer with nine-way local classification
- Task + bill + travel extraction with optional Calendar writes
- Food logging via text or photo (barcode / package / plate)
- Fitness goals + weekly `/plan` + outcome-aware coach tips (exercises from Google Health sync)
- Morning brief + evening reflection journals
- Local RAG memory over journals, digests, references, tasks
- Financial ledger, monthly summary, category budgets + spend alerts, net worth from portfolio statements
- Configurable stock research (OpenRouter + Finnhub: propose → cap filter → score → judge)
- Document drop folder for batch CSV/PDF imports
- Docker image for Linux / cloud deployment

---

## Prerequisites

1. **Rust Toolchain**: Install Rust (1.80+ recommended) via `rustup`.
2. **SQLite**: The database is stored locally in `chotu.db`.
3. **Ollama**: Install [Ollama](https://ollama.com/) locally for offline email classification and memory.
   Small 3–4B models (`llama3.2:3b`, `qwen3.5:4b`) work but misclassify edge cases often.
   Prefer `qwen3.5:9b` for triage accuracy (next step up from 4b in the Qwen 3.5 family):
   ```bash
   ollama pull qwen3.5:9b
   ollama pull nomic-embed-text
   ```
   Set `OLLAMA_MODEL=qwen3.5:9b` in `.env`. Optional: `OLLAMA_EMBED_MODEL=nomic-embed-text` (default) for `/memory` RAG.

---

## Getting Started

### 1. Initial Setup
Initialize configuration templates and dummy databases:
```bash
make setup
```

### 2. Configure Environment Variables (`.env`)
Fill in the credentials inside your `.env` file:
```env
TELEGRAM_BOT_TOKEN=your_telegram_bot_token
# Optional shared fallback for household pushes (brief, budgets, stock, reflection).
# Prefer per-member DMs via /link — see config.yaml telegram_chat_id.
TELEGRAM_CHAT_ID=your_personal_chat_id
GEMINI_API_KEY=your_gemini_api_key
OPENROUTER_API_KEY=your_openrouter_api_key
FINNHUB_API_KEY=your_finnhub_api_key

# Optional: override stock-research models (defaults shown)
# RESEARCH_PANEL_MODELS=openai/gpt-5.6-sol,anthropic/claude-opus-5,moonshotai/kimi-k3
# RESEARCH_JUDGE_MODEL=moonshotai/kimi-k3

# Compare future panel scorers locally (gold fixture; run on your laptop):
#   cargo run -p finance-advisor --bin research_bench -- --dry-run
#   cargo run -p finance-advisor --bin research_bench -- \
#     --baseline openai/gpt-5.6-sol --candidate qwen/qwen3.8-max --trials 2
# See evals/research/README.md

# Optional: Google Health/Gmail Client IDs for OAuth integration
FITBIT_CLIENT_ID=your_google_cloud_client_id
FITBIT_CLIENT_SECRET=your_google_cloud_client_secret

CHOTU_OAUTH_CLIENT_ID=your_google_gmail_client_id
CHOTU_OAUTH_CLIENT_SECRET=your_google_gmail_client_secret
CHOTU_EMAIL_USER=your_email@gmail.com
```

`GEMINI_API_KEY` is used for multimodal work (food photos, document ingest, nutrition). `OPENROUTER_API_KEY` powers `/research` LLMs (propose → score → judge). `FINNHUB_API_KEY` verifies market caps on the shared universe (optional; without it, model-estimated bands are used).

Also edit `config.yaml` (from `config.yaml.example`) for family members, nutrition/fitness goals, currency, and investment philosophy. Each adult should DM the bot and run `/link <member_id>` so food/tasks default to them and proactive messages reach their inbox.

### 3. Run the Agent
Run the supervisor and bot coordinator:
```bash
make run
```

---

## Linking Accounts (OAuth Setup)

Chotu uses browser redirects to secure logins locally without public ports.

### A. Google Health (per family member)
1. Register a project in the [Google Cloud Console](https://console.cloud.google.com/).
2. Enable the **Google Health API** and configure the **OAuth Consent Screen** (add each family member's Google account email as a test user).
3. Create a **Web Application** credential, setting the Authorized redirect URI to `http://localhost:8080/callback`.
4. Copy the Client ID & Secret to your `.env` (`FITBIT_CLIENT_ID` / `FITBIT_CLIENT_SECRET`).
5. Send `/login health <member_id>` in Telegram (e.g. `/login health praj`) and authorize with that member's Google account. Repeat for each person you want to track.
6. Chotu saves `HEALTH_REFRESH_TOKEN_<MEMBER>` to `.env` (the primary member also keeps legacy `FITBIT_REFRESH_TOKEN`). Re-run login if you previously authorized read-only scopes.

### B. Gmail (IMAP Email sync)
1. Using the same Google Cloud Console project, add your email to credentials (`CHOTU_EMAIL_USER`).
2. Add `CHOTU_OAUTH_CLIENT_ID` and `CHOTU_OAUTH_CLIENT_SECRET` to `.env`.
3. Send `/login gmail` in Telegram and click the authorization link.

### C. Google Calendar (per family member)
1. Ensure each adult has a `calendar:` block in `config.yaml` (see `config.yaml.example`).
2. Enable the **Google Calendar API** on the same OAuth client used for Gmail.
3. Send `/login calendar <member_id>` (e.g. `/login calendar praj`) and authorize with that member's Google account.
4. Chotu saves `CALENDAR_REFRESH_TOKEN_<MEMBER>` to `.env`. Action items, bill due dates, and travel dates from email are then auto-scheduled.

---

## Telegram Bot Commands

| Command | Description |
| :--- | :--- |
| `/help` | Displays the help text. |
| `/login <health <member>\|gmail\|calendar <member>>` | Interactive OAuth (Health/Calendar save per-member refresh tokens). |
| `/sync` | Triggers a manual sync of today's nutrition for every linked Google Health account. |
| `/food [member_id] <desc>` | Log food (defaults to the member linked to this DM). Relative days/times in the text are resolved (e.g. `/food yesterday's dinner pasta`). Pushes to that member's Google Health when linked. |
| `/undofood [member_id]` | Remove the last `/food` entry (and its Google Health log if synced). Defaults to linked member. |
| `/adjustfood [member_id] <cal> <P> <C> <F>` | Override today's nutrition totals (clears Telegram meals from Google Health first). |
| `/clearfood [member_id]` | Clear today's food logs and summary for a member. Defaults to linked member. |
| `/status` | Today's status (finance + health, exercises, fitness outcome progress, short local-Ollama coach tip per member). |
| `/plan [new]` | Show this week's training plan (generate if missing). `/plan new` regenerates from `fitness_goals` + recent activity via local Ollama. |
| `/brief` | Morning brief: today's calendar, open tasks, bills due, yesterday's nutrition vs goals, training countdown/session. Auto-sends at 7:00 local to all linked DMs (`MORNING_BRIEF_HOUR` to override; `TELEGRAM_CHAT_ID` optional shared fallback). |
| `/cal [today\|tomorrow\|week]` | Family calendar agenda (default today). Flags overlapping timed events across linked calendars. |
| `/memory <question>` | Queryable memory RAG over journals, newsletter digests, personal references, and tasks. Answers via local Ollama (`OLLAMA_MODEL`); Gemini only if Ollama fails. `/memory reindex` rebuilds the embedding index (`nomic-embed-text`). |
| `/trends [days]` | Multi-day nutrition/activity trends (default 7 days) plus a short coach tip per member with data. |
| `/tasks [open\|all\|completed\|snoozed] [member]` | List tasks. Create: `/task <title> [by\|due <when>]` or `/tasks add [member] <title> [due\|by <when>]` (defaults assignee to linked member; dated tasks go on that member's Google Calendar when linked). Actions: `/tasks complete <id\|all>`, `/tasks snooze <id> [days]`, `/tasks reassign <id> <member>`, `/tasks open <id>`. Timed dues ping the assignee's DM (else household). Reply `unactionable` to an email reminder to ignore similar mail. |
| `/reflect` | Manually trigger the evening reflection loop. |
| `/research [companies]` | Shared-universe stock research via OpenRouter + Finnhub (propose → cap filter → score → Kimi K3 judge). With args, seeds the universe and skips propose. e.g. `/research Apple, Nvidia`. |
| `/networth` | Invested net worth from portfolio holdings (cash balance not tracked yet). |
| `/monthly [YYYY-MM]` | Monthly transaction summary (includes budget progress when configured). |
| `/budget` | Category spend budgets for this month. `/budget set Food 800`, `/budget clear Entertainment`. YAML `spend_budgets` + Telegram overrides; 80%/100% alerts fan out to linked DMs. |
| `/chat` | View your current Telegram Chat ID. |
| `/link <member_id>` | Link this private chat to a family member (writes `telegram_chat_id` in `config.yaml`). Refuses if that member is already linked to a different chat — clear `telegram_chat_id` in config first to move. |
| `/whoami` | Show which family member this chat is linked to. |

Plain-text messages also work for common asks (e.g. "what's today", "tomorrow's schedule", "this week", "remind me to call the dentist tomorrow 3pm", "morning brief", "how's today", "open tasks", "what's today's workout", "show my training plan", "regenerate plan", "what was that recipe I saved", "log 2 eggs for praj", "yesterday's dinner was pasta", "sync health", "trends last 14 days", "net worth", "monthly spend", "how's food budget"). Unclear messages get a short clarifying question instead of the full command list.

**Food photos:** send a barcode, product package, or plated meal. Caption is optional; without a member id it logs for the linked DM member (e.g. `half the bowl` or `praj half the bowl`). Barcodes look up [Open Food Facts](https://world.openfoodfacts.org/); packages and plates use Gemini vision. Nutrients are logged the same way as `/food` (including Google Health push when that member is linked).

---

## Docker Deployment (Non-macOS Platforms)

A multi-stage `Dockerfile` is provided to run Project Chotu on Linux, Windows, or cloud environments.

### 1. Build the Image
```bash
docker build -t chotu:latest .
```

### 2. Run the Container
```bash
docker run -d \
  --name chotu-agent \
  -p 8080:8080 \
  -v $(pwd)/.env:/app/.env \
  -v $(pwd)/chotu.db:/app/chotu.db \
  -v $(pwd)/config.yaml:/app/config.yaml \
  chotu:latest
```

---

## Development

Run unit tests across the entire cargo workspace:
```bash
make test
```

See `ARCHITECTURE.md` for safety/concurrency guidelines and `TODO.md` for the roadmap.
