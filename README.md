# slack-cli

Read **your own** Slack data from the Slack desktop app's local cache.
Offline, read-only, a single binary for macOS.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Lakr233/slack-cli/main/install.sh | sh
```

This puts `slack-cli` in `~/.local/bin`. Remove it with
`sh install.sh --uninstall`. To build from source instead:

```sh
cargo install --git https://github.com/Lakr233/slack-cli
```

## Use

```sh
slack-cli teams                        # workspaces in the cache
slack-cli channels                     # channels, DMs, unread counts
slack-cli members
slack-cli history '#general' -n 50     # or '@alice', or a channel id
slack-cli history C0123 --thread 1700000000.000100
slack-cli search deploy --channel '#ops'
slack-cli files
slack-cli export -o slack.json         # everything as JSON, for an agent
```

Every command takes `--json`, `--team <id|name|domain>` and `--data-dir <path>`.

## Good to know

- **It is a cache, not your full history.** Slack only keeps messages from
  channels you opened recently. `history` says where messages are missing.
- **Nothing leaves your machine.** No network, no tokens, no Keychain.
  Tokens are always scrubbed from output; emails are hidden unless you pass
  `--emails`.
- **Slack writes the cache on its own schedule.** For the latest state,
  quit Slack (⌘Q) once.
- **Works with** Slack desktop 4.44 to 4.53, direct download and Mac App
  Store. Older versions can no longer sign in, so they have no cache.

## How it works

Slack keeps one snapshot of its Redux store per workspace in IndexedDB
(`~/Library/Application Support/Slack/IndexedDB/…indexeddb.blob`, or the
same path inside the App Store container). Each snapshot is a Chromium
IndexedDB value (`FF 11 02` + snappy), a Blink envelope (`FF 15 FE`) and a
V8 structured clone (`FF 0F`, or `FF 10` from Slack 4.53). slack-cli
decodes all three layers itself; see `src/decode.rs`.

## Development

```sh
cargo test
```

## License

MIT
