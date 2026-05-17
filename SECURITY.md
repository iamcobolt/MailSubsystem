# Security Policy

MailSubsystem is experimental local-first email automation software. It processes private email and can move messages through IMAP, so security reports and operational mistakes should be handled carefully.

## Supported Versions

This project is pre-1.0. Security fixes are handled on the default branch unless maintainers document a release branch.

## Reporting a Vulnerability

Please do not post secrets, mailbox contents, exploit payloads, or private infrastructure details in a public issue.

Preferred reporting path:

1. Use GitHub's private vulnerability reporting or Security Advisories for this repository, if available.
2. If private reporting is unavailable, open a public issue with a minimal description and ask for a private coordination path. Do not include sensitive data.

Useful details include:

- Affected command or component.
- Whether the issue can move, delete, expose, or misfile email.
- Provider involved, such as IMAP server, PostgreSQL, local LLM, Gemini, OpenAI, or Anthropic.
- Minimal reproduction using synthetic or anonymized data.

### Required Report Details

Security reports are easiest to prioritize when they include:

1. Title.
2. Severity assessment.
3. Impact and affected security boundary.
4. Affected component, command, or API route.
5. Technical reproduction steps.
6. Demonstrated impact using synthetic or redacted data.
7. Environment details, including OS, MailSubsystem version or commit, IMAP provider shape, database mode, and LLM provider path.
8. Suggested remediation, if known.

Reports without reproduction steps or demonstrated impact may be deprioritized, especially if they come from automated scanners.

## Operational Security Notes

- Keep `.env`, `database.toml`, `accounts.toml`, API keys, app passwords, certificates, and production database exports out of git.
- Use app-specific IMAP passwords where possible.
- Prefer sandbox mode and `file --dry-run` before applying mailbox moves.
- Treat cloud AI providers as third parties that may receive email content for analysis.
- Bind the local API to loopback unless you are using a Tailscale address with `API_AUTH_TOKEN`.
- Treat `API_AUTH_TOKEN` like a password; any client with it can read mailbox data through the local API.
- Do not expose the local API directly to the public internet.

## Known Risk Areas

- Misclassification can move mail to unexpected folders.
- Prompt injection inside email content may influence LLM output.
- Cloud LLM usage can create provider costs and data handling obligations.
- Generated test corpora may contain private email unless explicitly anonymized.

## Scope Notes

The following are important operational concerns, but they need a concrete boundary impact to be treated as vulnerabilities:

- Prompt injection that changes an advisory classification without causing an unauthorized mailbox action, data exposure, or privilege boundary bypass.
- Reports that only show the configured cloud LLM provider receives email content; this is expected when a cloud provider is enabled.
- Local API exposure caused by intentionally binding it to a non-loopback interface outside the supported loopback/Tailscale model.
- Scanner-only findings without a working reproduction against this project.

In-scope examples include unauthorized mailbox mutation, credential exposure, unintended raw email disclosure, local API authorization bypass, unsafe default exposure, or a prompt-injection chain that crosses from untrusted email content into a concrete destructive action.
