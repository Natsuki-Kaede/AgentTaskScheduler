//! Tests command recognition and shell structure.

use super::*;
use crate::profile::Profiles;

#[test]
fn directory_changes_follow_shell_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let profiles = Profiles::builtin();
    let recognizer = Recognizer::new(&profiles, &[]);
    for (source, changes_parent) in [
        ("sh -c 'cd /tmp; make'; make", false),
        ("(cd /tmp; make); make", false),
        ("{ cd /tmp; make; }; make", true),
        ("echo $(cd /tmp; pwd); make", false),
        ("cat <(cd /tmp; pwd); make", false),
        ("cd /tmp | cat; make", false),
        ("cd /tmp & make", false),
        ("cd /tmp; make", true),
    ] {
        let analysis = recognizer.shell(source, dir.path());
        assert!(
            analysis.unclear.is_none(),
            "{source}: {:?}",
            analysis.unclear
        );
        assert_eq!(
            analysis.segments.last().unwrap().cwd,
            if changes_parent {
                Path::new("/tmp")
            } else {
                dir.path()
            },
            "{source}"
        );
    }

    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"prebuild":"cd /tmp; make","build":"make","postbuild":"make"}}"#,
    )
    .unwrap();
    let cwd = dir.path().join("src");
    let analysis = recognizer.shell("npm run build; make", &cwd);
    let makes: Vec<_> = analysis
        .segments
        .iter()
        .filter(|s| s.tool.as_deref() == Some("make"))
        .map(|s| s.cwd.as_path())
        .collect();
    assert_eq!(makes, [Path::new("/tmp"), dir.path(), dir.path(), &cwd]);
}

