use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{accounts::Account, codex::MonitorSettings};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub monitor: MonitorConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    pub terminal_rows: u16,
    pub terminal_cols: u16,
    pub startup_timeout_seconds: u64,
    pub status_timeout_seconds: u64,
    pub model_turn_timeout_seconds: u64,
    pub refresh_pause_seconds: u64,
    pub overall_timeout_seconds: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            terminal_rows: 80,
            terminal_cols: 120,
            startup_timeout_seconds: 45,
            status_timeout_seconds: 25,
            model_turn_timeout_seconds: 90,
            refresh_pause_seconds: 4,
            overall_timeout_seconds: 300,
        }
    }
}

impl MonitorConfig {
    pub fn settings(&self) -> MonitorSettings {
        MonitorSettings {
            terminal_rows: self.terminal_rows,
            terminal_cols: self.terminal_cols,
            startup: Duration::from_secs(self.startup_timeout_seconds),
            status: Duration::from_secs(self.status_timeout_seconds),
            model_turn: Duration::from_secs(self.model_turn_timeout_seconds),
            refresh_pause: Duration::from_secs(self.refresh_pause_seconds),
            overall: Duration::from_secs(self.overall_timeout_seconds),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub interval_minutes: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            interval_minutes: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordConfig {
    pub webhook_env: String,
    pub request_timeout_seconds: u64,
    pub status_font_path: PathBuf,
    pub status_font_size: f32,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            webhook_env: "DISCORD_WEBHOOK_URL".to_owned(),
            request_timeout_seconds: 20,
            status_font_path: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".into(),
            status_font_size: 18.0,
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.accounts.is_empty() {
            bail!("configuration must contain at least one [[accounts]] entry");
        }
        let mut names = HashSet::new();
        for account in &self.accounts {
            if account.name.trim().is_empty() {
                bail!("account name cannot be empty");
            }
            if !names.insert(account.name.to_lowercase()) {
                bail!("duplicate account name: {}", account.name);
            }
            if account.executable.as_os_str().is_empty() {
                bail!("account {} has an empty executable", account.name);
            }
        }

        let interval = self.schedule.interval_minutes;
        if interval == 0 || interval > 60 || 60 % interval != 0 {
            bail!("schedule.interval_minutes must be a positive divisor of 60");
        }
        if self.monitor.terminal_rows < 20 || self.monitor.terminal_cols < 60 {
            bail!("monitor terminal size must be at least 20 rows by 60 columns");
        }
        if self.monitor.startup_timeout_seconds == 0
            || self.monitor.status_timeout_seconds == 0
            || self.monitor.model_turn_timeout_seconds == 0
            || self.monitor.overall_timeout_seconds == 0
        {
            bail!("monitor timeout values must be greater than zero");
        }
        if self.discord.webhook_env.trim().is_empty() {
            bail!("discord.webhook_env cannot be empty");
        }
        if self.discord.request_timeout_seconds == 0 {
            bail!("discord.request_timeout_seconds must be greater than zero");
        }
        if !(8.0..=48.0).contains(&self.discord.status_font_size) {
            bail!("discord.status_font_size must be between 8 and 48");
        }
        if !self.discord.status_font_path.is_file() {
            bail!(
                "discord.status_font_path is not a file: {}",
                self.discord.status_font_path.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn parses_accounts_and_defaults_optional_sections() {
        let config: AppConfig = toml::from_str(
            r#"
                [[accounts]]
                name = "Alt"
                executable = "codex"
                environment = { CODEX_HOME = "/tmp/codex-alt" }
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.accounts[0].environment["CODEX_HOME"],
            "/tmp/codex-alt"
        );
        assert_eq!(config.schedule.interval_minutes, 30);
        assert_eq!(config.monitor.terminal_cols, 120);
    }

    #[test]
    fn rejects_duplicate_account_names() {
        let config: AppConfig = toml::from_str(
            r#"
                [[accounts]]
                name = "Main"
                executable = "codex"
                [[accounts]]
                name = "main"
                executable = "codex"
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
