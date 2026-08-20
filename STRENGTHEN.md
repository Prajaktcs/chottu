# Strengthen existing features

Prefer hardening shipped surfaces over new domains. Work this checklist **one PR at a time**.

New product ideas (meal planning, medical-record coaching) stay parked under **Later domains** in [`TODO.md`](TODO.md). Do not pull them forward while this list is open.

## Backlog (priority order)

### 1. Structured exercise progress
- [x] Stop flattening Google Health activity fields into free-text descriptions
- [x] Persist activity type, duration, active kcal, start/end on `exercise_log`
- [x] Wire coach/plan progress to real fields (kill keyword heuristics like `"gym"` / `"workout"` for strength)
- [x] Measure `weekly_targets.cardio_minutes` from duration data, not prompts alone

**Why:** Sync already receives structured activity; coach progress guesses from text, so cardio can count as strength and cardio targets are never measured.

**Effort:** M

**Files:** `chotu-common/src/google_health.rs`, `health-coach/src/sync.rs`, `chotu-common/migrations/20260810000001_fitness_coach.sql` (or a follow-on migration), `health-coach/src/fitness_plan.rs`, `health-coach/src/coach_enrich.rs`, `health-coach/src/coaching.rs`

**Done:** `ExerciseSession` + migration `20260810000002`; progress via `classify_activity_type` / `count_strength_sessions` / `sum_cardio_minutes` (generic "Workout"/"Gym" no longer count as strength).

---

### 2. Safer `/tasks complete all`
- [x] Scope to assignee / “mine only” when invoked from a linked personal DM
- [x] Require confirmation before a household-wide wipe
- [x] Avoid completing another member’s open/snoozed backlog by accident

**Why:** Today it completes every open/snoozed row household-wide with no confirmation or assignee filter.

**Effort:** S

**Files:** `coordinator/src/telegram.rs` (`mark_all_tasks_complete`), `chotu-common/src/database.rs` (`complete_all_open_tasks`)

**Done:** Linked DMs complete `assigned_to = me OR NULL` immediately; household/unlinked chats preview then require `/tasks complete all confirm`.

---

### 3. Food mutation guards in linked DMs
- [x] From a linked personal DM, only allow food mutations for the chat’s member
- [x] Block `/food`, `/adjustfood`, `/undofood`, `/clearfood` (and free-text `FOOD` with another `member_id`) targeting other members
- [x] Keep household chat able to log for any member (existing behavior)

**Why:** Status/trends/brief health sections are scoped to self in linked DMs, but food writes still accept any member id.

**Effort:** S

**Files:** `coordinator/src/telegram.rs` (`resolve_food_member_and_description`, `resolve_optional_member_arg`, food handlers), `chotu-common/src/family.rs` (`member_for_telegram_chat`)

**Done:** `ensure_food_mutation_allowed` rejects cross-member `/food`, `/adjustfood`, `/undofood`, `/clearfood`, free-text FOOD, and food photos in linked DMs; household/unlinked chats unchanged.

---

### 4. Task ↔ calendar sync on snooze / complete
- [x] On snooze: update the linked Google Calendar event time when `calendar_event_id` is set
- [x] On complete (single or bulk): cancel/delete the linked calendar event
- [x] Keep create-path behavior (manual task with due time → calendar event) unchanged

**Why:** Create writes a calendar event and stores `calendar_event_id`; snooze only bumps SQLite dates; complete never cancels the event.

**Effort:** S–M

**Files:** `coordinator/src/telegram.rs` (`create_manual_task`, `snooze_task`, `mark_task_complete` / `mark_all_tasks_complete`), `chotu-common/src/calendar.rs`

**Done:** Snooze PATCHes start/end via `reschedule_at` (duration from `duration_minutes` or 30m); complete/complete-all delete the Google event and clear `calendar_event_id` only after successful delete (or confirmed 404); create path unchanged.

---

### 5. Morning brief privacy (tasks + calendar)
- [x] In linked DMs, filter open tasks to the recipient (or clearly labeled assignee scope)
- [x] In linked DMs, filter calendar to the recipient’s relevant events (not full `fetch_family_events`)
- [x] Leave shared bills/household sections intentional; do not regress nutrition/fitness `for_member_id` scoping

**Why:** Nutrition/training honor `for_member_id` in linked DMs; tasks and calendar still leak the household surface into private chats.

**Effort:** M

**Files:** `coordinator/src/brief.rs`, `chotu-common/src/agenda.rs` (`format_brief_calendar_section` / `compose_calendar_agenda`), brief scheduler in `coordinator/src/telegram.rs`

**Done:** Linked DM briefs/`/cal` fetch only the recipient’s calendar; brief tasks use assignee = me OR unassigned (same as complete-all). Household chats unchanged; bills stay shared.

---

### 6. Coach plan / progress wiring
- [x] Stop attaching “today’s plan” blindly into `/trends` context when the day has no plan row
- [x] Pass real exercise dates into enrich so day exercises are not always empty
- [x] Track cardio minutes and session adherence vs plan kind (depends on item 1 for clean data)

**Why:** `enrich_coach_context` always attaches today’s plan even for trends; cardio/adherence aren’t tracked beyond keyword counts.

**Effort:** S–M

**Files:** `health-coach/src/coach_enrich.rs`, `health-coach/src/trends.rs`, `health-coach/src/fitness_plan.rs`, `coordinator/src/brief.rs`, `coordinator/src/telegram.rs` (`handle_plan`)

**Done:** `CoachEnrichOpts` — trends skip today’s plan and load exercises for the trend window; status uses `for_day`. `plan_session_adherence` + `plan_cardio_minutes_on_cardio_days` feed coach tips, `/plan`, and morning brief.

---

### 7. Memory RAG owner boundary
- [ ] Scope search/index by owner / `member_id` so a linked DM cannot RAG the whole household brain
- [ ] Follow-on: avoid loading every `memory_chunks` row for in-process cosine scan (ANN / pruning)

**Why:** Index mixes journals/digests/refs/tasks with no owner boundary; any allowed chat can search the full store.

**Effort:** M (scoping S–M; ANN larger)

**Files:** `chotu-common/src/memory.rs`, `coordinator/src/telegram.rs` (`handle_memory`)

---

### 8. Research bench finish
- [ ] Commit `tool_choice: Auto` for OpenRouter structured extraction in `chotu-common/src/llm.rs` (Qwen thinking mode rejects `required`)
- [ ] Finish a clean Sol vs Qwen run with `summary.md` and a decision under the harness rule
- [ ] Tighten metrics: composite currently ignores `interest_label_accuracy`; `validate_score_report` only checks ticker coverage

**Why:** Harness exists; first live candidate arm failed on tool choice; decision rule needs a complete comparable run.

**Effort:** S–M

**Files:** `chotu-common/src/llm.rs`, `finance-advisor/src/bench.rs`, `finance-advisor/src/lib.rs`, `finance-advisor/src/bin/research_bench.rs`, `evals/research/`

---

## Honorable mentions

Not prioritized, but known thin spots:

| Area | Gap |
|------|-----|
| Nutrition | Gemini missing-nutrient fills when key absent; photo capture still ambiguous on portion/time |
| Intent router | Strong for status/tasks/food/plan/memory; thin for adjust/undo/clear food, complete-all safety, calendar vs brief ambiguity |
| Family privacy | Allowlist + link hijack guards are good; remaining leak is item 7 (memory RAG); household `TELEGRAM_CHAT_ID` still gets family-wide nutrition when unlinked |

## Suggested order

Max quality per week: **7 → 8** (items 1–6 done).

Take 7 next (memory RAG owner boundary).
