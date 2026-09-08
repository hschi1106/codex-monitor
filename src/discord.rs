use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::Serialize;

use crate::codex::AccountResult;
use crate::status_image::render_status_png;

const DISCORD_LIMIT: usize = 2_000;
const SAFE_CHUNK_LIMIT: usize = 1_900;

#[derive(Serialize)]
struct WebhookPayload<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: [String; 0],
}

pub struct PreparedWebhook {
    pub content: String,
    pub attachments: Option<StatusAttachments>,
}

pub struct StatusAttachments {
    pub png_name: String,
    pub png: Vec<u8>,
    pub text_name: String,
    pub text: Vec<u8>,
}

pub fn format_report(timestamp: &DateTime<Local>, results: &[AccountResult]) -> String {
    let mut report = format!(
        "**Codex Usage Monitor**\nTimestamp: {}\n",
        timestamp.format("%Y-%m-%d %H:%M %Z")
    );

    for result in results {
        report.push('\n');
        report.push_str(&format_account_section(result));
        report.push('\n');
    }
    report.trim_end().to_owned()
}

pub fn format_account_report(timestamp: &DateTime<Local>, result: &AccountResult) -> String {
    format!(
        "**Codex Usage Monitor**\nTimestamp: {}\n\n{}",
        timestamp.format("%Y-%m-%d %H:%M %Z"),
        format_account_section(result)
    )
}

pub fn prepare_account_webhook(
    timestamp: &DateTime<Local>,
    result: &AccountResult,
    font_path: &Path,
    font_size: f32,
) -> Result<PreparedWebhook> {
    match result {
        AccountResult::Success(status) => {
            let png = render_status_png(&status.rendered_status, font_path, font_size)?;
            let slug = filename_slug(&status.account_name);
            Ok(PreparedWebhook {
                content: status_message_title(status),
                attachments: Some(StatusAttachments {
                    png_name: format!("{slug}-status.png"),
                    png,
                    text_name: format!("{slug}-status.txt"),
                    text: format!("{}\n", status.rendered_status).into_bytes(),
                }),
            })
        }
        AccountResult::Failure { .. } => Ok(PreparedWebhook {
            content: format_account_report(timestamp, result),
            attachments: None,
        }),
    }
}

fn status_message_title(status: &crate::codex::CodexStatus) -> String {
    let mut title = format!(
        "**Codex Usage Monitor — {}**",
        escape_markdown(&status.account_name)
    );
    if let Some(error) = status.anchor_outcome.failure_message() {
        title.push_str(
            "\n⚠️ Automatic 5h-window anchor failed; showing the best available status: ",
        );
        title.push_str(&escape_markdown(error));
    }
    title
}

fn filename_slug(account_name: &str) -> String {
    let slug: String = account_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "account".to_owned()
    } else {
        slug.to_owned()
    }
}

fn format_account_section(result: &AccountResult) -> String {
    match result {
        AccountResult::Success(status) => {
            let warning = status
                .anchor_outcome
                .failure_message()
                .map(|error| {
                    format!(
                        "⚠️ Automatic 5h-window anchor failed; showing the best available status: {}\n",
                        escape_markdown(error)
                    )
                })
                .unwrap_or_default();
            format!(
                "**{}**\n{warning}```text\n{}\n```",
                escape_markdown(&status.account_name),
                escape_code_fence(&status.rendered_status)
            )
        }
        AccountResult::Failure {
            account_name,
            error,
        } => format!(
            "**{}**\n```text\nERROR: {}\n```",
            escape_markdown(account_name),
            escape_code_fence(error)
        ),
    }
}

