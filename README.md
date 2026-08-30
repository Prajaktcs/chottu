# Project Chotu

A household agent that lives on your machine: track money, health, and the calendar without handing that life to a third-party app.

## Why

I am bad at keeping tasks and at tracking things by hand. I have missed payments even when the money was sitting in another account.

So I started building something to watch finances across accounts and to keep a handle on net worth. Once that was moving, health and calendars were the same problem in different clothes — things I lose if I have to remember them myself.

I did not want to store that, or give it, to a third-party app. I also wanted the freedom to tweak any workflow when it stopped fitting. That is why this is not a cloud assistant.

V1 was only the money. I like calling it Chotu.

## Humans, AI, and where this stops

Two lines I will not cross: **privacy**, and **how far AI is allowed to go**.

AI is good at a targeted job. It is bad at knowing when to stop, and at noticing it does not have enough information and should ask. That is the human part. If you need real financial advice, medical advice, or a coach for your own life, go to a person who does that work. This system is here to help you track, and to offer suggestions. It is not here to steer you, or to replace those people — or the rest of human connection.

The same split applies to how the code gets written. This repo is built with heavy coding assistance (primarily Cursor). That is not an autonomous agent shipping unreviewed diffs.

| Who | Owns |
| :--- | :--- |
| **Human** | Product goals, architecture, integrations, privacy posture, reviewing diffs, deciding what ships — and the judgment calls the models should not make |
| **AI** | A large share of implementation drafts, refactors, tests, docs, and bug-fix scaffolding — always under human direction |

Nothing here runs or deploys without human review. Treat the codebase as **human-directed, AI-assisted**. The models that run *inside* the household agent are a different story — and they are why privacy has a sharp edge, not a slogan.

## What stays here, what leaves

The important line: **historical data stays local.** Mail is parsed by a local LLM. Notes are parsed by a local LLM. Memory indexing and retrieval are local; `/memory` answers prefer local Ollama, with an optional Gemini fallback if Ollama fails. The corpus — ledger, journals, embeddings, goals — lives on this machine (a Mac mini), not in someone else’s product.

What leaves is temporary: processing and extraction the mini cannot do yet. A food photo or a scanned PDF, a barcode lookup, a research panel, a live quote. That is an API call for a job, not a standing copy of the household. When the call is done, the record that remains is here.

## How it runs

The baseline this was built for is a Mac mini: local Ollama, SQLite, a Signal interface via `signal-cli`, optional API calls for the jobs the mini cannot do. `just setup`, fill `.env` and `config.yaml`, start the signal-cli daemon, then `just run`. How each command behaves is in [`docs/README.md`](docs/README.md). How the process is shaped is in [`ARCHITECTURE.md`](ARCHITECTURE.md).

## If you want to contribute

Two things this project will not do:

1. **Give up privacy.** Historical data stays local. Mail, notes, and RAG stay on the machine. PRs do not include live household data, real `config.yaml`, or `.env`.
2. **Hand the wheel to AI.** Targeted jobs, not autonomy that cannot tell when to stop or when to ask. Tracking and suggestions — not a stand-in for an advisor, a doctor, a coach, or a human review of what ships.

Everything else is open. Talk it through on GitHub issues. Pull requests are welcome. Harden what already ships if you can (`STRENGTHEN.md`, `just test`, `ARCHITECTURE.md`); fork and tweak workflows if your household needs something different.
