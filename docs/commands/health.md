# Health commands

Nutrition logging, Google Health sync, daily status, trends, and weekly training plans.

Linked personal DMs only see **that member’s** health/fitness. Household / unlinked chats can see family-wide reports where applicable. Linked DMs cannot mutate another member’s food (use a household chat or unlinked setup for that).

---

## `/food [member_id] <description>`

```text
/food 2 eggs and toast
/food praj yesterday's dinner pasta
```

Defaults member to the linked DM. Relative day/time phrases are resolved via local Ollama before logging; nutrients often go through Gemini when configured. Pushes to Google Health when that member is linked.

**Looks like**

```text
Got it — logging food for praj...
✅ Logged for praj (today):
• 2 eggs and toast — ~420 kcal | P 28g C 22g F 24g
```

Empty args → usage + configured member list.

### Food photos

Send a barcode, package, or plated meal (optional caption: `half the bowl` or `praj half the bowl`).

- Barcode → Open Food Facts (no key)
- Package / plate → Gemini vision (`GEMINI_API_KEY`)

Same logging path as `/food` (including Health push).

---

## `/undofood [member_id]`

Removes the last Telegram food entry (and its Google Health log if synced). Rebuilds today’s summary from remaining `food_log` rows.

---

## `/adjustfood [member_id] <cal> <P> <C> <F>`

```text
/adjustfood 2100 160 200 70
```

Overrides today’s totals. Clears Telegram meals from Google Health first so the next sync doesn’t double-count.

---

## `/clearfood [member_id]`

Wipes today’s food logs + summary for that member.

---

## `/sync`

Manual pull of today’s nutrition/activity for every linked Google Health account. Evening scheduled sync merges Telegram meals instead of overwriting. Late steps sync (~11pm ET by default) can nudge toward the step goal.

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