/// Discord caps message content at 2,000 characters. This splitter first keeps
/// account sections intact and, for a large status card, closes and reopens the
/// code fence at line boundaries. No source lines are discarded.
pub fn split_report(report: &str) -> Vec<String> {
    if report.chars().count() <= DISCORD_LIMIT {
        return vec![report.to_owned()];
    }

    let lines = expand_long_lines(report, SAFE_CHUNK_LIMIT - 16);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut in_code_block = false;

    for line in lines {
        let is_fence = line.starts_with("```");
        let closing_overhead = if in_code_block && !is_fence { 4 } else { 0 };
        let addition = line.chars().count() + usize::from(!current.is_empty());

        if !current.is_empty()
            && current.chars().count() + addition + closing_overhead > SAFE_CHUNK_LIMIT
        {
            if in_code_block {
                current.push_str("\n```");
            }
            chunks.push(current);
            current = if in_code_block {
                "**Codex Usage Monitor (continued)**\n```text".to_owned()
            } else {
                "**Codex Usage Monitor (continued)**".to_owned()
            };
        }

        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&line);
        if is_fence {
            in_code_block = !in_code_block;
        }
    }

    if !current.is_empty() {
        if in_code_block {
            current.push_str("\n```");
        }
        chunks.push(current);
    }

    debug_assert!(
        chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= DISCORD_LIMIT)
    );
    chunks
}

pub async fn send_webhook(
    client: &reqwest::Client,
    webhook_url: &str,
    content: &str,
) -> Result<()> {
    let response = client
        .post(webhook_url)
        .json(&WebhookPayload {
            content,
            allowed_mentions: AllowedMentions { parse: [] },
        })
        .send()
        .await
        .context("Discord webhook request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(500).collect();
        bail!("Discord webhook returned HTTP {status}: {excerpt}");
    }
    Ok(())
}

pub async fn send_prepared_webhook(
    client: &reqwest::Client,
    webhook_url: &str,
    message: PreparedWebhook,
) -> Result<()> {
    let Some(attachments) = message.attachments else {
        return send_webhook(client, webhook_url, &message.content).await;
    };
    let payload_json = serde_json::to_string(&WebhookPayload {
        content: &message.content,
        allowed_mentions: AllowedMentions { parse: [] },
    })?;
    let form = reqwest::multipart::Form::new()
        .text("payload_json", payload_json)
        .part(
            "files[0]",
            reqwest::multipart::Part::bytes(attachments.png)
                .file_name(attachments.png_name)
                .mime_str("image/png")?,
        )
        .part(
            "files[1]",
            reqwest::multipart::Part::bytes(attachments.text)
                .file_name(attachments.text_name)
                .mime_str("text/plain; charset=utf-8")?,
        );
    let response = client
        .post(webhook_url)
        .multipart(form)
        .send()
        .await
        .context("Discord webhook request with attachments failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(500).collect();
        bail!("Discord webhook returned HTTP {status}: {excerpt}");
    }
    Ok(())
}

fn escape_markdown(text: &str) -> String {
    text.replace('\\', "\\\\").replace('*', "\\*")
}

fn escape_code_fence(text: &str) -> String {
    text.replace("```", "``\u{200b}`")
}

