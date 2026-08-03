# Project Chotu: Multi-Agent Personal Assistant

Project Chotu is a powerful, local-first agentic workspace built in Rust. It utilizes a supervisor architecture to orchestrate multiple specialized background agents (Streamer, Janitor, Health Coach, Reflection Coordinator) running concurrently to automate daily workflows: health/nutrition tracking, financial ledger logs, and automated investment stock research.

---

## Features

- **Google Health Sync**: Two-way nutrition sync — imports daily calories/macros from the Google Health REST API (v4), and pushes Telegram `/food` entries (including photo-logged meals) back for the primary member.
- **Interactive OAuth Onboarding**: Run `/login fitbit` or `/login gmail` directly in Telegram, and the agent spawns a temporary callback server to authorize and auto-save credentials to your local `.env`.
- **Stock Research Agent**: Run automated investment analysis (hundred-bagger methodology) via Google Gemini 3.5 Flash, matching stock tickers dynamically.
- **Gmail IMAP Streamer**: Scrapes bank statements, receipts, and invoices automatically via Gmail OAuth2.
- **Janitor Agent**: Periodic deduplication, database indexing, and audit log cleaning.
- **Local LLM Integration**: Connects to Ollama for offline email classification and health coach reflection logs.

---

## Prerequisites

1. **Rust Toolchain**: Install Rust (1.80+ recommended) via `rustup`.
2. **SQLite**: The database is stored locally in `chotu.db`.
3. **Ollama**: Install [Ollama](https://ollama.com/) locally for offline email classification.
   Small 3–4B models (`llama3.2:3b`, `qwen3.5:4b`) work but misclassify edge cases often.
   Prefer `qwen3.5:9b` for triage accuracy (next step up from 4b in the Qwen 3.5 family):
   ```bash
   ollama pull qwen3.5:9b
   ```
   Set `OLLAMA_MODEL=qwen3.5:9b` in `.env`.

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
TELEGRAM_CHAT_ID=your_personal_chat_id
GEMINI_API_KEY=your_gemini_api_key

# Optional: Google Health/Gmail Client IDs for OAuth integration
FITBIT_CLIENT_ID=your_google_cloud_client_id
FITBIT_CLIENT_SECRET=your_google_cloud_client_secret

CHOTU_OAUTH_CLIENT_ID=your_google_gmail_client_id
CHOTU_OAUTH_CLIENT_SECRET=your_google_gmail_client_secret
CHOTU_EMAIL_USER=your_email@gmail.com
```

### 3. Run the Agent
Run the supervisor and bot coordinator:
```bash
make run
```

---

## Linking Accounts (OAuth Setup)

Chotu uses browser redirects to secure logins locally without public ports.

### A. Google Health (Fitbit)
1. Register a project in the [Google Cloud Console](https://console.cloud.google.com/).
2. Enable the **Google Health API** and configure the **OAuth Consent Screen** (adding your email as a test user).
3. Create a **Web Application** credential, setting the Authorized redirect URI to `http://localhost:8080/callback`.
4. Copy the Client ID & Secret to your `.env` (`FITBIT_CLIENT_ID` / `FITBIT_CLIENT_SECRET`).
5. Send `/login health` in Telegram and click the authorization link (includes nutrition **write** so Telegram `/food` can sync upstream). Re-run `/login health` if you previously authorized read-only scopes.

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
| `/login <health\|gmail\|calendar <member>>` | Interactive OAuth (Calendar saves `CALENDAR_REFRESH_TOKEN_<ID>`). |
| `/sync` | Triggers a manual sync of today's nutrition details. |
| `/food <member_id> <desc>` | Manually log food and push to Google Health for the primary member (e.g. `/food praj 2 eggs and toast`). |
| `/undofood [member_id]` | Remove the last `/food` entry (and its Google Health log if synced). |
| `/adjustfood [member_id] <cal> <P> <C> <F>` | Override today's nutrition totals (clears Telegram meals from Google Health first). |
| `/status` | Today's status (finance + health, goal progress). |
| `/brief` | Morning brief: today's calendar, open tasks, bills due, yesterday's nutrition vs goals. Auto-sends at 7:00 local when `TELEGRAM_CHAT_ID` is set (`MORNING_BRIEF_HOUR` to override). |
| `/trends [days]` | Multi-day nutrition/activity trends (default 7 days). |
| `/tasks [open\|all\|completed\|snoozed] [member]` | List tasks. Actions: `/tasks complete <id>`, `/tasks snooze <id> [days]`, `/tasks reassign <id> <member>`, `/tasks open <id>`. Reply `unactionable` to a reminder to ignore similar emails. |
| `/reflect` | Manually trigger the evening reflection loop. |
| `/research [companies]` | Run stock analysis (e.g., `/research Apple, Nvidia`). |
| `/networth` | Estimated net worth (cash + stocks) in base currency. |
| `/monthly [YYYY-MM]` | Monthly transaction summary. |
| `/holdings ...` | Set portfolio holdings. |
| `/chat` | View your current Telegram Chat ID. |

Plain-text messages also work for common asks (e.g. "morning brief", "how's today", "open tasks", "log 2 eggs for praj", "sync health", "trends last 14 days", "net worth", "monthly spend"). Unclear messages get a short clarifying question instead of the full command list.

**Food photos:** send a barcode, product package, or plated meal. Optional caption sets member/portion (e.g. `praj half the bowl`). Barcodes look up [Open Food Facts](https://world.openfoodfacts.org/); packages and plates use Gemini vision. Nutrients are logged the same way as `/food` (including Google Health push for the primary member).

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
