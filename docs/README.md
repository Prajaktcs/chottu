# Chotu operator docs

Living notes for running the household agent: credentials, services, and how Telegram commands behave.

| Page | What’s here |
| :--- | :--- |
| [Services & credentials](./services-and-credentials.md) | APIs, OAuth, env vars, what breaks if something is missing |
| [Setup commands](./commands/setup.md) | `/link`, `/whoami`, `/chat`, `/login` |
| [Health commands](./commands/health.md) | Food, sync, status, trends, plan |
| [Day loop](./commands/day-loop.md) | Brief, calendar, tasks, reflection |
| [Memory](./commands/memory.md) | `/memory` RAG |
| [Finance](./commands/finance.md) | Monthly, budget, net worth, research |
| [Condition tracking spec](./condition-tracking-spec.md) | M1 shipped (schema + config); M2–M6 still open |

**Secrets:** never put real API keys, tokens, or refresh tokens in these files. Values live only in local `.env` (gitignored). Document names and “where to get them,” not the secrets themselves.

The story of the project (why it exists, how it is built, what it is) lives in the root [`README.md`](../README.md). This folder is the operator manual.
