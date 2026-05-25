# Spend Safety Design

This is the first in-repo contract for MAIL-31, MAIL-39, and MAIL-40. It is
intentionally narrow: it defines the operator-facing UX, panic-disable policy,
and audit event shape that later runtime wiring should consume.

## Current Foundation

- Rust contract: `src/spend_safety.rs`
- AI config entry point: `AIConfig.spend_safety`
- Environment template: `.env.example`
- Default behavior: spend safety is enabled in `audit_only` mode, so it does
  not block existing provider calls until approval checks are wired.

## MAIL-31 Spend Confirmation UX

Spend confirmation is about user intent before provider-billed work starts.
Local providers are not budget-counted, but they may still emit audit events for
traceability.

### Shared Semantics

- `SPEND_APPROVAL_MODE=audit_only`: record estimates and decisions only.
- `SPEND_APPROVAL_MODE=prompt`: interactive surfaces ask when an estimate meets
  `SPEND_CONFIRMATION_THRESHOLD_CENTS`.
- `SPEND_APPROVAL_MODE=enforce`: non-interactive surfaces must present an
  approval token or fail closed before calling a billed provider.
- `SPEND_DAILY_BUDGET_CENTS` and `SPEND_MONTHLY_BUDGET_CENTS` are hard caps
  once budget accounting is connected.

### CLI

- Single-record commands should show provider, model, estimated cents, threshold,
  and remaining configured budget before continuing.
- Batch commands should summarize the batch estimate and the worst-case provider
  path before any frontier calls start.
- Future flags should be explicit, for example `--approve-spend <approval_id>`
  for automation and `--yes-spend` only for interactive one-shot commands.

### TUI

- The TUI should render a blocking approval dialog with provider, model,
  estimate, budget remaining, and the command that triggered the spend.
- Denial should return the user to the prior screen without queueing new billed
  work.
- Approval should produce the same audit event shape as CLI/API approvals.

### API

- API callers should receive a structured `approval_required` response when
  spend is blocked.
- Approval endpoints should return an approval token scoped to provider, model,
  estimate, actor, account, and short expiry.
- Runtime endpoints must not accept unscoped "approve everything" toggles.

## MAIL-39 Panic Threshold And Auto-Disable

The panic threshold is a compromise brake for unexpectedly high provider spend,
for example a leaked API key or runaway process.

- `SPEND_PANIC_THRESHOLD_CENTS` is evaluated over
  `SPEND_PANIC_WINDOW_SECS`.
- Threshold comparison is inclusive: reaching the configured spend is enough to
  trigger the panic path.
- `SPEND_PANIC_AUTO_DISABLE=true` means the provider should be disabled before
  the next billed call.
- Auto-disable should stop frontier queue processing, reject new frontier
  requests, and force local-only behavior where that is safe.
- Re-enable must be an explicit operator action and must emit an audit event.
- Runtime disable state should eventually be persisted so process restarts do
  not silently re-enable a suspected compromised key.

## MAIL-40 Spend Approval Audit Trail

The audit trail must answer who approved what spend, where it came from, and
what the system did after approval or denial. It must not store provider API
keys, message bodies, or prompt payloads.

Required event classes:

- `approval_requested`
- `approval_granted`
- `approval_denied`
- `budget_exceeded`
- `panic_threshold_triggered`
- `provider_auto_disabled`
- `provider_reenabled`
- `spend_recorded`

Required fields:

- timestamp
- event type
- surface: `cli`, `tui`, `api`, `core`, or `worker`
- provider and model
- account id, actor, request id, and approval id when known
- estimated and actual cost in cents
- decision and human-readable reason when applicable

The first persistence target can be JSONL at `SPEND_AUDIT_LOG_PATH` for simple
operator inspection. A database table should follow before multi-actor approval
history, API query endpoints, or durable provider-disable state depend on it.

## Recommended Next Steps

1. Add an append-only audit writer with fsync-safe JSONL writes and tests.
2. Add provider call-site hooks that produce `spend_recorded` events from token
   usage when providers return usage data.
3. Add CLI confirmation for `test-llm --frontier`, `analyze`, and
   `process-frontier-queue`.
4. Add API approval token endpoints and TUI approval dialog support.
5. Add durable provider-disable state and make frontier builders respect it.
