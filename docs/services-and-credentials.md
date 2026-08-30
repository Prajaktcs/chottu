# Services & credentials

Everything Chotu talks to, mapped to env vars. Fill values in `.env` after `just setup`. Never commit `.env` or paste live keys into docs/PRs.

Family shape, goals, budgets, and investment philosophy live in `config.yaml` (from `config.yaml.example`). Per-member OAuth refresh tokens are written into `.env` by `/login`, not into YAML.

---

## Minimum to get Signal talking

| Need | Env / config | Notes |
| :--- | :--- | :--- |
| signal-cli account | `SIGNAL_ACCOUNT` | The linked Signal account (E.164 or ACI). Chotu does not register a number. |
| signal-cli data dir | `SIGNAL_CLI_DATA_DIR` | Private daemon store |
| signal-cli socket | `SIGNAL_CLI_SOCKET` | Unix-domain JSON-RPC socket; **required for `just run`** |
| Optional household group | `SIGNAL_GROUP_ID` | Base64 group id from `signal-cli -a "$SIGNAL_ACCOUNT" listGroups` |
| Gemini | `GEMINI_API_KEY` | **Required for `just run`** (Signal coordinator won’t start without it) |
| Local LLM | Ollama running + `OLLAMA_MODEL` | `just setup` / `just prereqs` default to `qwen3.5:4b`; prefer `qwen3.5:9b` for triage |
| Family roster | `config.yaml` → `family.members` | At least one adult `id` for `/link` |

Start the daemon before `just run`:

```sh
signal-cli --data-dir "$SIGNAL_CLI_DATA_DIR" -a "$SIGNAL_ACCOUNT" daemon \
  --receive-mode=manual --socket "$SIGNAL_CLI_SOCKET"
```

One-time device provisioning is `signal-cli link` (or JSON-RPC `startLink`/`finishLink`). That is separate from Chotu `/link`. Keep the daemon data directory private and upgrade signal-cli at least every 90 days.

`just run` exits immediately if `SIGNAL_CLI_SOCKET` is missing/not a socket or `GEMINI_API_KEY` is missing, so the supervisor (Signal, Health Coach, Streamer, Janitor) never starts.

---

## Local compute (Ollama)

| Var | Default / example | Used for |
| :--- | :--- | :--- |
| `OLLAMA_HOST` | `http://localhost` | Base host |
| `OLLAMA_PORT` | `11434` | Port |
| `OLLAMA_BASE_URL` | derived from host+port | Embeddings client override |
| `OLLAMA_MODEL` | `qwen3.5:4b` from `just setup` | Email triage, memory answers, reflection, coach tips, `/plan` |
| `OLLAMA_EMBED_MODEL` | `nomic-embed-text` | `/memory` RAG index |

```bash
# What just prereqs pulls today:
ollama pull llama3.2:3b
ollama pull deepseek-r1:8b
ollama pull qwen3.5:4b

# Memory RAG embeddings (not in just prereqs — pull before /memory):
ollama pull nomic-embed-text

# Recommended upgrade for better triage (set OLLAMA_MODEL accordingly):
ollama pull qwen3.5:9b
```

Smaller 3–4B models work but misclassify more often; prefer `qwen3.5:9b` when you can.

---

## Cloud LLM & market data

| Service | Env | Required? | Powers |
| :--- | :--- | :--- | :--- |
| Gemini | `GEMINI_API_KEY` | **Required for Signal (`just run`)** | Food photos (package/plate), PDF ingest, some nutrition parsing. Health Coach scheduled sync can still run without multimodal Gemini fills. |
| OpenRouter | `OPENROUTER_API_KEY` | For `/research` | Propose → score → judge panel. Bot logs that research is disabled if unset. |
| Finnhub | `FINNHUB_API_KEY` | Optional | Market-cap filter on research universe; without it, model-estimated bands are used. |

Optional research overrides:

```env
# RESEARCH_PANEL_MODELS=openai/gpt-5.6-sol,qwen/qwen3.8-max,moonshotai/kimi-k3
# RESEARCH_JUDGE_MODEL=moonshotai/kimi-k3
```

