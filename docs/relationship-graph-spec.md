# Spec: Local Relationship Graph (YouSpot-shaped, Chotu-native)

Add a **local second brain for people**: stable Person nodes, multi-channel
identities, timed interactions, and follow-ups — then wire that graph into
`/brief`, `/cal`, and `/memory`. Inspired by AI personal CRMs like YouSpot,
but shaped as a Chotu domain extension, not a cloud CRM product.

**Status:** design only · **Owner agents (planned):** `chotu-common` (schema +
resolve/merge), Streamer / Janitor / calendar (ingest adapters), Coordinator
(brief, chat, memory person-scope)

---

## Motivation

Chotu already sees people as **side effects**:

- email senders on tasks (`email_sender`)
- calendar attendees (parsed in `calendar.rs`, then discarded when building
  `CalendarEvent`)
- journals, digests, personal references, and tasks in `/memory`

What is missing is a **stable person node** those edges hang off. Without it:

- “Who is Jane?” is keyword search, not identity
- meeting prep cannot say “last talked 3 weeks ago”
- follow-ups drift into orphan tasks with no relationship context
- each new channel (WhatsApp export, contacts sync) invents its own contact list

The product is not a contacts UI. The product is **attention**: who matters,
what is unfinished, and what context should surface where you already look
(`/brief`, `/cal`, `/memory`).

Finance asks “what do I own?” Health asks “what did I eat?” Relationships ask
“who matters, and what’s unfinished?”

---

## Design principles

1. **One graph, many adapters.** Email, calendar, contacts, Signal notes, chat
   exports, and future bridges all emit the same normalized ingest event.
   Domain logic never cares which pipe fired.
2. **Identity must be stable.** Exact email / phone / handle auto-links. Fuzzy
   name-only matches propose merge; they do not auto-merge.
3. **History stays local.** The graph, notes, and embeddings live on the Mac
   mini. Optional one-shot enrichment is fine; no standing copy of the social
   graph in a third-party CRM.
4. **Leverage existing surfaces first.** Prefer brief / calendar / memory
   injection over a new CRM chrome or pipeline stages.
5. **Attention over archive.** Completeness of every WhatsApp message is less
   important than last-touch freshness, open loops, and meeting prep.
6. **Household scoping still applies.** A member’s network is not automatically
   household-wide. Linked DMs stay private unless explicitly shared later.

---

## Non-goals (v0–v1)

- Becoming a YouSpot / HubSpot competitor (multi-tenant SaaS, billing, MCP for
  everyone, cloud agents that send mail)
- Live LinkedIn / X scraping or ToS-fragile social sync
- Outbound email / auto-messaging people (already a Chotu non-goal)
- Fancy CRM UI, deal pipelines, or enrichment APIs as source of truth
- Perfect entity resolution on day one

---

## Architecture fit

Keep it Chotu-shaped:

```text
email | calendar | contacts | signal notes | telegram/whatsapp export | csv
                              │
                              ▼
                   normalize → Identity hints
                              │
                              ▼
                   resolve/merge → Person
                              │
                              ▼
                   Interaction / Relation / Follow-up
                              │
                 ┌────────────┼────────────┐
                 ▼            ▼            ▼
              /brief        /cal        /memory
```

| Piece | Role |
| :--- | :--- |
| `chotu-common` | Schema, resolve/merge, queries, shared ingest types |
| `streamer` | Email adapter (senders, ACTION_ITEM people, threads) |
| Calendar client | Keep attendees; emit meeting interactions |
| `janitor` | Contacts CSV / vCard / LinkedIn export / chat export drops |
| `coordinator` | `/people` (later), brief section, memory person-scope |
| Optional later crate | Thin `relationship-coach` helpers — only after the loop is useful |

No separate WhatsApp-CRM daemon until every adapter shares one write path.

---

## Data model (proposed)

New migration in `chotu-common/migrations/` (names illustrative; finalize at
implementation time).

