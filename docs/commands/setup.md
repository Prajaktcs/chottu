# Setup commands

Onboarding: identify the chat, bind it to a family member, and complete Google OAuth.

---

## `/chat`

Shows the current Signal conversation (direct ACI or group id).

**Looks like**

```text
Current Signal conversation: direct:00000000-0000-0000-0000-000000000001
```

Use this for diagnostics. Household group targeting uses `SIGNAL_GROUP_ID`, not this command.

---

## `/link <member_id>`

Direct Signal conversations only. Writes `signal_aci` on that member in `config.yaml`.

```text
/link praj
```

**Looks like (success)**

```text
✅ Linked this chat to praj (Praj).
Food and tasks without a member id will default to you.
```

**Gotchas**

- Groups are rejected — open a 1:1 Signal conversation.
- If the member is already linked to a different ACI, clear `signal_aci` in config first.
- Once any member is linked, unknown conversations are rejected (`/chat` and `/link` still work for setup).

---

## `/whoami`

```text
You are linked as praj (Praj).
```

Unlinked chat gets a short “not linked” message.

---

## `/login …`

Interactive OAuth with a local browser callback (`http://localhost:8080/callback`). Tokens are appended to `.env`.

| Usage | Writes |
| :--- | :--- |
| `/login health <member_id>` | `HEALTH_REFRESH_TOKEN_<MEMBER>` (+ legacy `FITBIT_REFRESH_TOKEN` for primary) |
| `/login gmail` | `CHOTU_OAUTH_REFRESH_TOKEN` |
| `/login calendar <member_id>` | `CALENDAR_REFRESH_TOKEN_<MEMBER>` |
| `/login code <…>` | Manual code path if the browser dance fails |

**Looks like**

1. Bot sends an authorization URL.
2. You approve in the browser on the machine running Chotu.
3. Bot confirms login and that the refresh token was saved.

**Needs in `.env` first**

- Health: `FITBIT_CLIENT_ID` / `FITBIT_CLIENT_SECRET`
- Gmail / Calendar: `CHOTU_OAUTH_CLIENT_ID` / `CHOTU_OAUTH_CLIENT_SECRET` (+ `CHOTU_EMAIL_USER` for Gmail)
- Calendar members also need a `calendar:` block in `config.yaml`

Full console steps: [Services & credentials](../services-and-credentials.md) and root README “Linking Accounts.”
