# Codex Usage Monitor

<p align="center">
  <a href="https://www.rust-lang.org/"><img alt="Rust 2024" src="https://img.shields.io/badge/Rust-2024_Edition-000000?logo=rust"></a>
  <a href="#requirements"><img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-FCC624?logo=linux&amp;logoColor=black"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/hschi1106/codex-monitor"></a>
  <a href="https://github.com/hschi1106/codex-monitor/commits/main"><img alt="Last commit" src="https://img.shields.io/github/last-commit/hschi1106/codex-monitor"></a>
</p>

Keep dormant Codex 5-hour usage windows anchored across multiple local accounts,
while delivering complete, readable `/status` cards to Discord.

Codex `/status` is an interactive TUI command rather than ordinary command
output. Codex Usage Monitor opens a real pseudo-terminal, waits for the TUI,
runs `/status` three times in the same session, reconstructs the terminal with
a VT100 parser, and treats the complete third card as authoritative. When a
fully available 5-hour window still appears unanchored, it sends one minimal
real model turn and captures three fresh status cards before reporting.

Each successful account produces one Discord message containing:

- `Codex Usage Monitor — <account>`
- A responsive PNG rendering of the complete status card
- The original UTF-8 status card as a `.txt` attachment

No Discord bot, gateway connection, or public HTTP server is required—only an
incoming webhook.

> [!NOTE]
> This is an unofficial community project and is not affiliated with or
> supported by OpenAI.

## Why this exists

This project is more than a passive usage dashboard. An idle account at 100%
can show a reset timestamp that remains about five hours ahead and moves forward
with wall-clock time. In observed Codex CLI behavior, that suggests the next
5-hour window has not been anchored by a real model turn.

Every cycle first captures an authoritative status with three `/status` calls.
If the account is at 100% and its reset is approximately five hours away, the
monitor sends one tiny `Reply only OK. Do not use tools.` request, waits for the
model response to finish, then captures `/status` three more times. Discord
receives the final post-anchor card. Active windows skip the model request but
are still reported.

The default `HH:00` and `HH:30` schedule keeps each configured account checked
and anchors a dormant window without manually opening Codex.

> [!IMPORTANT]
> This workaround is based on observed Codex CLI behavior, not a documented or
> guaranteed API contract. Codex rate-limit handling may change over time.

## Features

- Monitor any number of independently authenticated Codex accounts
- Configure `CODEX_HOME` and other environment variables per account
- Invoke the real `codex` executable without shell aliases
- Reconstruct full-screen TUI output instead of stripping raw ANSI bytes
- Run three `/status` refreshes in one session to avoid stale rate-limit data
- Detect a likely dormant 5-hour window from 100% remaining and reset time
- Anchor a dormant window with exactly one minimal real model turn
- Refresh `/status` three more times and report the post-anchor third card
- Preserve every visible status row, including future unknown fields
- Isolate account failures so one timeout does not suppress other results
- Enforce startup, status, HTTP, and overall account timeouts
- Schedule on local wall-clock boundaries without accumulating drift
- Clean up Codex processes, process groups, PTYs, and reader threads
- Run once for testing or continuously through a systemd user service

## How it works

```text
Codex TUI in a PTY
        │
        ▼
/status × 3 → complete third card → parse 5h state
        │
        ├── active ───────────────────────────────┐
        │                                         │
        └── likely dormant                        │
                │                                 │
                ▼                                 │
        one minimal model turn                    │
                │                                 │
                ▼                                 │
        /status × 3 → post-anchor third card      │
                │                                 │
                └─────────────────────────────────┘
                                                  ▼
                                      local report + Discord PNG/TXT
```

## Requirements

