# Security

`mailbridge` is designed for read-first mailbox access with one narrow write path: creating unsent drafts. It should not print API tokens or OAuth secrets, but command output may contain private email metadata, message bodies, draft recipients, and local attachment paths.

The CLI must not expose send functionality. Draft support must not call Gmail `drafts.send`, JMAP `EmailSubmission/set`, SMTP submission, or any equivalent send API.

Do not commit:

- `~/.config/mailbridge/config.json` if it contains private account details
- Fastmail API tokens
- `gws` credential files
- downloaded message artifacts
- draft specs containing private recipients, message bodies, or attachment paths

Report security issues privately by opening a GitHub security advisory or contacting the maintainer directly.
