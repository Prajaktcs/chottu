# Chotu Project Architecture & Engineering Guidelines

This document is the runtime map for **Project Chotu** plus the Rust standards for the workspace. Operator walkthroughs (commands, env vars) live under [`docs/README.md`](docs/README.md); product surface is in [`README.md`](README.md).

---

## Runtime topology

`just run` starts the **coordinator** binary as a supervisor. It opens SQLite (`DATABASE_PATH`, default `chotu.db`), loads `config.yaml`, builds a local Ollama client, then `tokio::spawn`s four long-lived tasks:

| Task | Crate | Owns |
| :--- | :--- | :--- |
| **Streamer** | `streamer` | Gmail IMAP IDLE → nine-way local classification → ledger / tasks / bills / travel / digests / trash |
| **Janitor** | `janitor` | Watches `~/chotu_drop/` for CSV/PDF ingest + ledger hygiene |
| **Health Coach** | `health-coach` | Scheduled Google Health sync (~20:45 local nutrition merge; late steps ~23:00 ET), `/plan` storage, coach/trend helpers |
| **Coordinator** | `coordinator` | Telegram (Teloxide), OAuth localhost callback, morning brief, evening reflection, evening `/networth` overview, task due pings |

**Finance Advisor** (`finance-advisor`) is a library used by the bot (`/research`, `/networth`, `/monthly`, budgets). It is not a fifth spawned daemon. **chotu-evals** is a separate crate for golden-set classifier checks. Shared code lives in **chotu-common** (DB/migrations, family config, OAuth, LLM clients, calendar, memory RAG, quotes, Finnhub/Yahoo, food timing).

Agents persist through SQLite (and files under `CHOTU_BRAIN_DIR` / `~/chotu_brain`). In-process work uses cloned `SqlitePool` + `AppConfig` (Telegram holds config behind `tokio::sync::RwLock` so `/link` can rewrite YAML).

```text
Telegram / IMAP / drop folder / Google APIs
                 │
                 ▼
        coordinator binary (supervisor)
   ┌─────────┬──────────┬──────────────┬─────────────┐
   │streamer │ janitor  │ health-coach │ telegram +  │
   │         │          │              │ schedulers  │
   └────┬────┴────┬─────┴──────┬───────┴──────┬──────┘
        │         │            │              │
        └─────────┴────────────┴──────────────┘
                         │
              SQLite + ~/chotu_brain + config.yaml
```

---

## Compute tiers (what actually runs)

| Tier | Used for |
| :--- | :--- |
| **Local Ollama** (`OLLAMA_MODEL`, prefer `qwen3.5:9b`) | Email triage, free-text intent, memory answers, evening reflection, coach tips, weekly `/plan`, food date/time phrasing |
| **Local embeddings** (`nomic-embed-text`) | `/memory` RAG index over journals, newsletter digests, personal references, tasks |
| **Gemini** (`GEMINI_API_KEY` — **required to start the Telegram loop**) | Food photos (package/plate), PDF ingest, unstructured nutrition estimates |
| **OpenRouter** | `/research` propose → score panel (default Sol + Qwen3.8-Max + Kimi K3) → Kimi judge |
| **Finnhub** (optional) | Research universe market-cap filter |
| **Yahoo Finance** | Live `/networth` quotes; research cap fallback for class shares, Canadian ETFs, and other international symbols Finnhub mishandles |
| **Open Food Facts** | Barcode lookup (no key) |
| **Google** | Health two-way nutrition/activity/exercises, Gmail IMAP OAuth, per-member Calendar |

Health Coach scheduled sync still runs if Gemini is missing (omega-3 / triglyceride fills stay zero). `/research` refuses without `OPENROUTER_API_KEY`.

---

## Family isolation & Telegram

- Roster, `nutrition_goals`, `fitness_goals`, `core_values`, `spend_budgets`, and investment philosophy live in **gitignored** `config.yaml`.
- Each adult DMs the bot and `/link <member_id>` (writes `telegram_chat_id`). Once any member is linked, unknown chats are rejected (`/chat` and `/link` still work for setup).
- Linked personal DMs see **only that member’s** health, training plan, coach tips, trends, and (for `/brief`) calendar/tasks/nutrition slice. Food mutations from a linked DM are **self-only**.
- Household chat (or optional `TELEGRAM_CHAT_ID`) is the family-wide surface. Proactive fan-out: morning brief, evening reflection, portfolio overview (times from `config.yaml` `schedules`; blank = off), budget 80%/100% alerts, research reports — to linked DMs plus optional shared chat.

