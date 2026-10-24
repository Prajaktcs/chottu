# App API

The coordinator can serve an HTTP API for the Chotu app. It is off unless `api.bind` is set in `config.yaml`. The API listens on loopback only, and `tailscale serve` puts HTTPS in front of it. The API is never reachable from the LAN or the internet.

## Why `tailscale serve`

- **Certificates renew themselves.** A `tailscale cert` certificate expires after 90 days and does not renew on its own. `tailscale serve` renews it for you.
- **Only tailnet devices can connect.** The port is exposed on the tailnet only.
- **It identifies the caller.** Each request carries the caller's Tailscale login in the `Tailscale-User-Login` header.

## Setup

1. Enable the API in `config.yaml`:

   ```yaml
   api:
     bind: 127.0.0.1:8787
   ```

   Startup refuses any address that is not loopback (`127.0.0.0/8` or `::1`). That includes `0.0.0.0`, LAN addresses and Tailscale `100.x` addresses. The only way in is through `tailscale serve`.

2. Restart Chotu (`just run`). The log shows:

   ```text
   API server listening on http://127.0.0.1:8787 (expose with `tailscale serve`)
   ```

3. Put HTTPS in front of it, once, on the Mac mini:

   ```sh
   tailscale serve --bg --https=8787 http://127.0.0.1:8787
   ```

   `--bg` keeps the setting across reboots. Check it with `tailscale serve status`. Remove it with `tailscale serve --https=8787 off`.

4. From a phone on the tailnet, open `https://<mini>.<tailnet>.ts.net:8787/v1/health`. The response looks like:

   ```json
   {"status":"ok","server_version":"0.1.0+87dd739","min_app_version":"0.1.0"}
   ```

   `<mini>.<tailnet>.ts.net` is the mini's MagicDNS name. `tailscale status` shows it. The app uses this same URL as its server address.

## Check that only the tailnet can reach it

- From a device on the tailnet, the health check URL above should answer.
- From a LAN device that is not on the tailnet, `curl http://<mini-lan-ip>:8787/v1/health` should fail to connect.

## Endpoints

| Method | Path | Auth | Returns |
| --- | --- | --- | --- |
| GET | `/v1/health` | none | `status`, `server_version` and `min_app_version`. Holds no household data. |
| GET | `/v1/me` | device token; `X-Chotu-Member` optional | The device, the chosen profile (or `null`), and the profiles it may use. |
| GET | `/v1/workout` | device token + `X-Chotu-Member` | The member's open workout today, or `null`. See [Workout](#workout). |
| POST | `/v1/workout/sets` | device token + `X-Chotu-Member` + `Idempotency-Key` | Logs one set. |
| POST | `/v1/workout/done` | device token + `X-Chotu-Member` + `Idempotency-Key` | Closes today's workout and summarizes it. |
| DELETE | `/v1/workout/sets/{id}` | device token + `X-Chotu-Member` + `Idempotency-Key` | Removes one set from the open workout. |

`/v1/me` for a shared iPad that picked Jordan:

```json
{
  "device": "kitchen_ipad",
  "member": {"id": "jordan", "name": "Jordan"},
  "profiles": [{"id": "alex", "name": "Alex"}, {"id": "jordan", "name": "Jordan"}]
}
```

Without `X-Chotu-Member`, `member` is `null`. The app calls it that way to fill its profile picker.

## Versions

`/v1/health` reports two versions:

- `server_version` is the coordinator crate version plus the git commit it was built from, such as `0.1.0+87dd739`. The commit is `unknown` when the build had no git checkout.
- `min_app_version` is the oldest app the server supports. It is the `MIN_APP_VERSION` constant in `coordinator/src/api.rs`.

The app sends its own version on every request:

```text
X-Chotu-App-Version: 1.4.2
```

The version is `major.minor.patch`, compared numerically. A pre-release or build suffix (`1.4.2-beta`, `1.4.2+7`) is ignored.

| `X-Chotu-App-Version` | Result |
| --- | --- |
| Below `min_app_version` | `426`, with `{"error":"app update required","min_app_version":"0.1.0"}` |
| At or above `min_app_version` | The normal response |
| Not `major.minor.patch` | `400` |
| Missing | The normal response. The app always sends it; curl and scripts need not. |

The version is checked before the device token, so an outdated app is told to update even if its token is wrong. `/v1/health` skips the check. An outdated app can always read the minimum it needs.

### API compatibility rule

Sideloaded app builds lag the server, so the server must keep serving older apps:

