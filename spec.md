# Software Requirements Specification (SRS)
## Project Name: Project Chotu (Personal Financial & Family Reflection Suite)
## Target Stack: Rust (Asynchronous Tokio Runtime), Local Ollama API, Cloud Gemini API, SQLite Database, Teloxide (Telegram)

---

## 1. System Overview & Educational Objectives
Project Chotu—named after the agile, hyper-efficient apprentice helper in Mumbai slang—is a self-hosted, local-first multi-agent system designed to manage daily family financial metrics, multi-user health logging, and asynchronous personal journaling.
* **User Learning Objective:** The developer is mastering Rust systems patterns (lifetimes, async execution, type safety, and concurrency). The agent **must not** generate code blocks without explaining ownership boundaries, `tokio::spawn` mechanics, or structural error handling (`Result`/`Option`).
* **Antigravity Operational Mandate:** The agent must operate exclusively in **Planning Mode**. Before any file mutations, it must generate a step-by-step task artifact and wait for user comment approval via the Manager View.

---

## 2. Advanced Compute Tiering & Data Isolation Architecture

To maximize data privacy and eliminate API overhead, Project Chotu implements a dual-layer compute architecture based on payload density and cognitive load.

```
┌────────────────────────────────────────────────────────┐
│               Incoming Pipeline Payload                │
└────────────────────────────────────────────────────────┘
│
┌────────────────────┴────────────────────┐
▼ (Text Ingestion & Triage)               ▼ (Complex / Multi-page PDF / Nutritional)
┌──────────────────────────────┐          ┌──────────────────────────────┐
│     Tier 1: Local Ollama     │          │     Tier 2: Cloud Gemini     │
│ - Email Triage (Llama 3.2 3B)│          │ - Multi-page Document Ingest │
│ - Simple Ledger Extraction   │          │ - Family Nutritional Mapping │
│ - Bounded Prompt Generation  │          │ - Long-Context Quarterly Eval│
└──────────────────────────────┘          └──────────────────────────────┘
```

### Compute Tier Boundaries
* **Tier 1: Local Ollama (`localhost:11434`)** -> Handles standard text strings, rule-based classification hooks, and context-isolated prompt generation. Models: `llama3.2:3b` for fast triage, `deepseek-r1:8b` for structured night-time reflection.
* **Tier 2: Cloud Gemini API** -> Triggered dynamically via Rust match arms when handling multi-page compound PDF statements, long-context semantic trend analysis over multi-month journal logs, or web-grounded family health research tasks.

---

## 3. Core Agent Lifecycle & Operational Boundaries

### Agent 1: "The Streamer" (Email Triage Daemon)
* **Objective:** Securely idle on an IMAP mailbox, filter incoming promotional clutter, and parse transactional alerts.
* **Anti-Hallucination Guardrail:** Native structural JSON schema constraints applied at the Ollama API request level. The agent cannot output raw conversational text.
* **Classification Arms:** 
  * `TRASH` -> Moved to an isolated `AI-Trash` staging label for manual user review/purge.
  * `ARCHIVE` -> Receipts/logs committed to a history log.
  * `LEDGER_STREAM` -> Transaction payload forwarded to the storage layer.

### Agent 2: "The Janitor" (Document Drop Worker)
* **Objective:** Asynchronously monitor `~/chotu_drop/` for batch file mutations (CSVs, PDFs).
* **Compute Tier Logic:** 
  * Standard CSVs -> Parsed natively by the Rust `csv` crate (Zero LLM use).
  * Single-page direct text PDFs -> Handled by local `llama3.2:3b`.
  * Multi-page/Scanned Image PDFs -> Escalated to `CloudGemini` for advanced multi-modal extraction.

### Agent 3: "The Coordinator & Bookkeeper" (Evening Reflection Engine)
* **Objective:** Drive the asynchronous nightly reflection loop via Telegram and recalculate portfolio metrics via market data endpoints.
* **RAG Boundary:** The system prompt strips away global model knowledge. The engine can *only* summarize and construct prompts explicitly grounded in the day's compiled SQLite transaction logs.
* **Journal Storage:** Saves daily reflections as human-readable Markdown files in a local directory (`~/chotu_brain/Journal/YYYY/MM/YYYY-MM-DD.md`) formatted with structured YAML frontmatter.

### Agent 4: "The Health Coach" (Multi-User Family Sync)
* **Objective:** Ingest, isolate, and maintain daily nutrition, activity, and sleep telemetry for the entire family household (User, Wife, Toddler).
* **Ingestion Channels:**
  * **Food/Symptom Logs:** Parses incoming text strings via Telegram tagged with identifiers (e.g., `/food`, `/kid`), offloading unstructured nutritional descriptions to `CloudGemini` for calorie/macro approximation.
  * **Automated Device Sync:** Monitors the local iCloud Drive mount path (`~/Library/Mobile Documents/com~apple~CloudDocs/ChotuDrop/health/`) for unique telemetry files (`praj_sync.json`, `wife_sync.json`, `kid_sync.json`) pushed via mobile background shortcut automations.
* **Execution Boundary:** Enforces data isolation by applying strict `family_member_id` tags to entries before writing to the shared database.

