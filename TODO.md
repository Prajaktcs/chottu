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
- [ ] Optional: per-member nutrition goals in config + progress on `/status`/`/trends`
