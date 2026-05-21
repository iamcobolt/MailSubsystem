# MailSubsystem

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](Cargo.toml)

MailSubsystem is a local-first AI email automation runtime for IMAP mailboxes. It syncs email into PostgreSQL, classifies messages with LLMs, detects spam and phishing signals, recommends folders, generates digests, and can apply inbox-zero style filing with IMAP MOVE.

It is built for people who want a hackable Rust mail agent instead of a hosted inbox product: CLI first, local database first, a durable `core` work coordinator, optional local API, and optional TUI.

> **Experimental hobbyist software.** MailSubsystem can move and refile real email. Start in sandbox mode, use dry runs, and do not use it on business, corporate, legal, financial, or mission-critical mailboxes unless you are prepared to audit and operate it yourself. Cloud LLM providers receive email content when enabled. See [Security](SECURITY.md) and [Getting Started](docs/getting-started.md).

Release notes live in [CHANGELOG.md](CHANGELOG.md).

## What It Does

- **IMAP email sync**: imports folders, headers, flags, bodies, and message metadata into PostgreSQL.
- **AI email analysis**: classifies spam, phishing, marketing, category, topic, organization, one-time passwords, and summaries.
- **Inbox-zero filing**: recommends target folders and applies moves through IMAP after preview.
- **Local-first storage**: keeps the working dataset in your own PostgreSQL database with pgvector support for embeddings and RAG.
- **Mail Assistant multi-agent runtime**: one user-facing assistant creates scoped ephemeral sub-agents for classification, filing recommendations, digests, learning, and conflict review.
- **Core runtime**: coordinates sync, analysis, embedding, location, preview, and optional filing work through a durable local queue.
- **Safe test path**: uses the companion `mailsubsystem-dev-env` sandbox so you can test with exported `.eml` or `.mbox` files before touching a live mailbox.

Common search terms for this project: AI email assistant, local-first email automation, IMAP automation, inbox zero, LLM email classifier, phishing detection, Rust mail agent, PostgreSQL email archive, pgvector email search.

## Agent Model

Mail Assistant is the conversational front door. Internal sub-agents are short-lived workers with scoped skill bundles; they write structured artifacts and never speak directly to the user or mutate IMAP. Core remains the safety boundary for filing, folder creation, junk/trash actions, and anti-ping-pong policy.

Sub-agent skills can improve over time through bounded skill memory: reusable lessons are stored in PostgreSQL by skill bundle and injected into later worker runs as hints. New lessons start as candidates, carry provenance back to the worker run that produced them, and only repeated safe/general lessons become active. Lessons cannot mutate mail, rewrite prompts, install tools, or override core safety policy.

Durable analysis policy lives in `specs/skills/`, not in one-off Rust prompt
strings. The email analyzer, classification workers, and Rust analysis pipeline
share `email-taxonomy` and `threat-detection` guidance; Rust assembles evidence,
validates/repairs structured output, and persists results.

The runtime includes a quiet Mail Assistant heartbeat that checks mailbox completeness, stale work, user folder moves, and learning opportunities. User folder moves pin that message and become preference evidence so automation does not bounce the same email between folders.

## Pipeline

```text
IMAP mailbox
  -> sync envelopes, flags, folders, and bodies
  -> store in PostgreSQL
  -> analyze with Gemini, OpenAI, Anthropic, LM Studio, or Ollama
  -> generate embeddings and retrieve related context
  -> recommend folders
  -> preview or apply IMAP moves
```

The manual debugging flow is:

```text
sync -> analyze -> embed-backfill -> locate -> file --dry-run -> file
```

For normal local use, prefer `make core`. It runs the durable coordinator and keeps the pipeline moving as work appears. Use `make app` when you also want the local API available for the TUI or wrappers.

## Quick Start

Prerequisites:

- Rust 1.70+
- Docker or another PostgreSQL instance with pgvector
- An IMAP mailbox or exported mail for sandbox testing
- At least one LLM provider, unless you only want to sync and inspect data

### Sandbox Mode

Use this first. The companion dev-environment repo starts PostgreSQL plus a local Dovecot IMAP server and never touches your real mailbox.

