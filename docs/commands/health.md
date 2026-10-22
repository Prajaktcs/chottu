# Health commands

Nutrition logging, Google Health sync, daily status, trends, weekly training plans, and gym set logging.

Linked personal DMs only see **that member’s** health/fitness. Household / unlinked chats can see family-wide reports where applicable. Linked DMs cannot mutate another member’s food (use a household chat or unlinked setup for that).

---

## `/tags` · `/watch`

`/tags` lists the closed food-tag vocabulary with examples. Food logs already
store these tags; choose which ones to track with your conditions' watchlists.

| Command | Behavior |
| :--- | :--- |
| `/watch` | Show your configured conditions and current watchlists |
| `/watch add <condition_id> <tag>` | Add a tag to your condition's watchlist |
| `/watch remove <condition_id> <tag>` | Remove a tag |

```text
/tags
/watch
/watch add skin dairy
/watch remove skin dairy
```

`/watch` works only in a linked personal DM and always targets that member.
Condition ids come from the member's `health_conditions` in private `config.yaml`;
`/watch` shows valid ids. Unknown conditions or tags return usage without changing
data. Repeated adds/removes are safe. Watchlists are stored locally in SQLite and
survive restarts; editing them requires no model call or restart.

After a successful food log in your linked DM, matching tags add a heads-up:

```text
⚠️ On your Skin symptoms watchlist: dairy
```

Each tag is flagged once per member per logged day, even across restarts or food
undo/clear. Household food confirmations never show condition flags. The meal is
always saved; flags do not change its nutrients or block logging.

Private `/status` and `/trends` coach tips can reference your own watchlists,
reported scores from the last seven calendar days, and that day's food-tag matches.
Skipped scores remain unknown. Household coaching receives no condition details.
The coach is instructed not to invent triggers, claim food caused symptoms, or
recommend treatment. The deterministic condition section below is separate from
these coach tips.

---

## `/food [member_id] <description>`

```text
/food 2 eggs and toast
/food praj yesterday's dinner pasta
```

Defaults member to the linked DM. Relative day/time phrases are resolved via local Ollama before logging. Nutrition estimates use Gemini, falling back to OpenRouter Qwen (`OPENROUTER_API_KEY`) if Gemini fails. Without the OpenRouter key, a failed Gemini estimate reports that the fallback is unavailable. Pushes to Google Health when that member is linked.

**Looks like**

```text
Got it — logging food for praj...
✅ Logged for praj (today):
• 2 eggs and toast — ~420 kcal | P 28g C 22g F 24g
```

Empty args → usage + configured member list.

### Food photos

Send a barcode, package, or plated meal (optional caption: `half the bowl` or `praj half the bowl`).

- Barcode → Open Food Facts (no key) after vision reads the barcode
- Package / plate → Gemini vision (`GEMINI_API_KEY`), falling back to OpenRouter Qwen vision (`OPENROUTER_API_KEY`) on failure. The original image and caption are sent to the fallback; no caption is required.

Same logging path as `/food` (including Health push).

**Steering ingredients:** put facts in the same photo's caption, for example:

```text
/food praj Paneer bhurji with cream, not scrambled eggs.
No eggs anywhere in this meal. I ate half the bowl.
Include the bread and vegetables shown.
```

Explicit ingredient identities, exclusions, quantities and consumed portions take precedence over visual guesses. Exclusions also constrain food tags; saying the bhurji is not eggs does not remove eggs from a separate omelette.

Coordinated exclusions such as `without eggs or milk` apply to both named ingredients; a separately included side remains included. Standalone `no dairy or eggs` excludes both across the meal, and a trailing `anywhere in this meal` qualifier applies to the entire ingredient list. These exclusions survive portion-only corrections. Paneer itself still counts as dairy when only added milk is excluded.

A separate photo following your recent meal logs asks whether to update a meal or log a new one. Nothing from that photo is saved until you answer `update`, `new`, or `cancel`. With multiple candidates, use `update <meal id>` from the list. Choices belong to the sender and conversation, expire after 15 minutes, and reject superseded questions or meals changed since the question. A new photo replaces your previous unanswered photo choice.

