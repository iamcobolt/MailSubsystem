# Contributing

Thanks for taking a look at MailSubsystem. This project handles private email, so contributions should be conservative, reproducible, and privacy-aware.

## Before You Start

- Do not commit real `.env` files, credentials, private mailbox exports, generated certificates, or production database dumps.
- Prefer the local sandbox for development and demos.
- Use anonymized or synthetic email samples in issues, tests, and screenshots.
- Open an issue or draft PR for larger behavior changes so the safety model can be discussed early.
- Avoid refactor-only PRs unless they are part of a concrete bug fix, safety improvement, or requested maintainability task.
- Avoid test-only or CI-only PRs for known failures unless the test change validates new behavior in the same PR.

## What To Open

- **Bugs and small docs fixes**: open a focused PR.
- **Mailbox mutation changes**: start with an issue or draft PR and include the safety impact.
- **New agent behavior or architecture**: start with an issue so the scope and privacy boundaries can be reviewed.
- **Questions or unclear behavior**: open an issue with the command, environment shape, and sanitized output.

## Local Setup

```bash
git clone https://github.com/iamcobolt/MailSubsystem.git
git clone https://github.com/iamcobolt/mailsubsystem-dev-env.git
cd MailSubsystem
make dev-env-start
make dev-env-core-env
make dev-env-import EMAILS=~/path/to/exported-mail/
make check
```

Set an AI provider in `.env` before running analysis commands.

## Development Checks

Run the checks that match the files you touched. For Rust changes, use:

```bash
cargo build
cargo test
cargo clippy
cargo fmt --check
```

For filing, sync, analysis, or lifecycle changes, include a sandbox or dry-run validation path when possible:

```bash
make dev-env-start
make dev-env-core-env
make check
make sync
make analyze
make locate
make file-dry-run
```

## Pull Request Guidance

- Keep changes scoped to one behavior or documentation concern.
- Include dry-run output or sandbox reproduction steps when changing filing behavior.
- Add or update tests when changing parsing, database writes, analysis contracts, or mailbox mutation logic.
- Update docs when user-facing commands, environment variables, or safety behavior changes.
- Call out any migration, destructive action, provider cost, or privacy impact in the PR description.
- Do not mix unrelated cleanup with user-visible behavior changes.
- Describe what changed and why, not just which files changed.

## Behavior Proof

Mailbox automation needs more than "tests pass." For PRs that affect sync, analysis, filing, lifecycle cleanup, API behavior, or agent decisions, include a short behavior proof in the PR body:

- Setup used, such as sandbox, synthetic E2E range, or a redacted local mailbox.
- Exact command or UI/API path you ran.
- Before/after result or dry-run output.
- Evidence that sensitive data was redacted.
- Anything important you did not test.

Unit tests, lint, typechecks, and CI are valuable, but they are not a substitute for real behavior proof when a change can move or reinterpret email.

## AI-Assisted Contributions

AI-assisted PRs are welcome. Please make review easy:

- Mark AI-assisted work in the PR description.
- Confirm you understand the resulting code or docs.
- Include human-run validation from your own setup.
- Share prompts or session notes when they would help reviewers understand the approach.
- Address or explicitly dismiss review bot feedback before asking for another review.

## Review Conversations

If a reviewer or bot leaves comments, the PR author owns the follow-through:

- Resolve conversations only after the code or explanation addresses the concern.
- Reply and leave the thread open when you need maintainer judgment.
- Do not leave addressed review conversations for maintainers to clean up.

## Current Project Priorities

The most useful contributions are currently in these areas:

- **Safety and correctness**: preventing accidental mailbox mutation and improving dry-run clarity.
- **Sync and filing reliability**: IMAP edge cases, folder handling, and recoverable failures.
- **Privacy and security**: safer defaults, redaction, local API hardening, and clearer boundaries.
- **Local operator experience**: setup docs, diagnostics, TUI/API ergonomics, and sandbox workflows.
- **Performance and cost control**: batching, rate limits, local LLM support, and provider usage visibility.

## Coding Notes

- Match the existing Rust style and module boundaries.
- Prefer structured parsing and typed data over ad hoc string manipulation.
- Keep mailbox mutations explicit and previewable.
- Avoid logging secrets, raw email bodies, or provider keys.