fn expand_long_lines(text: &str, maximum: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        if line.chars().count() <= maximum {
            result.push(line.to_owned());
            continue;
        }
        let characters: Vec<char> = line.chars().collect();
        for part in characters.chunks(maximum) {
            result.push(part.iter().collect());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use chrono::Local;

    use super::{
        DISCORD_LIMIT, format_account_report, format_report, prepare_account_webhook,
        send_prepared_webhook, send_webhook, split_report,
    };
    use crate::codex::{AccountResult, AnchorOutcome, CodexStatus};

    #[test]
    fn report_includes_successes_and_failures() {
        let results = vec![
            AccountResult::Success(CodexStatus {
                account_name: "Main".to_owned(),
                rendered_status: "╭──╮\n│ok│\n╰──╯".to_owned(),
                five_hour_percent_left: Some(50),
                weekly_percent_left: Some(80),
                five_hour_reset_at: None,
                anchor_outcome: AnchorOutcome::NotNeeded,
            }),
            AccountResult::Failure {
                account_name: "Alt".to_owned(),
                error: "startup timeout".to_owned(),
            },
        ];
        let report = format_report(&Local::now(), &results);
        assert!(report.contains("**Main**"));
        assert!(report.contains("│ok│"));
        assert!(report.contains("**Alt**"));
        assert!(report.contains("ERROR: startup timeout"));
    }

    #[test]
    fn normal_account_reports_are_independent_discord_messages() {
        let timestamp = Local::now();
        let results = [
            AccountResult::Success(CodexStatus {
                account_name: "Main".to_owned(),
                rendered_status: "Main status row\n".repeat(60),
                five_hour_percent_left: None,
                weekly_percent_left: None,
                five_hour_reset_at: None,
                anchor_outcome: AnchorOutcome::NotNeeded,
            }),
            AccountResult::Success(CodexStatus {
                account_name: "Alt".to_owned(),
                rendered_status: "Alt status row\n".repeat(60),
                five_hour_percent_left: None,
                weekly_percent_left: None,
                five_hour_reset_at: None,
                anchor_outcome: AnchorOutcome::NotNeeded,
            }),
        ];

        let messages: Vec<String> = results
            .iter()
            .map(|result| format_account_report(&timestamp, result))
            .collect();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("**Main**"));
        assert!(!messages[0].contains("**Alt**"));
        assert!(messages[1].contains("**Alt**"));
        assert!(!messages[1].contains("**Main**"));
        assert!(
            messages
                .iter()
                .all(|message| split_report(message).len() == 1)
        );
    }

    #[test]
    fn prepares_title_with_png_and_complete_text() {
        let result = AccountResult::Success(CodexStatus {
            account_name: "Main Account".to_owned(),
            rendered_status: "╭────────────╮\n│ Model: x   │\n│ 5h limit: 60% left │\n│ Future: yes │\n╰────────────╯".to_owned(),
            five_hour_percent_left: Some(60),
            weekly_percent_left: None,
            five_hour_reset_at: None,
            anchor_outcome: AnchorOutcome::NotNeeded,
        });
        let prepared = prepare_account_webhook(
            &Local::now(),
            &result,
            std::path::Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
            18.0,
        )
        .unwrap();
        assert_eq!(prepared.content, "**Codex Usage Monitor — Main Account**");
        assert!(!prepared.content.contains("Model"));
        assert!(!prepared.content.contains("Timestamp"));
        let attachments = prepared.attachments.unwrap();
        assert_eq!(attachments.png_name, "main-account-status.png");
        assert!(attachments.png.starts_with(b"\x89PNG"));
        assert!(
            String::from_utf8(attachments.text)
                .unwrap()
                .contains("Future: yes")
        );
    }

    #[test]
    fn oversized_reports_split_without_losing_status_lines() {
        let mut report = String::from("**Codex Usage Monitor**\n```text\n");
        for index in 0..150 {
            report.push_str(&format!("unique-status-row-{index:03}\n"));
        }
        report.push_str("```");
        let chunks = split_report(&report);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= DISCORD_LIMIT)
        );
        for index in 0..150 {
            let needle = format!("unique-status-row-{index:03}");
            assert!(chunks.iter().any(|chunk| chunk.contains(&needle)));
        }
    }

    #[tokio::test]
    async fn webhook_posts_json_without_mentions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let count = connection.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("POST /hook HTTP/1.1"));
            assert!(request.contains(r#""content":"hello""#));
            assert!(request.contains(r#""allowed_mentions":{"parse":[]}"#));
            connection
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let client = reqwest::Client::new();
        send_webhook(&client, &format!("http://{address}/hook"), "hello")
            .await
            .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn webhook_uploads_png_and_complete_text_attachments() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let expected_length = loop {
                let count = connection.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap();
                break header_end + 4 + content_length;
            };
            while request.len() < expected_length {
                let count = connection.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.contains("multipart/form-data"));
            assert!(request_text.contains("main-status.png"));
            assert!(request_text.contains("main-status.txt"));
            assert!(request_text.contains("Future field: preserved"));
            assert!(request.windows(4).any(|bytes| bytes == b"\x89PNG"));
            connection
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let result = AccountResult::Success(CodexStatus {
            account_name: "Main".to_owned(),
            rendered_status: "╭─────────────────────────╮\n│ Model: test             │\n│ Future field: preserved │\n╰─────────────────────────╯".to_owned(),
            five_hour_percent_left: None,
            weekly_percent_left: None,
            five_hour_reset_at: None,
            anchor_outcome: AnchorOutcome::NotNeeded,
        });
        let prepared = prepare_account_webhook(
            &Local::now(),
            &result,
            std::path::Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
            18.0,
        )
        .unwrap();
        let client = reqwest::Client::new();
        send_prepared_webhook(&client, &format!("http://{address}/hook"), prepared)
            .await
            .unwrap();
        server.join().unwrap();
    }
}
