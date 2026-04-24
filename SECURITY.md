# Security

`mailbridge` is designed for read-only mailbox access. It should not print API tokens or OAuth secrets, but command output may contain private email metadata and message bodies.

Do not commit:

- `~/.config/mailbridge/config.json` if it contains private account details
- Fastmail API tokens
- `gws` credential files
- downloaded message artifacts

Report security issues privately by opening a GitHub security advisory or contacting the maintainer directly.
