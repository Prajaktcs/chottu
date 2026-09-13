# Pluggable chat transports

**Status:** Accepted for implementation

## Problem

Chotu's domain behavior is currently coupled to Signal at every layer:

- `coordinator/src/main.rs` always starts the Signal loop.
- `coordinator/src/signal.rs` combines transport I/O with command handling, authorization, schedulers, reminders, and OAuth replies.
- `health-coach/src/lib.rs` and `streamer/src/imap_client.rs` construct `SignalClient` directly for outbound notifications.
- `chotu-common/src/family.rs` models member identity as `signal_aci` and reads `SIGNAL_GROUP_ID`.
- Reminder correlation and scheduled-delivery records use Signal-specific table names and do not include a provider namespace.
- `just run` always validates and manages `signal-cli`.

Renaming user-facing wording from Telegram to Signal would describe the current implementation, but it would preserve this coupling. Users should instead be able to select a supported chat provider without changing domain code.

## Decision

Introduce a provider-neutral chat boundary with built-in Signal and Telegram adapters. A user selects one active provider at process startup. All command handling, authorization, privacy rules, reminders, schedulers, health notifications, and email-derived task notifications use normalized chat types.

The initial boundary is source-level pluggability plus runtime provider selection. It does not load arbitrary shared libraries or execute third-party plugin processes. Adding a provider requires a Rust adapter and one enum registration, but no changes to command or agent behavior.

Exactly one provider is active per process in the first cut. Running Signal and Telegram simultaneously would require an explicit duplicate-notification policy, cross-provider conversation state, and OAuth-flow ownership; those are separate product decisions.

## Goals

- Let users choose `signal` or `telegram` without recompiling.
- Preserve current Signal behavior and existing Signal configuration during migration.
- Give Telegram the same common-denominator behavior: direct and household-group authorization, text commands, free text, image attachments, replies, proactive schedules, and task reminders.
- Keep member privacy and household routing independent of provider details.
- Make a third adapter local to the transport layer.
- Require credentials only for the selected provider.
- Keep provider-specific retries, limits, polling, and attachment downloads inside the adapter.

## Non-goals

- Loading untrusted dynamic libraries or arbitrary executables as plugins.
- Activating several providers in one process.
- Restoring Telegram-only inline keyboards, callback queries, message edits, or Markdown rendering.
- Runtime identity linking or an open bootstrap chat.
- Changing command semantics, agent behavior, schedules, or OAuth authorization rules.
- Removing legacy Signal tables in the first migration stage.

## Observable acceptance

1. With no provider selection, an existing Signal installation starts and behaves as it does today.
2. With Telegram selected, `just run` requires a Telegram bot token but does not require `signal-cli`, a Signal socket, `jq`, `nc`, or `shlock`.
3. Both providers exercise the same command handler for text, replies, and images.
4. Direct messages are accepted only when the selected provider's direct ID is mapped to a family member. Household groups are accepted only by exact configured ID.
5. Personal health notifications never fall back to a household group.
6. Scheduled briefs, reflections, portfolio summaries, budget alerts, due reminders, and email-derived reminders route through the selected provider.
7. Reply-to-task correlation cannot collide across providers.
8. Missing credentials, unsupported providers, malformed IDs, webhook conflicts, and provider authentication failures stop startup with a provider-specific error.
9. Inactive-provider credentials and identifiers are ignored.
10. The docs and sample configuration describe chat behavior neutrally, with separate Signal and Telegram setup sections.

## Architecture

```text
                                      domain-owned
 inbound update                       behavior
      │                                  │
      ▼                                  ▼
┌───────────────┐   ChatInbound   ┌──────────────────────┐
│ SignalAdapter │ ──────────────► │ coordinator/src/chat │
└───────────────┘                 │ commands, auth,      │
                                  │ state, schedulers    │
┌────────────────┐  ChatInbound   └──────────┬───────────┘
│TelegramAdapter │ ──────────────►            │ ChatClient
└────────────────┘                            ▼
                                  selected adapter only

health-coach ─┐
streamer ─────┼──► ChatClient ───► selected adapter
coordinator ──┘
```

### Ownership

| Location | Responsibility |
| :--- | :--- |
| `chotu-common/src/chat.rs` | Normalized provider, address, inbound message, attachment, message ID, error classification, and client facade |
| `chotu-common/src/signal.rs` | Existing `signal-cli` JSON-RPC implementation; translates only between Signal and normalized chat types |
| `chotu-common/src/telegram.rs` | Telegram Bot API long polling, send, download, and normalized update translation |
| `chotu-common/src/family.rs` | Provider-aware member and household address resolution; exact authorization policy |
| `coordinator/src/chat.rs` | Renamed current coordinator Signal module; provider-neutral command and conversation behavior |
| `coordinator/src/scheduled_delivery.rs` | Provider-neutral recipient delivery state |
| `streamer/src/imap_client.rs` | Durable email-derived reminder outbox through `ChatClient` |
| `health-coach/src/lib.rs` | Private member notification through `ChatClient` |
| `coordinator/src/main.rs` | Resolve provider once and construct the selected adapter |
| `justfile` | Start prerequisites only for the selected provider |

