# codex-monitor

`codex-monitor` opens a real Codex CLI TUI for each configured local account,
runs `/status` three times in that same session, and reports the complete third
status card locally and through a Discord webhook. A VT100 terminal emulator
reconstructs the screen, so PTY escape sequences never reach logs or reports.

The service reads `config.toml` by default. The supplied configuration contains:

| Name | Executable | Environment |
| --- | --- | --- |
| Main | `codex` | Uses the process's default `CODEX_HOME` |
| Alt | `codex` | `CODEX_HOME=/home/hschi1106/.codex-alt` |

Both entries invoke the actual `codex` command directly; shell aliases are not
used. Add another `[[accounts]]` table to monitor another identity:

```toml
[[accounts]]
name = "Work"
executable = "codex"
environment = { CODEX_HOME = "/home/me/.codex-work" }
```

`environment` accepts any per-account environment variables. Omit it for the
normal Codex account. Authenticate each alternate account using the same
`CODEX_HOME` before running the monitor.

The other configuration sections control PTY dimensions, timeouts, the pause
between rate-limit refreshes, the wall-clock interval, and Discord transport:

```toml
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

The interval must be a positive divisor of 60, such as 5, 10, 15, 20, 30, or
60. The three `/status` calls are deliberately fixed because the third rendered
snapshot is the authoritative result.

## Discord setup

In Discord, open the destination channel's **Edit Channel → Integrations →
Webhooks**, create a webhook, and copy its URL. Export it only in the monitor's
runtime environment:

```sh
export DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/...'
```

The URL is never stored in this repository. Webhook failures are logged and do
not stop later accounts or scheduled cycles. Discord receives one message per
account containing only the account title, a PNG rendering that scales without
breaking the terminal layout, and the complete authoritative card as a `.txt`
attachment. If image rendering fails, the monitor falls back to lossless code
blocks and Discord-safe splitting.

## Running

Run every configured account immediately, print the clean report, send it to
Discord, and exit:

```sh
cargo run -- --once
```

Test locally without sending a webhook:

```sh
cargo run -- --once --no-discord
```

Use a different configuration file with:

```sh
cargo run -- --config /path/to/monitor.toml --once --no-discord
```

For PTY troubleshooting, `CODEX_MONITOR_DEBUG_SCREEN=1` prints the clean
reconstructed screen only when a `/status` capture times out. It never prints
the raw PTY stream.

Run continuously with:

```sh
cargo run --release
```

With the supplied 30-minute configuration, continuous mode waits for the next
local wall-clock `HH:00` or `HH:30` boundary, runs one cycle, then calculates
the following boundary. Runtime and restart time therefore do not cause
schedule drift.

## Run continuously with systemd on this machine

Build the executable once, install the supplied user service, and create its
private environment file:

```sh
cd /home/hschi1106/codex-monitor
cargo build --release
mkdir -p ~/.config/codex-monitor ~/.config/systemd/user
cp deploy/codex-monitor.env.example ~/.config/codex-monitor/env
cp deploy/codex-monitor.service ~/.config/systemd/user/codex-monitor.service
chmod 600 ~/.config/codex-monitor/env
```

Edit `~/.config/codex-monitor/env` and replace `REPLACE_ME` with the Discord
webhook URL. Then enable and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now codex-monitor.service
systemctl --user status codex-monitor.service
```

Follow its logs with:

```sh
journalctl --user -u codex-monitor.service -f
```

To keep the user service running after logout and start it during boot, enable
systemd lingering once:

```sh
sudo loginctl enable-linger hschi1106
```

After changing Rust code, rebuild and restart it:

```sh
cargo build --release
systemctl --user restart codex-monitor.service
```

After changing only `config.toml`, restart the service without rebuilding. Stop
or permanently disable it with `systemctl --user stop codex-monitor.service` or
`systemctl --user disable --now codex-monitor.service`.

## Output

Local output contains one logical report with every account. Discord sends each
account as its own title-only message with PNG and TXT attachments:

````text
**Codex Usage Monitor**
Timestamp: 2026-09-07 21:30 +08:00

**Main**
```text
╭────────────────────────────────────────────────╮
│  >_ OpenAI Codex (...)                         │
│  Model: ...                                    │
│  ...every row rendered by Codex /status...     │
│  Weekly limit: [████████████░░░░] ...          │
╰────────────────────────────────────────────────╯
```
````

If an account fails or times out, its section contains a clear error while
successful account cards remain in the report. Codex startup, each `/status`,
and the overall account operation have independent deadlines, and the process
group and PTY reader are cleaned up after every account.
