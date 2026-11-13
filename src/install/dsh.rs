//! DSH loads a Cordis bundle from each profile's package manifest. The bundle
//! has no npm dependencies; the installed copy travels with the ats binary.

use super::*;

const PACKAGE: &str = "@local/agent-task-scheduler";
const PLUGIN_FILES: [(&str, &str); 3] = [
    (
        "package.json",
        include_str!("../../plugins/dsh/package.json"),
    ),
    (
        "cordis.patch.yml",
        include_str!("../../plugins/dsh/cordis.patch.yml"),
    ),
    ("index.js", include_str!("../../plugins/dsh/index.js")),
];

pub struct DshConfig<'a> {
    pub home: PathBuf,
    pub profile: &'a str,
}

impl<'a> DshConfig<'a> {
    pub fn new(user: &Path, profile: &'a str) -> Self {
        Self {
            home: user.join(".dsh"),
            profile,
        }
    }

    pub fn from_env(user: &Path, profile: &'a str) -> Self {
        let mut config = Self::new(user, profile);
        if let Some(home) = std::env::var_os("DSH_HOME") {
            config.home = home.into();
        }
        config
    }

    pub fn manifest(&self) -> PathBuf {
        self.home
            .join("profiles")
            .join(self.profile)
            .join("package.json")
    }
}

fn validate_profile(profile: &str) -> Result<()> {
    if profile.is_empty()
        || !profile.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid DSH profile name: {profile:?}");
    }
    Ok(())
}

fn plugin_dir(user: &Path, config: &DshConfig<'_>) -> PathBuf {
    if config.home == user.join(".dsh") {
        user.join("dsh-plugins/agent-task-scheduler")
    } else {
        config.home.join("plugins/agent-task-scheduler")
    }
}

fn bundles(doc: &mut Value) -> Result<&mut Vec<Value>> {
    let mut node = doc;
    for key in ["dsh", "profile"] {
        node = node
            .as_object_mut()
            .context("DSH profile must be an object")?
            .entry(key)
            .or_insert_with(|| json!({}));
    }
    node.as_object_mut()
        .context("DSH profile must be an object")?
        .entry("bundles")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("DSH profile bundles must be an array")
}

fn dependencies(doc: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    doc.as_object_mut()
        .context("DSH manifest must be an object")?
        .entry("dependencies")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("DSH dependencies must be an object")
}

fn change(path: PathBuf, new: Option<String>) -> Result<Change> {
    Ok(Change {
        old: read(&path)?,
        path,
        new,
    })
}

pub(super) fn install(user: &Path, config: &DshConfig<'_>, ats: &Path) -> Result<Vec<Change>> {
    validate_profile(config.profile)?;
    let path = config.manifest();
    let old = read(&path)?.with_context(|| {
        format!(
            "DSH profile {} is not initialized; start it with DSH first",
            path.display()
        )
    })?;
    let mut doc = parse(&path, Some(&old))?;
    let directory = plugin_dir(user, config);
    dependencies(&mut doc)?.insert(
        PACKAGE.into(),
        json!(format!("link:{}", directory.display())),
    );
    let list = bundles(&mut doc)?;
    if !list.iter().any(|v| v == PACKAGE) {
        list.push(json!(PACKAGE));
    }
    let mut changes = vec![Change {
        path,
        old: Some(old),
        new: Some(json_text(&doc)),
    }];
    for (file, source) in PLUGIN_FILES {
        let source = if file == "index.js" {
            source.replacen(
                "const DEFAULT_ATS = 'ats';",
                &format!("const DEFAULT_ATS = {};", serde_json::to_string(ats)?),
                1,
            )
        } else {
            source.to_string()
        };
        changes.push(change(directory.join(file), Some(source))?);
    }
    let instructions = config.home.join("AGENTS.md");
    let old = read(&instructions)?;
    let new = with_block(
        old.as_deref().unwrap_or_default(),
        &compact_block(&config.home.join("ATS.md")),
    );
    changes.push(Change {
        path: instructions,
        old,
        new: Some(new),
    });
    changes.push(change(
        config.home.join("ATS.md"),
        Some(format!("{INSTRUCTIONS}\n")),
    )?);
    Ok(changes)
}

