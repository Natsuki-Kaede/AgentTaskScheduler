//! Path resolution and write-lock derivation for submission planning.
use crate::domain::resource::{CargoEnv, Lock};
use crate::profile::Writes;
use crate::recognize::Analysis;
use std::path::{Component, Path, PathBuf};

pub fn path(path: &Path) -> Lock {
    Lock::Path(normalize(path))
}

pub fn parse(value: &str, cwd: &Path) -> Lock {
    if value.contains('/') || value.starts_with('.') || value.starts_with('~') {
        let expanded = match value.strip_prefix('~') {
            Some(rest) => dirs::home_dir()
                .unwrap_or_default()
                .join(rest.trim_start_matches('/')),
            None => cwd.join(value),
        };
        path(&expanded)
    } else {
        Lock::Named(value.to_string())
    }
}

pub fn normalize(path: &Path) -> String {
    let mut full = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                full.pop();
            }
            Component::CurDir => {}
            other => {
                full.push(other);
                if let Ok(real) = full.canonicalize() {
                    full = real;
                }
            }
        }
    }
    let text = full.to_string_lossy().into_owned();
    if cfg!(target_os = "macos") {
        text.to_lowercase()
    } else {
        text
    }
}

const PROJECT_MARKERS: &[&str] = &[
    "package.json",
    "go.mod",
    "Cargo.toml",
    "pyproject.toml",
    "build.gradle",
    "build.gradle.kts",
    "Package.swift",
    "pom.xml",
    "CMakeLists.txt",
    "Makefile",
];

pub fn project_root(dir: &Path) -> PathBuf {
    let marked = dir
        .ancestors()
        .find(|d| PROJECT_MARKERS.iter().any(|m| d.join(m).is_file()));
    marked
        .or_else(|| dir.ancestors().find(|d| d.join(".git").exists()))
        .unwrap_or(dir)
        .to_path_buf()
}

fn cargo_flag_values<'a>(argv: &'a [String], flag: &'a str) -> impl Iterator<Item = &'a str> {
    let prefix = format!("{flag}=");
    argv.iter()
        .take_while(|arg| arg.as_str() != "--")
        .enumerate()
        .filter_map(move |(i, arg)| {
            arg.strip_prefix(&prefix).or_else(|| {
                (arg == flag)
                    .then(|| argv.get(i + 1).map(String::as_str))
                    .flatten()
            })
        })
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    if !path.is_file() {
        return None;
    }
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn config_target(doc: &toml::Value) -> Option<&str> {
    doc.get("build")?.get("target-dir")?.as_str()
}

fn cargo_config_target(path: &Path, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let doc = read_toml(path)?;
    let directory = path.parent()?;
    if let Some(target) = config_target(&doc) {
        return Some(directory.parent().unwrap_or(directory).join(target));
    }
    doc.get("include")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|entry| {
            let relative = entry.as_str().or_else(|| entry.get("path")?.as_str())?;
            cargo_config_target(&directory.join(relative), depth - 1)
        })
}

fn config_in(directory: &Path) -> Option<PathBuf> {
    let legacy = directory.join("config");
    let path = if legacy.exists() {
        legacy
    } else {
        directory.join("config.toml")
    };
    cargo_config_target(&path, 16)
}

fn cargo_target(
    argv: &[String],
    cwd: &Path,
    shell_env: &CargoEnv,
    caller_env: &CargoEnv,
) -> PathBuf {
    if let Some(target) = cargo_flag_values(argv, "--target-dir").last() {
        return cwd.join(target);
    }
    if let Some(target) = shell_env
        .target_dir
        .as_ref()
        .or(caller_env.target_dir.as_ref())
    {
        return cwd.join(target);
    }
    let mut target = None;
    for config in cargo_flag_values(argv, "--config") {
        let value = if config.contains('=') {
            toml::from_str::<toml::Value>(config)
                .ok()
                .and_then(|doc| config_target(&doc).map(|value| cwd.join(value)))
        } else {
            cargo_config_target(&cwd.join(config), 16)
        };
        target = value.or(target);
    }
    if let Some(target) = target {
        return target;
    }
    if let Some(target) = shell_env
        .build_target_dir
        .as_ref()
        .or(caller_env.build_target_dir.as_ref())
    {
        return cwd.join(target);
    }
    let cargo_home = shell_env
        .home
        .as_ref()
        .or(caller_env.home.as_ref())
        .map(|home| cwd.join(home))
        .or_else(default_cargo_home);
    if let Some(target) = cwd
        .ancestors()
        .find_map(|dir| config_in(&dir.join(".cargo")))
        .or_else(|| cargo_home.and_then(|home| config_in(&home)))
    {
        return target;
    }
    let manifest = cargo_flag_values(argv, "--manifest-path")
        .last()
        .map(|path| cwd.join(path))
        .unwrap_or_else(|| {
            cwd.ancestors()
                .map(|dir| dir.join("Cargo.toml"))
                .find(|path| path.is_file())
                .unwrap_or_else(|| cwd.join("Cargo.toml"))
        });
    let package = manifest.parent().unwrap_or(cwd);
    if let Some(workspace) = read_toml(&manifest).and_then(|doc| {
        doc.get("package")?
            .get("workspace")?
            .as_str()
            .map(str::to_string)
    }) {
        return package.join(workspace).join("target");
    }
    let root = package
        .ancestors()
        .find(|dir| {
            read_toml(&dir.join("Cargo.toml")).is_some_and(|doc| doc.get("workspace").is_some())
        })
        .unwrap_or(package);
    root.join("target")
}

