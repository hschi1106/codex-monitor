# codex-monitor

A small Rust service that monitors `/status` for multiple local Codex CLI
accounts and sends each account's status to Discord.

Codex `/status` is an interactive TUI command, so this project launches Codex
inside a pseudo-terminal and reconstructs the terminal with a VT100 parser. It
runs `/status` three times in the same session to allow Codex's asynchronous
rate-limit refresh to finish, then keeps the complete third status card.

Each successful Discord notification contains only an account title, a PNG of
the full Codex card, and the original UTF-8 card as a `.txt` attachment. The
image scales cleanly in Discord without breaking terminal borders. Account
failures are reported separately and do not stop the remaining accounts.

## Features

- Any number of independently configured local Codex accounts
- Per-account executable and environment variables such as `CODEX_HOME`
- Real PTY interaction with no dependency on shell aliases
- VT100 screen reconstruction instead of parsing raw escape sequences
- Three `/status` refreshes in one Codex session
- Per-account startup, status, and overall timeouts
- Discord incoming webhook delivery with PNG and TXT attachments
- Local wall-clock scheduling without interval drift
- Clean child-process, process-group, PTY, and reader-thread cleanup
- One-shot local testing and a supplied systemd user service

## Requirements

- Rust toolchain with Cargo
- A working `codex` command
- Each configured Codex account already authenticated
- A monospaced TrueType font; the supplied configuration uses DejaVu Sans Mono
- A Discord incoming webhook for notifications
- Linux and systemd if using the supplied service unit

## Quick start

Clone and build:

```sh
git clone https://github.com/hschi1106/codex-monitor.git
cd codex-monitor
cargo build --release
```

Configure the accounts in `config.toml`. The supplied configuration monitors
the normal Codex account and one alternate account:

```toml
[[accounts]]
name = "Main"
executable = "codex"

[[accounts]]
name = "Alt"
executable = "codex"
environment = { CODEX_HOME = "/home/hschi1106/.codex-alt" }
```

Authenticate the alternate account before starting the monitor:

```sh
CODEX_HOME=/home/hschi1106/.codex-alt codex
```

Run one cycle locally without Discord:

```sh
./target/release/codex-monitor --config config.toml --once --no-discord
```

## Configuration

The program reads `config.toml` by default. Use another file with
`--config /path/to/config.toml`.

```toml
[[accounts]]
name = "Main"
executable = "codex"

[[accounts]]
name = "Alt"
executable = "codex"
environment = { CODEX_HOME = "/home/hschi1106/.codex-alt" }

[monitor]
terminal_rows = 80
terminal_cols = 120
startup_timeout_seconds = 45
status_timeout_seconds = 25
refresh_pause_seconds = 4
overall_timeout_seconds = 150

[schedule]
interval_minutes = 30

[discord]
webhook_env = "DISCORD_WEBHOOK_URL"
request_timeout_seconds = 20
status_font_path = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
status_font_size = 18.0
```

`environment` accepts arbitrary per-account variables. Omit it for the default
Codex account. The schedule interval must be a positive divisor of 60, such as
5, 10, 15, 20, 30, or 60. A 30-minute schedule runs at `HH:00` and `HH:30`.

The monitor always performs exactly three `/status` commands because the third
rendered card is the authoritative snapshot.

## Discord setup

1. In Discord, open **Server Settings → Integrations → Webhooks**.
2. Create a webhook, choose the destination channel, and copy its URL.
3. Create a private environment file outside the repository:

```sh
mkdir -p ~/.config/codex-monitor
cp deploy/codex-monitor.env.example ~/.config/codex-monitor/env
chmod 600 ~/.config/codex-monitor/env
```

Edit `~/.config/codex-monitor/env` and replace `REPLACE_ME`:

```text
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/REPLACE_ME
```

Never commit the real webhook URL. If it is exposed, delete or regenerate it in
Discord.

Load the environment and perform an immediate end-to-end test:

```sh
set -a
source ~/.config/codex-monitor/env
set +a
./target/release/codex-monitor --config config.toml --once
```

Discord should receive one message per account. Each successful message has the
title `Codex Usage Monitor — <account>`, a PNG preview, and a complete TXT
attachment.

## Command reference

| Command | Purpose |
| --- | --- |
| `cargo run -- --once` | Run immediately, send to Discord, then exit |
| `cargo run -- --once --no-discord` | Run immediately and print locally only |
| `cargo run` | Wait for each configured wall-clock boundary and run continuously |
| `cargo run -- --config FILE` | Use a different configuration file |
| `CODEX_MONITOR_DEBUG_SCREEN=1 cargo run -- --once --no-discord` | Print the reconstructed screen when capture times out |

Operational progress is written separately from the clean report:

```text
[Main] starting Codex
[Main] TUI ready
[Main] /status refresh 1/3
[Main] /status refresh 2/3
[Main] /status refresh 3/3
[Main] status captured
```

## Run continuously with systemd

The supplied unit is configured for this machine at
`/home/hschi1106/codex-monitor`. Build the release binary and install the user
service:

```sh
cd /home/hschi1106/codex-monitor
cargo build --release
mkdir -p ~/.config/systemd/user ~/.config/codex-monitor
cp deploy/codex-monitor.service ~/.config/systemd/user/codex-monitor.service
cp -n deploy/codex-monitor.env.example ~/.config/codex-monitor/env
chmod 600 ~/.config/codex-monitor/env
systemctl --user daemon-reload
systemctl --user enable --now codex-monitor.service
```

Ensure `~/.config/codex-monitor/env` contains the real webhook URL. Allow the
user service to run after logout and start during boot:

```sh
loginctl enable-linger "$USER"
```

Basic service commands:

| Command | Purpose |
| --- | --- |
| `systemctl --user status codex-monitor.service --no-pager` | Check service health |
| `journalctl --user -u codex-monitor.service -f` | Follow live logs; `Ctrl+C` only exits the log viewer |
| `journalctl --user -u codex-monitor.service -n 100 --no-pager` | Show the latest 100 log lines |
| `systemctl --user restart codex-monitor.service` | Restart after configuration changes |
| `systemctl --user stop codex-monitor.service` | Stop monitoring |
| `systemctl --user start codex-monitor.service` | Start monitoring again |
| `systemctl --user disable --now codex-monitor.service` | Stop and disable automatic startup |

After changing Rust code, rebuild before restarting:

```sh
cargo build --release
systemctl --user restart codex-monitor.service
```

After changing only `config.toml`, restart without rebuilding. Avoid running a
manual continuous instance alongside systemd, or every cycle may be delivered
twice.

## Troubleshooting

Check the service, the schedule, and recent errors:

```sh
systemctl --user status codex-monitor.service --no-pager
journalctl --user -u codex-monitor.service -n 100 --no-pager
loginctl show-user "$USER" -p Linger
```

If `codex` cannot be found under systemd, update the `PATH` in
`deploy/codex-monitor.service`, copy the unit again, and reload systemd. The
supplied unit includes the Codex path currently used on this machine.

If PNG generation fails, verify `discord.status_font_path`. The monitor falls
back to complete text code blocks so status data is still delivered.

## Development

```sh
cargo fmt -- --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

Licensed under the [MIT License](LICENSE).