Open Food Facts (barcode lookup) needs no key.

---

## Google Cloud OAuth

Use one Google Cloud project. Redirect URI for local login: `http://localhost:8080/callback`.

### Health (per member)

| Env | Role |
| :--- | :--- |
| `FITBIT_CLIENT_ID` / `FITBIT_CLIENT_SECRET` | OAuth client (naming is legacy; this is Google Health) |
| `HEALTH_REFRESH_TOKEN_<MEMBER>` | Written by `/login health <member>` |
| `FITBIT_REFRESH_TOKEN` | Legacy primary-member token still accepted |

Enable Google Health API; add each family Google account as a consent-screen test user.

### Gmail (IMAP streamer)

| Env | Role |
| :--- | :--- |
| `CHOTU_OAUTH_CLIENT_ID` / `CHOTU_OAUTH_CLIENT_SECRET` | Same or separate OAuth client |
| `CHOTU_EMAIL_USER` | Mailbox address |
| `CHOTU_OAUTH_REFRESH_TOKEN` | Written by `/login gmail` |
| `CHOTU_IMAP_SERVER` / `CHOTU_IMAP_PORT` | Optional; default `imap.gmail.com` / `993` |

### Calendar (per adult)

| Env | Role |
| :--- | :--- |
| Same `CHOTU_OAUTH_*` client | Enable Google Calendar API on it |
| `CALENDAR_REFRESH_TOKEN_<MEMBER>` | Written by `/login calendar <member>` |
| Member `calendar:` block in `config.yaml` | Provider + email |

---

## Paths, DB, scheduling knobs

| Var | Default | Purpose |
| :--- | :--- | :--- |
| `DATABASE_PATH` | `chotu.db` | SQLite |
| `CHOTU_CONFIG_PATH` | `config.yaml` | Family / budgets / philosophy |
| `CHOTU_BRAIN_DIR` | `~/chotu_brain` | Journals, digests, RAG corpus |
| `timezone` in `config.yaml` | `America/Toronto` | IANA tz for `schedules` (fallback: `CHOTU_TIMEZONE` env) |
| `schedules.morning_brief` | `"07:00"` | Proactive `/brief` (blank = off) |
| `schedules.portfolio` | `"18:00"` | Evening `/networth` overview (blank = off) |
| `schedules.reflection` / health slots | see `config.yaml.example` | Evening reflect + Google Health sync |

Drop folder for CSV/PDF ingest: `~/chotu_drop/` (created by setup / janitor).

---

## What degrades gracefully

| Missing | Behavior |
| :--- | :--- |
| `GEMINI_API_KEY` | `just run` / Signal client will not start. Health Coach sync logic can still run without Gemini nutrient fills once something else hosts it. |
| `OPENROUTER_API_KEY` | `/research` refuses with a clear error |
| `FINNHUB_API_KEY` | Research continues with estimated cap bands |
| Gmail refresh token | Streamer skips IMAP until `/login gmail` |
| Health refresh token | `/sync` / coach have nothing to pull for that member |
| Calendar refresh token | Tasks/bills/travel won’t auto-schedule for that member |
| No `/link` yet | Food/tasks need explicit member ids; unknown chats accepted until first link |

---

## Setup order (practical)

1. Rust + Ollama models + `just setup` (+ `just prereqs` to pull models)
2. Link signal-cli as a secondary device, start the documented daemon, set `SIGNAL_CLI_SOCKET` **and** `GEMINI_API_KEY` → `just run` → DM Chotu → `/link` → optional `SIGNAL_GROUP_ID`
3. Edit `config.yaml` members → each adult `/link <id>`
4. Google OAuth clients → `/login health …`, `/login gmail`, `/login calendar …`
5. Optionally set `OLLAMA_MODEL=qwen3.5:9b` (and pull that model) for better triage
6. Add OpenRouter (+ Finnhub) when you want `/research`

See also: root README “Linking Accounts” and command pages under [`commands/`](./commands/).
