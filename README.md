# Project Chotu: Multi-Agent Personal Assistant

Project Chotu is a powerful, local-first agentic workspace built in Rust. It utilizes a supervisor architecture to orchestrate multiple specialized background agents (Streamer, Janitor, Health Coach, Reflection Coordinator) running concurrently to automate daily workflows: health/nutrition tracking, financial ledger logs, and automated investment stock research.

---

## Features

- **Google Health Sync**: Automatically imports daily calories, protein, carbs, and fats using the modern Google Health REST API (v4).
- **Interactive OAuth Onboarding**: Run `/login fitbit` or `/login gmail` directly in Telegram, and the agent spawns a temporary callback server to authorize and auto-save credentials to your local `.env`.
- **Stock Research Agent**: Run automated investment analysis (hundred-bagger methodology) via Google Gemini 3.5 Flash, matching stock tickers dynamically.
- **Gmail IMAP Streamer**: Scrapes bank statements, receipts, and invoices automatically via Gmail OAuth2.
- **Janitor Agent**: Periodic deduplication, database indexing, and audit log cleaning.
- **Local LLM Integration**: Connects to Ollama for offline email classification and health coach reflection logs.

---

## Prerequisites

1. **Rust Toolchain**: Install Rust (1.80+ recommended) via `rustup`.
2. **SQLite**: The database is stored locally in `chotu.db`.
3. **Ollama**: Install [Ollama](https://ollama.com/) locally to run offline classification models:
   ```bash
   ollama pull llama3.2:3b
   ollama pull deepseek-r1:8b
   ```

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
5. Send `/login fitbit` in Telegram and click the authorization link.

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
| `/food <member_id> <desc>` | Manually log food (e.g. `/food praj 2 eggs and toast`). |
| `/undofood [member_id]` | Remove the last `/food` entry and rebuild today's totals. |
| `/adjustfood [member_id] <cal> <P> <C> <F>` | Override today's nutrition totals. |
| `/status` | Today's status (finance + health, goal progress). |
| `/trends [days]` | Multi-day nutrition/activity trends (default 7 days). |
| `/tasks [open\|all\|completed\|snoozed] [member]` | List tasks. Actions: `/tasks complete <id>`, `/tasks snooze <id> [days]`, `/tasks reassign <id> <member>`, `/tasks open <id>`. Reply `unactionable` to a reminder to ignore similar emails. |
| `/reflect` | Manually trigger the evening reflection loop. |
| `/research [companies]` | Run stock analysis (e.g., `/research Apple, Nvidia`). |
| `/networth` | Estimated net worth (cash + stocks) in base currency. |
| `/monthly [YYYY-MM]` | Monthly transaction summary. |
| `/holdings ...` | Set portfolio holdings. |
| `/chat` | View your current Telegram Chat ID. |

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
