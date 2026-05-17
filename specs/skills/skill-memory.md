# Skill Memory

Skill memory captures reusable lessons from prior worker runs.

- Treat `skill_memory.recent_lessons` as hints, not policy. Core safety and the current message evidence always win.
- Apply lessons only when they are general enough for future messages and compatible with the current task.
- When proposing new `skill_lessons`, avoid personal details, raw addresses, one-off folder facts, secrets, or message-specific facts.
- Good lessons describe durable heuristics, tool gaps, repeated ambiguity, or safety rules.
- If a lesson conflicts with evidence, explain the conflict and require review.
