use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, LocalResult, NaiveDateTime, NaiveTime,
    TimeZone,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::accounts::Account;

const READY_MARKER: &str = "Ask Codex to do anything";

#[derive(Debug, Clone)]
pub struct MonitorSettings {
    pub terminal_rows: u16,
    pub terminal_cols: u16,
    pub startup: Duration,
    pub status: Duration,
    pub model_turn: Duration,
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
            model_turn: Duration::from_secs(90),
            refresh_pause: Duration::from_secs(4),
            overall: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AccountState {
    previous_five_hour_reset: Option<DateTime<Local>>,
    previous_observed_at: Option<DateTime<Local>>,
}

#[derive(Debug, Clone)]
pub enum AnchorOutcome {
    NotNeeded,
    AnchoredAutomatically,
    Failed(String),
}

impl AnchorOutcome {
    pub fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            Self::NotNeeded | Self::AnchoredAutomatically => None,
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
    pub five_hour_reset_at: Option<DateTime<Local>>,
    pub anchor_outcome: AnchorOutcome,
}

#[derive(Debug, Clone)]
pub enum AccountResult {
    Success(CodexStatus),
    Failure { account_name: String, error: String },
}

impl AccountResult {
    pub fn account_name(&self) -> &str {
        match self {
            Self::Success(status) => &status.account_name,
            Self::Failure { account_name, .. } => account_name,
        }
    }
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

