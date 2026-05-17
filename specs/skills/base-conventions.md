# MailSubsystem Agent Base Conventions

This file defines shared conventions inherited by runtime agents and ephemeral
workers. `AgentSpec::parse_file` loads this document before the requested spec
when the spec lives beside `specs/skills`.

---

## Tool Protocol

Tools are invoked by the harness — the agent includes a tool call in its response,
the harness executes it and appends the result before the next LLM turn.

**Native tool calling** is used when the provider supports it (Gemini, OpenAI, Anthropic).
**JSON fallback** is used for local models without native function call support:

```json
{"action": "use_tool", "tool": "<tool_name>", "args": {}}
```

Do not invent tool names. Only call tools declared in your agent's Available Tools section.
If a tool returns an error, do not retry with identical arguments — adjust or proceed
without that context.

---

## Scratchpad Protocol

The scratchpad is your persistent memory across emails. It is account-scoped: you cannot
read or write another account's scratchpad. All observations you write are specific to
this inbox and its owner.

**Read:**
```json
{"action": "read_scratchpad", "key": "<key>"}
```

**Write:**
```json
{"action": "write_scratchpad", "key": "<key>", "value": {}}
```

Write to the scratchpad after every email. The harness TTLs entries per key as
configured in [state] frontmatter. Stale entries expire automatically.

**Scratchpad is evidence, not assumption.** Do not pre-populate it with external
knowledge. Write only what you observed from this inbox's emails.

---

## Knowledge Boundary

Your knowledge comes from:
1. This email's content (headers, body, attachments)
2. Tool results (all scoped to this account's data)
3. Your scratchpad (built from previous emails in this account)

When scratchpad history exists for a sender or domain, prefer it over general LLM
pre-training knowledge. The first email from a new sender has no scratchpad history —
use the email's own content and tool results. By the tenth email from that sender,
your scratchpad is the primary signal.

Do not reference external databases, threat feeds, or knowledge from other accounts.

---

## Output Protocol

All agents return a single JSON object as their final answer. Rules:
- No markdown code fences
- No prose outside the JSON
- All required_fields from [output] frontmatter must be present
- Enum fields must use exactly the values listed in [output.validation]
- The harness validates before accepting — invalid output triggers a repair prompt

---

## Action Dispatch (Post-Analysis Rules)

After email-analyzer completes, the harness applies these rules in order:

| Condition | Action | Notes |
|-----------|--------|-------|
| `phishing_status = phishing` OR `threat_level = critical` | Move to Trash immediately | Does not go to location-agent |
| `phishing_status = phishing` AND `threat_level = high` | Move to Trash + flag account | High-risk, flag for review |
| `spam_status = spam` AND `confidence >= 0.85` | Move to Junk | High-confidence spam |
| `spam_status = spam` AND `confidence < 0.85` | Move to Junk + queue orchestrator review | Borderline spam |
| `otp_status = otp` | Extract OTP code, store in DB, move to OTP folder | Code stored before any move |
| `marketing_status = marketing` AND `spam_status = not-spam` | Run location-agent (may recommend Newsletters, Promotions, etc.) | Legitimate marketing |
| All other cases | Run location-agent → File to recommended folder | Normal email |

The orchestrator agent can override these rules for a batch (e.g., "this campaign
should be treated differently") via batch plan instructions injected into the worker's
system context.

---

## Error Handling and Escalation

The harness manages retry and escalation — agents do not retry themselves.
Agents should reflect uncertainty via the `confidence` field.

**Harness retry logic:**
1. Tool call failure → retry tool call once with same args, then continue without it
2. Invalid output (missing fields, wrong enum) → repair prompt, one retry
3. Max iterations hit → mark run as `timed_out`, escalate to frontier
4. Provider error / timeout → retry after backoff, then escalate to frontier
5. Frontier also fails → mark as `failed`, write to error log, require manual review

**Escalation triggers:**
- `confidence < threshold` (configured per-agent in [escalation] frontmatter)
- `threat_level = high` or `critical` (always frontier-verified)
- Max iterations hit on local model
- Provider error on local model

All failures are written to the structured log with: run_id, agent_name, task_id
(message_id), step, error, model_used, attempt_count.

---

## Classification Enums

Use exactly these values. The harness validates enum fields against this list.

```
spam_status:      "spam" | "not-spam"
phishing_status:  "phishing" | "not-phishing"
marketing_status: "marketing" | "not-marketing"
otp_status:       "otp" | "not-otp"
threat_level:     "none" | "low" | "medium" | "high" | "critical"

category: "personal" | "work" | "volunteering" | "financial" | "shopping" |
          "social" | "travel" | "health" | "education"

email_type: "newsletter" | "announcement" | "notification" | "actionable" |
            "conversation" | "transactional" | "receipt" | "reference"
```

---

## Portability Note

These agent files define behavior only. They contain no macOS-specific, PostgreSQL-
specific, or launchd-specific logic. The harness runtime (Rust binary) handles
infrastructure. This means agent files work unchanged when the runtime moves to a
different deployment target (e.g., Cloudflare Workers).
