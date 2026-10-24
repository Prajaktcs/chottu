# Chotu App — Technical Design

**Date:** 2026-10-24 · **Author:** Prajakt Shastry · **Live draft:** [Chotu App — Technical Design](https://claude.ai/artifact/6VoSPpxJ9QmDUcZC7e8nCz)

We will build one Expo (React Native) app, shipped first as a sideloaded APK on Android, with the code kept web-ready so iPhones can get a web build later, that talks to the coordinator on the Mac mini over a private HTTPS API, reachable only through Tailscale and served over TLS on every hop. Signal stays as a full interface for the whole build and afterwards. Both clients call the same shared Rust service functions, so a feature behaves the same whichever one you use. The first feature is gym set logging, then tasks and daily status.

## Problem statement

Signal is a good interface for conversation, but a poor one for structured, repeated input. Moving from Telegram to Signal removed tap-to-act buttons, in-place message edits and Markdown rendering (`docs/signal-vs-telegram.md`).

The cost shows up most in the new gym set logging (`/log`). Between sets, you type `135x8`, then `done`, on a phone keyboard with sweaty hands. Tasks need typed ids (`/tasks complete <id>`). `/plan`, `/status` and `/trends` arrive as plain-text walls with stray `*` and `_` characters.

Now is the time because set logging just landed and has one consumer. Separating its logic from Signal now costs much less than doing it after more features copy the same pattern.

## Goals

- Log a gym set in 2 taps or fewer, with the next target load pre-filled from `next_target`.
- Build a similar app like fitbot or hevy, where i can see videos right besides suggested workouts.
- Complete or snooze a task with one tap, no typed ids.
- Show `/status`, `/plan` and `/tasks` as app screens instead of plain-text replies.
- Keep Signal fully working throughout. A feature added to the app works the same in Signal, and the reverse.
- Keep the per-member privacy rules exactly as they are for linked Signal DMs: a phone sees and changes only its own member's data. For this we will keep it simple for now as this is meant for household, we will just have a profile selection on who is using the app between the configured people.

## Non-goals

- **Replacing Signal.** Signal stays for conversation, free text, food photos, household group fan-out and push notifications.
- **Public internet exposure.** The API is never reachable outside the Tailscale network.
- **Firebase Cloud Messaging or any Google-routed push.** That would send household metadata through a third party.
- **A cloud copy of data.** The phone keeps no durable copy; the SQLite database on the mini stays the source of truth. The one exception: sets logged offline wait on the phone until they are delivered.
- **Household-group features in the app.** The app is a personal surface, the same as a linked DM. Group-wide views stay in Signal for now.
- **App Store, Play Store or web distribution for now.** Android installs the APK by sideloading or `adb install` on family phones. No web build is published yet, but the code stays ready for one.

## Background

Command logic is tied to Signal today, so a second client has nothing to call. The relevant facts:

- **One process.** `just run` starts the coordinator, which spawns the streamer, janitor, health coach and Signal loop as tokio tasks sharing a `SqlitePool` and `Arc<AppConfig>` (`ARCHITECTURE.md`).
- **Signal-typed handlers.** `coordinator/src/signal.rs` is about 11k lines. Its handlers take `type Bot = SignalClient` and `type ChatId = SignalRecipient` and send replies themselves.
- **Text-returning libraries.** Even library code returns formatted Signal text. `health_coach::log_command_reply` and `workout_message_reply` return a `String` reply, not structured data.
- **Identity is a Signal ACI.** `caller_scope` maps the sender ACI to `CallerScope::LinkedDm { member_id }` or `HouseholdGroup`. `allows_task_assignee` and `oauth_member_target` enforce self-only rules from that scope.
- **Shared state lives in SQLite.** `fitness_workout_sessions` has a partial unique index allowing one open workout per member. That makes a workout naturally shared: start it in Signal, keep logging in the app.
- **No HTTP server yet.** The only listener is the one-shot OAuth redirect in `chotu-common/src/oauth.rs`.

## Proposed solution

Signal and the app become two thin front ends over one service layer. Each feature's logic lives once, returns typed data, and each transport only formats it.

1. **Service layer (Rust, existing crates).** For each feature the app needs, split today's text-returning function into a typed core plus a formatter. For example, `log_command_reply` becomes `log_set(pool, member_id, now, input) -> Result<WorkoutState, _>`, and the Signal path formats `WorkoutState` into the same reply text as today. The core lives where the feature already lives (`health-coach` for workouts, `chotu-common` for tasks).
2. **Caller identity.** Move `CallerScope` out of `signal.rs` into a shared coordinator module. Signal resolves a sender ACI to a scope as today. The API resolves a device token to `CallerScope::LinkedDm { member_id }`, and never to `HouseholdGroup`. The existing rules such as `allows_task_assignee` then apply to both transports unchanged.
3. **HTTP API (`coordinator/src/api.rs`).** An axum server spawned by the coordinator next to the Signal loop, sharing the same `SqlitePool` and `Arc<AppConfig>`. It is off unless `api.bind` is set in `config.yaml`, so existing installs behave exactly as before.
4. **App (`app/`).** One Expo codebase in TypeScript that calls the API and holds no durable data. It ships as an Android APK now. CI also exports a static web build, which is not served anywhere until we decide to publish it.

```text
 Signal DM / group              Android app (APK, later web)
        │                                 │  HTTPS over Tailscale
        ▼                                 ▼
 signal.rs: sender ACI → scope    api.rs: device token → scope
        └──────────────┬──────────────────┘
                       ▼
        CallerScope (LinkedDm { member_id } / HouseholdGroup)
                       ▼
   typed service layer (health-coach, chotu-common) → SQLite
```

Both clients enter the same coordinator process and pass the same scope check before any feature code runs; only the outer layer differs.

The first endpoints, in the order they ship:

| Method | Path | Backed by | Phase |
| --- | --- | --- | --- |
| GET | `/v1/me` | member lookup from token | 1 |
| GET | `/v1/workout` | open session, current exercise, today's sets, next target | 1 |
| POST | `/v1/workout/sets` | `log_set` (weight\_lb, reps, exercise) | 1 |
| POST | `/v1/workout/done` | close session | 1 |
| DELETE | `/v1/workout/sets/{id}` | undo a mistyped set | 1 |
| GET | `/v1/tasks` | open + snoozed tasks in scope | 2 |
| POST | `/v1/tasks/{id}/complete` | `complete_task_by_id` + calendar sync | 2 |
| POST | `/v1/tasks/{id}/snooze` | `snooze_task_by_id` + calendar sync | 2 |
| GET | `/v1/status` | today's nutrition, steps, exercise | 3 |
| GET | `/v1/plan` | current weekly fitness plan + progress | 3 |

## Key design decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Network boundary | Expose only on the mini's Tailscale address through tailscale serve, which terminates HTTPS with auto-renewed certs. The API itself binds to loopback only (`api.bind`, e.g. `127.0.0.1:8787`); startup refuses any other bind, such as `0.0.0.0` | WireGuard authenticates the device and keeps the port off the LAN and internet; TLS on top, with a Tailscale-issued cert for the mini's ts.net name (auto-renewed), encrypts every request end to end. Access from outside Tailscale is not planned |
| App auth | One random 256-bit bearer token per device, stored as a hash in `.env` (`API_TOKEN_<DEVICE>`), listing the members that phone may act as. The app's profile picker names one per request, and the server rejects any member not on the list, so a token is not tied to a single `member_id` | Mirrors how Signal ACIs are configured: set by the operator before startup, restart to change, no open sign-up. One token per device means a lost phone is revoked alone |
| Scope | Every token resolves to `LinkedDm { member_id }`; household-group views are not exposed | Reuses the existing self-only rules instead of inventing new ones |
| Shared logic | Typed service functions; Signal and the API are formatters | One behaviour per feature; tests sit on the typed core |
| API shape | JSON over REST, versioned under `/v1`, `serde` types in the coordinator | A handful of endpoints for one known client; OpenAPI and code generation would cost more than they save |
| Writes | Every POST and DELETE carries a client-generated `request_id` in the `Idempotency-Key` header; a repeat from the same member gets the stored response back (`docs/app-api.md`) | A retry over a flaky gym connection must not log a set twice |
| Push | None from the app. Signal keeps due reminders, the morning brief and evening reflection | Avoids Google-routed push. A self-hosted relay such as ntfy can come later if needed |
| App stack | Expo (React Native, TypeScript), Expo Router, fetch + TanStack Query; token in expo-secure-store on Android, browser storage on web | One codebase for Android and web, and native iOS later through EAS if ever needed. Only libraries with web support, so the web build never lags |
| iOS delivery | Not yet. When needed: the Expo web build, served by the coordinator over Tailscale HTTPS and added to the iPhone home screen | No $99/yr Apple account or 90-day TestFlight rebuilds. The same code can become a native iOS build later without a rewrite |
| Repo layout | `app/` in this repo, npm build separate from Cargo; the web export runs in CI only, ready for the coordinator to serve later | API and client changes land in one PR and are reviewed together |
| Offline logging | Sets logged without a connection queue on the phone with their request\_id and timestamp, show as pending, and replay in order when back online | Gyms often have poor reception; request\_id dedupe makes replays safe |
| Set timestamps | The app sends performed\_at (RFC 3339 with offset). The server stores it, derives the day in the household timezone, and rejects times more than 5 minutes ahead or 7 days old. Signal keeps using server time | A set lands when it happened, even when sent later; Signal and the app agree on "today" |
| Versioning | /v1/health reports the server version and min\_app\_version; an older app gets 426 and an update screen. Changes within /v1 are additive only | Sideloaded APKs lag the server; a clear prompt beats a broken screen |
| App signing and updates | One release keystore kept outside git and backed up like .env; updates ship by reinstalling a newer APK | A lost key forces every phone to uninstall first; EAS Update would route through Expo's servers |
| API contract | Rust tests write example JSON for every request and response; app tests check them against the TypeScript types | Hand-written types can't drift silently from the serde structs |
| Weight units | Per-member weight\_unit (lb or kg) in config.yaml; stored in pounds, converted at the edges in Signal, the API and the app | History and progression maths stay unchanged |
| Lint and format | oxlint (react, typescript, import, jsx-a11y plugins, type-aware, warnings fail CI), oxfmt, tsc --noEmit, jest-expo; hooks and CI run app checks only when app/ changes | Fast enough for pre-commit and covers the React hooks rules; oxfmt matches Prettier, so switching back is a config change |
| Expo conventions | Pinned SDK; native deps via npx expo install; no committed android/ or ios/ folders; strict TypeScript, typed routes, React Compiler; expo-doctor in CI; FlashList for long lists | Keeps upgrades and native config predictable |
| Exercise videos | Short muted loops (10 s or less, 720p or less, about 2 MB) with a poster image; only the visible one loads, one plays at a time, cached on the phone; first frame within 1 s from cache, 3 s on first load over Wi-Fi | Videos must never slow down logging a set |

## Alternatives considered

| Option | Pros | Cons | Why not chosen |
| --- | --- | --- | --- |
| Signal only, improve the text replies | No new surface or code to maintain | Still no buttons or steppers; set logging stays typed | Doesn't fix the problem the app is for |
| App sends Signal messages to Chotu | No server changes | Screen-scraping replies; no structured data; adds latency | Builds a fragile client on an interface made for people |
| App reads SQLite directly (synced file) | No API | Two writers to one database; bypasses privacy rules; copies the household onto the phone | Breaks the local-only and self-only rules |
| Separate API service process | Clear isolation | Second process, second config load, duplicate supervisor work | The coordinator already owns the pool, config and lifecycle |
| Flutter instead of Expo | One codebase for Android, iOS and web | Web build draws on a canvas, so it is heavier and feels less native in Safari; Dart is one more language | Expo's web build is the better fit for iPhones without the $99 account |
| Native Kotlin, Android only | Most native Android feel; standard Compose stack | No path to iPhones without a second codebase | Would mean a rewrite as soon as an iPhone user joins |
| Plain PWA, no framework | Simplest build; one web codebase | Moving to a native app later means switching frameworks | Avoids a later switch: Expo covers web now and native later |
| Expose on the LAN + reverse proxy, outside Tailscale | Works without Tailscale | Certificates to manage; reachable by any LAN device | Not planned: Tailscale already gives device identity and keeps the port off the LAN |
| Firebase Cloud Messaging for push | Standard Android push | Notification metadata passes through Google | Signal already delivers pushes privately |

## Implementation plan

Each phase ships on its own and leaves Signal working. A phase is done when its exit check passes.

1. **Phase 0: split set logging from Signal.** Extract a typed `log_set` / `workout_state` / `close_workout` core in `health-coach`, with `log_command_reply` formatting on top. Move `CallerScope` into a shared module.
   - Exit: `just test` passes and Signal `/log` replies are unchanged, checked by the existing tests plus a snapshot of the reply text.
2. **Phase 1: API skeleton and workout endpoints.** Add `api.rs` on loopback behind tailscale serve (HTTPS), token auth, `request_id` dedupe, `/v1/me` and the `/v1/workout` endpoints. Add `just api-token <device>` to mint a token and print its `.env` line.
   - Exit: integration tests show a set logged in Signal appears via `GET /v1/workout`, the reverse also holds, and one member's token cannot read another member's sets, and plain HTTP is refused.
3. **Phase 2: app MVP, Android APK.** Settings screen (server address + token), workout screen with exercise picker, weight and reps steppers pre-filled from the next target, Log and Done buttons. CI runs the web export so the code stays web-ready; nothing is published.
   - Exit: a full gym session is logged from an Android phone (APK) over HTTPS on Tailscale, and `/plan` progress in Signal reflects it. The web export builds cleanly in CI.
4. **Phase 3: tasks.** Typed task core, the three task endpoints, a task list screen with swipe to complete or snooze.
   - Exit: completing a task in the app stops its Signal due reminder and updates its Google Calendar event.
5. **Phase 4: status and plan screens.** Read-only `/v1/status` and `/v1/plan`, rendered as app screens.
   - Exit: the screens show the same numbers as the Signal replies for the same day.

Work is tracked in [#126](https://github.com/Prajaktcs/chottu/issues/126). Alongside the phases: UI design, which every screen builds on (#133), offline queue ([#128](https://github.com/Prajaktcs/chottu/issues/128)), app timestamps ([#127](https://github.com/Prajaktcs/chottu/issues/127)), version check ([#129](https://github.com/Prajaktcs/chottu/issues/129)), contract fixtures ([#130](https://github.com/Prajaktcs/chottu/issues/130)), weight units ([#131](https://github.com/Prajaktcs/chottu/issues/131)), repo docs ([#132](https://github.com/Prajaktcs/chottu/issues/132)) and exercise videos ([#125](https://github.com/Prajaktcs/chottu/issues/125)).

## Success metrics

- A set takes 2 taps or fewer when the pre-filled target is right, and under 5 seconds from screen-on.
- Zero duplicate sets in `fitness_set_log` from retries over 4 weeks of use.
- Zero changes to Signal reply text for features moved to the service layer, unless made on purpose.
- Zero API requests accepted from outside the Tailscale address or over plain HTTP, confirmed by the bind check and a test.
- After 4 weeks, most gym sessions are logged from the app rather than Signal. If not, the app isn't paying for itself and phases 3–4 should be reconsidered.

## Open questions

- [ ] Is Tailscale already running on the mini and the family phones, or does it need setting up first?
- [ ] Should food logging (text and photo) come to the app, or stay Signal-only given the Gemini round-trip?
- [ ] When an action happens in the app, should Signal also get a confirmation message, or stay silent?
- [ ] Does any family member besides you need the app in the first release?
- [ ] Later: read steps and workouts from Health Connect on the phone instead of the Google Health API (Android build only)?

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
| --- | --- | --- | --- |
| Splitting handlers changes Signal behaviour | Medium | High | Snapshot current reply text before each extraction; one feature per PR |
| API accidentally bound to all interfaces | Low | High | Startup refuses any non-loopback bind, including `0.0.0.0` and Tailscale (`100.64.0.0/10`) addresses; test covers it |
| Leaked or lost phone token | Low | High | Per-device tokens, stored hashed; remove the `.env` line and restart to revoke |
| App and Signal edit the same workout at once | Medium | Low | Both go through one service function; the open-session unique index and `request_id` dedupe keep state consistent |
| Two clients double the work for every new feature | Medium | Medium | The typed core is written once; only a feature that needs a screen gets an endpoint |
| A library has no web support, so the iPhone build breaks | Medium | Medium | Prefer Expo SDK modules; CI runs the web export on every PR |
| `signal.rs` refactor stalls under its size | Medium | Medium | Extract only what the app uses, phase by phase; no big-bang rewrite |
