//! Manages agent instructions, reminder hooks and the daemon login service.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::{Agent, quote};
use crate::paths::Paths;

mod dsh;
pub use dsh::DshConfig;
mod service;

pub enum Confirm<'a> {
    Ask(&'a mut dyn BufRead),
    DryRun,
    Yes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Service {
    #[cfg(test)]
    Skip,
    Ensure,
    Restart,
}

const BEGIN: &str = "<!-- ats:begin -->";
const END: &str = "<!-- ats:end -->";
const INCLUDE: &str = "@ats.md";
const INSTRUCTIONS: &str = "\
## Agent Task Scheduler (ats)

ats is a local queue for heavy shell commands. It starts each command when the machine has memory for it, so parallel agent sessions do not slow each other down.

Send a command through ats when it can use much CPU, memory, disk or network, or when it can run for more than about 30 seconds:

- builds, test suites, packaging and large lint runs: `cargo build`, `cargo test`, `npm run build`, `make`, `xcodebuild`, `gradle`, `pytest`
- dependency installs, large downloads and container builds: `npm install`, `pip install`, `docker build`
- model training or inference, data processing, and conversion or compression of large files

Write it as `ats run -- <program> <args>` or `ats run --sh '<shell command>'`. Put a `cd` before `ats run`, not in it: `cd sub && ats run -- make`.

Run other commands directly: `ls`, `cat`, `cd`, `mkdir`, `cp`, `mv`, `git status`, `git diff`, small `git` operations, `grep` or `rg` in a small tree, `ping`, version and help output, editors and other interactive commands, and servers that do not stop.

- When the command ends in 90 seconds or less, ats gives its output and exit code unchanged.
- Exit code 75 and an `[ats] submission ...` line: read the job state. While it is running or queued and can progress, use `ats wait <submission>` to get its output and exit code. Each call waits for up to 90 seconds. Repeat while your current goal needs the result and the job can progress. `ats detach <submission>` allows this turn to end and reports results or blocked work at a later turn; it does not wake a stopped agent. Detach only when deferring progress until another user turn is acceptable.
- A `capacity_insufficient` job exceeds the current memory budget. Use `ats why <submission>` to see the estimate and limit, then reduce the workload or cancel it. Repeated waiting cannot fix an unchanged capacity limit.
- A `blocked` job cannot start because a predecessor failed. Inspect `ats why <submission>` and the predecessor's result, resolve the failure, then choose `ats unblock <submission>` or `ats cancel <submission>`. Repeated waiting does not repair a failed dependency.
- Interpret 76 as an `--after` predecessor failure, 77 as rejection and 78 as an ats failure only when ats's diagnostic output confirms that cause. Completed commands return their own exit codes unchanged and may use the same numbers.
- Run `ats why <submission>` to see why a command waits. Do not use `ats run --now` unless the user asks to run something immediately.";

const EVENTS: [(&str, &str); 2] = [("Stop", "stop"), ("UserPromptSubmit", "user-prompt-submit")];
const OLD_EVENTS: [(&str, &str); 1] = [("PreToolUse", "pre-tool-use")];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Moved {
    pub group: usize,
    pub index: usize,
    pub hook: Value,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
pub struct Record {
    #[serde(default)]
    pub agents: BTreeMap<String, Vec<Moved>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dsh_relinks: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl Record {
    pub fn load(paths: &Paths) -> Result<Record> {
        match std::fs::read_to_string(&paths.installed) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", paths.installed.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.installed.display())),
        }
    }
}

fn name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Dsh => "dsh",
    }
}

fn files(home: &Path, agent: Agent) -> (PathBuf, PathBuf) {
    match agent {
        Agent::Claude => (
            home.join(".claude/settings.json"),
            home.join(".claude/CLAUDE.md"),
        ),
        Agent::Dsh => (
            home.join(".dsh/profiles/desktop/package.json"),
            home.join(".dsh/AGENTS.md"),
        ),
        Agent::Codex => (
            home.join(".codex/hooks.json"),
            home.join(".codex/AGENTS.md"),
        ),
    }
}

fn include_file(home: &Path, agent: Agent) -> PathBuf {
    match agent {
        Agent::Claude => home.join(".claude/ats.md"),
        Agent::Codex => home.join(".codex/ATS.md"),
        Agent::Dsh => home.join(".dsh/ATS.md"),
    }
}

pub fn agents(home: &Path, named: &[Agent], dsh: &DshConfig<'_>) -> Vec<Agent> {
    if !named.is_empty() {
        return named.to_vec();
    }
    [Agent::Claude, Agent::Codex, Agent::Dsh]
        .into_iter()
        .filter(|a| match a {
            Agent::Dsh => dsh.manifest().is_file(),
            _ => files(home, *a).0.parent().is_some_and(Path::is_dir),
        })
        .collect()
}

