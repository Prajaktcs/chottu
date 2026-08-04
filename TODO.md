# Project Chotu To-Do List

## Nutrition & Health Coach
- [x] For the agent to send status reports or trends on how the nutrition is going
- [x] Investigate if there is anything that we need to adjust in the nutrition phase
- [x] Improve formatting of the `/status` command response (currently contains a very long text list of macros/micros/minerals)
- [x] Send status updates/reports as a different message for each user (rather than a single giant combined message)
- [x] Fix Google Health nutrient unit conversion (grams → mg/mcg for micros)
- [x] Merge Telegram `/food` into evening `/sync` instead of overwriting
- [x] Allow scheduled Google Health sync when GEMINI_API_KEY is missing
- [x] Fix `/adjustfood` audit row + `/undofood` inconsistency (rebuild summary from food_log)
- [x] Slim noisy `/food` / `/undofood` replies (macros-first, like `/status`)
- [x] Optional: per-member nutrition goals in config + progress on `/status`/`/trends`
- [x] Two-way food sync: push Telegram `/food` to Google Health; `/sync` treats Google as shared store

## Tasks & Calendar
- [x] Telegram `/tasks` to list open tasks and mark them complete
- [x] Optional: snooze / reassign tasks from Telegram

## Personal Agent

### Short path (build in order)
- [x] Free-text intent router over existing tools (status, tasks, food, sync, trends, networth, monthly)
- [x] Multimodal food capture (Telegram photo of barcode / package / plate → Open Food Facts + Gemini → food log)
- [x] Morning brief (proactive daily digest: calendar, open tasks, bills due, nutrition vs goals)
- [x] User-created tasks and time-based reminders (not only email-derived)
- [x] Queryable memory over journals / digests / personal references / tasks (local RAG)

### Later domains
- [x] Calendar read/query UX (“what's today?”, conflicts)
- [x] Proactive coaching on `/status`/`/trends` (advice, not just numbers)
- [x] Spend alerts / category budgets
- [ ] Family multi-user Telegram UX
- [ ] Meal planning ↔ grocery lists

### Non-goals
- Outbound email / SMTP / creating drafts or replies
- Executive shell actions or arbitrary file deletion outside designated storage
