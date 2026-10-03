# Health commands

Nutrition logging, Google Health sync, daily status, trends, and weekly training plans.

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

---

## `/undofood [member_id]`

Removes the last food entry logged through chat (and its Google Health log if synced). Rebuilds today’s summary from remaining `food_log` rows.

---

## `/adjustfood [member_id] <cal> <P> <C> <F>`

```text
/adjustfood 2100 160 200 70
```

Overrides today’s totals. Clears meals logged through chat from Google Health first so the next sync doesn’t double-count.

---

## `/clearfood [member_id]`

Wipes today’s food logs + summary for that member.

---

## `/sync`

Manual pull of today’s nutrition/activity for every linked Google Health account. Evening scheduled sync merges meals logged through chat instead of overwriting. Late steps sync (~11pm ET by default) can nudge toward the step goal.

**Looks like**

```text
🔄 Syncing Google Health for linked members...
✅ praj: 1840 kcal | 9200 steps | …
```

Works once the Signal client is running (`just run` requires `GEMINI_API_KEY`). The Health Coach sync path itself does not need Gemini for the pull/merge; OAuth Health tokens are what matter for `/sync`.

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
<markdown week plan>
📌 Today: strength — upper body
Week progress: 2/4 sessions …
```

Without goals:

```text
⚠️ No fitness_goals for praj in config.yaml yet.
Add intent / target_date / sessions_per_week, then try /plan again.
```