- **Within `/v1`, changes are additive only.** New endpoints and new optional fields are fine. Removing or renaming a field, changing its type or meaning, or making a request field required is a breaking change.
- **A breaking change goes under `/v2`,** or it bumps `MIN_APP_VERSION` to the first app release that handles it. Apps below the new minimum then get `426` and an update prompt instead of a broken screen.
- Bump `MIN_APP_VERSION` only when the server drops something an older app needs.

## Devices and profiles

Each app install is a **device** with its own bearer token. A device lists the household members (profiles) it may act as. The app's profile picker chooses one of them, and the app sends it on every request.

1. Mint a token on the Mac mini:

   ```sh
   just api-token alex_phone
   ```

   It prints the token once, followed by a line like `API_TOKEN_ALEX_PHONE='$argon2id$v=19$...'`. The token looks like `alex_phone.<64 hex chars>`: the device name, then 256 random bits. Device names use ASCII letters, digits and `_`.

2. Add that line to `.env`, single quotes included (the hash contains `$`). Only the Argon2id hash is stored, using OWASP's recommended parameters (19 MiB, 2 passes). The raw token lives only on the device.

3. List the device in `config.yaml`:

   ```yaml
   api:
     bind: 127.0.0.1:8787
     devices:
       - name: alex_phone
         members: [alex]
       - name: kitchen_ipad
         members: [alex, jordan]
   ```

   Every member must be a `family.members[].id`. Startup fails on an unknown member, an empty list, a duplicate name, or a hash that is not an Argon2id PHC string.

4. Restart Chotu, then enter the token in the app.

Requests send:

```text
Authorization: Bearer <token>
X-Chotu-Member: alex
X-Chotu-App-Version: 1.4.2
```

| Request | Status |
| --- | --- |
| `X-Chotu-App-Version` below `min_app_version` | `426` |
| No token, or a token that matches no device | `401` |
| `X-Chotu-Member` names a member the device may not use | `403` |
| A member-data route without `X-Chotu-Member` | `400` |

Every authenticated request acts for exactly one member, the same way a linked Signal DM does. The app never acts for the household group.

**Revoke a device:** remove its `API_TOKEN_<DEVICE>` line from `.env` and restart. A configured device with no token line logs a warning at startup and cannot sign in. To rotate, mint a new token and replace the line.

## Writes are idempotent

Every `POST` and `DELETE` sends a client-generated `request_id` in the `Idempotency-Key` header. A retry with the same id gets the original response back, and the write is not applied a second time. The app generates a fresh UUIDv4 per write and keeps it when it retries or replays a queued write.

```text
Authorization: Bearer <token>
X-Chotu-Member: alex
Idempotency-Key: 3f2b9c4e-8a1d-4c55-9a8e-2b7f0c1d9e6a
```

- The id is a header rather than a body field, so writes without a body (`DELETE`, `POST /v1/workout/done`) carry it the same way.
- Ids are scoped to the member, not the device. The same id from Alex's phone and the kitchen iPad acting as Alex is one write. Jordan's writes never collide with Alex's.
- A repeat returns the stored status, `Content-Type` and body. Ids are kept for 7 days. After that, the same id applies again.
- A `5xx` response is not stored, so the client can retry it with the same id.

| Request | Status |
| --- | --- |
| A write without `Idempotency-Key`, or with one that is not 1–128 visible ASCII characters | `400` |
| The same id while its first attempt is still running | `409`; retry shortly |
| The same id on a different method or path | `422` |

Server side, every write handler extracts `Idempotency` and wraps its write in `Idempotency::run` (`coordinator/src/api/idempotency.rs`). Responses are stored in `api_write_requests`, keyed by `(member_id, request_id)`. Two concurrent attempts race on that primary key, and only the winner applies. Ids still in flight when the coordinator stopped are released at the next startup.

## Workout

The workout endpoints share one workout per member with Signal's `/log`. A workout started in Signal shows up in `GET /v1/workout`, and a set logged in the app shows up in Signal's `/log` status. Both go through the same `health_coach` functions, so "today" is the household timezone's day for both.

Weights in responses are always pounds (`weight_lb`). `null` means bodyweight. Errors are JSON: `{"error": "..."}`.

### `GET /v1/workout`

Returns the open workout for today, or `{"workout": null}` when none is open. A workout idle for 3 hours, or left open from an earlier day, is closed on read.