fn is_ats(hook: &Value, agent: Agent) -> bool {
    let command = hook["command"].as_str().unwrap_or_default();
    command.contains("ats")
        && EVENTS
            .iter()
            .chain(&OLD_EVENTS)
            .any(|(_, e)| command.ends_with(&format!(" hook {} {e}", name(agent))))
}

fn groups<'a>(doc: &'a mut Value, event: &str) -> Option<&'a mut Vec<Value>> {
    doc.get_mut("hooks")?.get_mut(event)?.as_array_mut()
}

fn tidy(doc: &mut Value) {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if hooks.is_empty() {
        doc.as_object_mut().expect("an object").remove("hooks");
    }
}

fn remove_ats(doc: &mut Value, agent: Agent) {
    for (event, _) in EVENTS.iter().chain(&OLD_EVENTS) {
        for group in groups(doc, event).into_iter().flatten() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.retain(|h| !is_ats(h, agent));
            }
        }
    }
    tidy(doc);
}

fn add_ats(doc: &mut Value, agent: Agent, ats: &Path) {
    let hooks = doc
        .as_object_mut()
        .expect("an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    for (event, hook_event) in EVENTS {
        let command = format!(
            "{} hook {} {hook_event}",
            quote(&ats.to_string_lossy()),
            name(agent)
        );
        let list = hooks
            .as_object_mut()
            .expect("an object")
            .entry(event)
            .or_insert_with(|| json!([]));
        list.as_array_mut()
            .expect("an array")
            .push(json!({ "hooks": [{ "type": "command", "command": command }] }));
    }
}

