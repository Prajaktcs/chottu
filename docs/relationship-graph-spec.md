# Spec: Local Relationship Graph (YouSpot-shaped, Chotu-native)

Add a **local second brain for people**: remember who you know, what you've
shared with them, and what still needs attention — then surface that where
Chotu already talks to you (`/brief`, `/cal`, `/memory`).

Inspired by AI personal CRMs like YouSpot, but as a **Chotu domain extension**,
not a cloud CRM product.

**Status:** design / product intent only. Schema, adapters, and merge mechanics
belong in a later implementation PR — not here.

---

## Motivation

Chotu already sees people as side effects of other work: email senders, calendar
attendees, journals, digests, tasks. What is missing is a **stable person** those
edges hang off.

Without that:

- “Who is Jane?” is keyword search, not identity
- meeting prep cannot say “last talked 3 weeks ago”
- follow-ups drift into orphan tasks with no relationship context
- each new channel invents its own contact list

The product is not a contacts UI. The product is **attention**: who matters,
what is unfinished, and what context should show up where you already look.

Finance asks “what do I own?” Health asks “what did I eat?” Relationships ask
“who matters, and what’s unfinished?”

---

## Design principles

1. **One graph, many adapters.** Email, calendar, contacts, chat notes/exports,
   and future bridges all feed the same people model. Channels are plugins.
2. **Identity over display names.** Exact emails / phones / handles link people;
   fuzzy name-only matches stay human-confirmed.
3. **History stays local.** Graph and notes live on the machine. No standing
   copy of the social graph in a third-party CRM.
4. **Leverage existing surfaces first.** Prefer `/brief`, `/cal`, and `/memory`
   over new CRM chrome.
5. **Attention over archive.** Last-touch, open loops, and meeting prep beat
   storing every message from every channel.
6. **Household scoping still applies.** A member’s network is not automatically
   household-wide.

---

## Core idea (concepts only)

| Concept | Meaning |
| :--- | :--- |
| **Person** | Stable node for someone you know |
| **Identity** | Channel-specific anchor (email, phone, handle, …) on a person |
| **Interaction** | Timed edge: message, meeting, note, contact sync |
| **Relation** | Optional label (friend, colleague, doctor, …) |
| **Follow-up** | Something still owed to / from a person |

Contacts seed **who**. Conversations add **edges and freshness**. The graph’s
job is to answer attention questions across channels — not to mirror any one
app’s inbox.

---

## Where it pays rent

Inject context into surfaces that already own attention:

- **`/brief`** — who you’re seeing today + last touch + open loop
- **`/cal`** — meeting prep (“last talked 3 weeks ago…”)
- **`/memory`** — retrieve by person, not only keyword
- **Reflection** — who went cold this week
- **Tasks** — follow-ups tied to people when useful

Eventually: last touch across channels, unanswered loops, decay (important +
cold), and channel preference.

---

## Ingestion (high level)

Order of ambition, not a build ticket list:

1. **Seed** — contacts (Google / vCard / CSV) + manual notes
2. **Reuse what Chotu already sees** — email counterparties, calendar attendees
3. **Chat later** — exports before live bridges (Signal notes, WhatsApp /
   Telegram exports); only where APIs and ToS allow

One write path into the shared graph. No per-channel mini-CRM.

---

## Non-goals

- Becoming a YouSpot / HubSpot competitor (SaaS, billing, cloud send-agents)
- Live LinkedIn / X scraping or fragile social sync
- Outbound email / auto-messaging people (already a Chotu non-goal)
- Fancy CRM UI or deal pipelines
- Perfect entity resolution on day one
- Spec’ing SQL, OAuth scopes, or adapter payloads in this doc

---

## Privacy

Matches Chotu’s existing line:

- Historical graph stays **local**
- Prefer OAuth-owned APIs and user-initiated exports over scrapers
- Linked personal DMs do not dump another member’s network by default
- Agents suggest follow-ups; humans decide whether to reach out

Exact ownership / sharing rules are an implementation decision when the domain
is built — default bias is private-per-member unless something is clearly
household (family doctor, school, etc.).

---

## Open product questions

1. Default to **per-member private** graphs, with optional shared household
   people?
2. How aggressive should auto-create-from-email be for noreply / marketing
   senders?
3. First contacts path: live Google Contacts, or vCard/CSV drop until the loop
   is proven useful?

---

## References

- [Architecture](https://github.com/Prajaktcs/chottu/blob/main/ARCHITECTURE.md)
- Day loop: [`docs/commands/day-loop.md`](./commands/day-loop.md)
- Memory: [`docs/commands/memory.md`](./commands/memory.md)
- [Later domains](https://github.com/Prajaktcs/chottu/blob/main/TODO.md)