- Linux
- Rust and Cargo
- A working, authenticated [Codex CLI](https://github.com/openai/codex)
- A monospaced TrueType font; the default is DejaVu Sans Mono
- A Discord incoming webhook
- systemd for the optional background service

## Quick Start

### 1. Clone and build

```sh
git clone https://github.com/hschi1106/codex-monitor.git
cd codex-monitor
cargo build --release
```

### 2. Configure accounts

Edit `config.toml`. The included configuration monitors the normal Codex
account and an alternate account with a separate `CODEX_HOME`:

```toml
[[accounts]]
name = "Main"
executable = "codex"

[[accounts]]
name = "Alt"
executable = "codex"
environment = { CODEX_HOME = "/home/hschi1106/.codex-alt" }
```

Authenticate each alternate account before using the monitor:

```sh
CODEX_HOME=/home/hschi1106/.codex-alt codex
```

### 3. Test locally

```sh
./target/release/codex-monitor --config config.toml --once --no-discord
```

The command immediately checks every account, anchors any detected dormant
window, prints a clean report, and exits. `--no-discord` disables only the HTTP
request; it does not disable automatic anchoring.

### 4. Connect Discord

In Discord, open **Server Settings → Integrations → Webhooks**, create a
webhook, select its destination channel, and copy the URL.

Create a private environment file outside the repository:

```sh
mkdir -p ~/.config/codex-monitor
cp deploy/codex-monitor.env.example ~/.config/codex-monitor/env
chmod 600 ~/.config/codex-monitor/env
```

Replace `REPLACE_ME` in `~/.config/codex-monitor/env`:

```text
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/REPLACE_ME
```

Load it and run an end-to-end test:

```sh
set -a
source ~/.config/codex-monitor/env
set +a
./target/release/codex-monitor --config config.toml --once
```

Never commit the webhook URL. Delete or regenerate it in Discord if it is
exposed.

## Configuration

The monitor reads `config.toml` by default. Pass `--config FILE` to use another
path. Unknown fields and invalid values fail at startup instead of being
silently ignored.

### Complete example

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
model_turn_timeout_seconds = 90
refresh_pause_seconds = 4
overall_timeout_seconds = 300

[schedule]
interval_minutes = 30

[discord]
webhook_env = "DISCORD_WEBHOOK_URL"
request_timeout_seconds = 20
status_font_path = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
status_font_size = 18.0
```

### Accounts

Add one `[[accounts]]` table for each identity.

| Key | Required | Description |
| --- | --- | --- |
| `name` | Yes | Unique label used in logs, filenames, and Discord titles |
| `executable` | Yes | Codex executable name or absolute path |
| `environment` | No | Environment variables applied only to this account's child process |

`environment` accepts arbitrary string values:

```toml
[[accounts]]
name = "Work"
executable = "/home/me/.local/bin/codex"
environment = { CODEX_HOME = "/home/me/.codex-work" }
```

The default account inherits the monitor process's environment. An alternate
account should normally override `CODEX_HOME`.

### Monitor

| Key | Default | Description |
| --- | ---: | --- |
| `terminal_rows` | `80` | Rows in the virtual terminal; minimum 20 |
| `terminal_cols` | `120` | Columns in the virtual terminal; minimum 60 |
| `startup_timeout_seconds` | `45` | Maximum time to wait for the real TUI prompt |
| `status_timeout_seconds` | `25` | Maximum time for each rendered `/status` card |
| `model_turn_timeout_seconds` | `90` | Maximum time for the minimal anchor model turn |
| `refresh_pause_seconds` | `4` | Pause for asynchronous rate-limit refreshes between calls |
| `overall_timeout_seconds` | `300` | Hard deadline for initial capture, optional anchor, and recapture |

The three initial `/status` invocations are intentionally fixed. A likely
dormant account receives exactly one model request followed by another fixed
set of three `/status` calls. A model-turn failure is reported, and the best
available pre-anchor card is still delivered.

### Schedule

| Key | Default | Description |
| --- | ---: | --- |
| `interval_minutes` | `30` | Wall-clock interval; must be a positive divisor of 60 |

Valid examples include `5`, `10`, `15`, `20`, `30`, and `60`. With `30`, the
monitor runs at `HH:00` and `HH:30`; execution time and restarts do not shift
future boundaries.

### Discord

| Key | Default | Description |
| --- | --- | --- |
| `webhook_env` | `DISCORD_WEBHOOK_URL` | Environment variable containing the secret webhook URL |
| `request_timeout_seconds` | `20` | Timeout for each Discord HTTP request |
| `status_font_path` | DejaVu Sans Mono | TrueType font used for status PNGs |
| `status_font_size` | `18.0` | PNG font size in pixels; valid range 8–48 |

If PNG rendering fails, the monitor logs the error and falls back to complete
text code blocks with lossless message splitting.

## CLI Reference

```text
codex-monitor

USAGE:
    codex-monitor [--once] [--no-discord] [--config PATH]

OPTIONS:
    --once          Run immediately, then exit
    --no-discord    Print locally without using a Discord webhook
    --config PATH   Read this TOML file (default: config.toml)
```

Common commands:

| Command | Purpose |
| --- | --- |
| `cargo run -- --once` | Run now, send to Discord, and exit |
| `cargo run -- --once --no-discord` | Run now and print locally only |
| `cargo run` | Run continuously on configured boundaries |
| `cargo run -- --config FILE` | Run with another configuration file |
| `CODEX_MONITOR_DEBUG_SCREEN=1 cargo run -- --once --no-discord` | Print the reconstructed screen only after a capture timeout |

Normal progress logs look like this:

```text
[Main] starting Codex
[Main] TUI ready
[Main] /status refresh 1/3
[Main] /status refresh 2/3
[Main] /status refresh 3/3
[Main] 5h window appears dormant
[Main] sending anchor turn
[Main] anchor turn completed
[Main] refreshing post-anchor status
[Main] /status refresh 1/3
[Main] /status refresh 2/3
[Main] /status refresh 3/3
[Main] status captured
[Main] Discord report sent
```

For an active window, the monitor logs `5h window already active` and skips the
model request. Discord reporting still occurs on every cycle, including when
another account fails.

Raw PTY escape sequences are never printed to the real terminal.

## Run as a systemd User Service

The included unit targets this installation at
`/home/hschi1106/codex-monitor`. Update its paths before installation if the
repository or Codex executable lives elsewhere.

```sh
cd /home/hschi1106/codex-monitor
cargo build --release
mkdir -p ~/.config/systemd/user ~/.config/codex-monitor
cp deploy/codex-monitor.service ~/.config/systemd/user/codex-monitor.service
cp -n deploy/codex-monitor.env.example ~/.config/codex-monitor/env
chmod 600 ~/.config/codex-monitor/env
systemctl --user daemon-reload
systemctl --user enable --now codex-monitor.service
loginctl enable-linger "$USER"
```

Make sure `~/.config/codex-monitor/env` contains the real webhook URL.

### Service operations

| Command | Purpose |
| --- | --- |
| `systemctl --user status codex-monitor.service --no-pager` | Check whether the daemon is active |
| `journalctl --user -u codex-monitor.service -f` | Follow live logs; `Ctrl+C` only exits the viewer |
| `journalctl --user -u codex-monitor.service -n 100 --no-pager` | Show the latest 100 lines |
| `systemctl --user restart codex-monitor.service` | Restart after configuration changes |
| `systemctl --user stop codex-monitor.service` | Stop monitoring |
| `systemctl --user start codex-monitor.service` | Start monitoring |
| `systemctl --user disable --now codex-monitor.service` | Stop and disable automatic startup |

After changing Rust code:

```sh
cargo build --release
systemctl --user restart codex-monitor.service
```

Changes to `config.toml` only require a restart. Do not run a manual continuous
instance alongside systemd, or Discord will receive duplicate cycles.

## Troubleshooting

Check service health, logs, and lingering:

```sh
systemctl --user status codex-monitor.service --no-pager
journalctl --user -u codex-monitor.service -n 100 --no-pager
loginctl show-user "$USER" -p Linger
```

- **`codex` is not found:** update `PATH` in the service unit or use an absolute
  account `executable`, then copy the unit and run `systemctl --user
  daemon-reload`.
- **A status capture times out:** retry with `CODEX_MONITOR_DEBUG_SCREEN=1` and
  `--once --no-discord` to inspect the clean reconstructed screen.
- **PNG rendering fails:** verify `discord.status_font_path` points to a readable
  TrueType font.
- **Discord receives nothing:** check the environment file, its permissions,
  and recent journal entries for an HTTP error.

## Development

```sh
cargo fmt -- --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

Licensed under the [MIT License](LICENSE).
