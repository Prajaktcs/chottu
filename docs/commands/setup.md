# Setup commands

Signal identity is configured by the operator before startup. There is no chat command that can claim or change an identity.

## Authorize Signal conversations

For each allowed direct conversation, set that member's ACI in `config.yaml`:

```yaml
family:
  members:
    - id: praj
      name: Praj
      role: adult
      signal_aci: 00000000-0000-0000-0000-000000000001
```

Optionally set `SIGNAL_GROUP_ID` in `.env` for the household group. Direct messages are accepted only when their sender ACI exactly matches a configured `signal_aci`. Group messages are accepted only when their group id exactly matches `SIGNAL_GROUP_ID`; group authorization does not depend on the sender being linked.

Restart Chotu after changing `signal_aci` or `SIGNAL_GROUP_ID`. Configuration is static for the process lifetime.

---

## `/chat`

Shows the current authorized Signal conversation (direct ACI or group id) for diagnostics.

```text
Current Signal conversation: direct:00000000-0000-0000-0000-000000000001
```

---

## `/whoami`

In an authorized direct conversation, shows the member configured for that ACI. In the configured group, confirms that it is the household group.

---

## `/login …`

OAuth mutation is forbidden in group conversations. In a linked direct conversation, Health and Calendar login can target only that linked member; omitting the member id defaults to self. Gmail uses the operator/global mailbox token but can be initiated only from an authorized direct conversation.

| Usage | Writes |
| :--- | :--- |
| `/login health [your_member_id]` | `HEALTH_REFRESH_TOKEN_<MEMBER>` (+ legacy `FITBIT_REFRESH_TOKEN` for primary) |
| `/login gmail` | `CHOTU_OAUTH_REFRESH_TOKEN` |
| `/login calendar [your_member_id]` | `CALENDAR_REFRESH_TOKEN_<MEMBER>` |
| `/login code health <your_member_id> <code_or_url>` | Manual Health code path, self only |
| `/login code calendar <your_member_id> <code_or_url>` | Manual Calendar code path, self only |
| `/login code gmail <code_or_url>` | Manual global Gmail code path, authorized DM only |

The callback listener uses `http://localhost:8080/callback` and writes refresh tokens to `.env`.

**Needs in `.env` first**

- Health: `FITBIT_CLIENT_ID` / `FITBIT_CLIENT_SECRET`
- Gmail / Calendar: `CHOTU_OAUTH_CLIENT_ID` / `CHOTU_OAUTH_CLIENT_SECRET` (+ `CHOTU_EMAIL_USER` for Gmail)
- Calendar members also need a `calendar:` block in `config.yaml`

Full console steps: [Services & credentials](../services-and-credentials.md) and root README “Linking Accounts.”
