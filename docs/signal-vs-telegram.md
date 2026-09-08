# Signal vs Telegram: what you keep / miss

Chotu’s chat transport is now **Signal via `signal-cli`** (Unix-socket JSON-RPC), not Telegram/Teloxide. Commands and household logic mostly carried over; the gaps below are the ones that matter day to day.

## You keep

- Slash commands and free-text intents (`/food`, `/status`, `/tasks`, `/brief`, `/memory`, `/budget`, …)
- Operator-configured personal DMs (`family.members[].signal_aci`) + optional household group (`SIGNAL_GROUP_ID`)
- Food photos (barcode / package / plate → Gemini + Open Food Facts)
- Task create / list / complete / snooze / reassign (as **typed commands**)
- Due reminders, email-inferred tasks, morning brief / evening reflection fan-out
- Per-member privacy rules (linked DM scoped to self; group stays household-wide)

## You miss (or change)

### 1. Tap-to-act task buttons

Telegram had **inline keyboards + callback queries** on reminders/task lists (“Complete” / “Snooze” buttons that edited the message).

On Signal those are gone. Reminders print plain-text instructions instead:

```text
/tasks complete <id>
/tasks snooze <id> [days]
```

### 2. Real Markdown rendering

Telegram `ParseMode::Markdown` made `*bold*` / `_italic_` render. Signal `send_text` is plain text, so leftover `*` / `_` show literally until formatters are cleaned.

Health sync/nudge copy and the budget/food mutation replies called out in PR review are plain text. Broader status/trends/plan Markdown cleanup may still remain.

### 3. In-place message edits

Telegram could clear/update keyboard markup and edit reminder text after a tap. Signal has **no edit-message UX** in this client — replies are new messages only.

### 4. Bot-platform conveniences

Gone with Teloxide / Bot API:

- Bot usernames / `/command@bot` routing quirks (mostly irrelevant on Signal)
- Telegram chat ids → replaced by **Signal ACI** (`signal_aci`) and optional group id
- Teloxide dependency / bot tokens (`TELEGRAM_*` env vars)

### 5. Ops surface is heavier

Telegram was “create bot → paste token.” Signal needs:

- `signal-cli` linked as a secondary device
- Long-running daemon with `--receive-mode=manual --socket …`
- `SIGNAL_CLI_SOCKET` (and optional `SIGNAL_GROUP_ID`)
- Each allowed member ACI configured in `config.yaml` before startup; changes require restart
- Exact group-id authorization: a linked sender does not authorize any other group

### 6. Reminder correlation model

Telegram stored `tasks.telegram_message_id` for reply correlation. Signal uses `task_signal_messages` keyed by **recipient + outbound timestamp** (and inbound quote timestamp when present). Email “unactionable” replies still work; the wiring is different under the hood.

## Non-goals that did **not** change with this switch

These were already out of scope on Telegram and stay out on Signal:

- Outbound email / SMTP / drafting replies
- Executive shell actions or arbitrary file deletion outside designated storage

## Practical takeaway

The biggest daily miss is **tap buttons on tasks** (type the command instead). Next is **ugly literal Markdown** anywhere formatters were not rewritten for plain text. Feature coverage otherwise matches the old bot surface.
