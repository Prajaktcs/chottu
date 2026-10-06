# Services & credentials

Everything Chotu talks to, mapped to env vars. Fill values in `.env` after `just setup`. Never commit `.env` or paste live keys into docs/PRs.

Family shape, goals, budgets, and investment philosophy live in `config.yaml` (from `config.yaml.example`). Per-member OAuth refresh tokens are written into `.env` by `/login`, not into YAML.

---

## Minimum to get Signal talking

| Need | Env / config | Notes |
| :--- | :--- | :--- |
| signal-cli account | `SIGNAL_ACCOUNT` | Linked account **E.164** phone number (must start with `+`). This is signal-cli’s `-a/--account`, not an ACI. Chotu does not register a number. |
| signal-cli data dir | `SIGNAL_CLI_DATA_DIR` | Private daemon store |
| signal-cli socket | `SIGNAL_CLI_SOCKET` | Unix-domain JSON-RPC socket; **required for `just run`** |
| Optional household group | `SIGNAL_GROUP_ID` | Base64 group id (see commands below) |
| Gemini | `GEMINI_API_KEY` | **Required for `just run`** (Signal coordinator won’t start without it) |
| Local LLM | Ollama running + `OLLAMA_MODEL` + `OLLAMA_DECISION_MODEL` | Ollama 0.35+ for local Jev decisions (`tev1:0.8b`); `just setup` uses `qwen3.5:4b` for processing, with `qwen3.5:9b` recommended when memory permits |
| Authorized direct messages | `config.yaml` → `family.members[].signal_aci` | Per-contact **ACI** UUID for each allowed DM; distinct from `SIGNAL_ACCOUNT`. Restart after changes |
| Runtime socket tools | `nc`, `plutil` | Standard macOS commands checked by `just setup`; required to probe and reuse the signal-cli socket |

Direct/group authorization is context-specific and configuration is static for the process lifetime. A linked sender in the wrong group is rejected.

`SIGNAL_ACCOUNT` identifies the local linked device to signal-cli. `family.members[].signal_aci` authorizes which remote senders may DM Chotu. Do not put an ACI in `SIGNAL_ACCOUNT`.

After the daemon account is linked, copy contact ACIs and (optionally) the household group id:

```sh
# Contact ACIs for config.yaml → family.members[].signal_aci
signal-cli --data-dir "$SIGNAL_CLI_DATA_DIR" -a "$SIGNAL_ACCOUNT" -o json listContacts

# Optional base64 group id for SIGNAL_GROUP_ID
signal-cli --data-dir "$SIGNAL_CLI_DATA_DIR" -a "$SIGNAL_ACCOUNT" -o json listGroups
```

`just run` probes `SIGNAL_CLI_SOCKET`. It reuses a daemon that passes the health check, or starts signal-cli when no socket exists and stops that managed daemon when the coordinator exits. If an existing socket fails the health check, startup exits and preserves it; see the recovery guidance below. On macOS it wraps the session in `caffeinate -ims` so idle/system sleep does not freeze the Signal websocket; the display may still sleep. `SIGNAL_CLI_DATA_DIR` and `SIGNAL_ACCOUNT` are only required when `just run` needs to start the daemon. Only one `just run` process may use a configured socket at a time; a concurrent invocation exits without disturbing the active coordinator or daemon.

`just setup` verifies that `nc` and `plutil` are available and that `plutil` supports the JSON operations used by the socket probe. Both commands ship with macOS and need no separate package installation. If either command is missing or incompatible, install current macOS updates, then rerun `just setup`. `just run` names the failed prerequisite before attempting to probe or reuse a daemon; it does not require `jq` or `shlock`.

To manage the daemon separately, start it before `just run`:

```sh
signal-cli --data-dir "$SIGNAL_CLI_DATA_DIR" -a "$SIGNAL_ACCOUNT" daemon \
  --receive-mode=manual --socket "$SIGNAL_CLI_SOCKET"
```

One-time device provisioning is `signal-cli link` (or JSON-RPC `startLink`/`finishLink`). Chotu has no runtime identity-linking command. Configure each member ACI in `config.yaml` before startup, keep the daemon data directory private, and upgrade signal-cli at least every 90 days.

