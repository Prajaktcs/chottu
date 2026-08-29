# Spec: Per-Member Health Condition Tracking

Make the health coach condition-aware: track chronic conditions (e.g. plaque
psoriasis) per family member, tag food logs against a fixed vocabulary, collect
a daily symptom score during evening reflection, and surface lag-aware trends —
without Chotu ever inventing medical advice.

**Status:** M1–M2 shipped (schema, config, tag-at-log-time) · remaining M3–M6 · **Owner agents:** Coordinator (Telegram, reflection),
Health Coach (tips, trends)

---

## Motivation

Conditions like psoriasis are hard to track because:

- Flares often lag food by 1–3 days, so same-day "food vs skin" comparisons are
  mostly noise.
- Trigger science is weak and individual — a generic "psoriasis diet" from an
  LLM would be worse than useless.
- Subjective symptoms only become data if capturing them is nearly free.

So the design is: **user-authored watchlists + one daily score + lag-aware
correlation**, not LLM diet opinions.

## Design principles

1. **Chotu never invents triggers or diagnoses.** Watchlists are authored by
   the member (from a doctor, an elimination trial, or experience). The coach
   prompt already enforces "never invent medical diagnoses" — condition context
   rides the same rule.
2. **Flag, don't block.** A watchlist hit adds one heads-up line to the food
   confirmation. The meal is always logged.
3. **Closed tag vocabulary.** Tags come from a fixed seeded set. Classification
   into a closed set is reliable for small local models; open-ended tagging
   fragments and breaks the lag join.
4. **Tag at write time.** Tags are stored on the food log row when it is
   created, so trend analysis is a plain SQL join — never a re-parse of
   history.
5. **Privacy matches existing health data.** Condition definitions live in
   gitignored `config.yaml`; scores and watchlists live in local `chotu.db`.
   Check-ins and flags go only to the member's linked DM, never the household
   chat.

## Non-goals

- Medical advice, diagnosis, or scraping health sites at log time.
- Photo-based skin assessment.
- Blocking foods or rewriting macro goals because of a condition.
- Multi-question symptom surveys (one 0–5 score per condition per day).

---

## Data model

### Config (`config.yaml`, gitignored)

Condition *definitions* follow the `fitness_goals` pattern: rarely edited,
validated at load, kept out of git.

```yaml
family:
  members:
    - id: alex
      # ... existing fields ...
      health_conditions:
        - id: psoriasis                # slug, unique per member
          label: "plaque psoriasis"    # display name
          check_in: true               # evening reflection asks about it
          lag_window: [1, 3]           # correlate food from 1–3 days prior
          notes: "flares on elbows; stress + late nights also matter"
```

`FamilyMember` gains `health_conditions: Vec<HealthCondition>` (serde default
empty). Load-time validation warnings (same style as
`FitnessGoals::validation_warnings`):

- duplicate condition ids within a member
- `lag_window` not `[min, max]` with `0 <= min <= max <= 14`
- empty `id`/`label`

Rationale for config vs DB split: config = who you are and what you are working
toward; DB = data that accumulates and gets tuned. Watchlists change often and
are managed from Telegram, so they are DB rows (below). Condition definitions
change ~yearly.

### Database (new migration in `chotu-common/migrations/`)

```sql
-- Fixed tag vocabulary. Seeded here; extendable by future migrations only.
CREATE TABLE IF NOT EXISTS food_tags (
    tag TEXT PRIMARY KEY,              -- e.g. 'alcohol'
    label TEXT NOT NULL,               -- e.g. 'Alcohol'
    description TEXT NOT NULL DEFAULT ''
);

-- Tags attached to a food log row at write time (same tx as the food insert).
CREATE TABLE IF NOT EXISTS food_log_tags (
    food_log_id TEXT NOT NULL,         -- FK -> food_log.id
    tag TEXT NOT NULL,                 -- FK -> food_tags.tag
    source TEXT NOT NULL DEFAULT 'llm', -- 'llm' | 'keyword' | 'manual'
    PRIMARY KEY (food_log_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_food_log_tags_tag ON food_log_tags (tag);

-- Per-member, per-condition watchlist (managed via /watch in Telegram).
CREATE TABLE IF NOT EXISTS condition_watchlist (
    family_member_id TEXT NOT NULL,
    condition_id TEXT NOT NULL,        -- matches config health_conditions.id
    tag TEXT NOT NULL,                 -- FK -> food_tags.tag
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (family_member_id, condition_id, tag)
);

-- Daily 0–5 symptom score captured during evening reflection.
CREATE TABLE IF NOT EXISTS condition_checkin (
    family_member_id TEXT NOT NULL,
    date TEXT NOT NULL,                -- YYYY-MM-DD local civil day
    condition_id TEXT NOT NULL,
    score INTEGER NOT NULL,            -- 0 = calm .. 5 = worst flare
    note TEXT,                         -- optional one-liner (itch / stress / ...)
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (family_member_id, date, condition_id)
);
```

Skipped days stay absent (no imputation). Re-answering the same day upserts.

### Seeded tag vocabulary

Drawn from common elimination-diet groups; small enough to fit in a
classification prompt:

| tag | examples |
| :-- | :-- |
| `alcohol` | beer, wine, cocktails, tiramisu |
| `added_sugar` | soda, desserts, sweetened drinks |
| `dairy` | milk, cheese, cream, butter-heavy dishes |
| `gluten` | wheat bread, pasta, most baked goods |
| `red_meat` | beef, lamb, pork |
| `processed_meat` | bacon, sausage, deli meats |
| `fried` | deep-fried anything |
| `spicy` | chili-forward dishes |
| `nightshades` | tomato, potato, eggplant, peppers |
| `caffeine` | coffee, energy drinks, strong tea |
| `shellfish` | shrimp, crab, mussels |
| `eggs` | eggs and egg-heavy dishes |
| `soy` | tofu, soy sauce, edamame |
| `citrus` | oranges, lemons, grapefruit |