```sql
-- Canonical person node
CREATE TABLE IF NOT EXISTS people (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Channel-specific identity anchors hanging off a person
CREATE TABLE IF NOT EXISTS person_identities (
    id TEXT PRIMARY KEY,
    person_id TEXT NOT NULL,          -- logical FK -> people.id
    kind TEXT NOT NULL,               -- email | phone | signal_aci | telegram | handle | other
    value TEXT NOT NULL,              -- normalized (lowercase email, E.164 phone, ...)
    display_name TEXT,
    source TEXT NOT NULL,             -- google_contacts | gmail | calendar | signal | import | manual
    confidence REAL NOT NULL DEFAULT 1.0,
    created_at TEXT NOT NULL,
    UNIQUE (kind, value)
);

CREATE INDEX IF NOT EXISTS idx_person_identities_person
    ON person_identities(person_id);

-- Timed edges: messages, meetings, notes, contact-card syncs
CREATE TABLE IF NOT EXISTS interactions (
    id TEXT PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,               -- message | meeting | note | contact_card | other
    source TEXT NOT NULL,             -- gmail | calendar | google_contacts | signal | whatsapp_export | ...
    external_id TEXT,                 -- message-id / event id / export row key
    summary TEXT,
    owner_member_id TEXT,             -- NULL = household-shared; else linked-DM owner
    created_at TEXT NOT NULL,
    UNIQUE (source, external_id)
);

CREATE INDEX IF NOT EXISTS idx_interactions_occurred
    ON interactions(occurred_at);

-- Participants on an interaction
CREATE TABLE IF NOT EXISTS interaction_people (
    interaction_id TEXT NOT NULL,
    person_id TEXT NOT NULL,
    role TEXT,                        -- from | to | attendee | mentioned | self
    PRIMARY KEY (interaction_id, person_id)
);

-- Typed relationship edges (optional early; can start as notes)
CREATE TABLE IF NOT EXISTS relations (
    id TEXT PRIMARY KEY,
    person_id TEXT NOT NULL,
    kind TEXT NOT NULL,               -- friend | colleague | investor | doctor | family | other
    strength REAL,                    -- optional 0..1
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Explicit or inferred follow-ups
CREATE TABLE IF NOT EXISTS followups (
    id TEXT PRIMARY KEY,
    person_id TEXT NOT NULL,
    due_at TEXT,
    status TEXT NOT NULL DEFAULT 'open',  -- open | done | snoozed
    reason TEXT,
    task_id TEXT,                     -- optional link into existing tasks
    owner_member_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

### Normalized ingest event (adapter contract)

Every source ends here:

```text
ingest_event {
  source: "gmail" | "calendar" | "google_contacts" | "signal" | "whatsapp_export" | ...
  external_id: string
  occurred_at: datetime
  participants: [{ kind, value, display_name? }]
  kind: "message" | "meeting" | "contact_card" | "note"
  summary?: string
  owner_member_id?: string
}
```

### Merge policy

| Signal | Action |
| :--- | :--- |
| Exact email / phone / handle | Auto-link to existing Person |
| Same display name only | Queue proposed merge; human confirms |
| Conflicting high-confidence identities | Never silent-merge; surface conflict |

Keep `source` + `confidence` on every identity and interaction so bad links are
auditable and reversible.

---

## Ingestion roadmap

### Phase 0 — Schema + manual notes

- Tables above
- Signal/manual: “met X about Y”, “remind me to ping Z”
- Enough to prove Person + Interaction + Follow-up without OAuth expansion

### Phase 1 — Free graph fuel already in-repo

| Source | What to keep / emit |
| :--- | :--- |
| **Gmail (Streamer)** | From/to, ACTION_ITEM counterparties, thread touch |
| **Google Calendar** | Attendees (stop discarding after RSVP self-status) |
| **Tasks** | Link `email_sender` → Person when present |

### Phase 2 — Contact sync (seed stable nodes)

| Source | Fit | Notes |
| :--- | :--- | :--- |
| **Google Contacts (People API)** | Best first live sync | Same OAuth family as Calendar |
| **vCard / CSV via `~/chotu_drop/`** | Easy batch | Janitor path |
| **LinkedIn export CSV** | Backfill only | Not live sync |

Contacts create **identity anchors**. Conversations create **edges + freshness**.

### Phase 3 — Chat channels (exports before live bridges)

| Channel | Realistic path | Difficulty |
| :--- | :--- | :--- |
| **Signal** | Notes + bot-visible household traffic; not full address-book scrape | Medium |
| **Telegram** | Export / owned-client path if pursued | Medium–hard |
| **WhatsApp** | Official export ZIP → Janitor; avoid shady live scrapers | Hard / fragile |

Order: prove value on contacts + email + calendar, then chat **exports**, then
live bridges only where APIs and ToS allow.

---

## Leverage surfaces (where the graph pays rent)

Do not build a contacts app first. Inject context into surfaces that already
own attention:

1. **`/brief`** — who you are seeing today + last touch + open loop
2. **`/cal`** — meeting prep card (“last talked 3 weeks ago; they mentioned X”)
3. **`/memory`** — retrieve by *person*, not only keyword
4. **Streamer** — attach people on classify, not just tasks/bills
5. **Reflection** — “who did I neglect this week?”
6. **Tasks** — follow-ups as first-class edges, optionally linked to `tasks`

Cross-channel questions the graph should eventually answer:

- last touch across channels (“email 12d, WhatsApp 2d”)
- open loops (asked something, no reply)
- decay (important + cold)
- channel preference (they answer on WhatsApp, not email)

---

## Privacy & safety

Matches Chotu’s existing line:

- Historical graph, notes, and embeddings **stay local**
- No standing social-graph copy in a SaaS CRM
- Prefer OAuth-owned APIs and user-initiated exports over scrapers
- `owner_member_id` on interactions / follow-ups mirrors `memory_chunks` scoping
- Linked personal DMs do not dump another member’s network by default
- Agents suggest follow-ups; humans decide whether to reach out (no auto-send)

---

## Suggested milestones

| Milestone | Outcome | Done when |
| :--- | :--- | :--- |
| **M1** | Schema + resolve helpers + manual note ingest | Can create Person, attach identity, log interaction from Signal |
| **M2** | Email + calendar adapters | Attendees and senders land on People; brief shows today’s people |
| **M3** | Google Contacts sync (or vCard drop) | Seeded identity anchors; fewer duplicate Janes |
| **M4** | `/memory` person-scope + follow-ups | “Who is X?” / “prep for meeting with Y” grounded in graph |
| **M5** | Chat export adapters | WhatsApp/Telegram history contributes last-touch without live scrape |

Ship M1–M2 before any new coach crate or CRM UI.

---

## Open questions

1. Should People default to **per-member private** graphs, with optional shared
   household people (family doctor, school, etc.)?
2. How aggressive should auto-create-from-email be for noreply / marketing
   senders? (Likely: skip machine senders; only human-ish counterparties.)
3. Do follow-ups always create a `tasks` row, or only link when the user wants
   a timed reminder?
4. First contacts path: Google People API OAuth now, or vCard/CSV drop to
   avoid new OAuth scopes until the loop is proven?

---

## References in this repo

- Architecture / agents: [`ARCHITECTURE.md`](../ARCHITECTURE.md)
- Day loop (`/brief`, `/cal`): [`docs/commands/day-loop.md`](./commands/day-loop.md)
- Memory RAG: [`docs/commands/memory.md`](./commands/memory.md)
- Later domains list: [`TODO.md`](../TODO.md)
- Calendar attendee parse (currently discarded beyond self RSVP):
  `chotu-common/src/calendar.rs`
- Email sender on tasks: `streamer` → `tasks.email_sender`