fn used_elsewhere(config: &DshConfig<'_>) -> Result<bool> {
    let entries = match std::fs::read_dir(config.home.join("profiles")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == config.profile || !entry.path().is_dir() {
            continue;
        }
        let path = entry.path().join("package.json");
        if let Some(text) = read(&path)? {
            let doc = parse(&path, Some(&text))?;
            if doc["dependencies"].get(PACKAGE).is_some()
                || doc["dsh"]["profile"]["bundles"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|v| v == PACKAGE))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn uninstall(user: &Path, config: &DshConfig<'_>) -> Result<Vec<Change>> {
    validate_profile(config.profile)?;
    let path = config.manifest();
    let mut changes = Vec::new();
    if let Some(old) = read(&path)? {
        let mut doc = parse(&path, Some(&old))?;
        if let Some(deps) = doc.get_mut("dependencies").and_then(Value::as_object_mut) {
            deps.remove(PACKAGE);
        }
        if let Some(list) = doc
            .pointer_mut("/dsh/profile/bundles")
            .and_then(Value::as_array_mut)
        {
            list.retain(|v| v != PACKAGE);
        }
        changes.push(Change {
            path,
            old: Some(old),
            new: Some(json_text(&doc)),
        });
    }
    if !used_elsewhere(config)? {
        for (file, _) in PLUGIN_FILES {
            changes.push(change(plugin_dir(user, config).join(file), None)?);
        }
        let path = config.home.join("AGENTS.md");
        if let Some(old) = read(&path)? {
            let new = without_block(&old);
            changes.push(Change {
                path,
                old: Some(old),
                new: Some(new).filter(|t| !t.trim().is_empty()),
            });
        }
        changes.push(change(config.home.join("ATS.md"), None)?);
    }
    Ok(changes)
}

pub(super) fn link(config: &DshConfig<'_>, out: &mut impl Write) -> Result<()> {
    let profile = config.profile;
    let binary = std::env::var_os("DSH_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let desktop = PathBuf::from(
                "/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh",
            );
            if cfg!(target_os = "macos") && desktop.is_file() {
                desktop
            } else {
                "dsh".into()
            }
        });
    let result = std::process::Command::new(&binary)
        .args([
            "plugin",
            "--profile",
            profile,
            "install",
            "--ignore-scripts",
            "--config.strict-dep-builds=false",
        ])
        .env("DSH_HOME", &config.home)
        .output()
        .with_context(|| {
            format!(
                "starting {}; set DSH_BIN to the DSH launcher path",
                binary.display()
            )
        })?;
    out.write_all(&result.stdout)?;
    out.write_all(&result.stderr)?;
    if !result.status.success() {
        bail!(
            "DSH package installation failed; retry `{0} plugin --profile {profile} install --ignore-scripts --config.strict-dep-builds=false`",
            binary.display()
        );
    }
    writeln!(
        out,
        "Restart the DSH {profile} profile to load its updated plugins."
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_reinstalls_and_removes_profiles_without_changing_other_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let paths = Paths::with(dir.path().join("ats"), dir.path().join("requests"));
        let initial = json!({
            "private": true,
            "dependencies": { "other": "link:/other" },
            "dsh": { "profile": { "bundles": ["other"] } }
        });
        for profile in ["desktop", "headless"] {
            let path = DshConfig::new(&user, profile).manifest();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, json_text(&initial)).unwrap();
        }
        let instructions = user.join(".dsh/AGENTS.md");
        std::fs::write(&instructions, "# My rules\n").unwrap();
        let binary = Path::new("/opt/My Tools/ats");
        let run_install = |profile, confirm, out: &mut Vec<u8>| {
            super::super::install(
                &paths,
                &user,
                &DshConfig::new(&user, profile),
                &[Agent::Dsh],
                binary,
                false,
                confirm,
                out,
            )
            .unwrap();
        };
        run_install("desktop", Confirm::DryRun, &mut Vec::new());
        assert!(!plugin_dir(&user, &DshConfig::new(&user, "desktop")).exists());
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# My rules\n"
        );
        run_install("desktop", Confirm::Yes, &mut Vec::new());
        let first = std::fs::read_to_string(DshConfig::new(&user, "desktop").manifest()).unwrap();
        let doc = parse(&DshConfig::new(&user, "desktop").manifest(), Some(&first)).unwrap();
        assert_eq!(doc["dsh"]["profile"]["bundles"], json!(["other", PACKAGE]));
        assert_eq!(doc["dependencies"]["other"], "link:/other");
        let source = std::fs::read_to_string(
            plugin_dir(&user, &DshConfig::new(&user, "desktop")).join("index.js"),
        )
        .unwrap();
        assert!(source.contains("const DEFAULT_ATS = \"/opt/My Tools/ats\";"));
        let mut out = Vec::new();
        run_install("desktop", Confirm::Yes, &mut out);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Nothing to change.")
        );
        assert_eq!(
            agents(&user, &[], &DshConfig::new(&user, "headless")),
            [Agent::Dsh]
        );
        run_install("headless", Confirm::Yes, &mut Vec::new());
        for profile in ["desktop", "headless"] {
            super::super::uninstall(
                &paths,
                &user,
                &DshConfig::new(&user, profile),
                &[Agent::Dsh],
                false,
                false,
                Confirm::Yes,
                &mut Vec::new(),
            )
            .unwrap();
            let path = DshConfig::new(&user, profile).manifest();
            assert_eq!(
                parse(&path, read(&path).unwrap().as_deref()).unwrap(),
                initial
            );
            assert_eq!(
                plugin_dir(&user, &DshConfig::new(&user, "desktop"))
                    .join("index.js")
                    .exists(),
                profile == "desktop"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&instructions).unwrap(),
            "# My rules\n"
        );
        assert!(!user.join(".dsh/ATS.md").exists());
    }

    #[test]
    fn rejects_missing_profiles_and_invalid_manifests_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path();
        for profile in ["", "../desktop", "/desktop", "a/b"] {
            assert!(install(user, &DshConfig::new(user, profile), Path::new("/opt/ats")).is_err());
        }
        assert!(
            install(
                user,
                &DshConfig::new(user, "desktop"),
                Path::new("/opt/ats")
            )
            .err()
            .unwrap()
            .to_string()
            .contains("not initialized")
        );
        let path = DshConfig::new(user, "desktop").manifest();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"dependencies": []}"#).unwrap();
        assert!(
            install(
                user,
                &DshConfig::new(user, "desktop"),
                Path::new("/opt/ats")
            )
            .is_err()
        );
        assert!(!plugin_dir(user, &DshConfig::new(user, "desktop")).exists());
    }

    #[test]
    fn custom_dsh_home_is_explicit_and_keeps_the_user_home_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let config = DshConfig {
            home: dir.path().join("custom-dsh"),
            profile: "web",
        };
        let path = config.manifest();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        let changes = install(&user, &config, Path::new("/opt/ats")).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.path == config.home.join("AGENTS.md"))
        );
        assert!(changes.iter().any(|c| c.path == config.home.join("ATS.md")));
        assert!(
            changes
                .iter()
                .all(|c| !c.path.starts_with(user.join(".dsh")))
        );
        assert!(!user.exists(), "planning never writes files");
    }
}
