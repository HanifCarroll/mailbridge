# Security

`mailbridge` is designed for read-first mailbox access with two narrow write paths: creating unsent drafts and archiving explicitly selected Fastmail messages. It should not print API tokens or OAuth secrets, but command output may contain private email metadata, message bodies, draft recipients, and local attachment paths.

The CLI must not expose send functionality. Draft support must not call Gmail `drafts.send`, JMAP `EmailSubmission/set`, SMTP submission, or any equivalent send API.

Fastmail archive support must accept explicit message IDs only. It must resolve both Inbox and Archive by JMAP mailbox role, then atomically add Archive membership and remove Inbox membership with `Email/set` for messages in Inbox. It must verify with `Email/get` that Archive is present, Inbox is absent, and all other mailbox membership is preserved before reporting success. Messages in neither mailbox must remain unchanged and report failure. It must not query-wide mutate, delete, trash, send, or alter other mailbox membership. Partial failures must be reported per message and must not print tokens or other credentials.

Do not commit:

- `~/.config/mailbridge/config.json` if it contains private account details
- Fastmail API tokens
- `gws` credential files
- downloaded message artifacts
- draft specs containing private recipients, message bodies, or attachment paths

Report security issues privately by opening a GitHub security advisory or contacting the maintainer directly.
