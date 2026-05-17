# MailSubsystem Docs

This directory is organized around how people approach the project, not around
every internal folder. Start here when you need the right document quickly.

## Read By Task

| Goal | Document |
| --- | --- |
| Try MailSubsystem safely | [Getting Started](getting-started.md) |
| Understand runtime architecture | [Architecture](ARCHITECTURE.md) |
| Understand agents, workers, and skills | [Agent System](AGENT_SYSTEM.md) |
| Inspect database ownership and schema posture | [Database Guide](DATABASE.md) |
| Review exact database DDL | [`../schema.sql`](../schema.sql) |
| Review release notes | [`../CHANGELOG.md`](../CHANGELOG.md) |

## Repository Boundaries

MailSubsystem Core is the Rust application: CLI, sync, analysis, local API,
terminal UI, durable queue, database access, and mailbox safety boundaries.

The companion
[`mailsubsystem-dev-env`](https://github.com/iamcobolt/mailsubsystem-dev-env)
repository owns local Docker/PostgreSQL/Dovecot setup, trusted sandbox
certificates, and exported-mail import helpers.

## Runtime Specs And Policy Layout

Durable prompt policy lives in [`../specs/skills`](../specs/skills/). Agents and workers compose
those skills instead of copying local variants:

- [`../specs/agents`](../specs/agents/) contains runtime agent specs.
- [`../specs/workers`](../specs/workers/) contains ephemeral internal worker specs.
- [`../specs/skills`](../specs/skills/) contains shared policy and prompt guidance.

Rust code assembles evidence, selects providers, validates and repairs
structured output, persists results, and owns mailbox mutation safety.

## Maintenance Rules

- Prefer updating an existing guide over adding another top-level document.
- Do not duplicate exact database DDL outside [`../schema.sql`](../schema.sql).
- Put durable taxonomy, threat, filing, digest, and skill-memory policy in
  [`../specs/skills`](../specs/skills/), not in agent, worker, or Rust-local prompt copies.
- Keep quickstart instructions in [Getting Started](getting-started.md); keep
  architecture explanations in [Architecture](ARCHITECTURE.md).
