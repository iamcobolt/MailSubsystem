# Agent System

MailSubsystem exposes one normal user-facing conversational agent: Mail
Assistant. Specialist work happens behind that surface through scoped runtime
agents and ephemeral workers. Shared skills hold durable behavior so prompts,
workers, and Rust prompt assembly stay aligned.

## Boundaries

| Layer | Location | Owns |
| --- | --- | --- |
| Mail Assistant and runtime agents | [`../specs/agents/`](../specs/agents/) | Roles, tools, evidence gathering, output contracts, and conversation behavior. |
| Ephemeral workers | [`../specs/workers/`](../specs/workers/) | Bounded background tasks that return structured artifacts and never talk directly to the user. |
| Shared skills | [`../specs/skills/`](../specs/skills/) | Durable taxonomy, threat, filing, digest, base convention, and skill-memory policy. |
| Rust runtime | [`../src/`](../src/) | Evidence assembly, provider selection, output validation/repair, persistence, queueing, and mailbox mutation safety. |

Core is the only mutation boundary. Agents and workers may recommend, summarize,
or produce artifacts; core applies IMAP moves only through explicit filing
policy and operator-approved modes.

## Policy Ownership

Put durable rules in skills:

- Classification categories, status semantics, summary expectations, and
  calibration heuristics belong in [`../specs/skills/email-taxonomy.md`](../specs/skills/email-taxonomy.md).
- Phishing and security-risk rules belong in
  [`../specs/skills/threat-detection.md`](../specs/skills/threat-detection.md).
- Folder recommendation and filing safety rules belong in
  [`../specs/skills/mailbox-filing.md`](../specs/skills/mailbox-filing.md).
- Digest behavior belongs in [`../specs/skills/mailbox-digest.md`](../specs/skills/mailbox-digest.md).
- Reusable worker lesson policy belongs in [`../specs/skills/skill-memory.md`](../specs/skills/skill-memory.md).
- Shared tool/output/scratchpad conventions belong in
  [`../specs/skills/base-conventions.md`](../specs/skills/base-conventions.md).

Agent and worker specs should reference skills with frontmatter such as:

```toml
skills = ["email-taxonomy", "threat-detection"]
```

Do not copy durable taxonomy, threat, filing, digest, or memory rules into a
single agent prompt. Specs should stay focused on role, tool use, task flow,
state updates, and output shape.

## Runtime Agent Specs

| Spec | Role |
| --- | --- |
| [`../specs/agents/mail-assistant.md`](../specs/agents/mail-assistant.md) | User-facing conversational front door and final synthesis layer. |
| [`../specs/agents/email-analyzer.md`](../specs/agents/email-analyzer.md) | Single-email classification, summaries, threat detection, and scratchpad updates. |
| [`../specs/agents/location-agent.md`](../specs/agents/location-agent.md) | Folder recommendation using mailbox history and filing policy. |
| [`../specs/agents/digest-agent.md`](../specs/agents/digest-agent.md) | Daily/weekly mailbox digest generation. |
| [`../specs/agents/folder-consolidator.md`](../specs/agents/folder-consolidator.md) | Redundant folder review and consolidation proposals. |
| [`../specs/agents/orchestrator.md`](../specs/agents/orchestrator.md) | Batch planning, escalation, and coordination. |

## Worker Specs

| Spec | Role |
| --- | --- |
| [`../specs/workers/classification-worker.md`](../specs/workers/classification-worker.md) | Batch classification artifacts. |
| [`../specs/workers/folder-recommendation-worker.md`](../specs/workers/folder-recommendation-worker.md) | Folder recommendation artifacts. |
| [`../specs/workers/folder-learning-worker.md`](../specs/workers/folder-learning-worker.md) | Reusable filing preference evidence. |
| [`../specs/workers/digest-worker.md`](../specs/workers/digest-worker.md) | Digest artifact generation. |
| [`../specs/workers/conflict-review-worker.md`](../specs/workers/conflict-review-worker.md) | Contradiction and review signals. |

Workers are internal support units. They return structured JSON artifacts,
optional generalized `skill_lessons`, confidence, evidence, recommendations, and
review flags. They do not mutate mailbox state.

## Skill Memory

Workers may return reusable `skill_lessons`. Core validates those lessons,
stores accepted candidates with provenance, and promotes only repeated,
generalized, non-mutating lessons into future runs as hints.

Skill memory cannot:

- Change prompts, tools, or core filing policy.
- Store one-off personal facts or raw mailbox details.
- Mutate IMAP or recommend direct mutation bypassing core.
- Override durable policy in `specs/skills/`.

User preferences and mailbox facts belong in database-backed folder learning,
location history, and conversation state, not in skill memory.

## Adding Or Changing Behavior

1. Decide whether the change is durable policy, task flow, or Rust runtime logic.
2. Put durable policy in [`../specs/skills/`](../specs/skills/).
3. Put role/task/output changes in the relevant agent or worker spec.
4. Keep Rust changes focused on evidence packaging, validation, repair, storage,
   queueing, or mailbox safety.
5. Run prompt/spec tests and release-grade Rust checks.