```bash
git clone https://github.com/iamcobolt/MailSubsystem.git
git clone https://github.com/iamcobolt/mailsubsystem-dev-env.git
cd mailsubsystem-dev-env
make start
make import EMAILS=~/path/to/exported-mail/
make core-env
```

`make start` creates local IMAP TLS certificates and configures the generated
core `.env` to trust them, so the sandbox uses the same strict TLS path as a
real mailbox.

Edit `../MailSubsystem/.env` and set an AI provider:

```env
AI_PROVIDER=gemini
GEMINI_API_KEY=your-key-here
```

Then run core safely:

```bash
cd ../MailSubsystem
make check
make core
```

For an interactive local stack with core and API together:

```bash
make app
```

For manual inspection, you can still run:

```bash
make sync
make analyze
make locate
make file-dry-run
```

When the preview looks right, `make file` applies moves inside the sandbox. See the [`mailsubsystem-dev-env`](https://github.com/iamcobolt/mailsubsystem-dev-env) README for sandbox management commands.

### Live Mailbox

```bash
git clone https://github.com/iamcobolt/MailSubsystem.git
cd MailSubsystem
cp .env.example .env
make build
```

Edit `.env` with your IMAP app password, PostgreSQL `DATABASE_URL`, and AI provider. Then:

```bash
make check
make core
```

Use `make file-dry-run` and review recommendations before enabling automatic filing.

## Runtime Modes

| Mode | Command | Purpose |
| --- | --- | --- |
| Core coordinator | `make core` | Runs durable background work without starting the HTTP API. |
| Core with filing | `make core-apply` | Runs core with eligible IMAP filing moves enabled. |
| Core status | `make core-status` | Shows queue depth, active work, pipeline timestamps, and recent errors. |
| Server app | `make app` | Starts core plus the local API together. Use `CORE_FILE_APPLY=true make app` to enable filing moves. |
| CLI pipeline | `make pipeline` | Runs sync, analysis, embeddings, location, and filing once. |
| Dry-run filing | `make file-dry-run` | Shows planned IMAP moves without changing the mailbox. |
| Local API | `make api` | Starts the HTTP API on `127.0.0.1:3100` for wrappers and clients. |
| TUI | `make tui` | Opens the terminal chat UI against the local API. |

The API binds to `127.0.0.1:3100` by default. Non-loopback binds are rejected unless the address is in the Tailscale range; Tailscale binds also require `API_AUTH_TOKEN`. The TUI sends that token automatically when it is set.

## Companion Database Port

If `15432` is already in use on your machine, set `MAILSUBSYSTEM_DB_PORT` in `../mailsubsystem-dev-env/.env`, restart that environment, and update `DATABASE_URL` in this repo's `.env` to match:

```bash
make -C ../mailsubsystem-dev-env reset
```

## Configuration

Configuration comes from `.env` or environment variables. Start from [.env.example](.env.example).

Required for live mailbox processing:

- `IMAP_SERVER`
- `IMAP_USERNAME`
- `IMAP_PASSWORD`
- `DATABASE_URL`
- One AI provider key or local LLM endpoint for analysis

Schema posture is conservative for release builds. By default,
`MAILSUBSYSTEM_SCHEMA_MODE=bootstrap` initializes an empty database, but it
refuses to silently mutate an existing or stale schema. For upgrades, review
`schema.sql` and run:

```bash
make migrate-schema
```

Set `MAILSUBSYSTEM_SCHEMA_MODE=auto` only when you intentionally want startup to
apply embedded schema changes automatically. Use `validate`/`manual`/`off` to
disable schema creation and migration entirely.

IMAP TLS certificate validation is strict by default. For private/self-hosted
servers, set `IMAP_TLS_CA_CERT_FILE` to a PEM CA bundle. If `IMAP_SERVER`
connects through an IP or alias but the certificate is issued for a different
DNS name, set `IMAP_TLS_SERVER_NAME` to that certificate name.

Supported LLM paths:

- Google Gemini
- OpenAI
- Anthropic
- Local OpenAI-compatible servers such as LM Studio or Ollama
- Hybrid local plus frontier escalation

Core keeps one local work coordinator active for sync, analysis, embedding,
folder recommendations, previews, and optional filing. By default it also runs a
lightweight background incremental sync (`CORE_BACKGROUND_SYNC=true`) so new
mail can enter the database while slower AI work continues. On shutdown, core
waits briefly (`CORE_SHUTDOWN_GRACE_SECS`, default 30) so claimed work can mark
itself complete or retryable instead of leaving stale `processing` rows.

Filing recommendations are normalized before IMAP moves are attempted. Core
rewrites container-only targets such as `Personal` to selectable leaves such as
`Personal/General`, and sanitizes Dovecot-hostile folder characters in generated
recommendations.

## Commands

Run `make help` for Make targets or `./target/release/mailsubsystem help` for CLI usage.

| Command | Description |
| --- | --- |
| `check` | Verify database and IMAP connectivity. |
| `sync` | Sync folders, envelopes, flags, and message bodies. |
| `sync-window --days N` | Sync a bounded date window for safer first runs. |
| `sync-incremental` | Sync changes using IMAP incremental state. |
| `status` | Show folder and email table state. |
| `analyze [--force] [message_id]` | Run AI classification and summaries. |
| `embed-backfill [--limit N]` | Generate embeddings for semantic retrieval. |
| `locate [--force] [message_id]` | Recommend destination folders. |
| `file [--dry-run]` | Preview or apply IMAP folder moves. |
| `core` | Normal local runtime: durable work coordinator, no HTTP API. |
| `core-apply` | Run core and allow eligible IMAP filing moves. |
| `core-status` | Show queue, active work, pipeline timestamps, and errors. |
| `app` | Start core plus the local API together. |
| `api` | Start the local HTTP API without the core coordinator. |
| `tui` | Start the terminal UI. |
| `digest --daily` / `--weekly` | Generate inbox activity digests. |
| `migrate-schema [--apply]` | Validate or intentionally apply the embedded database schema. |
| `consolidate [--apply]` | Propose or apply redundant folder consolidation. |
| `lifecycle-cleanup --dry-run` | Preview cleanup of expired OTP and stale newsletter mail. |
| `agent ...` | Run and inspect agent harness workflows. |

## Project Structure

```text
MailSubsystem/
  src/                  Rust CLI, IMAP sync, API, TUI, harness, and pipeline code
  docs/                 Documentation hub and release-facing guides
  specs/agents/          Runtime agent specs consumed by the harness
  specs/workers/         Ephemeral worker specs consumed by the sub-agent runtime
  specs/skills/          Shared policy guidance composed into specs and analysis prompts
  schema.sql            Canonical PostgreSQL schema
  .env.example          Environment variable reference
  Makefile              Common local workflows
```

## Documentation

Docs by goal:

| Goal | Start Here |
| --- | --- |
| Find the right doc | [Docs Hub](docs/INDEX.md) |
| First run | [Getting Started](docs/getting-started.md) |
| Understand the system | [Architecture](docs/ARCHITECTURE.md) |
| Understand agents, workers, and skills | [Agent System](docs/AGENT_SYSTEM.md) |
| Inspect schema and stored fields | [Database Schema](docs/DATABASE.md) |
| Review executable prompt specs | [Agent System](docs/AGENT_SYSTEM.md) |
| Test without a real mailbox | [`mailsubsystem-dev-env`](https://github.com/iamcobolt/mailsubsystem-dev-env) |
| Contribute safely | [Contributing](CONTRIBUTING.md) |
| Report a vulnerability | [Security](SECURITY.md) |

Operator quick refs:

- Health check: `make check`
- Normal local runtime: `make core`
- Interactive local stack: `make app`
- Runtime with automatic filing: `make core-apply`
- Queue health: `make core-status`
- Preview moves: `make file-dry-run`
- API for wrappers: `make api`
- Terminal UI: `make tui`

## Development

```bash
cargo build
cargo test
cargo clippy
cargo fmt --check
```

## Public Repo Topics

Suggested GitHub topics:

```text
email, imap, inbox-zero, ai-email-assistant, llm, rust, postgres, pgvector, phishing-detection, local-first
```

## License

[GNU Affero General Public License v3.0](LICENSE) (AGPL-3.0-only).

## Disclaimer

THIS SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND. You are responsible for your mailbox, your data, your provider costs, and your operational safeguards. Always test with companion sandbox data and dry runs before applying changes to real email.
