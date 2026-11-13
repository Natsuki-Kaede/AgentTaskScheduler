//! Delivers session reminders through agent hooks.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client;
use crate::domain::job::JobState;
use crate::paths::Paths;
use crate::protocol::Reminder;

const REMINDER_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_CHARS: usize = 120;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Dsh,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    PreToolUse,
    Stop,
    UserPromptSubmit,
}

#[derive(Debug, PartialEq)]
pub enum Answer {
    Nothing,
    Json(Value),
}

pub async fn run(paths: &Paths, event: Event) -> Result<Answer> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let input: Value = serde_json::from_str(&text).context("reading the hook input")?;
    Ok(match event {
        Event::PreToolUse => Answer::Nothing,
        Event::Stop => stop(paths, &input).await,
        Event::UserPromptSubmit => user_prompt_submit(paths, &input).await,
    })
}

async fn reminders(
    paths: &Paths,
    method: &str,
    input: &Value,
    what: &str,
) -> Option<Vec<Reminder>> {
    let session = input["session_id"].as_str()?;
    let path = format!("/v1/sessions/{session}/{what}");
    match client::call_with_timeout(paths, method, &path, None, REMINDER_TIMEOUT).await {
        Ok((answer, _)) if answer.status == 200 => serde_json::from_value(answer.body).ok(),
        Ok((answer, _)) => {
            eprintln!("ats: {path}: daemon answered {}", answer.status);
            None
        }
        Err(e) => {
            eprintln!("ats: {path}: {e:#}");
            None
        }
    }
}

fn line(r: &Reminder) -> String {
    let mut command: String = r.command.chars().take(COMMAND_CHARS).collect();
    if command.len() < r.command.len() {
        command.push('…');
    }
    let code = r
        .exit_code
        .map(|c| format!(", exit code {c}"))
        .unwrap_or_default();
    let mut text = format!(
        "- {} (job {}) {}{code}: {command}",
        r.submission, r.job, r.state
    );
    match r.state {
        JobState::Blocked => text.push_str(&format!(
            "\n  Action required: a predecessor failed; this job will not run automatically. Inspect `ats why {}` and the predecessor's result. Resolve the failure before choosing `ats unblock {}` or `ats cancel {}`.",
            r.submission, r.job, r.submission
        )),
        JobState::CapacityInsufficient => text.push_str(&format!(
            "\n  Action required: the request exceeds its memory budget. Inspect `ats why {}` and adjust the work or cancel it; repeated waiting alone will not resolve this state.",
            r.submission
        )),
        _ => {}
    }
    text
}

async fn reminders_with_attention(
    paths: &Paths,
    input: &Value,
    method: &str,
    what: &str,
) -> Vec<Reminder> {
    let (normal, attention) = tokio::join!(
        reminders(paths, method, input, what),
        reminders(paths, "POST", input, "attention"),
    );
    merge_reminders(normal.unwrap_or_default(), attention.unwrap_or_default())
}

fn merge_reminders(mut normal: Vec<Reminder>, attention: Vec<Reminder>) -> Vec<Reminder> {
    for notice in attention {
        match normal
            .iter_mut()
            .find(|row| row.submission == notice.submission)
        {
            Some(row) => *row = notice,
            None => normal.push(notice),
        }
    }
    normal
}

async fn stop(paths: &Paths, input: &Value) -> Answer {
    if input["stop_hook_active"] == true {
        return Answer::Nothing;
    }
    let running = reminders_with_attention(paths, input, "GET", "running").await;
    if running.is_empty() {
        return Answer::Nothing;
    }
    let lines: Vec<String> = running.iter().map(line).collect();
    Answer::Json(json!({
        "decision": "block",
        "reason": format!(
            "ats: submissions from this session are still running or need attention. Use `ats wait <submission>` and resolve any blocked work before continuing a goal that needs its result. Only use `ats detach <submission>` when deferring the result until a later user turn is acceptable: detached work does not wake a stopped agent.\n{}",
            lines.join("\n")
        ),
    }))
}

async fn user_prompt_submit(paths: &Paths, input: &Value) -> Answer {
    let ended = reminders_with_attention(paths, input, "POST", "ended").await;
    if ended.is_empty() {
        return Answer::Nothing;
    }
    let lines: Vec<String> = ended.iter().map(line).collect();
    Answer::Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": format!(
                "ats: submissions from this session ended since your last turn or need attention; `ats wait <submission>` shows the result. Resolve blocked work rather than waiting for it to start automatically.\n{}",
                lines.join("\n")
            ),
        }
    }))
}

pub fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("s-1_a.b"), "s-1_a.b");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn claimed_attention_replaces_an_older_running_snapshot() {
        let row = |submission: &str, state| Reminder {
            submission: submission.into(),
            job: "j_job".into(),
            state,
            exit_code: None,
            command: "make".into(),
        };
        let notices = merge_reminders(
            vec![
                row("s_first", JobState::Queued),
                row("s_other", JobState::Running),
            ],
            vec![
                row("s_first", JobState::Blocked),
                row("s_detached", JobState::CapacityInsufficient),
            ],
        );
        let rendered: Vec<_> = notices.iter().map(line).collect();
        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].contains("s_first (job j_job) blocked"));
        assert!(rendered[0].contains("Action required"));
        assert!(rendered[1].contains("s_other (job j_job) running"));
        assert!(rendered[2].contains("s_detached (job j_job) capacity_insufficient"));
    }
}