fn restore(doc: &mut Value, moved: &[Moved]) {
    let mut order: Vec<&Moved> = moved.iter().collect();
    order.sort_by_key(|m| (m.group, m.index));
    for m in order {
        let hooks = doc
            .as_object_mut()
            .expect("an object")
            .entry("hooks")
            .or_insert_with(|| json!({}));
        let groups = hooks
            .as_object_mut()
            .expect("an object")
            .entry("PreToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("an array");
        match groups.get_mut(m.group).filter(|g| g["matcher"] == "Bash") {
            Some(group) => {
                let list = group["hooks"].as_array_mut().expect("an array");
                list.insert(m.index.min(list.len()), m.hook.clone());
            }
            None => groups.insert(
                m.group.min(groups.len()),
                json!({ "matcher": "Bash", "hooks": [m.hook] }),
            ),
        }
    }
}

fn without_block(text: &str) -> String {
    match (text.find(BEGIN), text.find(END)) {
        (Some(b), Some(e)) if b < e => {
            let head = text[..b].trim_end_matches('\n');
            let tail = text[e + END.len()..].trim_start_matches('\n');
            match (head.is_empty(), tail.is_empty()) {
                (true, _) => tail.to_string(),
                (false, true) => format!("{head}\n"),
                (false, false) => format!("{head}\n\n{tail}"),
            }
        }
        _ => text.to_string(),
    }
}

fn with_block(text: &str, block: &str) -> String {
    let base = without_block(text);
    if base.trim().is_empty() {
        block.to_string()
    } else {
        format!("{}\n\n{block}", base.trim_end_matches('\n'))
    }
}

pub fn instructions_block() -> String {
    format!("{BEGIN}\n{INSTRUCTIONS}\n{END}\n")
}

fn compact_block(reference: &Path) -> String {
    format!(
        "{BEGIN}\n\
Use `ats run -- <program> <args>` or `ats run --sh '<command>'` for resource-heavy or >30s commands. Put `cd` before ats; small or interactive commands and persistent servers bypass ats.\n\
For queued jobs or ats errors, read and follow `{}`. Use `--now` only when asked.\n\
{END}\n",
        reference.display()
    )
}

fn without_include(text: &str) -> String {
    let base = without_block(text);
    let line = format!("{INCLUDE}\n");
    if base == line {
        return String::new();
    }
    for added in [format!("\n\n{line}"), format!("\n{line}")] {
        if let Some(i) = base.find(&added) {
            return format!("{}\n{}", &base[..i], &base[i + added.len()..]);
        }
    }
    base
}

fn with_include(text: &str) -> String {
    let base = without_include(text);
    if base.trim().is_empty() {
        format!("{INCLUDE}\n")
    } else {
        format!("{}\n\n{INCLUDE}\n", base.trim_end_matches('\n'))
    }
}

fn claude_asks(doc: &Value) -> bool {
    !matches!(
        doc["permissions"]["defaultMode"].as_str(),
        Some("auto" | "bypassPermissions")
    )
}

struct Change {
    path: PathBuf,
    old: Option<String>,
    new: Option<String>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn parse(path: &Path, text: Option<&str>) -> Result<Value> {
    let doc: Value = match text {
        Some(t) if !t.trim().is_empty() => serde_json::from_str(t)
            .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?,
        _ => json!({}),
    };
    if !doc.is_object() {
        bail!("{} does not hold a JSON object", path.display());
    }
    Ok(doc)
}

fn json_text(doc: &Value) -> String {
    serde_json::to_string_pretty(doc).expect("a JSON value") + "\n"
}

fn ask(input: &mut dyn BufRead, out: &mut impl Write, question: &str) -> Result<bool> {
    write!(out, "{question} [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

#[allow(clippy::too_many_arguments)]
pub fn install<W: Write>(
    paths: &Paths,
    home: &Path,
    dsh: &DshConfig<'_>,
    agents: &[Agent],
    ats: &Path,
    service: Service,
    confirm: Confirm,
    out: &mut W,
) -> Result<bool> {
    #[cfg(test)]
    if service == Service::Skip {
        return install_with(
            paths,
            home,
            dsh,
            agents,
            ats,
            false,
            None::<fn(bool, &mut W) -> Result<()>>,
            confirm,
            out,
        );
    }
    install_with(
        paths,
        home,
        dsh,
        agents,
        ats,
        service == Service::Restart,
        Some(|restart, out: &mut W| {
            if agents.contains(&Agent::Dsh) {
                dsh::link(dsh, out)?;
            }
            service::load(home, restart, out)
        }),
        confirm,
        out,
    )
}

pub async fn daemon_instance(paths: &Paths) -> Option<String> {
    daemon_health(paths)
        .await
        .ok()
        .map(|health| health.instance)
}

async fn daemon_health(paths: &Paths) -> Result<crate::protocol::Health> {
    let (answer, _) =
        crate::client::call_with_timeout(paths, "GET", "/v1/health", None, Duration::from_secs(1))
            .await?;
    if answer.status != 200 {
        bail!("health endpoint answered {}", answer.status);
    }
    Ok(serde_json::from_value(answer.body)?)
}

pub async fn verify_daemon(paths: &Paths, previous: Option<&str>) -> Result<()> {
    verify_health(previous, || daemon_health(paths), Duration::from_secs(10)).await
}

async fn verify_health(
    previous: Option<&str>,
    mut probe: impl AsyncFnMut() -> Result<crate::protocol::Health>,
    timeout: Duration,
) -> Result<()> {
    let deadline = crate::timing::Deadline::after(Some(timeout));
    let expected = env!("CARGO_PKG_VERSION");
    loop {
        let error = match probe().await {
            Ok(health) if previous == Some(health.instance.as_str()) => {
                anyhow::anyhow!("the previous daemon instance is still responding")
            }
            Ok(health) if health.version != expected && previous.is_none() => {
                bail!(
                    "daemon is running ats {}; this binary is {expected}. After draining active jobs and backing up the database, rerun ats install with --restart",
                    health.version
                );
            }
            Ok(health) if health.version != expected => {
                anyhow::anyhow!("daemon reports ats {}, expected {expected}", health.version)
            }
            Ok(_) => return Ok(()),
            Err(error) => error,
        };
        if deadline.expired() {
            return Err(error).context("the daemon login service did not become ready");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn install_with<W: Write>(
    paths: &Paths,
    home: &Path,
    dsh: &DshConfig<'_>,
    agents: &[Agent],
    ats: &Path,
    restart: bool,
    start: Option<impl FnOnce(bool, &mut W) -> Result<()>>,
    confirm: Confirm,
    out: &mut W,
) -> Result<bool> {
    let mut record = Record::load(paths)?;
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    for &agent in agents {
        if agent == Agent::Dsh {
            changes.extend(dsh::install(home, dsh, ats)?);
            continue;
        }
        let (hooks_file, instructions) = files(home, agent);
        let old = read(&hooks_file)?;
        let mut doc = parse(&hooks_file, old.as_deref())?;
        remove_ats(&mut doc, agent);
        let moved = record.agents.remove(name(agent)).unwrap_or_default();
        if !moved.is_empty() {
            notes.push(format!(
                "An earlier ats install moved these hooks of {}; they go back: {}",
                hooks_file.display(),
                moved
                    .iter()
                    .filter_map(|m| m.hook["command"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        restore(&mut doc, &moved);
        add_ats(&mut doc, agent, ats);
        if agent == Agent::Claude && claude_asks(&doc) {
            notes.push("Claude Code asks before each `ats run` command in this permission mode. Do not allow `Bash(ats run:*)`: it would allow any command.".into());
        }
        changes.push(Change {
            path: hooks_file,
            new: Some(json_text(&doc)),
            old,
        });
        let old = read(&instructions)?;
        let file = include_file(home, agent);
        changes.push(Change {
            old: read(&file)?,
            new: Some(format!("{INSTRUCTIONS}\n")),
            path: file,
        });
        let new = match agent {
            Agent::Claude => with_include(old.as_deref().unwrap_or_default()),
            Agent::Codex | Agent::Dsh => with_block(
                old.as_deref().unwrap_or_default(),
                &compact_block(&include_file(home, agent)),
            ),
        };
        changes.push(Change {
            path: instructions,
            new: Some(new),
            old,
        });
    }
    let service = service::file(home);
    let (old, new) = (read(&service)?, service::text(paths, ats));
    let changed = old.as_deref() != Some(new.as_str());
    changes.push(Change {
        old,
        new: Some(new),
        path: service,
    });
    if agents.contains(&Agent::Codex) {
        notes.push("Codex asks you to trust the changed hooks the next time it starts.".into());
    }
    for note in &notes {
        writeln!(out, "Note: {note}")?;
    }
    let unchanged = changes.iter().all(|c| c.old == c.new);
    let dry_run = matches!(&confirm, Confirm::DryRun);
    let action = restart.then_some("Restart the daemon login service.");
    let approved = review(changes, action, confirm, out)?;
    if let Some(changes) = &approved
        && !changes.is_empty()
    {
        write(paths, changes, &record, out)?;
    }
    if let Some(start) = start
        && (approved.is_some() || (unchanged && !restart && !dry_run))
    {
        start(changed || restart, out)?;
        return Ok(true);
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
pub fn uninstall(
    paths: &Paths,
    home: &Path,
    dsh: &DshConfig<'_>,
    agents: &[Agent],
    all: bool,
    start: bool,
    confirm: Confirm,
    out: &mut impl Write,
) -> Result<()> {
    uninstall_with(
        paths,
        home,
        dsh,
        agents,
        all,
        start.then_some(|home: &Path| service::unload(home)),
        |home, profile, out| dsh::link_profile(home, profile, out),
        confirm,
        out,
    )
}

#[allow(clippy::too_many_arguments)]
fn uninstall_with<W: Write>(
    paths: &Paths,
    home: &Path,
    dsh: &DshConfig<'_>,
    agents: &[Agent],
    all: bool,
    unload: Option<impl FnOnce(&Path) -> Result<()>>,
    mut link: impl FnMut(&Path, &str, &mut W) -> Result<()>,
    confirm: Confirm,
    out: &mut W,
) -> Result<()> {
    let mut record = Record::load(paths)?;
    let mut changes = Vec::new();
    let profiles = if all || agents.contains(&Agent::Dsh) {
        let (dsh_changes, profiles) = dsh::uninstall(home, dsh, all)?;
        changes.extend(dsh_changes);
        profiles
    } else {
        BTreeSet::new()
    };
    for &agent in agents.iter().filter(|&&agent| agent != Agent::Dsh) {
        let (hooks_file, instructions) = files(home, agent);
        if let Some(old) = read(&hooks_file)? {
            let mut doc = parse(&hooks_file, Some(&old))?;
            remove_ats(&mut doc, agent);
            if let Some(moved) = record.agents.get(name(agent)) {
                restore(&mut doc, moved);
            }
            let new = (doc != json!({})).then(|| json_text(&doc));
            changes.push(Change {
                path: hooks_file,
                new,
                old: Some(old),
            });
        }
        if let Some(old) = read(&instructions)? {
            let new = match agent {
                Agent::Claude => without_include(&old),
                Agent::Codex | Agent::Dsh => without_block(&old),
            };
            changes.push(Change {
                path: instructions,
                new: Some(new).filter(|t| !t.trim().is_empty()),
                old: Some(old),
            });
        }
        let file = include_file(home, agent);
        if let Some(old) = read(&file)? {
            changes.push(Change {
                path: file,
                old: Some(old),
                new: None,
            });
        }
    }
    let service = service::file(home);
    let old_service = if all { read(&service)? } else { None };
    let remove_service = old_service.is_some();
    if let Some(old) = old_service {
        changes.push(Change {
            path: service,
            old: Some(old),
            new: None,
        });
    }
    let mut relinks: BTreeSet<String> = record
        .dsh_relinks
        .get(&dsh.home)
        .into_iter()
        .flatten()
        .filter(|profile| all || (agents.contains(&Agent::Dsh) && profile.as_str() == dsh.profile))
        .cloned()
        .collect();
    relinks.extend(profiles);
    let manage_services = unload.is_some();
    let action = (manage_services && !relinks.is_empty()).then(|| {
        format!(
            "Refresh DSH package links for {}.",
            relinks.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    });
    if let Some(changes) = review(changes, action.as_deref(), confirm, out)? {
        if remove_service && let Some(unload) = unload {
            unload(home)?;
        }
        if !changes.is_empty() {
            if manage_services && !relinks.is_empty() {
                record
                    .dsh_relinks
                    .entry(dsh.home.clone())
                    .or_default()
                    .extend(relinks.iter().cloned());
                private_dir(&paths.home)?;
                write_record(paths, &record)?;
            }
            for agent in agents {
                record.agents.remove(name(*agent));
            }
            write(paths, &changes, &record, out)?;
        }
        if manage_services {
            for profile in relinks {
                link(&dsh.home, &profile, out)?;
                if let Some(pending) = record.dsh_relinks.get_mut(&dsh.home) {
                    pending.remove(&profile);
                    if pending.is_empty() {
                        record.dsh_relinks.remove(&dsh.home);
                    }
                }
                write_record(paths, &record)?;
            }
        }
    }
    Ok(())
}

fn review(
    changes: Vec<Change>,
    action: Option<&str>,
    confirm: Confirm,
    out: &mut impl Write,
) -> Result<Option<Vec<Change>>> {
    let changes: Vec<Change> = changes.into_iter().filter(|c| c.old != c.new).collect();
    if changes.is_empty() && action.is_none() {
        writeln!(out, "Nothing to change.")?;
        return Ok(None);
    }
    for c in &changes {
        out.write_all(
            diff(
                &c.path,
                c.old.as_deref().unwrap_or_default(),
                c.new.as_deref().unwrap_or_default(),
            )?
            .as_bytes(),
        )?;
        if c.new.is_none() {
            writeln!(out, "{} will be removed.", c.path.display())?;
        }
    }
    if let Some(action) = action {
        writeln!(out, "{action}")?;
    }
    let agreed = match confirm {
        Confirm::Ask(input) => ask(
            input,
            out,
            if changes.is_empty() {
                "Proceed?"
            } else {
                "Write these changes?"
            },
        )?,
        Confirm::DryRun => {
            writeln!(out, "Nothing written (dry run).")?;
            return Ok(None);
        }
        Confirm::Yes => true,
    };
    if !agreed {
        writeln!(out, "Nothing written.")?;
        return Ok(None);
    }
    Ok(Some(changes))
}

fn write(paths: &Paths, changes: &[Change], record: &Record, out: &mut impl Write) -> Result<()> {
    private_dir(&paths.home)?;
    let backup = if changes.iter().any(|change| change.old.is_some()) {
        let backups = paths.home.join("backups");
        private_dir(&backups)?;
        Some(
            tempfile::Builder::new()
                .prefix(&format!(
                    "{}-",
                    humantime::format_rfc3339_seconds(std::time::SystemTime::now())
                ))
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in(backups)?
                .keep(),
        )
    } else {
        None
    };
    for (i, c) in changes.iter().enumerate() {
        if let (Some(old), Some(backup)) = (&c.old, &backup) {
            let name = c.path.file_name().expect("a file name").to_string_lossy();
            write_atomic(&backup.join(format!("{i}-{name}")), old)?;
        }
        match &c.new {
            Some(text) => write_atomic(&c.path, text)?,
            None => std::fs::remove_file(&c.path)?,
        }
    }
    write_record(paths, record)?;
    if let Some(backup) = backup {
        writeln!(
            out,
            "Written. The previous files are in {}",
            backup.display()
        )?;
    } else {
        writeln!(out, "Written.")?;
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_record(paths: &Paths, record: &Record) -> Result<()> {
    write_atomic(
        &paths.installed,
        &(serde_json::to_string_pretty(record)? + "\n"),
    )
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().expect("a parent directory");
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    match std::fs::metadata(path) {
        Ok(meta) => tmp.as_file().set_permissions(meta.permissions())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("reading permissions on {}", path.display()));
        }
    }
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

struct DiffFiles {
    old: tempfile::NamedTempFile,
    new: tempfile::NamedTempFile,
    _dir: tempfile::TempDir,
}

impl DiffFiles {
    fn new(old: &str, new: &str) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("ats-install-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let mut a = tempfile::NamedTempFile::new_in(dir.path())?;
        let mut b = tempfile::NamedTempFile::new_in(dir.path())?;
        a.write_all(old.as_bytes())?;
        b.write_all(new.as_bytes())?;
        Ok(Self {
            old: a,
            new: b,
            _dir: dir,
        })
    }
}

fn diff(path: &Path, old: &str, new: &str) -> Result<String> {
    let files = DiffFiles::new(old, new)?;
    let label = path.display().to_string();
    let output = std::process::Command::new("diff")
        .args(["-u", "--label", &label, "--label", &label])
        .arg(files.old.path())
        .arg(files.new.path())
        .output()
        .context("running diff")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_the_service_after_files_were_written() {
        let (_dir, paths, home) = setup();
        let mut out = Vec::new();
        let ats = Path::new("/opt/ats");
        let count = std::cell::Cell::new(0);
        let result = install_with(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Codex],
            ats,
            false,
            Some(|changed: bool, _: &mut Vec<u8>| {
                assert!(changed);
                count.set(count.get() + 1);
                anyhow::bail!("service start denied")
            }),
            Confirm::Yes,
            &mut out,
        );
        assert!(result.unwrap_err().to_string().contains("denied"));
        out.clear();
        install_with(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Codex],
            ats,
            false,
            Some(|changed: bool, _: &mut Vec<u8>| {
                assert!(!changed);
                count.set(count.get() + 1);
                Ok(())
            }),
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert_eq!(count.get(), 2);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Nothing to change.")
        );

        for (confirm, binary) in [
            (Confirm::DryRun, ats),
            (Confirm::Ask(&mut b"n\n".as_slice()), Path::new("/new/ats")),
        ] {
            install_with(
                &paths,
                &home,
                &DshConfig::new(&home, "desktop"),
                &[Agent::Codex],
                binary,
                false,
                Some(|_, _: &mut Vec<u8>| -> Result<()> { panic!("service should not start") }),
                confirm,
                &mut Vec::new(),
            )
            .unwrap();
        }
    }

    #[test]
    fn explicit_restart_is_previewed_even_when_the_service_file_is_unchanged() {
        let (_dir, paths, home) = setup();
        let config = DshConfig::new(&home, "desktop");
        let ats = Path::new("/opt/ats");
        install(
            &paths,
            &home,
            &config,
            &[],
            ats,
            Service::Skip,
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap();

        let mut no = &b"n\n"[..];
        for (confirm, restarted) in [
            (Confirm::DryRun, false),
            (Confirm::Ask(&mut no), false),
            (Confirm::Yes, true),
        ] {
            let invoked = std::cell::Cell::new(false);
            let mut out = Vec::new();
            let started = install_with(
                &paths,
                &home,
                &config,
                &[],
                ats,
                true,
                Some(|restart: bool, _: &mut Vec<u8>| {
                    assert!(restart);
                    invoked.set(true);
                    Ok(())
                }),
                confirm,
                &mut out,
            )
            .unwrap();
            assert_eq!(invoked.get(), restarted);
            assert_eq!(started, restarted);
            assert!(
                String::from_utf8(out)
                    .unwrap()
                    .contains("Restart the daemon login service.")
            );
        }
    }

    #[tokio::test]
    async fn service_health_requires_the_installed_version_and_a_new_instance_on_restart() {
        let health = |version: &str, instance: &str| crate::protocol::Health {
            version: version.into(),
            pid: 1,
            started_at: String::new(),
            instance: instance.into(),
        };
        let expected = env!("CARGO_PKG_VERSION");
        let error = verify_health(None, async || Ok(health("0.1.0", "old")), Duration::ZERO)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("--restart"));
        let error = verify_health(
            Some("old"),
            async || Ok(health(expected, "old")),
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("previous daemon instance"));

        let mut replies = [health(expected, "old"), health(expected, "new")].into_iter();
        verify_health(
            Some("old"),
            async || Ok(replies.next().expect("new instance should pass")),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    }

    #[test]
    fn failed_service_start_leaves_private_backups_and_preserves_config_mode() {
        let (_dir, paths, home) = setup();
        let settings = files(&home, Agent::Claude).0;
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, SETTINGS).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o640)).unwrap();
        let error = install_with(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Claude],
            Path::new("/opt/ats"),
            false,
            Some(|_, _: &mut Vec<u8>| anyhow::bail!("service start denied")),
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("service start denied"));

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let backups = paths.home.join("backups");
        let backup = std::fs::read_dir(&backups)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        for path in [&paths.home, &backups, &backup] {
            assert_eq!(mode(path), 0o700);
        }
        let settings_backup = backup.join("0-settings.json");
        assert_eq!(std::fs::read_to_string(&settings_backup).unwrap(), SETTINGS);
        assert_eq!(mode(&settings_backup), 0o600);
        assert_eq!(mode(&paths.installed), 0o600);
        assert_eq!(mode(&settings), 0o640);
    }

    #[test]
    fn uninstall_leaves_an_unrelated_dsh_home_untouched() {
        let (_dir, paths, home) = setup();
        let config = DshConfig::new(&home, "desktop");
        let manifest = config.manifest();
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let dsh_settings = "{ broken unrelated manifest";
        std::fs::write(&manifest, dsh_settings).unwrap();
        let notes = config.home.join("ATS.md");
        std::fs::write(&notes, "unrelated notes").unwrap();

        uninstall(
            &paths,
            &home,
            &config,
            &[Agent::Dsh],
            true,
            false,
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(notes).unwrap(), "unrelated notes");
        assert_eq!(std::fs::read_to_string(manifest).unwrap(), dsh_settings);
    }

    #[test]
    fn diff_inputs_are_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let files = DiffFiles::new("credential", "new credential").unwrap();
        let dir = files._dir.path().to_path_buf();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in [&files.old, &files.new] {
            assert_eq!(
                file.as_file().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_to_string(files.old.path()).unwrap(),
            "credential"
        );
        drop(files);
        assert!(!dir.exists());
    }

    const SETTINGS: &str = r#"{
  "$schema": "https://example.com/settings.json",
  "hooks": {
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "observe post" }] }
    ],
    "PreToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "observe pre" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "wrap hook claude", "timeout": 5 }] }
    ]
  },
  "theme": "dark"
}
"#;

    fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("user");
        let paths = Paths::with(dir.path().join("ats"), dir.path().join("requests"));
        (dir, paths, home)
    }

    fn run<T>(
        f: impl FnOnce(&mut dyn BufRead, &mut Vec<u8>) -> Result<T>,
        answers: &str,
    ) -> String {
        let mut out = Vec::new();
        f(&mut answers.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn installs_and_uninstalls_without_a_trace() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = home.join(".claude/settings.json");
        let instructions = home.join(".claude/CLAUDE.md");
        let original = json_text(&serde_json::from_str(SETTINGS).unwrap());
        std::fs::write(&settings, &original).unwrap();
        std::fs::write(&instructions, "# Mine\n\nKeep this.\n").unwrap();
        let ats = Path::new("/opt/ats");
        let agents = agents(&home, &[], &DshConfig::new(&home, "desktop"));
        assert_eq!(agents, [Agent::Claude], "only the agent set up here");

        let shown = run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &agents,
                    ats,
                    Service::Skip,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        assert!(
            shown.contains("asks before each `ats run`"),
            "default mode: {shown}"
        );
        assert!(
            std::fs::read_to_string(service::file(&home))
                .unwrap()
                .contains("/opt/ats")
        );
        let doc = read_json(&settings);
        let keys: Vec<&String> = doc.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "hooks", "theme"], "key order kept");
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            read_json_str(SETTINGS)["hooks"]["PreToolUse"],
            "other hooks stay"
        );
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/opt/ats hook claude stop"
        );
        assert_eq!(
            doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "/opt/ats hook claude user-prompt-submit"
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Mine\n\nKeep this.\n\n@ats.md\n"
        );
        assert!(
            std::fs::read_to_string(include_file(&home, Agent::Claude))
                .unwrap()
                .contains("ats run --")
        );
        assert!(
            paths
                .home
                .join("backups")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );

        assert!(
            run(
                |i, o| install(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &agents,
                    ats,
                    Service::Skip,
                    Confirm::Ask(i),
                    o
                ),
                ""
            )
            .contains("Nothing to change.")
        );

        run(
            |i, o| {
                uninstall(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &agents,
                    true,
                    false,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            original,
            "byte for byte"
        );
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# Mine\n\nKeep this.\n"
        );
        assert!(!include_file(&home, Agent::Claude).exists() && !service::file(&home).exists());
    }

    fn read_json_str(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn replaces_what_an_earlier_version_installed() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = home.join(".claude/settings.json");
        let mut earlier = read_json_str(SETTINGS);
        earlier["hooks"]["PreToolUse"][1] = json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "/opt/ats hook claude pre-tool-use" }] });
        earlier["permissions"] = json!({ "defaultMode": "auto" });
        std::fs::write(&settings, json_text(&earlier)).unwrap();
        let record = json!({ "agents": { "claude": [
            { "group": 1, "index": 0, "hook": { "type": "command", "command": "wrap hook claude", "timeout": 5 } }
        ] } });
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(&paths.installed, record.to_string()).unwrap();
        std::fs::write(
            home.join(".claude/CLAUDE.md"),
            format!("# Mine\n\n{BEGIN}\nold\n{END}\n"),
        )
        .unwrap();

        let shown = run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &[Agent::Claude],
                    Path::new("/opt/ats"),
                    Service::Skip,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        assert!(
            shown.contains("they go back: wrap hook claude") && !shown.contains("asks before each"),
            "{shown}"
        );
        let doc = read_json(&settings);
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            read_json_str(SETTINGS)["hooks"]["PreToolUse"],
            "the moved hook is back"
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
            "# Mine\n\n@ats.md\n"
        );
        assert_eq!(Record::load(&paths).unwrap(), Record::default());
    }

    #[test]
    fn declining_writes_nothing() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), SETTINGS).unwrap();
        let shown = run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &[Agent::Claude],
                    Path::new("/opt/ats"),
                    Service::Skip,
                    Confirm::Ask(i),
                    o,
                )
            },
            "n\n",
        );
        assert!(shown.ends_with("Nothing written.\n"), "{shown}");
        assert_eq!(
            std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
            SETTINGS
        );
        assert!(
            !home.join(".claude/CLAUDE.md").exists()
                && !include_file(&home, Agent::Claude).exists()
                && !paths.installed.exists()
        );
    }

    #[test]
    fn codex_gets_a_compact_entrypoint_and_full_reference_then_removes_both() {
        let (_dir, paths, home) = setup();
        let ats = Path::new("/Applications/My Tools/ats");
        run(
            |i, o| {
                install(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &[Agent::Codex],
                    ats,
                    Service::Skip,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        let hooks = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
        assert!(
            hooks.contains(r"'/Applications/My Tools/ats' hook codex stop"),
            "{hooks}"
        );
        let agents_md = std::fs::read_to_string(home.join(".codex/AGENTS.md")).unwrap();
        assert!(agents_md.starts_with(BEGIN) && agents_md.contains("ats run --"));
        assert!(agents_md.contains(&format!(
            "`{}`",
            include_file(&home, Agent::Codex).display()
        )));
        assert!(!agents_md.contains("model training or inference"));
        assert_eq!(
            std::fs::read_to_string(include_file(&home, Agent::Codex)).unwrap(),
            format!("{INSTRUCTIONS}\n")
        );
        run(
            |i, o| {
                uninstall(
                    &paths,
                    &home,
                    &DshConfig::new(&home, "desktop"),
                    &[Agent::Codex],
                    true,
                    false,
                    Confirm::Ask(i),
                    o,
                )
            },
            "y\n",
        );
        assert!(
            !home.join(".codex/hooks.json").exists()
                && !home.join(".codex/AGENTS.md").exists()
                && !include_file(&home, Agent::Codex).exists()
        );
    }

    #[test]
    fn codex_reinstall_replaces_legacy_inline_guidance_and_preserves_user_rules() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let file = home.join(".codex/AGENTS.md");
        let original = "# Mine\n\nKeep these user rules.\n";
        std::fs::write(&file, format!("{original}\n{}", instructions_block())).unwrap();
        let ats = Path::new("/opt/ats");
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Codex],
            ats,
            Service::Skip,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        let installed = std::fs::read_to_string(&file).unwrap();
        assert!(installed.starts_with(original));
        assert_eq!(installed.matches(BEGIN).count(), 1);
        assert!(!installed.contains("model training or inference"));
        out.clear();
        install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Codex],
            ats,
            Service::Skip,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Nothing to change.")
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), installed);
        uninstall(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Codex],
            false,
            false,
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), original);
        assert!(!include_file(&home, Agent::Codex).exists());
    }

    #[test]
    fn refuses_a_file_it_cannot_read() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{ not json").unwrap();
        let mut out = Vec::new();
        let err = install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Claude],
            Path::new("/opt/ats"),
            Service::Skip,
            Confirm::Ask(&mut "y\n".as_bytes()),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not valid JSON"), "{err:#}");
    }

    #[test]
    fn an_agent_shows_the_changes_then_writes_them() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let ats = Path::new("/opt/ats");
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Claude],
            ats,
            Service::Skip,
            Confirm::DryRun,
            &mut out,
        )
        .unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(
            shown.contains("+@ats.md") && shown.ends_with("Nothing written (dry run).\n"),
            "{shown}"
        );
        assert!(
            !home.join(".claude/settings.json").exists()
                && !include_file(&home, Agent::Claude).exists()
        );
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[Agent::Claude],
            ats,
            Service::Skip,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert!(
            include_file(&home, Agent::Claude).exists()
                && home.join(".claude/settings.json").exists()
        );
    }

    #[test]
    fn the_service_alone_touches_no_agent() {
        let (_dir, paths, home) = setup();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let mut out = Vec::new();
        install(
            &paths,
            &home,
            &DshConfig::new(&home, "desktop"),
            &[],
            Path::new("/opt/ats"),
            Service::Skip,
            Confirm::Yes,
            &mut out,
        )
        .unwrap();
        assert!(service::file(&home).exists());
        assert!(
            !home.join(".claude/settings.json").exists()
                && !home.join(".claude/CLAUDE.md").exists()
        );
        assert!(
            instructions_block().starts_with(BEGIN)
                && instructions_block().ends_with(&format!("{END}\n"))
        );
    }

    #[test]
    fn the_import_line_comes_and_goes_cleanly() {
        for (text, back) in [
            ("", ""),
            ("# Mine\n", "# Mine\n"),
            ("# Mine\n\n\n", "# Mine\n"),
            ("@ats.md\n", ""),
        ] {
            let with = with_include(text);
            assert!(
                with.ends_with("@ats.md\n") && with.matches(INCLUDE).count() == 1,
                "{text:?}"
            );
            assert_eq!(without_include(&with), back, "{text:?}");
        }
    }
}
