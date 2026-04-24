# mailbridge

`mailbridge` is a read-only mailbox bridge for local automations and coding agents. It provides one JSON-oriented CLI over:

- Gmail accounts accessed through the local `gws` CLI
- Fastmail accounts accessed through JMAP

It is intentionally conservative: search, read, thread lookup, attachment download, and local evidence export only. It does not send, archive, delete, label, or otherwise mutate mailbox state.

## Install

```sh
cargo install --path .
```

## Quick Start

Create a config file:

```sh
mkdir -p ~/.config/mailbridge
cp config.example.json ~/.config/mailbridge/config.json
```

Edit the config so each account points at the right local credential profile. You can also point `MAILBRIDGE_CONFIG` at another JSON file.

Then verify the setup:

```sh
mailbridge --json doctor
mailbridge --json accounts
```

## Config

`mailbridge` works without a config file by using generic defaults:

- `personal`: `~/.config/gws`
- `domain`: `~/.config/gws-domain`
- `fastmail`: `~/.config/mailbridge/fastmail.env`

For real use, create `~/.config/mailbridge/config.json`:

```json
{
  "accounts": {
    "personal": {
      "email": "person@example.com",
      "config": "/home/you/.config/gws"
    },
    "domain": {
      "email": "you@company.example",
      "config": "/home/you/.config/gws-domain"
    },
    "fastmail": {
      "email": "you@fastmail.example",
      "config": "/home/you/.config/mailbridge/fastmail.env"
    }
  }
}
```

The Fastmail env file must contain:

```sh
FASTMAIL_API_TOKEN=...
```

Do not commit credential files, API tokens, downloaded attachments, or message artifacts.

## Commands

```sh
mailbridge --json doctor
mailbridge --json accounts

mailbridge --json search --account personal "from:sender@example.com newer_than:7d" --limit 10
mailbridge --json read --account personal --message-id MESSAGE_ID
mailbridge --json thread --account personal --thread-id THREAD_ID

mailbridge --json pull --account personal --query "from:sender@example.com newer_than:30d" --out /tmp/mail-run
mailbridge --json pull --account personal --message-id MESSAGE_ID --out /tmp/mail-run --no-body
mailbridge pull --account personal --message-id MESSAGE_ID --out /tmp/mail-run --open

mailbridge --json attachments list --account personal --message-id MESSAGE_ID
mailbridge --json attachments download --account personal --message-id MESSAGE_ID --out /tmp/mail-attachments

mailbridge --json search --account domain "from:sender@example.com newer_than:7d" --limit 10
mailbridge --json search --account fastmail "search text" --limit 10
```

## Output Artifacts

`pull` writes:

- `run.json`
- `messages.json`
- `evidence.md`
- `attachments/`

`attachments download` writes only attachment files and returns a manifest.

## Account Names

The CLI currently exposes three account slots:

- `personal`: Gmail through a `gws` config directory
- `domain`: Gmail through another `gws` config directory
- `fastmail`: Fastmail through JMAP

These names are intentionally stable for scripts. Use config values to point them at your actual accounts.
