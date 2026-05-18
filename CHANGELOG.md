# Changelog

## Unreleased

### Fixed

- Fixed a core sync loop where `sync_full` erased folder UID cursors before each
  backfill run, causing folders to restart from UID 1 instead of resuming.
- Fixed observed-empty mailboxes being treated as permanently incomplete, which
  repeatedly queued `sync_full` with `blank_database` even after IMAP folders had
  been discovered and reported zero messages.
- Clarified full-sync folder logs so empty and up-to-date folders are reported
  explicitly instead of appearing to restart from UID 1.

## v0.1.0 - 2026-05-17

Initial public-release candidate for MailSubsystem Core.

### Highlights

- Local-first IMAP sync, PostgreSQL storage, AI analysis, folder recommendation, digest, local API, and terminal UI workflows.
- Mail Assistant is the main user-facing chat surface; specialist agents and workers remain internal/runtime-routed.
- Runtime agent, worker, and skill specifications live under `specs/`, while `docs/` is reserved for human-facing project documentation.
- `schema.sql` is the canonical database schema, with explicit migration behavior for existing databases.
- The companion `mailsubsystem-dev-env` repository owns the local PostgreSQL + Dovecot sandbox setup.

### Release Hardening

- Redacted secret-bearing debug output for account, database, and AI-provider configuration.
- Defaulted local API binding to loopback and required token auth for non-loopback Tailscale access.
- Removed insecure IMAP TLS release paths and moved IMAP TLS to native certificate validation with optional `IMAP_TLS_CA_CERT_FILE` trust roots.
- Updated dependency posture:
  - `async-imap` now uses its Tokio runtime, removing `async-std` from the resolved runtime tree.
  - IMAP TLS no longer depends on the old `async-tls` / `rustls 0.20` / `ring 0.16` stack.
  - SQLx default features are disabled for the core Postgres-only dependency declaration.
  - `ratatui`, `crossterm`, and `rig-core` are updated to current compatible root versions.
- Added `deny.toml` for release advisory, license, and source checks.

### Architecture Cleanup

- Removed retired direct analyzer and internal evaluation surfaces; harness-backed email analysis is the supported path.
- Removed old compatibility aliases and obsolete direct-path AI tuning environment variables.
- Split email analysis into focused modules:
  - `src/email_analysis/mod.rs` for orchestration and escalation.
  - `src/email_analysis/harness_io.rs` for harness input/output conversion.
  - `src/email_analysis/result_normalization.rs` for schema normalization and DB persistence.
  - `src/email_analysis/tests.rs` for analysis pipeline tests.
- Preserved attachment analysis as an active safety capability and feeds attachment summaries into harness input.

### Validation

Release-candidate checks run locally:

- `cargo check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- `cargo deny check advisories licenses sources`
- `cargo audit --ignore RUSTSEC-2023-0071`
- `cargo outdated --root-deps-only`
- `git diff --check`

`RUSTSEC-2023-0071` is ignored for `cargo audit` because it is reported through SQLx's optional MySQL lockfile metadata. MailSubsystem is Postgres-only, and `cargo tree -i sqlx-mysql@0.8.6 --target all` reports no resolved dependency path.

### History Audit

The current tracked tree contains no `.env`, certificate/key, mailbox export,
log, database dump, or sandbox/test-env files.

Before public release, git history was backed up offline and rewritten to remove
retired sandbox/test-env, web, log, certificate/key, mailbox-export, and dump
paths. Stale remote feature branches that still referenced pre-purge history
were removed, and the public `main` branch and `v0.1.0` tag now point at the
cleaned history. The public repository history was then restarted from the
clean release tree as a single root commit.

The backup is intentionally outside the repository under the local developer
workspace and is documented in Confluence.
