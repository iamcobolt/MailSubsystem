# Getting Started

> **WARNING: This is a hobbyist project. Use at your own risk.**
>
> MailSubsystem is experimental software provided as-is with no warranties of any kind. It connects to your email via IMAP and can **move, refile, and modify the folder structure** of your mailbox. While it only acts when you explicitly run commands or enable core-owned work that applies filing changes, mistakes can happen — emails could be misfiled, moved to unexpected folders, or become harder to find.
>
> - **Do not use on business, corporate, or mission-critical email accounts.** The authors strongly discourage use on any mailbox where email loss or misorganization could have professional, legal, or financial consequences.
> - **This project makes no security guarantees.** Email content is stored in a local PostgreSQL database. **Cloud AI providers** (Google Gemini, OpenAI, Anthropic) transmit email content to their APIs for analysis — be aware of your provider's data handling and retention policies. **Local AI providers** (LM Studio, Ollama) keep all data on your machine.
> - **The authors accept no responsibility** for any data loss, email misorganization, service disruption, API charges, or other issues arising from use of this software.
> - **Always start with `--dry-run`** before applying any filing changes, and **use sandbox mode** for your first run.
>
> By using this software, you acknowledge these risks and accept full responsibility for its use in your environment.

This guide walks you through trying MailSubsystem for the first time. By the end, you'll have synced email, seen AI classify it, and previewed how it would organize your inbox. Total time: ~15 minutes.

There are two ways to try MailSubsystem:

