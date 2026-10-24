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

4. From a phone on the tailnet, open `https://<mini>.<tailnet>.ts.net:8787/v1/health`. The response is:

   ```json
   {"status":"ok"}
   ```

   `<mini>.<tailnet>.ts.net` is the mini's MagicDNS name. `tailscale status` shows it. The app uses this same URL as its server address.

## Check that only the tailnet can reach it

- From a device on the tailnet, the health check URL above should answer.
- From a LAN device that is not on the tailnet, `curl http://<mini-lan-ip>:8787/v1/health` should fail to connect.

## Endpoints

| Method | Path | Auth | Returns |
| --- | --- | --- | --- |
| GET | `/v1/health` | none | `{"status":"ok"}`. Holds no household data. |

## Request logs

Every request logs one line:

```text
api_request method=GET route=/v1/health status=200 duration_ms=0 member_id=- tailscale_user="alex@example.com"
```

- `route` is the route pattern, not the raw path. Requests that match no route log `<unmatched>`.
- `member_id` is `-` until device-token auth lands.
- `tailscale_user` is the `Tailscale-User-Login` header, or `-` if it is missing. It is logged only and does not affect authorization.
- Tokens and request bodies are never logged.

## Shutdown

On Ctrl-C, or when another coordinator task exits, the API stops accepting connections. It then waits up to 5 seconds for in-flight requests to finish.