Replying to a meal confirmation with a photo explicitly updates that meal. A `/food ...` photo caption explicitly starts a new meal, even when sent as a reply. Send one image at a time.

A barcode cannot replace an existing whole meal: Chotu asks for the product, amount eaten, and component to replace, leaving the meal unchanged. Reply with those facts in text and explicitly retain any sides, or use `/food` to log a separate meal.

If a photo update returns no meal description, Chotu asks you to retry with a clearer photo or the ingredients and amount eaten. The existing description, nutrition, user facts, revision, and daily totals remain unchanged; a partial caption never replaces the whole meal.

### Correcting a logged meal

Reply to its confirmation with a correction such as `It's paneer, not eggs; keep the cream and half-bowl portion.` Confirmations show an eight-character meal ID.

```text
/correctfood <meal id> The bhurji is paneer, not eggs. No eggs in this meal.
```

An unquoted correction can select a meal only when there is exactly one candidate you logged in this conversation within the last 20 minutes; otherwise Chotu asks which meal. This window uses interaction time, not the meal's consumption date. While evening reflection is open, use a reply or `/correctfood` so ordinary reflection text is not interpreted as a correction. Historical meals without new confirmation mappings can be targeted by ID in your linked DM; group corrections require recorded sender provenance.

A quoted correction whose confirmation cannot be mapped never falls back to the most recent meal. Reply to a current confirmation or supply an explicit meal ID with `/correctfood`; an unmapped old quote cannot silently revise a newer lunch.

Corrections replace nutrition and tags on the existing entry, preserving its member and consumption time, unaffected ingredients and portions, external nutrition, and activity. The original day's totals are rebuilt; no additional meal is created. Ambiguous model results ask for clarification without changing the entry. Analysis remains sequential: a correction sent during an estimate is processed after that estimate finishes, not acknowledged as an immediate interruption.

Previously supplied exclusions remain active during portion-only corrections, even if the new model description guesses an excluded ingredient. A later explicit addition can replace the corresponding exclusion without reviving other old ingredient guesses.

Initial meal facts and revised facts commit in the same transaction as nutrition, tags, and the day's summary. If facts or their sender/conversation provenance cannot be saved, the entire meal write rolls back; a later correction cannot read half-committed history.

Google Health's anonymous nutrition logs cannot be edited, so a synced correction deletes the old remote log and creates its replacement. Local success is reported separately from remote success. Interrupted replacements keep durable, revision-specific retry state; `/sync` retries outstanding corrections, including meals on earlier dates. While a replacement is unresolved, sync refuses to overwrite corrected local nutrition with a stale remote rollup.

Initial uploads use the same durable, named retry path (revision zero), so a scheduled upload already in flight cannot overwrite a newer correction's remote reference. Updated confirmations retain the existing private condition-watchlist behavior for newly applicable tags.

Google may assign a numeric DataPoint ID even when Chotu requests a revision-specific name. A successful completed create uses the returned nutrition-log name as the authoritative identity. Chotu replaces the provisional resource in its cleanup ledger and updates any retained correction/deletion references before retrying cleanup, so later sync and deletion use the actual Google resource.