The coordinator, Streamer, and Health Coach must not import `SignalClient`, `TelegramClient`, or provider-specific recipient types after the cutover. Only `chotu-common::chat` and adapter modules may do so.

## Normalized contract

Illustrative types:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChatProvider {
    Signal,
    Telegram,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConversationKind {
    Direct,
    Group,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatAddress {
    pub provider: ChatProvider,
    pub kind: ConversationKind,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatAttachment {
    pub id: String,
    pub content_type: String,
    pub size: Option<u64>,
    pub caption: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatInbound {
    pub sender_id: String,
    pub conversation: ChatAddress,
    pub text: Option<String>,
    pub reply_to: Option<ChatMessageId>,
    pub attachments: Vec<ChatAttachment>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChatMessageId(pub String);
```

`ChatAddress` always carries its provider. Comparing or persisting a bare provider ID is prohibited because Signal and Telegram identifiers occupy different namespaces.

The facade uses enum dispatch rather than `Arc<dyn Trait>`:

```rust
pub enum ChatClient {
    Signal(SignalAdapter),
    Telegram(TelegramAdapter),
}

impl ChatClient {
    pub async fn connect(provider: ChatProvider) -> Result<Self, ChatError>;
    pub async fn subscribe(&self) -> Result<ChatReceiver, ChatError>;
    pub async fn send_text(
        &self,
        recipient: &ChatAddress,
        text: &str,
    ) -> Result<ChatMessageId, ChatError>;
    pub async fn download_attachment(
        &self,
        recipient: &ChatAddress,
        attachment_id: &str,
    ) -> Result<Vec<u8>, ChatError>;
}
```

Enum dispatch keeps the hot send path allocation-free apart from provider/network payloads, makes provider mismatches explicit, and avoids an `async_trait` dependency. Adding an adapter extends the enum and its four dispatch sites; domain code remains unchanged.

`ChatError` classifies errors as configuration, authentication, transient transport, permanent request, or protocol. Retry policy consumes that classification instead of matching Signal error variants in the coordinator and Streamer.

## Common-denominator behavior

The domain emits plain text. Adapters must not reinterpret Markdown-like characters. This preserves Signal output and avoids divergent provider rendering.

The normalized contract supports only behavior needed by both initial adapters:

- receive text and image-bearing messages;
- send text;
- download an attachment;
- identify direct versus group conversations;
- identify the sender;
- correlate a reply with a previously sent message.

Provider-only capabilities such as inline buttons and message editing are intentionally absent. A capabilities API should be added only when a concrete domain feature needs graceful degradation.

Message-size enforcement belongs to the adapter. The current research report splitter should consume `ChatClient::max_text_chars()` rather than hard-code `4000`.

## Provider mappings

### Signal

- Direct address ID: Signal ACI.
- Group address ID: Signal group ID.
- Outbound message ID: Signal send timestamp converted to a decimal string.
- Reply ID: quoted Signal timestamp converted to the same string.
- Attachment ID: `signal-cli` attachment ID.
- Receive: existing Unix-socket `subscribeReceive` flow.
- Send prefix: preserve `[Chotu]` plus its trailing space and the Note-to-Self loop guard.
- Transient errors: socket I/O, EOF, reconnecting, and timeout where the request is known not to have completed.

### Telegram

- Direct address ID: decimal private-chat ID stored as a string.
- Group address ID: decimal group/supergroup chat ID stored as a string.
- Sender ID: Telegram `from.id` stored as a string.
- Outbound message ID: Telegram `message_id` stored as a decimal string.
- Reply ID: `reply_to_message.message_id` stored identically.
- Attachment ID: largest photo's `file_id`, or an image document's `file_id`.
- Receive: Bot API `getUpdates` long polling with monotonically advanced `offset` and `allowed_updates = ["message"]`.
- Download: `getFile`, then the Bot API file URL.
- Text: `message.text`; image captions populate both the attachment caption and the text visible to routing.
- Command normalization: strip Telegram's `/command@botname` suffix in the adapter before the common parser.
- Startup validation: call `getMe`; treat a `getUpdates` webhook conflict as fatal instead of retrying it forever.
- Transient errors: network failures, HTTP 429, and HTTP 5xx. Other Bot API errors are permanent for that attempt.

Only the coordinator subscribes for inbound updates. Streamer and Health Coach create send-only clients, so Telegram never starts competing long-poll consumers.

## Configuration

Provider selection belongs in `.env` because `just run` must choose provider prerequisites before Rust loads `config.yaml`:

```dotenv
# Existing installations may omit this; omission means signal.
CHOTU_CHAT_PROVIDER=signal

# Signal credentials and daemon configuration.
SIGNAL_ACCOUNT=
SIGNAL_CLI_DATA_DIR=
SIGNAL_CLI_SOCKET=

# Telegram credential. Required only when CHOTU_CHAT_PROVIDER=telegram.
TELEGRAM_BOT_TOKEN=
```

Non-secret routing identifiers stay in `config.yaml`:

```yaml
chat:
  household_ids:
    signal: "base64-signal-group-id"
    telegram: "-1001234567890"

family:
  members:
    - id: alex
      name: Alex
      role: adult
      chat_ids:
        signal: "00000000-0000-0000-0000-000000000001"
        telegram: "123456789"
```

IDs are strings even when a provider currently uses integers. That prevents numeric narrowing and keeps the config contract extensible.

### Validation

Configuration loading must fail when:

- `CHOTU_CHAT_PROVIDER` is not a compiled provider;
- a selected-provider member ID is blank or duplicated across members;
- a household ID equals a member direct ID;
- a Telegram ID is not a signed decimal integer;
- a Signal direct ID is malformed for the accepted ACI format;
- both legacy and new fields are present with conflicting values.

Identifiers for inactive providers are retained and syntax-checked, but missing inactive-provider identifiers are valid.

### Compatibility window

The first release accepts current Signal configuration:

- `family.members[].signal_aci` maps to `chat_ids.signal`;
- `SIGNAL_GROUP_ID` maps to `chat.household_ids.signal`;
- omitted `CHOTU_CHAT_PROVIDER` maps to `signal`.

If a legacy and new value are both present, equal values are accepted and conflicting values fail loudly. Serialization writes only the new shape. Legacy readers remain in place for one documented compatibility release, then are removed in a separate contraction change.

There is no automatic migration of the user's gitignored `config.yaml`. The startup message prints the equivalent new field once when a legacy field is used.

## Authorization and privacy invariants

1. The provider is resolved once at startup and passed explicitly; helpers must not re-read it from the environment.
2. A direct conversation resolves to exactly one family member by `(provider, direct_id)`.
3. Duplicate direct IDs for one provider are invalid configuration.
4. A group is authorized only by exact `(provider, group_id)` match.
5. A sender linked in a direct conversation does not authorize an unconfigured group.
6. Personal health syncs target only that member's selected-provider direct address. No household fallback.
7. Assigned task reminders prefer the assignee's direct address and may use only the selected provider's configured household group as fallback.
8. OAuth mutation remains direct-message-only and self-only.
9. Logs may print provider and non-secret conversation IDs, but never bot tokens or attachment download URLs containing tokens.

## Persistence

Provider must be part of every durable delivery or reply-correlation key. Message IDs become text because provider identifiers are not universally numeric.

Add transport-neutral tables in a new migration; do not edit committed SQLx migrations:

```sql
CREATE TABLE task_chat_messages (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    conversation_kind TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (provider, conversation_kind, conversation_id, message_id)
);

CREATE TABLE task_chat_due_reminder_deliveries (
    task_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    conversation_kind TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    PRIMARY KEY (task_id, provider, conversation_kind, conversation_id)
);
```

The same provider column and text IDs apply to replacements for `scheduled_signal_deliveries` and `email_task_signal_deliveries`.
Delivery outboxes use `pending`, `sending`, `delivered`, and terminal `failed` states. Only transient transport failures return to `pending`; authentication, configuration, protocol, and permanent request failures move to `failed`.

### Expand and rollback sequence

1. Create the neutral tables alongside existing Signal tables.
2. Backfill current Signal rows with `provider = 'signal'` and cast message timestamps to text.
3. Run the new binary in dual-write mode for Signal: neutral tables are authoritative, while successful Signal writes also update legacy tables.
4. Telegram writes only neutral tables.
5. Verify existing Signal databases, fresh Signal databases, fresh Telegram databases, retry state, and reply correlation.
6. Keep legacy tables during the compatibility release. Rolling the binary back restores Signal behavior from the legacy tables without data loss for Signal sends made during the new release.
7. Remove dual writes and legacy tables only in a later, explicitly approved contraction migration.

A rollback to the old Signal-only binary intentionally ignores Telegram-only state; it must never reinterpret Telegram IDs as Signal recipients.

## Runtime flow

1. `main` parses `CHOTU_CHAT_PROVIDER` once.
2. Configuration loads and resolves legacy/new IDs for that provider.
3. Startup validates only the selected provider's credentials and runtime prerequisites.
4. `ChatClient::connect` constructs the adapter and performs a provider health check.
5. The coordinator subscribes and receives normalized messages.
6. Authorization resolves `ChatInbound.conversation` through provider-aware family configuration.
7. Existing per-conversation queues serialize handlers by full `ChatAddress`; the global semaphore remains unchanged.
8. All outbound paths receive the same selected `ChatClient` or construct a send-only client through the same factory.
9. Durable records use the full provider-scoped address and text message ID.

## Launcher behavior

`just run` keeps the current Signal daemon lifecycle only in the Signal branch:

- `signal`: require `SIGNAL_CLI_SOCKET` and Gemini; validate `jq`, `nc`, `shlock`; reuse or start `signal-cli`; stop only the daemon this invocation started.
- `telegram`: require `TELEGRAM_BOT_TOKEN` and Gemini; do not inspect Signal variables or tools; run the coordinator directly.
- unsupported value: fail before starting any agent.

`just setup` writes both provider credential sections and defaults `CHOTU_CHAT_PROVIDER=signal`, preserving current installations.

## Implementation sequence

Each stage remains buildable and has a focused proof.

1. **Normalize types without behavior change**
   - Add `ChatProvider`, `ChatAddress`, `ChatInbound`, `ChatAttachment`, `ChatMessageId`, and `ChatError`.
   - Adapt Signal into the facade.
   - Migrate coordinator, scheduler, Streamer, and Health Coach to the facade.
   - Keep Signal as the only selectable provider and run the existing Signal proofs.

2. **Expand configuration and persistence**
   - Add `chat_ids`, household IDs, and compatibility readers.
   - Add neutral tables, Signal backfill, and Signal dual writes.
   - Prove current config/database upgrade and rollback compatibility.

3. **Add Telegram adapter**
   - Implement `getMe`, `getUpdates`, `sendMessage`, `getFile`, and file download.
   - Normalize commands, replies, photos, direct chats, and groups.
   - Exercise against a local fake Bot API server; no live user account in automated tests.

4. **Select provider at startup**
   - Add `CHOTU_CHAT_PROVIDER` parsing and selected credential validation.
   - Branch `just run` prerequisites.
   - Prove that each provider starts without the inactive provider's dependencies.

5. **Make documentation transport-neutral**
   - Update README, architecture, command docs, health text, comments, and prompts.
   - Keep historical Signal-versus-Telegram material explicitly labeled as history.
   - Add separate provider setup instructions.

6. **Contract later**
   - After one compatibility release, remove legacy config readers, Signal dual writes, and Signal-named tables in a separate migration.

## Verification matrix

| Surface | Signal | Telegram |
| :--- | :--- | :--- |
| Startup credentials | socket/daemon health | `getMe` |
| Inbound direct command | ACI mapping | private chat ID mapping |
| Inbound configured group | exact group ID | exact group/supergroup ID |
| Unauthorized direct/group | rejected | rejected |
| Bot-addressed command | unchanged | suffix normalized |
| Text reply | send timestamp | message ID |
| Reply correlation | quote timestamp | replied-to message ID |
| Image food flow | attachment download | `getFile` download |
| Scheduled household fan-out | direct IDs + group | direct IDs + group |
| Private health nudge | member DM only | member DM only |
| Email task outbox retry | transient socket failure | 429/5xx/network failure |
| Existing config | legacy accepted | not applicable |
| Existing database | backfilled, dual-written | provider-isolated new rows |
| Inactive dependency absent | no Telegram token needed | no Signal tools/socket needed |

Focused tests should use the existing fake Signal Unix socket and a local fake Telegram HTTP server. A manual smoke run should send `/chat`, `/whoami`, `/tasks`, a replied `unactionable`, and one photo on each provider before release.

## Risks and controls

- **Privacy regression during genericization:** keep authorization policy in one provider-aware resolver and retain direct/group rejection tests for both providers.
- **Duplicate scheduled sends after migration:** backfill before neutral reads and dual-write Signal results during rollback window.
- **Provider ID collision:** include provider in every address, map key, and database key.
- **Telegram update loss or duplication:** enqueue every normalized update before advancing to `max(update_id) + 1`. This matches the current in-memory inbound durability; durable task-delivery paths remain idempotent.
- **Two Telegram pollers:** only the coordinator may subscribe; other agents use send-only clients.
- **Webhook conflict:** detect it at startup and return an actionable error.
- **Behavior drift from provider formatting:** plain text is the domain contract.
- **Overbuilt plugin system:** no dynamic loading, capability registry, or concurrent providers until a concrete requirement needs it.

## Confirmed scope

- Pluggability means runtime selection among built-in Rust adapters.
- Exactly one provider is active per Chotu process.

These choices cover user choice while preserving the current privacy and delivery model. External plugin processes and concurrent providers remain explicit non-goals.
