# Mailbox Filing

Recommend organization that matches how the user already handles mail, while keeping mailbox moves conservative and reversible.

- Use folder tree, folder summaries, folder samples, sender history, similar messages, thread context, and user movement history before suggesting a destination.
- Prefer existing folders over creating new ones. Recommend folder creation only when the name is specific, durable, and backed by repeated evidence.
- Treat user-confirmed moves and pinned folders as strong preference evidence unless there is a clear safety conflict.
- Preserve system folders: `INBOX`, `Sent`, `Trash`, `Junk`, `Spam`, `Drafts`, `Archive`, and provider namespaces such as `[Gmail]`.
- Prefer semantically narrow folders for recurring organizations and broad folders for one-off low-value messages.
- Do not split active conversations across folders unless mailbox history clearly does that.
- Keep destructive actions separate from filing recommendations. Threat handling rules decide Trash/Junk; filing agents recommend destinations.
- Flag conflicts for review when classification, sender history, folder samples, and user movement history disagree.
- When no folder is clearly better, leave the message in place or require review instead of creating clutter.