Vocabulary changes ship as forward migrations (never edit the seed migration —
see `.cursor/rules/sql-migrations.mdc`).

---

## Behavior

### 1. Tagging at log time

Every path that inserts a `food_log` row also writes `food_log_tags` in the
same transaction:

- **LLM path** (`/food`, photo captions, natural-language chat): the existing
  nutrient-estimation call (Gemini or Ollama) gains one instruction — *"pick
  zero or more tags from this closed list"* — and returns a `tags` array.
  Unknown tags are dropped.
- **Deterministic fallback**: a small keyword/alias map (beer → `alcohol`,
  latte → `dairy` + `caffeine`) runs when no LLM is available (e.g. the
  slash-`/food` fast path) or the LLM returns nothing. `source` records which
  path produced each tag.

Tags are attached to **all** members' food logs regardless of conditions, so a
condition added later has history to analyze from day one.

`/undofood`, `/clearfood`, `/adjustfood` delete or rebuild `food_log_tags`
alongside `food_log` rows.

### 2. Soft flags after `/food`

After a successful log, if the row's tags intersect the member's
`condition_watchlist`, append one line to the confirmation:

```text
✅ Logged for praj (today):
• 2 beers and nachos — ~780 kcal | P 18g C 62g F 38g
⚠️ On your psoriasis watchlist: alcohol, fried
```

- **Deduped per member per day per tag** — the second beer of the day does not
  re-flag `alcohol`.
- Linked-DM only. Household chats logging for a member do not broadcast that
  member's condition flags.
- Never blocks or edits the log.

### 3. Evening reflection check-in

For each condition with `check_in: true`, the `/reflect` prompt (scheduled or
manual) appends a structured closer after the normal journaling prompt:

```text
Also — plaque psoriasis today, 0–5 (0 = calm, 5 = flare)?
Optional one word after the number: itch / plaques / sleep / stress.
```

Reply parsing: first integer 0–5 found per condition line → `condition_checkin`
upsert; any following word(s) on that line → `note`. Everything else remains
the normal journal entry saved under `~/chotu_brain/Journal/`, and the score is
also written into the journal markdown so memory search sees it.

No score in the reply → no row (skip is fine, never nag twice).

### 4. Coach awareness (`/status`, `/trends` tips)

`FitnessCoachContext` gains:

- `conditions: Vec<(label, watchlist tags)>`
- `recent_condition_scores: Vec<(label, last-7-day scores)>`
- today's watchlist-tag hits

The coach system prompt already forbids invented diagnoses; add: *"You may
reference the member's own watchlist and their reported scores. Never propose
new trigger foods."*

### 5. `/watch` and `/tags` commands (Telegram)

| Command | Behavior |
| :--- | :--- |
| `/tags` | List the tag vocabulary |
| `/watch` | Show my conditions and their watchlists |
| `/watch add <condition> <tag>` | Add a vocabulary tag to my watchlist |
| `/watch remove <condition> <tag>` | Remove it |

Scoped like other personal commands: a linked DM manages only that member's
watchlists. Unknown condition/tag → usage message listing valid values.

### 6. Lag-aware trends (`/trends`, weekly line)

For each condition with ≥ ~7 check-ins in the window, `/trends` renders a
condition block:

```text
🩺 plaque psoriasis (last 14 days, 11 check-ins)
   2 1 1 3 4 2 1 . 1 2 3 1 2 1   (. = skipped)
   ▲ alcohol in lag window on: Aug 12, Aug 16
```

- The lag join: a day's score is compared against watch tags present in
  `food_log_tags` during that day's `lag_window` (e.g. days −1 to −3).
- **Sleep from `health_family_summary` is included in the join from day one**
  — otherwise a food takes credit for a bad week of sleep.
- Association sentences ("scores averaged 1.2 higher after alcohol days") are
  **gated**: render only when both arms have ≥ 10 days, always phrased as
  tentative, never causal.
- A short weekly Sunday line to the linked DM replaces per-meal "correlation"
  spam.

---

## Milestones

| # | Scope | Depends on |
| :-- | :--- | :--- |
| **M1** | Migration (4 tables + seed), `health_conditions` config + validation | — |
| **M2** | Tag emission in nutrient parse + keyword fallback; tags written/deleted with food rows | M1 |
| **M3** | `/reflect` check-in question, reply parsing, `condition_checkin` upsert, journal inclusion | M1 |
| **M4** | Soft flags (per-day dedupe) + coach context enrichment | M2 |
| **M5** | `/watch` + `/tags` Telegram commands | M1 |
| **M6** | Trends condition block, sleep-aware lag join, gated association text, weekly line | M2 + M3 |

**M1+M2 ship first** (tagged food history is the slowest asset to build), then
**M3** (every week without check-ins is a week of trends that can't be computed
later). M4/M5 in either order; M6 last, once real data exists.

## Open questions

- Should `note` on check-ins be constrained to suggested words (itch / plaques
  / sleep / stress) for future grouping, or stay free text? (Lean: free text,
  group later with the local model.)
- Backfill: run the keyword tagger over historical `food_log` rows in M2, or
  start clean? (Lean: backfill with `source = 'keyword'` — cheap and honest.)
- Weekly summary day/time — reuse an existing schedule slot or add
  `condition_weekly` to `schedules`?
