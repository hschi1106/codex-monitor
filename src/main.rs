mod accounts;
mod codex;
mod config;
mod discord;
mod scheduler;
mod status_image;

use std::{collections::HashMap, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::Local;
use codex::{AccountResult, AccountState};
use config::AppConfig;

#[derive(Debug, Clone)]
struct Options {
    once: bool,
    no_discord: bool,
    config_path: PathBuf,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut options = Self {
            once: false,
            no_discord: false,
            config_path: "config.toml".into(),
        };
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--once" => options.once = true,
                "--no-discord" => options.no_discord = true,
                "--config" => {
                    options.config_path = arguments
                        .next()
                        .context("--config requires a file path")?
                        .into();
                }
                "-h" | "--help" => {
                    println!(
                        "codex-monitor\n\nUSAGE:\n    codex-monitor [--once] [--no-discord] [--config PATH]\n\nOPTIONS:\n    --once          Run immediately, then exit\n    --no-discord    Print locally without using a Discord webhook\n    --config PATH   Read this TOML file (default: config.toml)\n"
                    );
                    std::process::exit(0);
                }
                unknown => bail!("unknown argument: {unknown}"),
            }
        }
        Ok(options)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse()?;
    let config = AppConfig::load(&options.config_path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.discord.request_timeout_seconds))
        .build()
        .context("failed to create HTTP client")?;

    if options.once {
        let mut account_states = HashMap::new();
        run_cycle(&client, &config, options.no_discord, &mut account_states).await;
        return Ok(());
    }

    eprintln!(
        "codex-monitor started; using {} and a {}-minute wall-clock schedule",
        options.config_path.display(),
        config.schedule.interval_minutes
    );
    let mut account_states = HashMap::new();
    loop {
        let next = scheduler::next_boundary(Local::now(), config.schedule.interval_minutes);
        let delay = scheduler::duration_until(next);
        eprintln!(
            "next monitoring cycle at {}",
            next.format("%Y-%m-%d %H:%M:%S %Z")
        );
        tokio::time::sleep(delay).await;
        run_cycle(&client, &config, options.no_discord, &mut account_states).await;
    }
}

async fn run_cycle(
    client: &reqwest::Client,
    config: &AppConfig,
    no_discord: bool,
    account_states: &mut HashMap<String, AccountState>,
) {
    let mut results = Vec::new();
    let settings = config.monitor.settings();

    for account in &config.accounts {
        let account = account.clone();
        let account_name = account.name.clone();
        let worker_settings = settings.clone();
        let mut account_state = account_states.remove(&account_name).unwrap_or_default();
        let task = tokio::task::spawn_blocking(move || {
            let result = codex::monitor_account(&account, &worker_settings, &mut account_state);
            (result, account_state)
        });

        let result =
            match tokio::time::timeout(settings.overall + Duration::from_secs(5), task).await {
                Ok(Ok((Ok(status), state))) => {
                    account_states.insert(account_name.clone(), state);
                    AccountResult::Success(status)
                }
                Ok(Ok((Err(error), state))) => {
                    account_states.insert(account_name.clone(), state);
                    eprintln!("[{account_name}] failed: {error:#}");
                    AccountResult::Failure {
                        account_name,
                        error: format!("{error:#}"),
                    }
                }
                Ok(Err(error)) => {
                    eprintln!("[{account_name}] monitor worker failed: {error}");
                    AccountResult::Failure {
                        account_name,
                        error: format!("monitor worker failed: {error}"),
                    }
                }
                Err(_) => {
                    let error = format!("overall monitoring timeout after {:?}", settings.overall);
                    eprintln!("[{account_name}] {error}");
                    AccountResult::Failure {
                        account_name,
                        error,
                    }
                }
            };
        results.push(result);
    }

    let timestamp = Local::now();
    let report = discord::format_report(&timestamp, &results);
    println!("{report}");

    if no_discord {
        eprintln!("Discord delivery disabled by --no-discord");
        return;
    }

    let webhook_url = match std::env::var(&config.discord.webhook_env) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!(
                "{} is not set; report was only printed locally",
                config.discord.webhook_env
            );
            return;
        }
    };

    let mut message_number = 0;
    for result in &results {
        let account_name = result.account_name().to_owned();
        match discord::prepare_account_webhook(
            &timestamp,
            result,
            &config.discord.status_font_path,
            config.discord.status_font_size,
        ) {
            Ok(message) => {
                message_number += 1;
                if let Err(error) =
                    discord::send_prepared_webhook(client, &webhook_url, message).await
                {
                    eprintln!(
                        "[{account_name}] Discord message {message_number} failed: {error:#}"
                    );
                } else {
                    eprintln!("[{account_name}] Discord report sent");
                }
            }
            Err(error) => {
                eprintln!("could not render Discord status image: {error:#}; using text fallback");
                let account_report = discord::format_account_report(&timestamp, result);
                for message in discord::split_report(&account_report) {
                    message_number += 1;
                    if let Err(error) = discord::send_webhook(client, &webhook_url, &message).await
                    {
                        eprintln!(
                            "[{account_name}] Discord fallback message {message_number} failed: {error:#}"
                        );
                    } else {
                        eprintln!("[{account_name}] Discord report sent");
                    }
                }
            }
        }
    }
}