- **[Sandbox mode](#sandbox-mode-try-without-connecting-your-email)** — Use a local test IMAP server with your own exported emails. No risk to your real mailbox. Great for a first look.
- **[Live mode](#live-mode-connect-your-real-email)** — Connect directly to your email provider (iCloud, Gmail, etc.) for the real experience.

> **Contributor note:** The sandbox is the recommended quickstart because it
> gives new contributors PostgreSQL, Dovecot, and trusted local IMAP TLS without
> touching a real mailbox. The companion
> [`mailsubsystem-dev-env`](https://github.com/iamcobolt/mailsubsystem-dev-env)
> repository owns Docker Compose, certificate setup, seeded mail, and local
> service wiring. This guide remains the canonical entry point, and the core repo
> still builds, lints, and documents the required IMAP/PostgreSQL environment
> independently.

## Prerequisites

- [Rust](https://rustup.rs/) (1.70+)
- [Docker Desktop](https://docs.docker.com/get-docker/)
- An AI API key or local LLM endpoint
- For live mode: an IMAP email account (iCloud, Gmail, Outlook, Fastmail, etc.)

---

## Sandbox mode: try without connecting your email

The sandbox spins up a local Dovecot IMAP server alongside PostgreSQL. You import your own exported `.eml` files or `.mbox` archives, and MailSubsystem processes them exactly as it would with a live mailbox — but nothing touches your real email.

### Step 1: Clone and start the sandbox

```bash
git clone https://github.com/iamcobolt/MailSubsystem.git
git clone https://github.com/iamcobolt/mailsubsystem-dev-env.git
cd mailsubsystem-dev-env
make start
make core-env
```

This starts PostgreSQL + a local IMAP server, creates local IMAP TLS
certificates, and writes `../MailSubsystem/.env` pre-configured for the sandbox.
Strict certificate validation works out of the box because the generated core
`.env` points `IMAP_TLS_CA_CERT_FILE` at the sandbox CA.

### Step 2: Export some emails

You need to supply your own emails. Export a batch from your email client:

| Client | How to export |
|--------|--------------|
| **Apple Mail** | Select emails, drag to a Finder folder (creates `.eml` files) |
| **Thunderbird** | Select emails > right-click > **Save As** > choose a folder |
| **Gmail** | [Google Takeout](https://takeout.google.com) > select **Mail** > `.mbox` format |
| **Outlook** | Select email > **File** > **Save As** > `.eml` format |
| **Fastmail** | Select emails > **More** > **Download as .eml** |
| **mutt/neomutt** | Save message with `s` or pipe with `\| cat > email.eml` |

Put the exported files in a folder (e.g. `~/test-emails/`).

### Step 3: Import into the sandbox

```bash
# Import a folder of .eml files
make import EMAILS=~/test-emails/

# Or import a Gmail Takeout .mbox file
make import EMAILS=~/Takeout/Mail/All\ mail.mbox

# Import into a specific folder
python3 scripts/import-emails.py --folder "Work" ~/work-emails/

# Preview without importing
python3 scripts/import-emails.py --dry-run ~/test-emails/
```

Expected output:

```
Connecting to localhost:1143 as testuser...
Found 47 .eml file(s)

Imported 47 email(s) into INBOX

Next steps:
  cd ../MailSubsystem
  make check              # verify connectivity
  make app                # start the server app (core coordinator + local API)
  make tui                # in another terminal, open the terminal UI
  make core-status        # inspect queue and pipeline state
```

### Step 4: Set up your AI provider

Edit `../MailSubsystem/.env` and uncomment one AI provider with your API key (see [Step 3 in live mode](#step-3-get-an-ai-api-key) for details on getting a key):

```bash
nano ../MailSubsystem/.env    # or vim, code, etc.
```

```env
ANALYSIS_MODEL=gemini/gemini-2.0-flash
ANALYSIS_API_KEY=your-key-here
EMBEDDING_MODEL=gemini/gemini-embedding-001
EMBEDDING_API_KEY=your-key-here
```

### Step 5: Run the app

```bash
cd ../MailSubsystem
make check                # verify DB + IMAP connectivity
make app                  # start the server app (core coordinator + local API)
```

`make app` is the canonical way to run MailSubsystem locally. It starts the durable work coordinator together with the local HTTP API on `127.0.0.1:3100`, ready for the TUI or local wrappers to connect. Console output stays quiet (warnings and errors only); detailed `info`-level logs are written to `logs/app.log` for later inspection. If you only want the work coordinator without the HTTP API, `make core` is still available.

The local API is loopback-only by default. If you need to reach it from another trusted device, bind it to a Tailscale address and set `API_AUTH_TOKEN`; public, wildcard, and normal LAN binds are rejected. The TUI automatically sends `API_AUTH_TOKEN` when it is present in the environment.

In another terminal, inspect what the app is doing:

```bash
make core-status
```

Core owns the normal local work queue. It keeps a lightweight background incremental sync enabled by default, so new messages can be imported while longer AI work is still running. Downstream work is backlog-driven: core only queues analysis, embedding, location, and filing-preview work when the database snapshot says that work is needed, and it coalesces active work so the same backlog is not processed twice. Filing remains conservative: IMAP moves are not applied by core unless `CORE_FILE_APPLY=true` is explicitly set. Use `make core-apply` when you want the continuous runtime to apply eligible folder recommendations under core filing policy, or `CORE_FILE_APPLY=true make app` when you want the same behavior with the API attached. To preview and apply moves manually, use `make file-dry-run` first, then `make file` only when you're satisfied. Core takes a per-account runtime lease, gives active work a short shutdown grace window (`CORE_SHUTDOWN_GRACE_SECS`, default 30), releases only the current worker's claimed queue rows if shutdown has to abort a task, and recovers orphaned processing rows when it starts without another live owner.

Mail Assistant sub-agents also build a bounded skill memory over time. Worker outputs may include reusable `skill_lessons`; core validates them, stores accepted lessons as candidates with provenance, and promotes only repeated safe/general lessons into future runs as hints. Lessons cannot mutate mail, store one-off mailbox preferences, or override filing safety.

Durable classification and threat policy lives in `specs/skills/`. Agent specs,
worker specs, and the Rust analysis pipeline all use the same shared
`email-taxonomy` and `threat-detection` guidance; Rust remains responsible for
assembling evidence, validating/repairing structured output, and applying core
safety boundaries. See [Agent System](AGENT_SYSTEM.md) for the full layout.

### Sandbox management

```bash
cd ../mailsubsystem-dev-env
make stop                 # stop (preserves imported emails)
make start                # restart
make reset                # wipe everything and start fresh
```

When you're ready to use MailSubsystem with your real email, switch to live mode below.

---

## Live mode: connect your real email

### Step 1: Clone and bootstrap

```bash
git clone https://github.com/iamcobolt/MailSubsystem.git
cd MailSubsystem
cp .env.example .env
make build
```

Edit `.env` with your IMAP credentials, PostgreSQL `DATABASE_URL`, and AI provider key. If you want local PostgreSQL without managing it yourself, use the companion [`mailsubsystem-dev-env`](https://github.com/iamcobolt/mailsubsystem-dev-env) repo, then replace the sandbox IMAP settings with your live mailbox credentials.

MailSubsystem initializes an empty database by default, but it does not silently
upgrade an existing schema. If startup reports that the schema is stale, review
`schema.sql` and run:

```bash
make migrate-schema
```

For development-only environments where automatic startup migrations are
acceptable, set `MAILSUBSYSTEM_SCHEMA_MODE=auto`.

### Step 2: Get your IMAP credentials

MailSubsystem connects to your email via IMAP. Most providers require an **app-specific password** (not your regular login password).

### iCloud Mail

1. Go to [appleid.apple.com](https://appleid.apple.com) and sign in
2. Navigate to **Sign-In and Security** > **App-Specific Passwords**
3. Click **Generate an app-specific password**, name it "MailSubsystem"
4. Copy the generated password (format: `xxxx-xxxx-xxxx-xxxx`)

```env
ICLOUD_IMAP_SERVER=imap.mail.me.com:993
ICLOUD_USERNAME=you@icloud.com
ICLOUD_PASSWORD=xxxx-xxxx-xxxx-xxxx
```

### Gmail

1. Enable 2-Factor Authentication on your Google account if not already enabled
2. Go to [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords)
3. Select **Mail** and your device, then click **Generate**
4. Copy the 16-character password

```env
IMAP_SERVER=imap.gmail.com:993
IMAP_USERNAME=you@gmail.com
IMAP_PASSWORD=abcdefghijklmnop
```

> **Note:** Gmail uses `IMAP_*` variables instead of `ICLOUD_*`. Both work — MailSubsystem checks for either.

### Outlook / Hotmail

1. Enable 2-Factor Authentication at [account.microsoft.com/security](https://account.microsoft.com/security)
2. Go to **Security** > **Advanced security options** > **App passwords**
3. Create a new app password

```env
IMAP_SERVER=outlook.office365.com:993
IMAP_USERNAME=you@outlook.com
IMAP_PASSWORD=your-app-password
```

### Fastmail

1. Go to **Settings** > **Privacy & Security** > **Integrations** > **App passwords**
2. Create a new password with IMAP access

```env
IMAP_SERVER=imap.fastmail.com:993
IMAP_USERNAME=you@fastmail.com
IMAP_PASSWORD=your-app-password
```

### Other providers

Any standard IMAP server works. You need:
- The IMAP hostname and port (usually 993 for SSL)
- Your email address
- An app-specific password (or regular password if app passwords aren't required)

Set `IMAP_SERVER`, `IMAP_USERNAME`, and `IMAP_PASSWORD` in your `.env`.

MailSubsystem validates IMAP TLS certificates by default. If a private or
self-hosted server uses an internal certificate authority, set
`IMAP_TLS_CA_CERT_FILE=/path/to/private-ca-bundle.pem`. If you connect through
an IP address or alias but the certificate is issued for another DNS name, set
`IMAP_TLS_SERVER_NAME=mail.example.com`.

### Step 3: Get an AI API key

MailSubsystem uses an LLM to classify and summarize your emails. You need at least one API key.

### Recommended first provider: Google Gemini

1. Go to [aistudio.google.com/apikey](https://aistudio.google.com/apikey)
2. Click **Create API Key** and select a project (or create one)
3. Copy the key

```env
ANALYSIS_MODEL=gemini/gemini-2.0-flash
ANALYSIS_API_KEY=your-key-here
EMBEDDING_MODEL=gemini/gemini-embedding-001
EMBEDDING_API_KEY=your-key-here
```

Check Google's current pricing and rate limits before running large batches.

### Alternative: OpenAI

1. Go to [platform.openai.com/api-keys](https://platform.openai.com/api-keys)
2. Create a new secret key
3. Review current pricing and billing requirements

```env
ANALYSIS_MODEL=openai/gpt-4o
ANALYSIS_API_KEY=sk-your-key-here
```

### Alternative: Codex subscription

If you have access to Codex through your ChatGPT plan and want to avoid using an
OpenAI API key for MailSubsystem LLM calls, sign in to the local Codex CLI first:

```bash
codex login
```

MailSubsystem checks `codex login status` before each Codex-backed request. If
the CLI is not logged in, it will stop with instructions to run `codex login` or
`codex login --device-auth` for headless environments.

Then configure MailSubsystem to use the Codex CLI provider:

```env
ANALYSIS_MODEL=codex/your-codex-model
# CODEX_BIN=codex        # optional; PATH is used by default
CODEX_SANDBOX=read-only
CODEX_TIMEOUT_SECS=300
```

For hybrid local/frontier routing, use your local model for routine work and Codex
as the frontier provider:

```env
AI_PROVIDER=hybrid
LOCAL_LLM_URL=http://localhost:1234/v1
LOCAL_LLM_MODEL=your-model-name
ANALYSIS_MODEL=codex/your-codex-model
```

This adapter runs `codex exec` as a subprocess using your signed-in Codex client.
It does not require `OPENAI_API_KEY`, but it is slower than direct API calls and
counts against your Codex plan limits or Codex credits.
`ANALYSIS_MODEL` is passed to `codex exec --model`; `CODEX_BIN` is optional
because the daemon searches `PATH` for `codex` by default.

### Alternative: Anthropic (Claude)

1. Go to [console.anthropic.com/settings/keys](https://console.anthropic.com/settings/keys)
2. Create a new API key
3. Add billing ($5 minimum credit)

```env
ANALYSIS_MODEL=anthropic/claude-3-5-sonnet-20241022
ANALYSIS_API_KEY=sk-ant-your-key-here
```

### Alternative: Local LLM (free, no API key)

If you have [LM Studio](https://lmstudio.ai/) or [Ollama](https://ollama.ai/) running locally:

```env
ANALYSIS_MODEL=lmstudio/your-model-name
LOCAL_LLM_URL=http://localhost:1234/v1
LOCAL_LLM_CONCURRENCY=2
```

No API key needed, but quality depends on your model and hardware. Local-only
analysis and location recommendations default to two concurrent LLM requests;
increase `LOCAL_LLM_CONCURRENCY`, or set `ANALYZE_CONCURRENCY` /
`LOCATE_CONCURRENCY` separately, if your local server and GPU can handle more
parallel requests.

If you use a local model for analysis but want frontier embeddings for RAG,
set `EMBEDDING_MODEL=gemini/gemini-embedding-001` and configure
`EMBEDDING_API_KEY`. For local embeddings, set `EMBEDDING_MODEL` to a
provider/model ref for an embedding model installed on your local model server.

### Cost awareness

Each email analysis is usually one LLM API call. Costs and rate limits change, so check your provider's current pricing before processing a large mailbox.

| Provider path | Cost note |
|----------|-------------|
| Gemini, OpenAI, Anthropic | Provider-billed; review pricing and quotas first |
| Local LLM | No API bill, but limited by your hardware |

For a first live run, sync a small window such as one week of email and review results before increasing batch sizes.

Spend confirmation, panic-disable, and approval audit policy are tracked in
[Spend Safety](SPEND_SAFETY.md). The current foundation is audit-only until the
CLI, TUI, API, and runtime provider call sites are wired to enforce approvals.

### Step 4: Configure and verify

Edit your `.env` with the credentials from steps 2 and 3:

```bash
# Use your editor of choice
nano .env      # or vim, code, etc.
```

Then verify everything connects:

```bash
make check
```

Expected output:

```
[+] Database: connected (tables: 8, emails: true, imap_folders: true)
[+] IMAP: connected and authenticated (imap.mail.me.com:993)

All checks passed.
```

If IMAP fails, double-check your password is an **app-specific password**, not your regular login.

Optionally, test your AI provider:

```bash
make test-llm-frontier
```

Expected output:

```
test-llm (frontier): provider created, sending one completion...
response: OK
finish_reason: stop
```

### Optional: run the local API and TUI

MailSubsystem includes a local HTTP API for wrappers and the TUI. If you want to
keep the API running for TUI testing, use:

```bash
make app
# then in another terminal
make tui
```

`make app` starts the core coordinator and API together. It does not enable automatic filing unless you run `CORE_FILE_APPLY=true make app`.

The API bind is intentionally narrow for release safety:

- Default bind: `127.0.0.1:3100`.
- Allowed remote bind: Tailscale addresses only, such as `100.64.1.2:3100`.
- Required for Tailscale binds: `API_AUTH_TOKEN`.
- Rejected binds: public, wildcard, and normal LAN addresses.

When `API_AUTH_TOKEN` is set, HTTP clients should send `Authorization: Bearer <token>` or `X-API-Token: <token>`. `make tui` reads the same environment and sends the token automatically.

If you prefer separate processes, use:

```bash
make core
make api
make tui
```

`make api` does not run the core coordinator by itself.

If the companion dev environment cannot bind to `15432` because another local service is already using it, set `MAILSUBSYSTEM_DB_PORT` in `../mailsubsystem-dev-env/.env`, restart that environment, and update `DATABASE_URL` in this repo's `.env` to match:

```bash
make -C ../mailsubsystem-dev-env reset
```

### Step 5: First run with the app

For normal local use, start the server app:

```bash
make app
```

`make app` starts the durable work coordinator and binds the local HTTP API on `127.0.0.1:3100`. On a fresh database it enqueues sync work, then follow-up analysis, embedding, location, and filing-preview work. Agents and manual commands can also enqueue missing prerequisites into the same queue, so local tooling observes the same runtime state. Console output is intentionally quiet — only warnings and errors are printed — and detailed logs land in `logs/app.log` (override with `LOG_DIR`, `LOG_FILE_PATH`, `LOG_CONSOLE_FILTER`, or `LOG_FILE_FILTER`).

If you override `API_BIND`, keep it on loopback unless you are using a Tailscale address with `API_AUTH_TOKEN`. Treat that token like a mailbox credential because the API can expose mailbox data to authorized clients.

If you don't need the HTTP API, run `make core` instead — it runs the same coordinator without binding a port.

In another terminal:

```bash
make core-status
```

`core-status` shows whether core is idle, syncing, analyzing, locating, queued, or in an error state, plus queue depth, active work, recent failures, and pipeline timestamps.

### Manual debugging path

The older step-by-step pipeline commands are still useful when you want to isolate one stage. For a small first sync, use the last 7 days instead of your entire mailbox:


```bash
./target/release/mailsubsystem sync-window --days 7
```

Expected output:

```
Syncing with window: last 7 days (since 2026-03-25)
  INBOX: 45 envelopes synced
  Sent Messages: 12 envelopes synced
  ...
Body sync queue depth: pending=38 failed=0 processing=0 dead=0
Slow sync: 38 full messages fetched.
```

Check what was synced:

```bash
./target/release/mailsubsystem status
```

### Manual step: Analyze a few emails

Start with a small batch:

```bash
./target/release/mailsubsystem analyze
```

Expected output:

```
Analyzing 10 emails (limit 10)
Analyzed: <message-id-1@mail.example.com>
  analyzed_by: provider-model
  tokens: in=1234 out=456 total=1690
  spam=not-spam phishing=not-phishing marketing=not-marketing otp=not_otp category=personal org=None type=conversation
  summary: Discussion about weekend plans with family...
Analyzed: <message-id-2@mail.example.com>
...
```

To see the full analysis for any email:

```bash
./target/release/mailsubsystem show "<message-id>"
```

### Manual step: Preview folder recommendations

```bash
./target/release/mailsubsystem locate
```

Expected output:

```
Locating 10 emails (limit 10)
Located: <message-id-1@mail.example.com> -> Personal/Family
Located: <message-id-2@mail.example.com> -> Financial/Banking
...
```

Preview what moves would happen (nothing is actually moved):

```bash
make file-dry-run
```

Expected output:

```
[dry-run] Would move <message-id-1> from INBOX to Personal/Family
[dry-run] Would move <message-id-2> from INBOX to Financial/Banking
...
```

### Manual step: Apply (when you're ready)

Once you're happy with the recommendations:

```bash
make file
```

This creates any missing folders and moves emails via IMAP. The changes appear in your email client immediately.

## Running continuously

Once you've verified everything works, keep the server app running to process work continuously and serve the local API:

```bash
make app
```

If you don't need the HTTP API, `make core` runs the same coordinator without binding a port.

To let core automatically apply recommended moves after previewing them:

```bash
make core-apply
# or, with the API enabled too:
CORE_FILE_APPLY=true make app
```

This uses `CORE_FILE_APPLY=true` for that process and moves only emails that already have location recommendations.

The foreground process (Ctrl+C to stop) starts:
- The durable core work coordinator
- The local HTTP API (with `make app`)
- A shared queue for sync, body sync, analysis, embedding, location, and filing-preview work

Start `make api` separately only when you need the local HTTP API without the core coordinator.

Timers, agents, and manual commands enqueue work into core rather than acting like separate runtime systems.

## Troubleshooting

### "IMAP: connection/auth failed"

- **iCloud:** Make sure you're using an [app-specific password](https://support.apple.com/en-us/102654), not your Apple ID password
- **Gmail:** Ensure [IMAP is enabled](https://support.google.com/mail/answer/7126229) in Gmail settings and you're using an app password
- **Firewall:** Port 993 (IMAPS) must be open for outbound connections
- **Server address:** Double-check the hostname and port — `host:993` format

### "Database: connection failed"

- Confirm `DATABASE_URL` in `.env` points at a reachable PostgreSQL database.
- If you use the companion dev environment, `docker ps` should show `mailsubsystem-dev-db`.
- Restart the companion database: `make dev-env-start`
- Reset the companion database from scratch: `make dev-env-reset`

### "Database schema is not current"

- Review `schema.sql`.
- Apply the embedded schema intentionally: `make migrate-schema`.
- Use `MAILSUBSYSTEM_SCHEMA_MODE=auto` only for development databases where startup migrations are acceptable.

### "Failed to create AI provider" or empty analysis results

- Verify your API key: `make test-llm-frontier`
- Check the key is uncommented in `.env` (no leading `#`)
- If you hit provider pressure, wait and retry; the daemon backs off automatically.

### "429 Too Many Requests" or "RESOURCE_EXHAUSTED"

You've hit your API provider's rate limit. Options:
- Wait and retry; the adaptive provider-pressure controller backs off and recovers.
- Use `API_RATE_LIMIT_RPM` only as a conservative initial guardrail.
- Switch to a local LLM for bulk processing

### Analysis is slow

- Local LLMs are slower but free. For faster results, use a frontier provider (Gemini Flash is fast and cheap)
- The first sync of a large mailbox takes time — subsequent incremental syncs are fast
- Body sync fetches full email content; this is the slowest part of initial sync

### Console is quiet — where do the logs go?

By default, `make app` (and other `mailsubsystem` commands) only print warnings and errors to the console. Detailed `info`-level logs are written to `logs/<command>.log` (for example `logs/app.log` or `logs/core.log`).

- Tail the file: `tail -f logs/app.log`
- Make the console louder: `LOG_CONSOLE_FILTER=info make app`
- Change the file destination: `LOG_FILE_PATH=/tmp/mail.log make app`
- Disable file logging entirely: `LOG_FILE_PATH=off make app`
- Tune per-target verbosity in either stream: `LOG_FILE_FILTER=debug,sqlx=warn make app`

### "pgvector" or "vector extension" errors

The companion dev environment includes pgvector automatically. If you're running PostgreSQL manually:
- **macOS:** `brew install pgvector` then restart PostgreSQL
- **Linux:** Follow [pgvector install instructions](https://github.com/pgvector/pgvector#installation) for your distro
- Then run: `psql -d mailsubsystem -c "CREATE EXTENSION IF NOT EXISTS vector;"`

### I want to start over

```bash
make dev-env-reset  # wipes the companion sandbox database and reinitializes schema
make app            # core queues sync/rebuild work from the empty database
make core-status    # watch queue depth, active work, and errors
```

Your email is untouched by the database reset. MailSubsystem reads via IMAP and only moves messages when filing is explicitly applied (`make file`) or when you deliberately enable core filing apply with `make core-apply` / `CORE_FILE_APPLY=true`.

## What's next

- **Tune the AI:** Try `AI_PROVIDER=hybrid` with a local LLM for cheap bulk analysis + frontier for hard cases
- **Embeddings:** Run `./target/release/mailsubsystem embed-backfill` to enable semantic search (RAG) for better analysis context
- **Custom folders:** The location agent creates folders based on your email patterns — review and adjust by running `locate` + `file --dry-run` before applying
- **Multiple accounts:** Create an `accounts.toml` to manage multiple mailboxes (see `.env.example` for the format)