---

## 4. Storage Layer Schema (SQLite + SQLx)

Compiled and type-verified at build time using `sqlx`. Includes metadata tables to facilitate automated regression tracking and evaluation recording.

```sql
CREATE TABLE IF NOT EXISTS financial_ledger (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    amount REAL NOT NULL,
    currency TEXT NOT NULL,
    institution TEXT NOT NULL,
    merchant TEXT NOT NULL,
    category TEXT NOT NULL,
    source_type TEXT NOT NULL     -- 'EMAIL_STREAM' or 'BATCH_DROP'
);

CREATE TABLE IF NOT EXISTS portfolio_holdings (
    ticker TEXT PRIMARY KEY,
    shares_owned REAL NOT NULL,
    average_cost REAL NOT NULL,
    last_updated DATETIME NOT NULL
);

-- Re-designed Multi-User Family Health Ledger
CREATE TABLE IF NOT EXISTS health_family_summary (
    date TEXT NOT NULL,                  -- YYYY-MM-DD
    family_member_id TEXT NOT NULL,      -- 'praj', 'wife', 'kid'
    total_calories_ingested INTEGER DEFAULT 0,
    protein_grams REAL DEFAULT 0.0,
    carbs_grams REAL DEFAULT 0.0,
    fats_grams REAL DEFAULT 0.0,
    step_count INTEGER DEFAULT 0,
    active_calories_burned INTEGER DEFAULT 0,
    sleep_hours REAL,
    perceived_energy INTEGER,            -- (Adults only)
    PRIMARY KEY (date, family_member_id) -- Prevents duplicate entries per person per day
);

-- Raw Food and Health Log Audit Table for Tracking Descriptions
CREATE TABLE IF NOT EXISTS food_log (
    id TEXT PRIMARY KEY,               -- UUID
    timestamp DATETIME NOT NULL,
    family_member_id TEXT NOT NULL,
    raw_text_description TEXT NOT NULL,
    estimated_calories INTEGER NOT NULL
);

-- Evaluation Metric Audit Log for Tracking System Prompts over time
CREATE TABLE IF NOT EXISTS evaluation_log (
    eval_id TEXT PRIMARY KEY,
    test_timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
    prompt_version TEXT NOT NULL,
    model_name TEXT NOT NULL,
    triage_accuracy REAL NOT NULL,
    extraction_faithfulness REAL NOT NULL
);
```

---

## 5. Automated Evaluation Framework (Eval Suite)

To prevent system decay, prompt drifting, or classification regressions when updating local models, Project Chotu implements an integrated automated testing suite executed via an independent testing binary (`cargo run --bin chotu_evals`).

### A. Ground-Truth Golden Dataset (`evals/dataset.json`)

The workspace maintains a structured array of real-world inputs coupled with expected programmatic assertions:

```json
[
  {
    "test_id": "tc_001",
    "input_payload": "Your Scotiabank credit card was charged $42.50 CAD at Uber Eats.",
    "expected_category": "LEDGER_STREAM",
    "expected_fields": {
      "amount": 42.50,
      "merchant": "Uber Eats"
    }
  },
  {
    "test_id": "hc_001",
    "input_payload": "Had two scrambled eggs and an avocado toast for breakfast",
    "expected_fields": {
      "dominant_macro": "fat_or_protein",
      "minimum_protein_estimate": 12.0
    }
  }
]
```

### B. The Evaluation Run Mechanics

1. **Deterministic Assertions:** For categorical routing (e.g. `TRASH` vs `LEDGER_STREAM`), the testing suite executes strict matching assertions against compiled Rust Enums.
2. **Semantic Judge Assertions:** For non-deterministic strings (e.g. Nightly Journal Summaries), the evaluation runner uses the **LLM-as-a-Judge** pattern. The actual pipeline output is passed along with the expected ground-truth baseline to a judge model context block, enforcing numeric quality logging between `0.0` and `1.0`.
3. **Regression Blocking:** If any automated evaluation run slips below a configured performance baseline (e.g., Triage Accuracy drops below 95%), the evaluation wrapper throws a compile/execution failure to prevent corrupted deployments.

---

## 6. Security, Isolation & Safety Gates

* **Strict Memory Safety:** No `unsafe` blocks are allowed anywhere in the Chotu implementation workspace.
* **Zero Executive Autonomy:** The LLM agents operate entirely as text processors/extractors. Under no circumstances can an LLM response trigger shell tool execution, system file deletion, or state modifications outside of appending data entries to the designated SQLite engine.
* **Environment Secret Segregation:** All external network parameters (IMAP servers, Telegram Bot HTTP tokens, Gemini API access tokens) must be pulled exclusively from the machine environment variables via `std::env::var` and are barred from being hardcoded inside any workspace file.
* **Zero Transmission Output (Email Sending Prohibited):** The email pipeline is strictly read-only (IMAP). The system is barred from importing SMTP or email transmission libraries (e.g. `lettre`, `mail-send`, `sendgrid`), sending emails, creating drafts, or executing forwarding/reply actions. This constraint is programmatically verified in the test suite.