fn render(a: &Analysis) -> String {
    fn node(a: &Analysis, n: &Node) -> String {
        match n {
            Node::Cmd { segment } => {
                let s = &a.segments[*segment];
                s.tool.clone().unwrap_or_else(|| "(assign)".into())
            }
            Node::Seq { items } => format!(
                "seq({})",
                items
                    .iter()
                    .map(|i| node(a, i))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Node::Par { items } => format!(
                "par({})",
                items
                    .iter()
                    .map(|i| node(a, i))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Node::Background { item } => format!("bg({})", node(a, item)),
        }
    }
    a.structure
        .as_ref()
        .map_or("unclear".into(), |n| node(a, n))
}

struct Case {
    src: &'static str,
    structure: &'static str,
    light: bool,
    profile: &'static str,
}

const fn case(
    src: &'static str,
    structure: &'static str,
    light: bool,
    profile: &'static str,
) -> Case {
    Case {
        src,
        structure,
        light,
        profile,
    }
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    std::fs::create_dir(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"scripts": {"prebuild": "rm -rf dist", "build": "tsc && vite build",
            "pretest": "eslint .", "test": "jest --ci", "dev": "vite"}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("small.txt"), "hello\n").unwrap();
    let big = std::fs::File::create(dir.path().join("big.bin")).unwrap();
    big.set_len(64 << 20).unwrap();
    dir
}

const CASES: &[Case] = &[
    case("ls -la", "ls", true, "ls"),
    case(
        "pwd; echo hi > /dev/null 2>&1",
        "seq(pwd,echo)",
        true,
        "shell-builtin",
    ),
    case("echo hi > out.txt", "echo", false, "shell-builtin"),
    case(
        "cat small.txt | head -n 2",
        "par(cat,head)",
        true,
        "read-file",
    ),
    case("cat big.bin", "cat", false, "read-file"),
    case("wc -l < big.bin", "wc", false, "read-file"),
    case("cat *.log", "cat", false, "read-file"),
    case("ls -R /", "ls", false, "ls-recursive"),
    case("rg TODO .", "rg", false, "search"),
    case("mkdir -p build", "mkdir", false, "file-change"),
    case("FOO=1", "(assign)", true, "-"),
    case("cd /tmp && pwd", "seq(cd,pwd)", true, "shell-builtin"),
    case(
        "echo $(git rev-parse HEAD)",
        "seq(git,echo)",
        true,
        "shell-builtin",
    ),
    case(
        "diff <(sort small.txt) <(sort small.txt)",
        "par(diff,sort,sort)",
        true,
        "read-file",
    ),
    case("git status --short", "git", false, "git-read"),
    case(
        "env RUST_LOG=debug cargo test -p core",
        "cargo",
        false,
        "cargo-test",
    ),
    case(
        "sh -c 'make -j4 && make test'",
        "seq(make,make)",
        false,
        "make",
    ),
    case(
        "bash -lc \"cargo +nightly build --release\"",
        "cargo",
        false,
        "cargo-build",
    ),
    case("npx tsc --noEmit", "tsc", false, "tsc"),
    case(
        "uv run --with rich python -m pytest -q",
        "python",
        false,
        "python-pytest",
    ),
    case("time nice -n 10 make all", "make", false, "make"),
    case("outfilter -q git status", "git", false, "git-read"),
    case("command -v cargo", "command", true, "command-lookup"),
    case(
        "make -j8 2>&1 | tee build.log",
        "par(make,tee)",
        false,
        "file-change",
    ),
    case(
        "./serve & curl -sf localhost:8080/health; wait -n; pytest",
        "seq(bg(serve),curl,wait,pytest)",
        false,
        "pytest",
    ),
    case(
        "cargo build && cargo test || echo failed",
        "seq(cargo,cargo,echo)",
        false,
        "shell-builtin",
    ),
    case(
        "(cd app && make) > build.log",
        "seq(cd,make)",
        false,
        "make",
    ),
    case(
        "mlx_lm.server --model m &",
        "bg(mlx_lm.server)",
        false,
        "mlx-server",
    ),
    case(
        "cd app && npm run build",
        "seq(cd,seq(rm,seq(tsc,vite)))",
        false,
        "js-bundle",
    ),
    case(
        "cd app && npm test",
        "seq(cd,seq(eslint,jest))",
        false,
        "js-test",
    ),
    case("cd app && pnpm run dev", "seq(cd,vite)", false, "js-bundle"),
    case("npm run build", "npm", false, "-"),
    case("./build.sh", "build.sh", false, "-"),
    case("make release", "make", false, "make"),
    case(
        "git commit -m \"$(cat <<'EOF'\nfix: thing\nEOF\n)\"",
        "seq(cat,git)",
        false,
        "git-write",
    ),
    case("for f in *.rs; do rustfmt $f; done", "unclear", false, "-"),
    case("$EDITOR notes.md", "$", false, "-"),
    case("eval \"$CMD\"", "eval", false, "-"),
];

#[test]
fn design_examples() {
    let dir = project();
    let profiles = Profiles::builtin();
    let wrappers = vec!["outfilter".to_string()];
    let r = Recognizer::new(&profiles, &wrappers);
    let mut failures = Vec::new();
    for c in CASES {
        let a = r.shell(c.src, dir.path());
        let profile = a
            .segments
            .last()
            .and_then(|s| s.profile.clone())
            .unwrap_or("-".into());
        let got = (render(&a), a.light, profile.as_str());
        if got != (c.structure.to_string(), c.light, c.profile) {
            failures.push(format!(
                "{:?}\n  want {:?} light={} profile={}\n  got  {:?} light={} profile={}",
                c.src, c.structure, c.light, c.profile, got.0, got.1, got.2
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn interactive_commands() {
    let dir = project();
    let profiles = Profiles::builtin();
    let r = Recognizer::new(&profiles, &[]);
    for (src, interactive, light) in [
        ("vim notes.md", true, false),
        ("python3", true, false),
        ("python3 tool.py", false, false),
        ("tail -f app.log", true, true),
        ("git rebase -i HEAD~3", true, false),
        ("git rebase main", false, false),
        ("cargo --version", false, true),
        ("python3 -V", false, true),
        ("cargo build --help", false, false),
    ] {
        let a = r.shell(src, dir.path());
        assert_eq!((a.interactive, a.light), (interactive, light), "{src}");
    }
}

#[test]
fn argv_entry_and_history_keys() {
    let dir = project();
    let profiles = Profiles::builtin();
    let r = Recognizer::new(&profiles, &[]);
    let argv: Vec<String> = ["git", "-C", "repo", "log", "--oneline"]
        .map(String::from)
        .into();
    let a = r.argv(&argv, dir.path());
    let s = &a.segments[0];
    assert_eq!(
        (s.tool.as_deref(), s.subcommand.as_deref()),
        (Some("git"), Some("log"))
    );
    let argv: Vec<String> = ["echo", ">", "x"].map(String::from).into();
    assert!(r.argv(&argv, dir.path()).light);
}

#[test]
fn script_recursion_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts": {"loop": "npm run loop"}}"#,
    )
    .unwrap();
    let profiles = Profiles::builtin();
    let a = Recognizer::new(&profiles, &[]).shell("npm run loop", dir.path());
    assert!(a.structure.is_none() && !a.light);
}
