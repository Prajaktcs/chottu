# Day loop commands

Calendar, tasks, morning brief, and evening reflection — the daily rhythm around the bot.

---

## `/brief`

Manual morning digest (also scheduled ~07:00 local; override with `MORNING_BRIEF_HOUR`). Fans out to linked adult DMs; `TELEGRAM_CHAT_ID` is an optional shared fallback.

**Looks like**

```
☀️ Building morning brief...
*Morning brief — 2026-08-20*

📅 Calendar …
✅ Open tasks …
💳 Bills due …
🥗 Yesterday nutrition vs goals …
🏋️ Training — N days to target; today’s session …
```

Linked DMs get a **private** slice (your calendar/tasks/nutrition/training). Household chat stays family-wide.

Plain text: `morning brief`, `how's today`.

---

## `/cal [today|tomorrow|week]`

Family-merged agenda from linked Google Calendars. Default `today`. Flags overlapping timed events across members.

```
/cal
/cal tomorrow
/cal week
```

Plain text: `what's today`, `tomorrow's schedule`, `this week`.

Needs `/login calendar <member>` + `calendar:` in config for each adult you care about.

---

## `/tasks …` · `/task …`

### List

```
/tasks
/tasks open
/tasks all
/tasks completed
/tasks snoozed
/tasks open praj
```

Open/snoozed lists include inline **✅ Done** / **😴 +1d** buttons (no id typing required).

### Add

```
/task call dentist tomorrow 3pm
/tasks add praj buy milk due Friday
/tasks add remind me to submit timesheet by tomorrow
```

Defaults assignee to the linked member. Dated tasks can land on that member’s Google Calendar when linked.

Plain text: `remind me to …`, `open tasks`.

### Mutate

| Command | Notes |
| :--- | :--- |
| `/tasks complete <id>` | Prefix ≥4 chars of id |
| `/tasks complete all` | Linked DM: yours + unassigned |
| `/tasks complete all confirm` | Required in household chat |
| `/tasks snooze <id> [days]` | Default 1 day (1–90); moves linked calendar events |
| `/tasks reassign <id> <member>` | |
| `/tasks open <id>` | Unsnooze |

Timed `due_at` triggers one Telegram reminder to the assignee’s DM (else household targets).

**Email feedback:** reply `unactionable` to a task reminder to suppress similar mail next time.

---

## `/reflect`

Starts the evening journaling loop (also schedulable when delivery targets exist).

**Looks like**

```
Querying daily metrics and generating evening reflection prompt via local Ollama...

📝 Evening Journaling Reflection Prompt:

_<prompt grounded in today's health + spend, steered by config core_values>_

Reply directly to this message to record your daily reflection entry in your journal.
```

Your reply is saved under `~/chotu_brain/Journal/` (or `CHOTU_BRAIN_DIR`). Needs Ollama; uses today’s ledger + health rows from SQLite.

`core_values` in `config.yaml` (optional) shape the prompt toward your anchors (default Growth + Contribution when unset in code paths that supply defaults — keep real values only in private config).
