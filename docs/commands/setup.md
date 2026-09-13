# Setup commands

Chat identity is configured by the operator before startup. There is no chat
command that can claim or change an identity.

## Select and authorize a provider

Set `CHOTU_CHAT_PROVIDER=signal` or `telegram` in `.env`. Omitting it preserves
the existing Signal default. Direct and household IDs live in `config.yaml`:

```yaml
chat:
  household_ids:
    signal: aG91c2Vob2xk
    telegram: "-1001234567890"
family:
  members:
    - id: praj
      name: Praj
      role: adult
      chat_ids:
        signal: 00000000-0000-0000-0000-000000000001
        telegram: "123456789"
```

Only the selected provider's IDs authorize conversations. A direct message must
match one member ID exactly. A group must match that provider's household ID
exactly; a linked sender does not authorize another group. Restart Chotu after
changing provider or authorization.

### Signal

Set `SIGNAL_CLI_SOCKET`; `just run` reuses a listening daemon or starts
`signal-cli` from `SIGNAL_ACCOUNT` and `SIGNAL_CLI_DATA_DIR`. The legacy
`SIGNAL_GROUP_ID` and member `signal_aci` keys remain accepted for migration;
new configuration should use the provider-scoped YAML above.

Chotu can use your phone's account when `signal-cli` is linked as a secondary
device. Configure that account's ACI for your member, restart Chotu, then use
Signal's **Note to Self**. Every reply starts with `[Chotu]` followed by a
space; Chotu ignores Note-to-Self sync messages carrying that reserved prefix
and ignores messages sent from the account to other Signal contacts.

### Telegram

Create a bot with BotFather, set `TELEGRAM_BOT_TOKEN`, and select
`CHOTU_CHAT_PROVIDER=telegram`. Send the bot a direct message to learn your
numeric user/chat ID from Bot API `getUpdates`; add the bot to the household
group to learn its negative group or supergroup ID. Store those IDs as quoted
strings in `chat_ids.telegram` and `chat.household_ids.telegram`.

Telegram startup validates the token with `getMe` and checks `getUpdates` for
webhook conflicts. It does not require or inspect Signal credentials,
`signal-cli`, `jq`, `nc`, or `shlock`.

---

## `/chat`

Shows the current authorized provider, conversation kind, and ID:

```text
Current chat conversation: telegram:direct:123456789
```

---

## `/whoami`

In an authorized direct conversation, shows the member configured for that
provider ID. In the configured group, confirms that it is the household group.

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