fn default_cargo_home() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        dirs::home_dir().map(|home| home.join(".cargo"))
    }
}

pub fn for_line(
    analysis: &Analysis,
    cwd: &Path,
    profiles: &crate::profile::Profiles,
    extra: &[String],
    cargo_env: &CargoEnv,
) -> Vec<Lock> {
    let mut locks = Vec::new();
    for s in &analysis.segments {
        let profile = profiles
            .find(&s.argv)
            .filter(|p| Some(&p.name) == s.profile.as_ref());
        match s.writes {
            Writes::None => {}
            Writes::Args => {
                for arg in s.argv.iter().skip(1).filter(|a| !a.starts_with('-')) {
                    locks.push(path(&s.cwd.join(arg)));
                }
            }
            Writes::Paths => {
                let root = project_root(&s.cwd);
                for p in profile
                    .map(|p| p.write_paths.as_slice())
                    .unwrap_or_default()
                {
                    let target = if s.tool.as_deref() == Some("cargo") && p == "target" {
                        cargo_target(&s.argv, &s.cwd, &s.cargo_env, cargo_env)
                    } else {
                        root.join(p)
                    };
                    locks.push(path(&target));
                }
            }
            Writes::Project => locks.push(path(&project_root(&s.cwd))),
        }
        for file in &s.redirect_writes {
            locks.push(path(&s.cwd.join(file)));
        }
    }
    locks.extend(extra.iter().map(|e| parse(e, cwd)));
    let mut out: Vec<Lock> = Vec::new();
    for l in locks {
        let covered = out.iter().any(|o| match (o, &l) {
            (Lock::Path(a), Lock::Path(b)) => Path::new(b).starts_with(a),
            _ => *o == l,
        });
        if !covered {
            out.retain(|o| !matches!((o, &l), (Lock::Path(a), Lock::Path(b)) if Path::new(a).starts_with(b)));
            out.push(l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profiles;
    use crate::recognize::Recognizer;

    #[test]
    fn path_conflicts_follow_components() {
        let p = |s: &str| Lock::Path(s.into());
        assert!(p("/repo/app").conflicts(&p("/repo/app/target")));
        assert!(p("/repo/app/target").conflicts(&p("/repo/app")));
        assert!(p("/repo/app").conflicts(&p("/repo/app")));
        assert!(
            !p("/repo/app").conflicts(&p("/repo/app2")),
            "not a bare string prefix"
        );
        assert!(!p("/repo/app").conflicts(&Lock::Named("/repo/app".into())));
        assert!(Lock::Named("device:phone".into()).conflicts(&Lock::Named("device:phone".into())));
    }

    #[test]
    fn normalizes_missing_paths_through_existing_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let real = normalize(dir.path());
        let missing = normalize(&dir.path().join("a/../b/./c"));
        assert_eq!(missing, format!("{real}/b/c"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                normalize(&dir.path().join("Target")),
                format!("{real}/target")
            );
        }
    }

    #[test]
    fn resolves_symlinks_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real/child");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        let alias = path(&dir.path().join("link/../missing/output"));
        let real = path(&dir.path().join("real/missing/output"));
        assert_eq!(alias, real);
        assert!(alias.conflicts(&real));
        assert!(!alias.conflicts(&path(&dir.path().join("missing/output"))));
    }

    #[test]
    fn finds_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let app = repo.join("packages/app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(
            project_root(&app.join("src")),
            repo,
            "no marker: the Git root"
        );
        std::fs::write(app.join("package.json"), "{}").unwrap();
        assert_eq!(
            project_root(&app.join("src")),
            app,
            "the nearest marker wins"
        );
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(project_root(&plain), plain);
    }

    #[test]
    fn locks_for_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let profiles = Profiles::builtin();
        let r = Recognizer::new(&profiles, &[]);
        let locks = |src: &str, extra: &[&str]| {
            let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
            for_line(
                &r.shell(src, &root),
                &root,
                &profiles,
                &extra,
                &CargoEnv::default(),
            )
            .into_iter()
            .map(|l| match l {
                Lock::Path(p) => p.replace(&normalize(&root), "<root>"),
                Lock::Named(n) => format!("named:{n}"),
            })
            .collect::<Vec<_>>()
        };
        assert_eq!(locks("ls -la", &[]), Vec::<String>::new());
        assert_eq!(locks("cargo build", &[]), ["<root>/target"]);
        assert_eq!(locks("cargo build && cargo fmt", &[]), ["<root>"]);
        assert_eq!(locks("./build.sh", &[]), Vec::<String>::new());
        assert_eq!(locks("make", &[]), ["<root>"]);
        assert_eq!(
            locks("cd sub && rm -f a.txt b.txt", &[]),
            ["<root>/sub/a.txt", "<root>/sub/b.txt"]
        );
        assert_eq!(locks("echo hi > out.log", &[]), ["<root>/out.log"]);
        assert_eq!(
            locks("adb install app.apk", &["device:phone"]),
            ["named:device:phone"]
        );
    }

    #[test]
    fn crates_in_one_repository_build_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let profiles = Profiles::builtin();
        let r = Recognizer::new(&profiles, &[]);
        let lock = |name: &str| {
            let krate = repo.join(name);
            std::fs::create_dir_all(&krate).unwrap();
            std::fs::write(krate.join("Cargo.toml"), "").unwrap();
            for_line(
                &r.shell("cargo build", &krate),
                &krate,
                &profiles,
                &[],
                &CargoEnv::default(),
            )
        };
        let (a, b) = (lock("a"), lock("b"));
        assert!(!a[0].conflicts(&b[0]), "{a:?} {b:?}");
    }
    #[test]
    fn cargo_workspace_members_lock_the_shared_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let profiles = Profiles::builtin();
        let r = Recognizer::new(&profiles, &[]);
        for member in ["a", "b"] {
            let cwd = root.join(member);
            std::fs::create_dir(&cwd).unwrap();
            std::fs::write(
                cwd.join("Cargo.toml"),
                format!("[package]\nname = {member:?}\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            for (command, expected) in [
                ("cargo build", root.join("target")),
                ("cargo test --target-dir ../shared", root.join("shared")),
                ("cargo check --target-dir=../shared", root.join("shared")),
            ] {
                let locks = for_line(
                    &r.shell(command, &cwd),
                    &cwd,
                    &profiles,
                    &[],
                    &CargoEnv::default(),
                );
                assert_eq!(locks, [path(&expected)], "{member}: {command}");
            }
        }
        let locks = for_line(
            &r.shell("cargo build --manifest-path a/Cargo.toml", root),
            root,
            &profiles,
            &[],
            &CargoEnv::default(),
        );
        assert_eq!(locks, [path(&root.join("target"))]);
    }

    #[test]
    fn cargo_output_locks_follow_config_and_caller_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = root.join(".cargo");
        let first = root.join("first");
        let second = root.join("second");
        let cargo_home = root.join("cargo-home");
        for directory in [&config, &first, &second, &cargo_home] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(config.join("config.toml"), "[build]\ntarget-dir = 'shared'").unwrap();
        std::fs::write(
            cargo_home.join("config.toml"),
            "[build]\ntarget-dir = 'global'",
        )
        .unwrap();
        std::fs::write(
            config.join("override.toml"),
            "[build]\ntarget-dir = 'custom'",
        )
        .unwrap();
        for project in [&first, &second] {
            std::fs::write(project.join("Cargo.toml"), "").unwrap();
        }
        let profiles = Profiles::builtin();
        let recognizer = Recognizer::new(&profiles, &[]);
        let locks = |source: &str, cwd: &Path, env: &CargoEnv| {
            for_line(&recognizer.shell(source, cwd), cwd, &profiles, &[], env)
        };
        let home_only = CargoEnv {
            home: Some(cargo_home.to_str().unwrap().into()),
            ..CargoEnv::default()
        };
        let a = locks("cargo build", &first, &home_only);
        let b = locks("cargo build", &second, &home_only);
        assert_eq!(a, [path(&root.join("shared"))]);
        assert_eq!(a, b);
        for (source, env, expected) in [
            (
                "cargo build --config ../.cargo/override.toml",
                &home_only,
                root.join("custom"),
            ),
            (
                "CARGO_TARGET_DIR=../inline cargo build",
                &home_only,
                root.join("inline"),
            ),
            (
                "env CARGO_TARGET_DIR=../inline cargo build",
                &home_only,
                root.join("inline"),
            ),
            (
                "cargo build",
                &CargoEnv {
                    target_dir: Some("../external".into()),
                    ..Default::default()
                },
                root.join("external"),
            ),
            (
                "cargo build",
                &CargoEnv {
                    build_target_dir: Some("../external".into()),
                    ..Default::default()
                },
                root.join("external"),
            ),
            (
                "cargo build --target-dir ../explicit",
                &CargoEnv {
                    target_dir: Some("../external".into()),
                    ..Default::default()
                },
                root.join("explicit"),
            ),
            (
                "cargo test -- --target-dir ../test-arg",
                &home_only,
                root.join("shared"),
            ),
        ] {
            assert_eq!(locks(source, &first, env), [path(&expected)], "{source}");
        }
        std::fs::write(config.join("config"), "include = ['override.toml']").unwrap();
        assert_eq!(
            locks("cargo build", &first, &home_only),
            [path(&root.join("custom"))]
        );
        std::fs::remove_dir_all(config).unwrap();
        assert_eq!(
            locks("cargo build", &first, &home_only),
            [path(&root.join("global"))]
        );
    }
}