`just run` exits immediately if `SIGNAL_CLI_SOCKET` or `GEMINI_API_KEY` is missing. It also rejects a non-socket path. If an existing socket fails the health check, startup exits and leaves the socket intact because a timeout does not prove its daemon has stopped. Check the external daemon and retry; remove a genuinely stale socket manually only after confirming no daemon is running. When no socket exists, `just run` starts signal-cli; the supervisor (Signal, Health Coach, Streamer, Janitor) only starts after a daemon is listening.

---

## Local compute (Ollama)

| Var | Default / example | Used for |
| :--- | :--- | :--- |
| `OLLAMA_HOST` | `http://localhost` | Base host |
| `OLLAMA_PORT` | `11434` | Port |
| `OLLAMA_BASE_URL` | derived from host+port | Embeddings client override |
| `OLLAMA_MODEL` | `qwen3.5:4b` from `just setup` | Email detail extraction / classification fallback, Signal arguments, memory answers, reflection, coach tips, `/plan` |
| `OLLAMA_DECISION_MODEL` | `tev1:0.8b` | Local Jev-compatible email / Signal intent classification; `off` disables the decision path |
| `OLLAMA_EMBED_MODEL` | `nomic-embed-text` | `/memory` RAG index |

```bash
# What just prereqs pulls today:
ollama pull llama3.2:3b
ollama pull deepseek-r1:8b
ollama pull qwen3.5:4b
ollama pull tev1:0.8b

# Memory RAG embeddings (not in just prereqs — pull before /memory):
ollama pull nomic-embed-text

# Recommended upgrade for better triage (set OLLAMA_MODEL accordingly):
ollama pull qwen3.5:9b
```

For generative extraction/fallback, prefer `qwen3.5:9b` when memory permits;
smaller 3–4B chat models misclassify more often.