**Previously rejected successful creates:** a `done: true` response containing a valid nutrition-log name means Google already created that meal, even if an older Chotu version reported “no matching nutrition-log DataPoint name.” Recover the exact returned name from that completed response before retrying the pending upload; another create can duplicate the meal when Google does not preserve the requested ID. Do not infer identity from similar foods or timestamps. The [DataPoint contract](https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints#DataPoint) permits client-provided or system-generated IDs; requesting a name alone does not establish idempotency when the server replaces it.

Undo, clear, and adjust record deletion intent before remote cleanup. In-flight uploads settle into a durable resource ledger; every possibly created resource must be resolved and removed before local meals are deleted. Failed cleanup retains the selected entries and their retry state. `/sync` resumes outstanding deletions on their original days, including after restart or midnight, and does not overwrite nutrition while cleanup is unresolved.

**Legacy Operation references:** older code could store a Google `Operation` name instead of a completed nutrition-log `DataPoint` name. The published [Health API discovery](https://health.googleapis.com/$discovery/rest?version=v4) exposes no Operation-read method, and [DataPoint listing](https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints/list) has no authoritative Operation correlation. `/sync` cannot recover identity by itself. Chotu does not guess endpoints, match similar foods/timestamps, or create a replacement while the identity is unknown.

Recovery accepts only the **original completed create response from a trusted record**, such as a captured Google Health HTTP response. The [Operation contract](https://developers.google.com/health/reference/rest/Shared.Types/Operation) supplies the exact Operation `name`, `done: true`, and a successful `response` containing the nutrition-log DataPoint and its name. The [create contract](https://developers.google.com/health/reference/rest/v4/users.dataTypes.dataPoints/create) establishes that the returned Operation belongs to that create. A handwritten Operation/DataPoint pair, a similar listed meal, or an arbitrary JSON file is not authoritative evidence. The tool validates structure and exact stored identity; it cannot authenticate a file's provenance. If no trusted completed response exists, recovery remains blocked and local meals, totals, and cleanup state stay retained.

After running the upgraded application once to apply migrations, stop the supervisor and all other database writers. Import one response for one explicitly selected member and meal:

```sh
cargo run -p health-coach --bin recover-health-operation -- \
  --apply --database /path/to/chotu.db \
  --backup /path/to/chotu-before-recovery.db \
  --member alex --meal MEAL_ID --response /path/to/original-completed-operation.json
```

On Unix, the backup destination is reserved atomically with owner-only permissions (`0600`) before SQLite writes any household data. A permissive umask cannot make the backup group- or world-readable.

The backup path must not already exist. The offline tool opens an existing database, makes a consistent SQLite backup, then commits the original response, mapping, resource ledger, and matching meal/correction/deletion-intent references in one transaction. It performs no HTTP requests, does not authorize deletion, and does not change meal revisions, nutrition, consumption times, or cleanup intent. Restart the supervisor and run `/sync`; existing authorized deletions and corrections resume on their original days. Reimporting the same mapping is safe after restart or completed cleanup; conflicting mappings are refused. Unknown, unfinished, failed, malformed, or wrong-meal responses do not change meal state. To roll back before resuming sync, keep all writers stopped and restore the standalone backup to a clean database path; a database rollback cannot undo remote cleanup already performed.

Remote nutrition mutations are serialized within one supervisor process. Run only one writer process against the database; stop the old binary before starting an upgraded one.

---

## `/undofood [member_id]`

Removes today's latest chat food entry by consumption timestamp (and its Google Health log if synced). Rebuilds today's summary from remaining `food_log` rows while preserving external nutrition and activity. If deletion cannot finish, the local entry and durable cleanup state are retained. Retrying `/undofood` resumes today's pending deletion before considering a newer meal; `/sync` can resume it on a later day.

---

## `/adjustfood [member_id] <cal> <P> <C> <F>`

```text
/adjustfood 2100 160 200 70
```

Overrides today’s macros after removing the explicitly selected chat meals and their Google Health logs. A meal arriving after selection is not deleted. The local adjustment is a signed audit delta against the current external-plus-remaining-meal base, so undo restores that base; micronutrients and activity are preserved.

Sync reads local meal and adjustment audits inside its guarded summary-write transaction. An adjustment committed while remote metrics are being fetched is included before the sync writes totals.

If remote deletion cannot be confirmed, the adjustment is not applied locally. If deletion succeeds but the adjustment cannot be saved, Chotu reports that separate failure and asks you to check `/status` before retrying.

---

## `/clearfood [member_id]`

Removes today's chat food logs selected when the command starts. Rebuilds the summary while preserving external nutrition, activity, and meals arriving after selection.

If deletion cannot finish, selected local meals are retained with durable retry state. Retry `/clearfood`, or use `/sync` to resume outstanding deletions after the day changes.

---

## `/sync`

Manual pull of today’s nutrition/activity for every linked Google Health account. Evening scheduled sync merges meals logged through chat instead of overwriting. Late steps sync (~11pm ET by default) can nudge toward the step goal.

**Looks like**

```text
🔄 Syncing Google Health for linked members...
✅ praj: 1840 kcal | 9200 steps | …
```

Works once the Signal client is running (`just run` requires `GEMINI_API_KEY`). The Health Coach sync path itself does not need Gemini for the pull/merge; OAuth Health tokens are what matter for `/sync`.

### Backfilling past days

`/sync` only rewrites today. To rebuild earlier daily summaries (for example after a nutrition sync fix), run from the repo root with the same `.env`:

```sh
just backfill-health                                  # dry run: every logged day up to yesterday
just backfill-health --from 2026-10-01 --member praj  # narrow the range
just backfill-health --apply                          # rewrite the listed days
```

The dry run opens the database read-only (no migrations) and prints stored vs recomputed calories, saturated fat and steps for every day `--apply` would write or skip. `--apply` writes exactly the snapshot it fetched: nutrition, activity, sleep and exercise. It never pushes anything to Google. Any failed Google request fails the day, which is retried instead of being written as zeros. These days are skipped and listed for manual review:

- days with unsynced local meals, which may already be in Google, so adding them would double count
- days where Google now reports fewer calories than stored (rechecked inside the write transaction)

If corrections or deletions are pending, the run stops; run `/sync` first. Run the upgraded app once so migrations are applied, and back up the database first (`sqlite3 chotu.db ".backup chotu-before-backfill.db"`). The run is safe to repeat.

---

## `/status`

Two-part reply:

1. **Financial ledger** for today (spend total + merchants)
2. **Per-member health** (activity, sleep/energy if present, exercises, fitness outcome progress, macros vs goals) ending with a short local-Ollama coach tip

Linked DM → only your health block. Household → all members with data.

---

## `/trends [days]`

Default `7`. Multi-day nutrition/activity plus a short coach tip per member with data.

```text
/trends
/trends 14
```

Plain text also works: `trends last 14 days`.

In a linked personal DM, conditions with at least seven check-ins in the requested
window also show a score timeline (`.` means skipped), your watchlist tags logged
in each score's configured lag window, and average recorded sleep on scored days.
With fewer check-ins, the latest score and count are shown instead. This works
even without nutrition summaries. Match dates refer to **score days**, not meal
days; lag windows use local calendar days, including daylight-saving changes.

For tentative comparisons, use a longer window such as `/trends 30`. Each tag
needs at least ten scored days with a match and ten without a logged match,
paired by recorded sleep within half an hour. Controls are never reused. Both
groups require food logs on every day in the lag window; missing scores, sleep,
or food-log days are excluded. A missing tag means **no matching tag logged**,
not proof you avoided that food. These are descriptive comparisons, not causal
findings or treatment suggestions.

Sunday's private morning brief includes a compact condition summary for the seven
completed days through Saturday, using the existing morning-brief schedule and
delivery retries. It reports check-in coverage, average scores, lag-window match
counts, and recorded sleep. Household briefs and reports never show conditions.

---

## `/plan` · `/plan new`

Requires non-empty `fitness_goals` for the (linked) member in `config.yaml`.

| Command | Behavior |
| :--- | :--- |
| `/plan` | Show stored plan for current week; generate via Ollama if missing |
| `/plan new` | Regenerate (`regen` / `refresh` / … also accepted) |

**Looks like**

```text
🏋️ Building this week's training plan (local Ollama)…
🏋️ Training plan (week of 2026-10-19)
• Monday: strength — upper push, RPE 7
    ◦ Bench press — 3×8-10
    ◦ Seated row — 3×10-12
    ◦ Plank — 3×45s
• Tuesday: cardio — easy aerobic
    ◦ Bike — 30 min easy
• Wednesday: rest
…
📌 Today: strength — upper push, RPE 7
    ◦ Bench press — 3×8-10
    …
Week progress: 2/4 sessions …
```

Each strength/mixed day lists 4–7 exercises with sets × reps; cardio days list
duration; rest days list none. Muscle groups rotate across the week.

The plan uses data already synced from Google Health: the previous 7 days'
workouts and step/active-calorie/sleep averages, plus how many of last week's
planned sessions were completed. Low adherence shrinks the week; full adherence
progresses it slightly; average sleep under 6h keeps intensity moderate. Google
Health only reports workout type and duration, so loads come from `/log`
instead (below). A day with logged sets also counts as a completed strength
session.

Plans follow a training phase based on how many earlier weeks have logged sets:

| Weeks logged | Phase | Programming |
| :--- | :--- | :--- |
| 0–3 | Foundation | Full-body days, machines + basic compounds, same lifts every week, 2–3 × 8–12 |
| 4–11 | Build | Keep the main lifts, add free-weight variations and 1–2 accessories |
| 12+ | Expand | Upper/lower split, barbell main lifts at 5–8 reps, then accessories |

`equipment: gym` (or `mixed`) is described to the model as a full GoodLife gym.
The prompt also carries each lift's latest logged session, and the model is told
to reuse those exact names so progression carries across weeks. The model never
prescribes loads. The 📌 Today block shows targets from your log, for example
`Leg press — 3×8-12 @ 190 lb, aim 8/8/8 (+10 lb)`.

Without goals:

```text
⚠️ No fitness_goals for praj in config.yaml yet.
Add intent / target_date / sessions_per_week, then try /plan again.
```

---

## `/log` — gym workout

Linked personal DM only (household chats are told to use the DM). Weights are in
lb. No model call is involved.

`/log` starts today's workout, or resumes it if one is open, and lists today's
planned lifts with targets:

```text
/log
▶️ Workout started — Monday strength: full body
 1. Leg press — 3×8-12 @ 180 lb, aim 12/12/11
 2. Chest press — 3×8-12 · first time: pick a weight you can lift 12 times at RPE 7
 3. Plank — 3×45s

Now: Leg press
Send `180x12` per set · `x11` same weight · `next` or `#3` to switch · `undo` · `done`
```

While the workout is open, plain messages in the DM log sets:

| Message | Effect |
| :--- | :--- |
| `180x12` | One set: 180 lb × 12 to the current lift (`180 x 12`, `180×12` also work) |
| `180x10x3` | Three sets of 10 at 180 lb |
| `x11` or `11` | Same weight as the previous set of this lift |
| `bwx12` | Bodyweight set |
| `180x12 x11 x10` | Several sets in one message |
| `bench 135x8` | Log to a named lift: a planned one (`bench` matches "Bench press") or an extra |
| `next` · `#3` | Move to the next / third planned lift |
| `undo` | Remove the last logged set |
| `done` | Finish and show next-time targets |

After a lift's planned sets are logged, the next unfinished lift is shown. `/log
<message>` works the same way and starts a workout if needed (e.g. catching up
with `/log bench 135x8x3`).

```text
x10
✓ Leg press set 3: 180 lb × 10
→ Next: 2. Chest press — 3×8-12 · first time: pick a weight you can lift 12 times at RPE 7
…
done
🏁 Workout logged: 9 sets across 3 exercises.
• Leg press: 180 lb × 12/11/10 → next 180 lb, aim 12/12/11
• Chest press: 90 lb × 12/12/12 → next 95 lb, aim 8/8/8
```

Anything else (food text, questions, photos, quoted replies, and evening
reflection answers) goes to the normal handlers. Commands like `/food` work
mid-workout. A workout closes on `done`, after 3 hours without workout messages, or
when the local day changes. Logged sets are kept either way.

**Progression** (double progression, per lift, using the planned rep range):

- Every working set at the top of the range adds load and restarts at the bottom
  of the range: +10 lb for squats, deadlifts, leg press, hip thrusts and RDLs;
  +5 lb for everything else.
- Otherwise the load holds and the aim is one more rep per set.
- Two sessions in a row with no set reaching the bottom of the range at the same
  load cause a deload to 90% (rounded to 5 lb).
- Bodyweight lifts add reps; at the top of the range, Chotu suggests a harder variation.
- Timed or distance work (`45s`, `30 min`) gets no target.

Lifts are matched across weeks by name, ignoring case and punctuation
("Pull-ups" = "pullups").