    fn request_anchor_turn(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<()> {
        const ANCHOR_PROMPT: &str = "Reply only OK. Do not use tools.";

        let (before, starting_revision, _) = self.snapshot()?;
        for byte in ANCHOR_PROMPT.bytes() {
            self.writer
                .as_mut()
                .context("PTY writer is unavailable")?
                .write_all(&[byte])?;
            self.writer.as_mut().unwrap().flush()?;
            thread::sleep(Duration::from_millis(12));
        }

        let deadline = deadline_for(timeout, overall_deadline);
        loop {
            let (screen, _, _) = self.snapshot()?;
            if screen.contains(ANCHOR_PROMPT) {
                break;
            }
            if Instant::now() >= deadline {
                self.print_debug_screen(&screen, "waiting for Codex to accept anchor input");
                bail!("timed out waiting for Codex to accept anchor input");
            }
            thread::sleep(Duration::from_millis(25));
        }

        self.writer.as_mut().unwrap().write_all(b"\x1b[13u")?;
        self.writer.as_mut().unwrap().flush()?;
        let sent_at = Instant::now();

        loop {
            let (screen, revision, last_update) = self.snapshot()?;
            let completed = revision > starting_revision
                && screen != before
                && screen.contains(READY_MARKER)
                && sent_at.elapsed() >= Duration::from_secs(1)
                && last_update.elapsed() >= Duration::from_millis(750);
            if completed {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.print_debug_screen(&screen, "waiting for anchor model turn completion");
                bail!("timed out waiting for anchor model turn completion");
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

trait CodexInteraction {
    fn status_card(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<String>;
    fn anchor_turn(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<()>;
}

impl CodexInteraction for Session {
    fn status_card(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<String> {
        self.request_status(timeout, overall_deadline)
    }

    fn anchor_turn(&mut self, timeout: Duration, overall_deadline: Instant) -> Result<()> {
        self.request_anchor_turn(timeout, overall_deadline)
    }
}

pub fn monitor_account(
    account: &Account,
    settings: &MonitorSettings,
    state: &mut AccountState,
) -> Result<CodexStatus> {
    eprintln!("[{}] starting Codex", account.name);
    let overall_deadline = Instant::now() + settings.overall;
    let mut session = Session::start(account, settings)?;

    let result = (|| {
        session.wait_until_ready(settings.startup, overall_deadline)?;
        eprintln!("[{}] TUI ready", account.name);

        monitor_interaction(
            &mut session,
            &account.name,
            settings,
            state,
            overall_deadline,
        )
    })();

    session.shutdown();
    result
}

fn monitor_interaction<I: CodexInteraction>(
    interaction: &mut I,
    account_name: &str,
    settings: &MonitorSettings,
    state: &mut AccountState,
    overall_deadline: Instant,
) -> Result<CodexStatus> {
    let observed_at = Local::now();
    let pre_anchor = capture_status_triplet(interaction, account_name, settings, overall_deadline)?;
    let mut status = build_status(
        account_name,
        pre_anchor,
        observed_at,
        AnchorOutcome::NotNeeded,
    );

    if !appears_dormant(&status, observed_at, state) {
        eprintln!("[{account_name}] 5h window already active or cannot be identified as dormant");
        update_account_state(state, &status, observed_at);
        eprintln!("[{account_name}] status captured");
        return Ok(status);
    }

    eprintln!("[{account_name}] 5h window appears dormant");
    eprintln!("[{account_name}] sending anchor turn");
    if let Err(error) = interaction.anchor_turn(settings.model_turn, overall_deadline) {
        let message = format!("anchor model turn failed: {error:#}");
        eprintln!("[{account_name}] {message}");
        status.anchor_outcome = AnchorOutcome::Failed(message);
        update_account_state(state, &status, observed_at);
        eprintln!("[{account_name}] using pre-anchor status");
        return Ok(status);
    }

    eprintln!("[{account_name}] anchor turn completed");
    eprintln!("[{account_name}] refreshing post-anchor status");
    match capture_status_triplet(interaction, account_name, settings, overall_deadline) {
        Ok(rendered) => {
            let post_anchor_observed_at = Local::now();
            let final_status = build_status(
                account_name,
                rendered,
                post_anchor_observed_at,
                AnchorOutcome::AnchoredAutomatically,
            );
            update_account_state(state, &final_status, post_anchor_observed_at);
            eprintln!("[{account_name}] status captured");
            Ok(final_status)
        }
        Err(error) => {
            let message = format!("post-anchor status refresh failed: {error:#}");
            eprintln!("[{account_name}] {message}");
            status.anchor_outcome = AnchorOutcome::Failed(message);
            update_account_state(state, &status, observed_at);
            eprintln!("[{account_name}] using pre-anchor status");
            Ok(status)
        }
    }
}

fn capture_status_triplet<I: CodexInteraction>(
    interaction: &mut I,
    account_name: &str,
    settings: &MonitorSettings,
    overall_deadline: Instant,
) -> Result<String> {
    let mut authoritative = None;
    for refresh in 1..=3 {
        ensure_before(overall_deadline, "overall account monitoring")?;
        eprintln!("[{account_name}] /status refresh {refresh}/3");
        let rendered = interaction.status_card(settings.status, overall_deadline)?;
        if refresh == 3 {
            authoritative = Some(rendered);
        } else {
            sleep_until(settings.refresh_pause, overall_deadline)?;
        }
    }
    authoritative.context("third /status snapshot was not captured")
}

fn build_status(
    account_name: &str,
    rendered_status: String,
    observed_at: DateTime<Local>,
    anchor_outcome: AnchorOutcome,
) -> CodexStatus {
    CodexStatus {
        account_name: account_name.to_owned(),
        five_hour_percent_left: parse_percent_left(&rendered_status, "5h limit:"),
        weekly_percent_left: parse_percent_left(&rendered_status, "Weekly limit:"),
        five_hour_reset_at: parse_five_hour_reset(&rendered_status, observed_at),
        anchor_outcome,
        rendered_status,
    }
}

fn appears_dormant(
    status: &CodexStatus,
    observed_at: DateTime<Local>,
    state: &AccountState,
) -> bool {
    if status.five_hour_percent_left != Some(100) {
        return false;
    }
    let Some(reset_at) = status.five_hour_reset_at else {
        return false;
    };

    let until_reset = reset_at.signed_duration_since(observed_at);
    let near_five_hours =
        until_reset >= ChronoDuration::minutes(295) && until_reset <= ChronoDuration::minutes(305);

    let reset_is_drifting = match (state.previous_five_hour_reset, state.previous_observed_at) {
        (Some(previous_reset), Some(previous_observed)) => {
            let wall_clock_advance = observed_at.signed_duration_since(previous_observed);
            let reset_advance = reset_at.signed_duration_since(previous_reset);
            wall_clock_advance > ChronoDuration::zero()
                && reset_advance > ChronoDuration::zero()
                && (reset_advance - wall_clock_advance).num_seconds().abs() <= 600
        }
        _ => false,
    };

    near_five_hours || reset_is_drifting
}

fn update_account_state(
    state: &mut AccountState,
    status: &CodexStatus,
    observed_at: DateTime<Local>,
) {
    state.previous_five_hour_reset = status.five_hour_reset_at;
    state.previous_observed_at = Some(observed_at);
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

fn parse_five_hour_reset(status: &str, observed_at: DateTime<Local>) -> Option<DateTime<Local>> {
    let line = status.lines().find(|line| line.contains("5h limit:"))?;
    let reset_text = line.split_once("resets ")?.1;
    let reset_text = reset_text.split([')', '│']).next()?.trim();

    let dated_candidate = ["%Y %H:%M on %e %b", "%Y %I:%M %p on %e %b"]
        .into_iter()
        .find_map(|format| {
            NaiveDateTime::parse_from_str(&format!("{} {reset_text}", observed_at.year()), format)
                .ok()
        });

    let Some(mut candidate) = dated_candidate else {
        let time = ["%H:%M", "%I:%M %p"]
            .into_iter()
            .find_map(|format| NaiveTime::parse_from_str(reset_text, format).ok())?;
        let mut candidate = observed_at.date_naive().and_time(time);
        let initial = local_datetime(candidate)?;
        if initial < observed_at - ChronoDuration::minutes(5) {
            candidate = candidate.checked_add_signed(ChronoDuration::days(1))?;
        }
        return local_datetime(candidate);
    };

    let six_months = ChronoDuration::days(183);
    let initial = local_datetime(candidate)?;
    if initial < observed_at - six_months {
        candidate = candidate.with_year(observed_at.year() + 1)?;
    } else if initial > observed_at + six_months {
        candidate = candidate.with_year(observed_at.year() - 1)?;
    }
    local_datetime(candidate)
}

fn local_datetime(value: NaiveDateTime) -> Option<DateTime<Local>> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(earliest, _) => Some(earliest),
        LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        time::{Duration, Instant},
    };

    use anyhow::{Result, bail};
    use chrono::{Datelike, Local, TimeZone};

    use super::{
        AccountState, AnchorOutcome, CodexInteraction, MonitorSettings, appears_dormant,
        build_status, extract_last_complete_card, monitor_interaction, parse_five_hour_reset,
        parse_percent_left,
    };

    struct FakeInteraction {
        cards: VecDeque<String>,
        status_calls: usize,
        anchor_calls: usize,
        anchor_error: bool,
    }

    impl FakeInteraction {
        fn new(cards: impl IntoIterator<Item = String>) -> Self {
            Self {
                cards: cards.into_iter().collect(),
                status_calls: 0,
                anchor_calls: 0,
                anchor_error: false,
            }
        }
    }

    impl CodexInteraction for FakeInteraction {
        fn status_card(&mut self, _: Duration, _: Instant) -> Result<String> {
            self.status_calls += 1;
            self.cards
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no fake card"))
        }

        fn anchor_turn(&mut self, _: Duration, _: Instant) -> Result<()> {
            self.anchor_calls += 1;
            if self.anchor_error {
                bail!("fake anchor failure");
            }
            Ok(())
        }
    }

    fn card(percent: u8, reset: &str, marker: &str) -> String {
        format!(
            "╭────╮\n│ Marker: {marker} │\n│ 5h limit: {percent}% left (resets {reset}) │\n╰────╯"
        )
    }

    fn test_settings() -> MonitorSettings {
        MonitorSettings {
            refresh_pause: Duration::ZERO,
            overall: Duration::from_secs(10),
            ..MonitorSettings::default()
        }
    }

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

    #[test]
    fn parses_reset_time_and_handles_year_rollover() {
        let december = Local.with_ymd_and_hms(2026, 12, 31, 22, 0, 0).unwrap();
        let parsed =
            parse_five_hour_reset("│ 5h limit: 100% left (resets 03:00 on 1 Jan) │", december)
                .unwrap();
        assert_eq!(parsed.year(), 2027);
        assert_eq!(parsed.month(), 1);
        assert_eq!(parsed.day(), 1);
    }

    #[test]
    fn parses_same_day_reset_without_a_date() {
        let morning = Local.with_ymd_and_hms(2026, 9, 8, 8, 30, 0).unwrap();
        let parsed =
            parse_five_hour_reset("│ 5h limit: 100% left (resets 13:30) │", morning).unwrap();
        assert_eq!(
            parsed,
            Local.with_ymd_and_hms(2026, 9, 8, 13, 30, 0).unwrap()
        );
    }

    #[test]
    fn dormant_heuristic_requires_full_capacity_for_primary_signal() {
        let now = Local.with_ymd_and_hms(2026, 9, 8, 8, 0, 0).unwrap();
        let dormant = build_status(
            "Main",
            card(100, "13:00 on 8 Sep", "dormant"),
            now,
            AnchorOutcome::NotNeeded,
        );
        let active = build_status(
            "Main",
            card(99, "13:00 on 8 Sep", "active"),
            now,
            AnchorOutcome::NotNeeded,
        );
        assert!(appears_dormant(&dormant, now, &AccountState::default()));
        assert!(!appears_dormant(&active, now, &AccountState::default()));
    }

    #[test]
    fn reset_drift_is_additional_dormant_evidence() {
        let previous_observed = Local.with_ymd_and_hms(2026, 9, 8, 8, 0, 0).unwrap();
        let now = Local.with_ymd_and_hms(2026, 9, 8, 8, 30, 0).unwrap();
        let status = build_status(
            "Main",
            card(100, "13:20 on 8 Sep", "drifting"),
            now,
            AnchorOutcome::NotNeeded,
        );
        let state = AccountState {
            previous_five_hour_reset: Some(Local.with_ymd_and_hms(2026, 9, 8, 12, 50, 0).unwrap()),
            previous_observed_at: Some(previous_observed),
        };
        assert!(appears_dormant(&status, now, &state));
    }

    #[test]
    fn active_window_uses_three_statuses_without_anchor() {
        let cards = (1..=3).map(|index| card(80, "12:00 on 8 Sep", &index.to_string()));
        let mut fake = FakeInteraction::new(cards);
        let mut state = AccountState::default();
        let status = monitor_interaction(
            &mut fake,
            "Main",
            &test_settings(),
            &mut state,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(fake.status_calls, 3);
        assert_eq!(fake.anchor_calls, 0);
        assert!(status.rendered_status.contains("Marker: 3"));
        assert!(matches!(status.anchor_outcome, AnchorOutcome::NotNeeded));
    }

    #[test]
    fn dormant_window_anchors_once_and_reports_post_anchor_third_status() {
        let reset = (Local::now() + chrono::Duration::hours(5))
            .format("%H:%M on %-d %b")
            .to_string();
        let cards = (1..=6).map(|index| card(100, &reset, &index.to_string()));
        let mut fake = FakeInteraction::new(cards);
        let mut state = AccountState::default();
        let status = monitor_interaction(
            &mut fake,
            "Main",
            &test_settings(),
            &mut state,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(fake.status_calls, 6);
        assert_eq!(fake.anchor_calls, 1);
        assert!(status.rendered_status.contains("Marker: 6"));
        assert!(matches!(
            status.anchor_outcome,
            AnchorOutcome::AnchoredAutomatically
        ));
    }

    #[test]
    fn anchor_failure_returns_best_pre_anchor_status() {
        let reset = (Local::now() + chrono::Duration::hours(5))
            .format("%H:%M on %-d %b")
            .to_string();
        let mut fake =
            FakeInteraction::new((1..=3).map(|index| card(100, &reset, &index.to_string())));
        fake.anchor_error = true;
        let status = monitor_interaction(
            &mut fake,
            "Main",
            &test_settings(),
            &mut AccountState::default(),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(fake.status_calls, 3);
        assert_eq!(fake.anchor_calls, 1);
        assert!(status.rendered_status.contains("Marker: 3"));
        assert!(matches!(status.anchor_outcome, AnchorOutcome::Failed(_)));
    }
}