Email triage and natural-language Signal routing first call local Ollama's
[`/v1/systemone` endpoint](https://ollama.com/blog/ollama-now-supports-jev-style-decision-models).
This is the Jev-compatible API, not TypeSafe's hosted service. With
`OLLAMA_HOST=http://localhost`, email/message classification stays on this machine;
no classification API key is needed.

- A known email category with confidence **at least 0.80** skips generative
  classification, except actionable candidates when unactionable feedback exists:
  those receive a full local classification with the feedback before accepting
  an action item. Feedback stays out of the compact decision request.
  Receipts, tasks, trips, bills and reference notes still use `OLLAMA_MODEL`
  for their existing detail extraction.
- Confident Signal intents without arguments (status, brief, sync, net worth,
  budget and help) skip generative intent classification. Training-plan requests
  also require a confident yes/no decision about regeneration. Command-handler
  processing is unchanged.
- Calendar windows, food/member/date/time details, task titles, filters, monthly
  periods, memory queries and clarification still use the generative classifier.
  Its schema requests every nullable argument key; unrelated fields must be
  `null`, while calendar windows and food portions/member/date/time stay explicit.
- Uncertain, malformed, unavailable or oversized decision requests fall back to
  local `OLLAMA_MODEL`, never to a hosted classifier. `tev1:0.8b` accepts up to 2,050
  input tokens per decision; long messages or custom prompts can exceed that.
- The default fast pass uses typed category criteria and the original
  disambiguation rules, without the longer generative definitions/examples.
  Custom email prompt files are passed intact; the generative fallback always
  receives the full active prompt and user feedback.

Local structured extraction, including email classification fallback and email
detail extraction, sends `think: false` to Ollama. Model selection, prompts,
tool schemas, confidence thresholds and extraction retries are unchanged.
Free-form prompt generation keeps its existing settings; disabling thinking
does not unload the model or reduce its weight memory.

With non-thinking extraction, `qwen3.5:9b` plus `tev1:0.8b` passed the same
22-case triage corpus (22/22, 81.0 s). A separate five-case live extraction smoke
passed amount, merchant and date checks for a receipt, non-transaction alert,
task, bill and round trip; the observed chat responses contained no thinking
trace. These are functional checks, not a before/after resource benchmark or a
guarantee of production accuracy.

Use `OLLAMA_DECISION_MODEL=off` for chat-only classification; restart the coordinator
after changing the setting. `chotu-evals` uses the same setting and reports accuracy,
classification wall time, and both model names in its evaluation log.

Deterministic regressions cover confident email decisions, low-confidence/error
fallbacks and actionable-feedback review. They also validate the emitted intent
schema: omitted keys are rejected, optional arguments accept `null`, and intent
and reason remain non-nullable. Run
`cargo test -p chotu-common --locked --lib llm::tests::` for this coverage.

### Decision model sizing

Keep `tev1:0.8b` as the first pass on the 16 GB Mac. With Ollama 0.35.1,
`qwen3.5:9b` for fallback, the same 0.80 confidence gate and the 22-case email
corpus in `evals/dataset.json`, the measured classification runs were:

| First pass | Cascade accuracy | Finished in Jev | Classification wall time |
| :--- | :--- | :--- | :--- |
| `tev1:0.8b` | 22/22 | 15/22 | 81.8 s |
| `tev1:4b` | 22/22 | 17/22 | 231.7 s |
| `off` (Qwen only) | 22/22 | — | 158.5 s |

In the mixed classification/extraction smoke, 0.8B (893 MB loaded) and Qwen
(5.5 GB loaded) remained resident together. After the equivalent 4B smoke,
only 4B (4.7 GB loaded) remained resident. Fewer fallbacks did not make 4B
faster overall on this machine.

4B handled standalone training-plan requests in about 1.7 s, while 0.8B
escalated those requests to Qwen. Calendar and food requests still need argument
extraction; their measured 4B cascades were slower than the 0.8B cascades.
The small corpus does not establish production accuracy, and confidence scores
are not an accuracy guarantee.

For the optional larger first pass, run `ollama pull tev1:4b`, then set
`OLLAMA_DECISION_MODEL=tev1:4b` and restart the coordinator.

---

## Cloud LLM & market data

| Service | Env | Required? | Powers |
| :--- | :--- | :--- | :--- |
| Gemini | `GEMINI_API_KEY` | **Required for Signal (`just run`)** | Primary food-photo vision, PDF ingest, some nutrition parsing. Health Coach scheduled sync can still run without multimodal Gemini fills. |
| OpenRouter | `OPENROUTER_API_KEY` | Optional for text meals and food photos; required for `/research` | Qwen3.8 Max estimates text meals or analyzes the original photo if Gemini fails (including photos without captions); also powers the `/research` panel. Without the key, failed Gemini nutrition requests report that the fallback is unavailable. |
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

Set `email_sync_enabled: false` in `config.yaml` and restart Chotu to stop the
Streamer before Gmail OAuth or IMAP connection. Existing installs default to
`true` when the setting is omitted.

Email catch-up stays sequential, with a five-second asynchronous cooldown before
each message after the first in an unread batch. The first message has no added
delay and the last has no trailing cooldown. Disappeared/skipped messages still
count toward pacing, and there is no batch cutoff that could strand unread mail.
This gives other work gaps between background email requests; it slows backlog
processing but does not cap peak GPU use or unload resident models.

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

Budget progress (`/budget` and `/monthly`) enters watch status at 80% and warns when the limit is reached at 100%. Alerts distinguish the remaining amount below the limit, exactly reaching the limit, and the overage amount above it. With no configured category budgets, the display provides `/budget set` setup guidance and no budget alerts are generated.

Drop folder for CSV/PDF ingest: `~/chotu_drop/` (created by setup / janitor).

CSV imports recognize Wealthsimple credit-card statements, already-signed card activities, monthly account statements, and multi-account activity exports from their columns. Generic CSVs must supply signed cash-flow amounts and a stable, nonempty `account_id`/`account` column, or a recognized account ID in the filename; an institution name is not an account identity. New CSV ledger rows use `CSV_IMPORT`: outflows are negative, inflows positive. Structural categories inferred from transaction kinds (Income, Transfer, Investment, Fees, Taxes) take precedence over export labels. Otherwise, real export categories describe spending; merchantless card payments are retained as transfers. Re-import also corrects structural transactions previously stored under export labels.

Card activities accept only `Completed`/`Posted` rows (case-insensitive). Known pending, declined, cancelled, failed, voided, reversed, and authorized states are skipped and counted; empty or unrecognized statuses reject the entire file. Legacy CSV hashes are still recovered for skipped rows so repair removes previously imported non-posted transactions.

Card statements and activities honor a nonempty `account_id`/`account` column and keep separate card accounts distinct across overlapping formats. Card reconciliation also uses normalized merchant names, including for explicit account identities, so same-value purchases at different merchants remain separate. Without an explicit identity, they retain the single-card assumption; multi-card exports must provide account identities.

Account statements need the account ID in their filename, or an explicit account column. Activity exports retain account IDs and effective times; timestamps without a timezone are stored as UTC. Activity dates take precedence over statement dates independently of category metadata, and category enrichment does not discard effective times. Overlapping exports reuse transactions, while repeated rows within one source remain separate. Statement posting/execution dates bridge the supported statement/activity formats. Without transaction IDs or times, overlapping date-only exports are reconciled by occurrence count: import complete exports for each covered date, not disjoint partial slices of identical transactions.

### Repair historical CSV imports

Rehearse on a separate SQLite database snapshot and archive copy before repairing the live ledger. Before the live run, stop Chotu/Janitor, their supervisor or service, and any other database writers. Keep writers stopped and the archive unchanged throughout backup, repair, verification, and any rollback; restart only after accepting the repaired database or completing rollback. Backup creation and the repair transaction are separate operations: concurrent writes between them would not be recoverable from the pre-repair backup.

```sh
cargo run -p janitor --bin repair-csv-ledger -- \
  --apply --database chotu.db --archive "$HOME/chotu_drop/archive" \
  --backup chotu.before-csv-repair.db --currency CAD
```

Use the configured base currency for `--currency`. The command creates a consistent SQLite backup including committed WAL contents and refuses to overwrite an existing backup. It parses and validates each archived CSV, reports each failed file and its error, and shows the skipped file count in the summary. It rebuilds usable sources in a single atomic transaction, removing only legacy CSV records identified by those sources' hashes. Skipped sources' legacy rows remain unreconciled; correct those exports and rerun to finish their repair. An empty archive or an archive with no usable sources fails without changing the ledger. Email/receipt records and unrelated tables stay untouched. Reruns reconcile existing corrected rows rather than adding duplicates; use a new backup filename each time.

CSV identity tables follow the repository's logical-reference convention, without SQLite foreign keys. Imports explicitly clean orphaned metadata and transactionally upgrade tables created by earlier preview repairs, preserving ledger identities. Date-only preview records retain at least statement-level date authority, so a statement replay cannot downgrade their saved dates; an activity replay can still upgrade authority.

To roll back, stop Chotu before restoring the backup with SQLite's `.restore` command; do not copy over a database while its WAL is open:

```sh
sqlite3 chotu.db ".restore chotu.before-csv-repair.db"
```

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
| No member `signal_aci` | Direct messages are rejected; the exact configured `SIGNAL_GROUP_ID` remains authorized |

---

## Setup order (practical)

1. Rust + Ollama models + `just setup` (+ `just prereqs` to pull models)
2. Link signal-cli as a secondary device; starting the documented daemon separately is optional
3. Set each allowed member's `signal_aci` in `config.yaml`; optionally set `SIGNAL_GROUP_ID`; set `SIGNAL_ACCOUNT`, `SIGNAL_CLI_DATA_DIR`, `SIGNAL_CLI_SOCKET`, and `GEMINI_API_KEY`; then `just run`
4. Google OAuth clients → run `/login health …`, `/login gmail`, or `/login calendar …` from an authorized DM (Health/Calendar are self-only; groups cannot mutate OAuth)
5. Optionally set `OLLAMA_MODEL=qwen3.5:9b` (and pull that model) for better triage
6. Add OpenRouter (+ Finnhub) when you want `/research`

See also: the [project overview](https://github.com/Prajaktcs/chottu/blob/main/README.md) and the [setup commands](./commands/setup.md) and [health commands](./commands/health.md).