```json
{
  "workout": {
    "session_id": "6c1d4f0e-2b9a-4f7e-9d1c-0a5e3b7c2f18",
    "current_exercise": "Bench press",
    "plan_day": {"weekday": "Tuesday", "kind": "strength", "notes": "Push"},
    "exercises": [
      {
        "name": "Bench press",
        "sets": 3,
        "reps": "8-10",
        "target": {"weight_lb": 135.0, "reps": [9, 9, 8], "note": null}
      }
    ],
    "sets": [
      {"id": "b0a7e5c2-1f3d-4e8a-9b6c-7d2e4f1a3c59", "exercise": "Bench press", "weight_lb": 135.0, "reps": 8}
    ]
  }
}
```

- `plan_day` is `null` without a weekly plan covering today. `exercises` is empty on rest days and without a plan.
- `target` is the next prescription for a planned lift, or `null`.
- `sets` are oldest first, including lifts outside the plan.

### `POST /v1/workout/sets`

Logs one set and starts today's workout if none is open.

```json
{"exercise": "bench press", "weight": 60, "unit": "kg", "reps": 8, "performed_at": "2026-10-24T18:05:00-04:00"}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `exercise` | yes | A planned lift (matched loosely, so `bench` finds `Bench press`) or any other lift. |
| `weight` | yes, may be `null` | The load in `unit`. `null` is bodyweight. |
| `unit` | yes | `lb` or `kg`. A `kg` weight is converted to pounds before it is stored. |
| `reps` | yes | 1–100. |
| `performed_at` | no | When the set was done, RFC 3339 **with an offset**. Defaults to server time. |

`performed_at` lets a set queued offline land when it was done:

- Within 5 minutes of server time, the set counts as live and joins the open workout, as a Signal set would.
- Earlier, it joins that day's latest workout in the household timezone, even one already closed. If that day has none, a closed one is created.
- More than 5 minutes in the future or more than 7 days old is refused with `422`.

The response is `201` with the stored set and the workout it joined. For an earlier set, that can be a closed workout from that day.

```json
{
  "set": {"id": "b0a7e5c2-1f3d-4e8a-9b6c-7d2e4f1a3c59", "exercise": "Bench press", "weight_lb": 132.277357308, "reps": 8},
  "workout": {"session_id": "6c1d4f0e-2b9a-4f7e-9d1c-0a5e3b7c2f18", "current_exercise": "Bench press", "plan_day": null, "exercises": [], "sets": ["..."]}
}
```

`weight_lb` is the exact conversion, unrounded. Signal shows it to one decimal (`132.3 lb`).

### `POST /v1/workout/done`

Closes today's workout. No body. Returns `200` with what was done per exercise and the next target for planned lifts:

```json
{
  "exercises": [
    {
      "name": "Bench press",
      "sets": [{"weight_lb": 135.0, "reps": 9}, {"weight_lb": 135.0, "reps": 8}],
      "next": {"weight_lb": 135.0, "reps": [10, 9, 9], "note": null}
    }
  ]
}
```

### `DELETE /v1/workout/sets/{id}`

Removes one set from the member's open workout, for example a mistyped one. No body. Returns `200` with the removed set and the updated workout, in the same shape as `POST /v1/workout/sets`. That set's lift becomes the current exercise.

### Workout errors

| Request | Status |
| --- | --- |
| Malformed JSON | `400` |
| A body without `Content-Type: application/json` | `415` |
| A missing field, a wrong type, or an unknown `unit` | `422` |
| `reps` or the weight out of range, or a blank `exercise` | `422` |
| `performed_at` without an offset, more than 5 minutes ahead, or more than 7 days old | `422` |
| `POST /v1/workout/done` with no open workout | `409` |
| `DELETE` of a set that is not in the member's open workout, including another member's set | `404` |

## Request logs

Every request logs one line:

```text
api_request method=GET route=/v1/me status=200 duration_ms=0 device=kitchen_ipad member_id=jordan tailscale_user="alex@example.com"
```

- `route` is the route pattern, not the raw path. Requests that match no route log `<unmatched>`.
- `device` is `-` when the token is missing or matches no device. A `403` still logs the device that asked.
- `member_id` is `-` when the request names no profile or its profile was rejected.
- `tailscale_user` is the `Tailscale-User-Login` header, or `-` if it is missing. It is logged only and does not affect authorization.
- Tokens and request bodies are never logged.

## Shutdown

On Ctrl-C, or when another coordinator task exits, the API stops accepting connections. It then waits up to 5 seconds for in-flight requests to finish.
