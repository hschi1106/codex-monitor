use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::accounts::Account;

const READY_MARKER: &str = "Ask Codex to do anything";

#[derive(Debug, Clone)]
pub struct MonitorSettings {
    pub terminal_rows: u16,
    pub terminal_cols: u16,
    pub startup: Duration,
    pub status: Duration,
    pub refresh_pause: Duration,
    pub overall: Duration,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            terminal_rows: 80,
            terminal_cols: 120,
            startup: Duration::from_secs(45),
            status: Duration::from_secs(25),
            refresh_pause: Duration::from_secs(4),
            overall: Duration::from_secs(150),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexStatus {
    pub account_name: String,
    pub rendered_status: String,
    #[allow(dead_code)]
    // Retained for future alert thresholds; rendered_status stays authoritative.
    pub five_hour_percent_left: Option<u8>,
    #[allow(dead_code)]
    // Retained for future alert thresholds; rendered_status stays authoritative.
    pub weekly_percent_left: Option<u8>,
}

#[derive(Debug, Clone)]
pub enum AccountResult {
    Success(CodexStatus),
    Failure { account_name: String, error: String },
}

struct TerminalState {
    parser: vt100::Parser,
    revision: u64,
    last_update: Instant,
    closed: bool,
    read_error: Option<String>,
}

struct Session {
    account_name: String,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    reader_thread: Option<thread::JoinHandle<()>>,
    terminal: Arc<Mutex<TerminalState>>,
    process_group_leader: Option<i32>,
}

impl Session {
    fn start(account: &Account, settings: &MonitorSettings) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: settings.terminal_rows,
            cols: settings.terminal_cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new(&account.executable);
        command.arg("-c");
        command.arg("check_for_update_on_startup=false");
        command.env("TERM", "xterm-256color");
        for (key, value) in &account.environment {
            command.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to start {}", account.executable.display()))?;
        drop(pair.slave);

        #[cfg(unix)]
        let process_group_leader = pair.master.process_group_leader();
        #[cfg(not(unix))]
        let process_group_leader = None;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let terminal = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(settings.terminal_rows, settings.terminal_cols, 0),
            revision: 0,
            last_update: Instant::now(),
            closed: false,
            read_error: None,
        }));
        let reader_terminal = Arc::clone(&terminal);
        let reader_thread = thread::Builder::new()
            .name(format!("codex-pty-{}", account.name))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            reader_terminal
                                .lock()
                                .expect("terminal mutex poisoned")
                                .closed = true;
                            break;
                        }
                        Ok(count) => {
                            let mut state =
                                reader_terminal.lock().expect("terminal mutex poisoned");
                            state.parser.process(&buffer[..count]);
                            state.revision = state.revision.wrapping_add(1);
                            state.last_update = Instant::now();
                        }
                        Err(error) => {
                            let mut state =
                                reader_terminal.lock().expect("terminal mutex poisoned");
                            state.read_error = Some(error.to_string());
                            state.closed = true;
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            account_name: account.name.clone(),
            child: Some(child),
            master: Some(pair.master),
            writer: Some(writer),
            reader_thread: Some(reader_thread),
            terminal,
            process_group_leader,
        })
    }

    fn snapshot(&self) -> Result<(String, u64, Instant)> {
        let state = self
            .terminal
            .lock()
            .map_err(|_| anyhow!("terminal parser mutex was poisoned"))?;
        if let Some(error) = &state.read_error {
            bail!("PTY reader failed: {error}");
        }
        if state.closed {
            bail!("Codex PTY closed unexpectedly");
        }
        Ok((
            state.parser.screen().contents(),
            state.revision,
            state.last_update,
        ))
    }

    fn wait_until_ready(&self, timeout: Duration, overall_deadline: Instant) -> Result<()> {
        let deadline = deadline_for(timeout, overall_deadline);
        loop {
            let (screen, _, last_update) = self.snapshot()?;
            if screen.contains(READY_MARKER) && last_update.elapsed() >= Duration::from_millis(750)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.print_debug_screen(&screen, "waiting for TUI readiness");
                bail!("timed out waiting for Codex TUI readiness marker {READY_MARKER:?}");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn request_status(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<String> {
        let (before, starting_revision, _) = self.snapshot()?;
        let sent_at = Instant::now();
        for byte in b"/status" {
            self.writer
                .as_mut()
                .context("PTY writer is unavailable")?
                .write_all(&[*byte])?;
            self.writer.as_mut().unwrap().flush()?;
            thread::sleep(Duration::from_millis(20));
        }

        let deadline = deadline_for(timeout, overall_deadline);
        loop {
            let (screen, _, last_update) = self.snapshot()?;
            if screen.contains("show current session configuration and token usage")
                && last_update.elapsed() >= Duration::from_millis(100)
            {
                break;
            }
            if Instant::now() >= deadline {
                self.print_debug_screen(&screen, "waiting for Codex to accept /status input");
                bail!("timed out waiting for Codex to accept /status input");
            }
            thread::sleep(Duration::from_millis(25));
        }
        // Codex enables the CSI-u keyboard protocol in its TUI. Encode Enter
        // explicitly so submission does not depend on PTY line discipline.
        self.writer.as_mut().unwrap().write_all(b"\x1b[13u")?;
        self.writer.as_mut().unwrap().flush()?;

        loop {
            let (screen, revision, last_update) = self.snapshot()?;
            let rendered = extract_last_complete_card(&screen);
            let command_completed = revision > starting_revision
                && screen != before
                && screen.contains(READY_MARKER)
                && sent_at.elapsed() >= Duration::from_millis(750)
                && last_update.elapsed() >= Duration::from_millis(650);

            if command_completed && let Some(card) = rendered {
                return Ok(card);
            }
            if Instant::now() >= deadline {
                self.print_debug_screen(&screen, "waiting for rendered /status card");
                bail!("timed out waiting for a complete rendered /status card");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn print_debug_screen(&self, screen: &str, operation: &str) {
        if std::env::var_os("CODEX_MONITOR_DEBUG_SCREEN").is_some() {
            eprintln!(
                "[{}] reconstructed screen while {}:\n--- screen ---\n{}\n--- end screen ---",
                self.account_name, operation, screen
            );
        }
    }

    fn shutdown(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            // Ctrl-C twice exits an idle Codex session without relying on the
            // slash-command composer or its keyboard protocol.
            let _ = writer.write_all(b"\x03\x03");
            let _ = writer.flush();
        }

        if let Some(child) = self.child.as_mut() {
            let graceful_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < graceful_deadline {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            if matches!(child.try_wait(), Ok(None)) {
                terminate_process_group(self.process_group_leader, false);
                thread::sleep(Duration::from_millis(250));
            }
            if matches!(child.try_wait(), Ok(None)) {
                terminate_process_group(self.process_group_leader, true);
                let _ = child.kill();
            }
            let _ = child.wait();
        }

        self.writer.take();
        self.master.take();
        self.child.take();
        if let Some(reader_thread) = self.reader_thread.take()
            && reader_thread.join().is_err()
        {
            eprintln!(
                "[{}] PTY reader thread panicked during cleanup",
                self.account_name
            );
        }
    }
}

#[cfg(unix)]
fn terminate_process_group(process_group_leader: Option<i32>, force: bool) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    if let Some(pid) = process_group_leader.filter(|pid| *pid > 1) {
        let signal = if force {
            Signal::SIGKILL
        } else {
            Signal::SIGTERM
        };
        let _ = killpg(Pid::from_raw(pid), signal);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_process_group_leader: Option<i32>, _force: bool) {}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn monitor_account(account: &Account, settings: &MonitorSettings) -> Result<CodexStatus> {
    eprintln!("[{}] starting Codex", account.name);
    let overall_deadline = Instant::now() + settings.overall;
    let mut session = Session::start(account, settings)?;

    let result = (|| {
        session.wait_until_ready(settings.startup, overall_deadline)?;
        eprintln!("[{}] TUI ready", account.name);

        let mut authoritative = None;
        for refresh in 1..=3 {
            ensure_before(overall_deadline, "overall account monitoring")?;
            eprintln!("[{}] /status refresh {refresh}/3", account.name);
            let rendered = session.request_status(settings.status, overall_deadline)?;
            if refresh == 3 {
                authoritative = Some(rendered);
            } else {
                sleep_until(settings.refresh_pause, overall_deadline)?;
            }
        }

        let rendered_status = authoritative.context("third /status snapshot was not captured")?;
        eprintln!("[{}] status captured", account.name);
        Ok(CodexStatus {
            account_name: account.name.clone(),
            five_hour_percent_left: parse_percent_left(&rendered_status, "5h limit:"),
            weekly_percent_left: parse_percent_left(&rendered_status, "Weekly limit:"),
            rendered_status,
        })
    })();

    session.shutdown();
    result
}

fn deadline_for(stage_timeout: Duration, overall_deadline: Instant) -> Instant {
    (Instant::now() + stage_timeout).min(overall_deadline)
}

fn ensure_before(deadline: Instant, operation: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("timed out during {operation}");
    }
    Ok(())
}

fn sleep_until(duration: Duration, overall_deadline: Instant) -> Result<()> {
    let wake_at = Instant::now() + duration;
    if wake_at > overall_deadline {
        bail!("overall account timeout expired while waiting for rate-limit refresh");
    }
    thread::sleep(duration);
    Ok(())
}

fn extract_last_complete_card(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let mut cards = Vec::new();

    for (start_row, line) in lines.iter().enumerate() {
        let Some(start_col) = line.chars().position(|character| character == '╭') else {
            continue;
        };
        if !line
            .chars()
            .skip(start_col + 1)
            .any(|character| character == '╮')
        {
            continue;
        }

        for (end_row, bottom) in lines.iter().enumerate().skip(start_row + 1) {
            let bottom_chars: Vec<char> = bottom.chars().collect();
            if bottom_chars.get(start_col) != Some(&'╰')
                || !bottom_chars
                    .iter()
                    .skip(start_col + 1)
                    .any(|character| *character == '╯')
            {
                continue;
            }

            let cropped = lines[start_row..=end_row]
                .iter()
                .map(|row| {
                    row.chars()
                        .skip(start_col)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            cards.push(cropped);
            break;
        }
    }

    cards.pop()
}

fn parse_percent_left(status: &str, label: &str) -> Option<u8> {
    let line = status.lines().find(|line| line.contains(label))?;
    let before_percent = line.split('%').next()?;
    let digits: String = before_percent
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{extract_last_complete_card, parse_percent_left};

    #[test]
    fn extracts_the_last_complete_box_and_preserves_unknown_rows() {
        let screen = "old\n╭────╮\n│ Old│\n╰────╯\n  ╭────────────────────╮\n  │ Model: x           │\n  │ Future field: yes  │\n  │ Weekly limit: 72% left │\n  ╰────────────────────╯\nprompt";
        let card = extract_last_complete_card(screen).unwrap();
        assert!(card.starts_with('╭'));
        assert!(card.contains("Future field: yes"));
        assert!(card.contains("Weekly limit: 72% left"));
        assert!(!card.contains("Old"));
    }

    #[test]
    fn terminal_emulation_applies_control_sequences_before_extraction() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process(b"raw-looking-old-text\x1b[2J\x1b[H");
        parser.process("╭──────╮\r\n│ New  │\r\n╰──────╯".as_bytes());
        let screen = parser.screen().contents();
        let card = extract_last_complete_card(&screen).unwrap();
        assert_eq!(card, "╭──────╮\n│ New  │\n╰──────╯");
        assert!(!screen.contains("raw-looking-old-text"));
    }

    #[test]
    fn parses_optional_known_percentages() {
        let status = "│ 5h limit: [████░░] 61% left │\n│ Weekly limit: 7% left │";
        assert_eq!(parse_percent_left(status, "5h limit:"), Some(61));
        assert_eq!(parse_percent_left(status, "Weekly limit:"), Some(7));
        assert_eq!(parse_percent_left(status, "Credits:"), None);
    }
}
