# Architecture

MailSubsystem is a local-first mail automation pipeline. The application is a Rust binary with CLI commands, a durable core coordinator, an optional local HTTP API, and an optional terminal client.

## Goals

- Keep mailbox data and automation state under the operator's control.
- Make every destructive mailbox action previewable through dry runs.
- Support cloud and local LLM providers without coupling the rest of the system to one vendor.
- Keep the core useful from scripts, terminals, and wrappers rather than requiring a hosted SaaS UI.

## Data Flow

```text
IMAP provider
  -> sync commands
  -> PostgreSQL + pgvector
  -> core coordinator
  -> Mail Assistant heartbeat
  -> scoped ephemeral sub-agents
  -> folder recommendations
  -> core filing policy
  -> dry-run or approved IMAP MOVE apply
  -> optional API and TUI clients
```

## Main Components

| Component | Location | Responsibility |
| --- | --- | --- |
| CLI parser | `src/cli_parser.rs` | Maps command-line invocations to runtime commands. |
| Command handlers | `src/cli_commands/` | Implements sync, analysis, filing, API, TUI, digest, lifecycle, and maintenance commands. |
| IMAP client | `src/imap_client.rs` | Connects to IMAP, reads folders/messages, and applies mailbox moves. |
| Sync runtime | `src/mailbox_sync_runtime.rs`, `src/body_sync_service.rs` | Coordinates full, incremental, and missing-body mailbox sync. |
| Database layer | `src/database/`, `schema.sql` | Stores emails, folders, agent runs, embeddings, and derived analysis. `schema.sql` is the canonical executable schema. |
| AI analysis | `src/ai_provider.rs`, `src/email_analysis/`, `specs/skills/` | Selects providers, assembles per-email evidence prompts from shared skill policy, validates/repairs model output, and records classification output. |
| Embeddings/RAG | `src/embedding_service.rs`, `src/mailbox_retrieval.rs` | Generates vectors and retrieves related mailbox context. |
| Agent harness | `src/agent_harness/`, `specs/agents/`, `specs/workers/`, `specs/skills/` | Runs structured Mail Assistant, long-lived agents, and ephemeral workers with shared skill guidance, captures tool/run state, and records artifacts. |
| Worker runtime | `src/worker_runtime.rs` | Runs ephemeral internal workers with scoped skill bundles and stores their results. |
| Local API | `src/local_api/` | Exposes local HTTP and websocket surfaces for wrappers and clients, loopback by default with token-protected Tailscale binds as the only supported remote mode. |
| Terminal UI | `src/terminal_ui/` | Provides a terminal chat interface against the local API and forwards `API_AUTH_TOKEN` when configured. |

## Storage Model

PostgreSQL is the durable source of truth for synced mailbox metadata, message content, folder state, analysis output, agent runs, and embeddings. pgvector is used for semantic retrieval when embeddings are enabled.

The mailbox itself remains the source of truth for message placement. Filing actions are applied back to IMAP only when `file` runs without `--dry-run`.

## Mail Assistant Multi-Agent Model

Mail Assistant is the only normal user-facing conversational agent. It owns the user conversation, task decomposition, and final synthesis. Specialist work is performed by ephemeral internal sub-agents with scoped skills, such as classification, folder recommendation, digest generation, folder learning, and conflict review.

Sub-agents return structured artifacts into PostgreSQL. They do not create chat messages for the user and they do not mutate IMAP. This lets the core run independent worker jobs in parallel while keeping the product surface simple.

Sub-agent skills improve through a bounded memory loop inspired by autonomous experiment ledgers: workers can return optional reusable `skill_lessons`, core validates those lessons, stores accepted lessons by `skill_bundle`, and later workers receive active lessons as scoped `skill_memory`. New lessons start as `candidate`; only repeated, generalized, non-mutating lessons are promoted to `active`. Lessons carry provenance back to the task, run, worker, and agent spec version that produced them.

Lessons are hints, not authority. They cannot change prompts, tools, core filing policy, or IMAP state directly. Core rejects message-specific facts, one-off folder preferences, and mailbox mutation directives from skill memory. User preferences and mailbox facts belong in folder learning/location history tables, not in sub-agent skill lessons.

Durable classification policy lives in `specs/skills/`, especially
`email-taxonomy.md` and `threat-detection.md`. Agent and worker specs compose
those skills instead of copying local policy variants. The Rust analysis
pipeline embeds the same shared skill docs into the per-email prompt, then keeps
code focused on evidence assembly, enum/schema validation, repair heuristics,
and persistence.

The public agent catalog follows the same boundary: by default, API and TUI clients see only Mail Assistant. Internal specialists remain addressable by the coordinator and can be exposed for development with `MAIL_ASSISTANT_DIRECT_SUBAGENTS=true`.

See [Agent System](AGENT_SYSTEM.md) for the spec inventory and policy ownership
rules.

The core coordinator supports two internal work types for this model:

- `assistant_heartbeat` scans mailbox completeness, stale work, and learning opportunities.
- `subagent_task` runs a bounded ephemeral worker and records the result artifact.

The heartbeat is quiet by default. It records assistant insights and creates bounded sub-agent tasks instead of interrupting the user or duplicating the normal sync/analyze/locate pipeline.

Interactive Mail Assistant turns do not block on full mailbox readiness. The
harness injects a `mailbox_preparation` snapshot into the normal agent run and
lets Mail Assistant use its full tool suite against the data currently prepared
in Postgres. Tool results are treated as partial evidence: if a requested
summary, category, safety label, body, or filing recommendation has not been
prepared yet, Mail Assistant should say that specific detail is missing or
pending instead of refusing the whole conversation.

## Core Work Coordinator

The core runtime is a durable local coordinator backed by `core_work_queue`. It keeps the mailbox moving through sync, body sync, analysis, embedding, folder recommendation, filing preview, and optional filing apply work.

The planner is backlog-driven. A sync pass only queues downstream analysis when unanalyzed mail exists. Analysis queues embedding and location only when those backlogs exist. Location queues filing preview only when there are pending filing recommendations. Core claim priority keeps pipeline work ahead of heartbeat and sub-agent support work, so Mail Assistant background tasks cannot block sync, analysis, location, or filing progress. This keeps an idle core from repeatedly spending compute on empty analysis/location passes or printing the same filing preview over and over.

Filing remains opt-in:

- `make core` keeps sync and prerequisite work current, but does not apply IMAP moves.
- `make core-apply` runs core with `CORE_FILE_APPLY=true`, allowing eligible recommendations to be applied through core filing policy.
- `make app` starts core and the local API together for interactive TUI/wrapper use. It keeps filing disabled unless `CORE_FILE_APPLY=true make app` is used.
- `make file-dry-run` and `make file` remain the manual preview/apply path.

Core takes a per-account Postgres advisory lease before it can process work, so two local runtimes cannot claim the same account queue at the same time. Each core process also owns claimed work with a generated `worker_id`, a stable `locked_at` claim timestamp, and a `lease_expires_at` deadline. Long-running work renews that deadline on a bounded cadence (`CORE_WORK_LEASE_SECS`, `CORE_WORK_LEASE_RENEWAL_INTERVAL_SECS`). Completion and retry updates are accepted only while the original claim is still current; late completion after expiry is rejected with a warning instead of mutating a row another claim may own. During normal shutdown, the coordinator releases any `processing` rows still owned by that worker. If the shutdown grace period expires and the runtime task is aborted, the runtime performs the same owner-aware cleanup from outside the task. Cleanup only releases rows for that worker, clears locks, makes the work immediately retryable, and resets attached running sub-agent tasks to pending. On startup, once the runtime lease proves there is no other live owner, core recovers any orphaned `processing` rows left behind by a crashed or killed process.

Core queue backpressure is fail-fast and bounded by active rows (`pending`, `failed`, or `processing`). `CORE_WORK_QUEUE_MAX_ACTIVE` defaults to 10000. When a new active row would exceed the bound, direct enqueue returns a backpressure error and emits `core_work_enqueue_backpressure_total`; the state-driven coordinator logs the deferred enqueue and relies on the next backlog pass to retry once pressure falls. Refreshing an existing active idempotency key is still allowed because it does not increase queue pressure. Core status includes active/max pressure so dashboards can render the queue as constrained before operators misread a saturated queue as idle.

## Runtime Choices

- **One-shot commands** are best for first runs, testing, and manual inspection.
- **Core mode** is best after the operator has validated sync, analysis, and dry-run filing behavior.
- **Server app** via `make app` is best for interactive use: it starts the core coordinator and API together while preserving the API as a separate client access layer. The API is loopback by default; Tailscale binds require `API_AUTH_TOKEN`, and public/LAN wildcard binds are rejected.
- **Sandbox mode** is best for demos, contributor onboarding, and testing exported mail without touching a live inbox.

## LLM Boundaries

MailSubsystem can use frontier providers or local OpenAI-compatible endpoints. When cloud providers are enabled, email content needed for analysis is sent to those providers. Local providers keep inference on the operator's own machine, subject to the operator's local model server configuration.

Provider selection and limits are environment-driven. See [.env.example](../.env.example).

## Safety Boundaries

- Use `sync-window --days N` for first live runs.
- Use `file --dry-run` before any real filing.
- Keep `.env`, account files, local databases, generated certificates, and sample corpora out of git.
- Treat generated agent output as advisory until you have validated it against your own mailbox rules.
- Core is the mutation boundary: sub-agents recommend, but only core may apply IMAP moves.
- Core normalizes filing targets before IMAP mutation, including rewriting container-only folders to selectable leaves and sanitizing provider-hostile folder characters.
- User folder moves are treated as strong preference evidence. They pin the moved message and start a filing cooldown so workers cannot move the same email back and forth.
