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

fn profiles(config: &DshConfig<'_>) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(config.home.join("profiles")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().is_dir()
            && let Ok(name) = entry.file_name().into_string()
            && validate_profile(&name).is_ok()
        {
            profiles.push(name);
        }
    }
    profiles.sort();
    Ok(profiles)
}

pub(super) fn uninstall(
    user: &Path,
    config: &DshConfig<'_>,
    all: bool,
) -> Result<(Vec<Change>, BTreeSet<String>)> {
    let profiles = profiles(config)?;
    if !all {
        validate_profile(config.profile)?;
    }
    let mut changes = Vec::new();
    let mut affected = BTreeSet::new();
    let mut used_elsewhere = false;
    for profile in profiles {
        let path = config
            .home
            .join("profiles")
            .join(&profile)
            .join("package.json");
        let Some(old) = read(&path)? else {
            continue;
        };
        let mut doc = match parse(&path, Some(&old)) {
            Ok(doc) => doc,
            Err(_) if !old.contains(PACKAGE) => continue,
            Err(error) => return Err(error),
        };
        let linked = doc["dependencies"].get(PACKAGE).is_some()
            || doc["dsh"]["profile"]["bundles"]
                .as_array()
                .is_some_and(|items| items.iter().any(|v| v == PACKAGE));
        if !linked {
            continue;
        }
        if !all && profile != config.profile {
            used_elsewhere = true;
            continue;
        }
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
        affected.insert(profile);
    }
    if !used_elsewhere {
        let directory = plugin_dir(user, config);
        let mut installed = !affected.is_empty();
        for (file, _) in PLUGIN_FILES {
            let plugin = change(directory.join(file), None)?;
            installed |= plugin.old.is_some();
            changes.push(plugin);
        }
        let path = config.home.join("AGENTS.md");
        if let Some(old) = read(&path)? {
            let new = without_block(&old);
            if new != old {
                installed = true;
                changes.push(Change {
                    path,
                    old: Some(old),
                    new: Some(new).filter(|t| !t.trim().is_empty()),
                });
            }
        }
        if installed {
            changes.push(change(config.home.join("ATS.md"), None)?);
        }
    }
    Ok((changes, affected))
}

pub(super) fn link(config: &DshConfig<'_>, out: &mut impl Write) -> Result<()> {
    link_profile(&config.home, config.profile, out)
}

pub(super) fn link_profile(home: &Path, profile: &str, out: &mut impl Write) -> Result<()> {
    validate_profile(profile)?;
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
        .env("DSH_HOME", home)
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
            "DSH package installation failed for profile {profile}; rerun the ats command to retry"
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
                Service::Skip,
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
    fn full_uninstall_removes_nondefault_profiles_and_retries_failed_links() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let paths = Paths::with(dir.path().join("ats"), dir.path().join("requests"));
        let binary = Path::new("/opt/ats");
        let untouched = "{\"dependencies\":{\"other\":\"link:/other\"}}\n";
        for profile in ["headless", "web", "tui"] {
            let config = DshConfig::new(&user, profile);
            let path = config.manifest();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, untouched).unwrap();
            if profile != "tui" {
                super::super::install(
                    &paths,
                    &user,
                    &config,
                    &[Agent::Dsh],
                    binary,
                    Service::Skip,
                    Confirm::Yes,
                    &mut Vec::new(),
                )
                .unwrap();
            }
        }
        let config = DshConfig::new(&user, "desktop");
        let agents = agents(&user, &[], &config);
        assert!(agents.is_empty());
        let out = &mut Vec::new();
        super::super::uninstall_with(
            &paths,
            &user,
            &config,
            &agents,
            true,
            Some(|_: &Path| panic!("dry run should not unload")),
            |_, _, _| panic!("dry run should not link"),
            Confirm::DryRun,
            out,
        )
        .unwrap();
        let preview = String::from_utf8_lossy(out);
        assert!(preview.contains("Refresh DSH package links for headless, web."));
        assert!(plugin_dir(&user, &config).join("index.js").exists());
        assert!(service::file(&user).exists());

        let mut linked = Vec::new();
        let error = super::super::uninstall_with(
            &paths,
            &user,
            &config,
            &agents,
            true,
            Some(|_: &Path| Ok(())),
            |home, profile, _| {
                assert_eq!(home, config.home);
                linked.push(profile.to_string());
                if profile == "web" {
                    bail!("relink denied");
                }
                Ok(())
            },
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "relink denied");
        assert_eq!(linked, ["headless", "web"]);
        let pending = Record::load(&paths).unwrap().dsh_relinks;
        assert_eq!(pending[&config.home], BTreeSet::from(["web".into()]));
        assert!(!plugin_dir(&user, &config).join("index.js").exists());
        assert!(!service::file(&user).exists());
        for profile in ["headless", "web", "tui"] {
            let path = DshConfig::new(&user, profile).manifest();
            let actual = std::fs::read_to_string(&path).unwrap();
            let doc = parse(&path, Some(&actual)).unwrap();
            assert_eq!(doc["dependencies"]["other"], "link:/other");
            assert!(doc["dependencies"].get(PACKAGE).is_none());
            if profile == "tui" {
                assert_eq!(actual, untouched);
            } else {
                assert_eq!(doc["dsh"]["profile"]["bundles"], json!([]));
            }
        }

        super::super::uninstall_with(
            &paths,
            &user,
            &config,
            &agents,
            true,
            Some(|_: &Path| panic!("service was removed on the first attempt")),
            |home, profile, _: &mut Vec<u8>| {
                assert_eq!(home, config.home);
                linked.push(profile.to_string());
                Ok(())
            },
            Confirm::Yes,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(linked, ["headless", "web", "web"]);
        assert!(Record::load(&paths).unwrap().dsh_relinks.is_empty());
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