---

## Health & day loop (implementation notes)

- Telegram `/food` (and photos) resolve relative days and **meal windows** (breakfast / lunch / snacks / dinner) in `chotu-common` before logging; Google Health meal labels use matching hour buckets.
- Slow Gemini food work sends a short progress nudge on the same chat.
- Exercises persist as structured `exercise_log` (type, duration, active kcal, start/end) and feed `/plan` week progress + coach tips — not keyword heuristics on free text.
- Tasks: email-derived or Telegram-created; dated tasks can write Google Calendar; complete/snooze keep the event in sync; open/snoozed lists and due reminders use inline Done / +1d buttons.
- `/tasks complete all` in a linked DM completes that member’s + unassigned tasks; household chat requires `confirm`.

---

## Storage

- **SQLite** via sqlx (compile-time checked queries where used): ledger, food/exercise logs, daily health summaries, tasks, bills, evaluation logs, spend-budget overrides, weekly fitness plans, etc.
- **Journals / RAG corpus**: `~/chotu_brain/` (override `CHOTU_BRAIN_DIR`) — reflection Markdown, newsletter digests, personal references, research reports.
- **OAuth refresh tokens**: written to `.env` by `/login` (`HEALTH_REFRESH_TOKEN_<MEMBER>`, `CALENDAR_REFRESH_TOKEN_<MEMBER>`, Gmail token). Not stored in YAML.

---

## 1. Safety & The "Zero-Unsafe" Policy

- **Strict Safe Rust**: We enforce a strict **zero-`unsafe`** code policy. No `unsafe` blocks are allowed in the source code of any crate in this workspace.
- **FFI & External Boundaries**: If a dependency or OS-level feature requires `unsafe` code (e.g., interfacing with a C library), it must be:
  1. Flagged explicitly in an architectural review.
  2. Wrapped in a completely safe, idiomatic Rust API boundary in `chotu-common`.
  3. Documented with a `// SAFETY:` comment explaining why the invariants cannot be violated.

---

## 2. Asynchronous Tokio & Concurrency Patterns

- **Task Decoupling**: Each agent runs as an independent Tokio task. They communicate via the database (persistent state) or cloned handles (`SqlitePool`, `Bot`, `RwLock<AppConfig>`), not a custom IPC bus.
- **Non-Blocking Execution**: Never call blocking OS operations (like `std::fs` or `std::thread::sleep`) inside an async context. Instead, use their Tokio equivalents (`tokio::fs` or `tokio::time::sleep`). If a blocking call is unavoidable (e.g. standard CSV parsing), wrap it in `tokio::task::spawn_blocking`.
- **Locking Minimization**:
  - Prefer Tokio channels (`tokio::sync::mpsc` or `broadcast`) over sharing state via `Arc<Mutex<T>>`.
  - If a `Mutex` is necessary, use standard `std::sync::Mutex` if the lock is **never** held across an `.await` boundary. Use `tokio::sync::Mutex` *only* if the lock must be held across yield points, to avoid blocking the runtime executor thread.

---

## 3. Idiomatic Error Handling

We distinguish between **application binaries** (agents) and **libraries** (`chotu-common`, `finance-advisor`, `health-coach`):

### In library crates

- Do **not** use `anyhow` for errors that callers might need to inspect and handle.
- Instead, define semantic, strongly-typed error enums using the `thiserror` crate (or custom implementations of the `std::error::Error` trait). This allows calling agents to match on specific error kinds.
- Avoid `.unwrap()` or `.expect()`. Use the `?` operator or map errors explicitly.

### In Agent Binaries (`streamer`, `janitor`, `coordinator`)

- Use the `anyhow` crate for high-level error propagation and context.
- Add context to errors using `context("...")` to produce human-readable backtraces when an agent fails.

---

## 4. Ownership, Lifetimes, and API Design

- **Prefer Owned Data in Tasks**: To satisfy Tokio's `'static` requirement for spawned tasks, favor moving owned data (`String`, `PathBuf`) or reference-counted pointers (`Arc<T>`) rather than dealing with complex lifetime parameters (`'a`).
- **Newtype Pattern**: Use Rust's newtype pattern to enforce compile-time validation (e.g., `struct EmailAddress(String);` or `struct TransactionAmount(Decimal);`). This prevents passing incorrect data formats to functions.
- **Compile-Time SQL Validation**: Leverage `sqlx`'s type checking to catch schema-query mismatches at compilation time.
