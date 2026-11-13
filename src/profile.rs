//! Loads and matches tool profiles.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

const BUILTIN: &str = include_str!("../profiles/builtin.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Light,
    Medium,
    Heavy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Writes {
    #[default]
    None,
    Args,
    Paths,
    Project,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parallelism {
    #[serde(default)]
    pub env: Vec<String>,
    pub flag: Option<String>,
    #[serde(default)]
    pub jobserver: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub program: Vec<String>,
    #[serde(default)]
    pub subcommand: Vec<String>,
    #[serde(default)]
    pub args_any: Vec<String>,
    #[serde(default)]
    pub args_none: Vec<String>,
    #[serde(default)]
    pub value_flags: Vec<String>,
    pub class: Class,
    #[serde(default, deserialize_with = "size")]
    pub max_input: Option<u64>,
    #[serde(default, deserialize_with = "size")]
    pub mem: Option<u64>,
    #[serde(default)]
    pub writes: Writes,
    #[serde(default)]
    pub write_paths: Vec<String>,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub interactive_without_args: bool,
    #[serde(default)]
    pub service: bool,
    #[serde(default)]
    pub parallelism: Parallelism,
}

#[derive(Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profile: Vec<Profile>,
}

fn size<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bytes(u64),
        Text(String),
    }
    match Option::<Raw>::deserialize(d)? {
        None => Ok(None),
        Some(Raw::Bytes(b)) => Ok(Some(b)),
        Some(Raw::Text(t)) => parse_size(&t).map(Some).map_err(serde::de::Error::custom),
    }
}

pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let (num, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len()));
    let shift = match unit
        .trim()
        .to_ascii_uppercase()
        .trim_end_matches('B')
        .trim_end_matches('I')
    {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("unknown size unit in {text:?}")),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad size {text:?}"))?;
    let bytes = n * (1u64 << shift) as f64;
    if !(0.0..i64::MAX as f64).contains(&bytes) {
        return Err(format!("size out of range: {text:?}"));
    }
    Ok(bytes as u64)
}

pub fn positionals<'a>(args: &'a [String], value_flags: &[String]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
        } else if a == "--" {
            break;
        } else if a.starts_with('-') {
            skip_next = value_flags.iter().any(|f| f == a);
        } else if !a.starts_with('+') {
            out.push(a.as_str());
        }
    }
    out
}

fn has_arg(args: &[String], wanted: &str) -> bool {
    args.iter()
        .any(|a| a == wanted || (a.starts_with(wanted) && a[wanted.len()..].starts_with('=')))
}

impl Profile {
    fn score(&self, argv: &[String]) -> Option<usize> {
        let program = Path::new(argv.first()?).file_name()?.to_str()?;
        if !self.program.iter().any(|p| p == program) {
            return None;
        }
        let args = &argv[1..];
        let mut score = 1;
        if !self.subcommand.is_empty() {
            let pos = positionals(args, &self.value_flags);
            let words = self
                .subcommand
                .iter()
                .map(|s| s.split_whitespace().collect::<Vec<_>>())
                .filter(|s| pos.len() >= s.len() && pos[..s.len()] == s[..])
                .map(|s| s.len())
                .max()?;
            score += 2 * words;
        }
        if !self.args_any.is_empty() {
            self.args_any
                .iter()
                .any(|a| has_arg(args, a))
                .then_some(())?;
            score += 1;
        }
        if !self.args_none.is_empty() {
            (!self.args_none.iter().any(|a| has_arg(args, a))).then_some(())?;
            score += 1;
        }
        Some(score)
    }
}

pub struct Profiles {
    list: Vec<Profile>,
}

impl Profiles {
    pub fn load(dir: Option<&Path>) -> Result<Self> {
        let mut list = parse(BUILTIN).context("built-in profiles")?;
        if let Some(dir) = dir.filter(|d| d.is_dir()) {
            let mut files: Vec<_> = std::fs::read_dir(dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect();
            files.sort();
            for file in files {
                let text = std::fs::read_to_string(&file)?;
                for p in parse(&text).with_context(|| format!("profile file {}", file.display()))? {
                    list.retain(|q| q.name != p.name);
                    list.push(p);
                }
            }
        }
        Ok(Profiles { list })
    }

    #[cfg(test)]
    pub fn builtin() -> Self {
        Self::load(None).expect("built-in profiles parse")
    }

    pub fn find(&self, argv: &[String]) -> Option<&Profile> {
        self.list
            .iter()
            .filter_map(|p| p.score(argv).map(|s| (s, p)))
            .fold(None, |best: Option<(usize, &Profile)>, (s, p)| match best {
                Some((b, _)) if b > s => best,
                _ => Some((s, p)),
            })
            .map(|(_, p)| p)
    }
}

fn parse(text: &str) -> Result<Vec<Profile>> {
    Ok(toml::from_str::<ProfileFile>(text)?.profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn name(p: &Profiles, cmd: &str) -> Option<String> {
        p.find(&argv(cmd)).map(|p| p.name.clone())
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("2GiB"), Ok(2 << 30));
        assert!(parse_size("3 parsecs").is_err());
        assert!(parse_size("-1G").is_err());
        assert!(parse_size("100000000000G").is_err());
    }

    #[test]
    fn picks_the_most_specific_profile() {
        let p = Profiles::builtin();
        assert_eq!(
            name(&p, "cargo build --release").as_deref(),
            Some("cargo-build")
        );
        assert_eq!(
            name(&p, "cargo +nightly test -p x").as_deref(),
            Some("cargo-test")
        );
        assert_eq!(
            name(&p, "/usr/bin/git -C repo status").as_deref(),
            Some("git-read")
        );
        assert_eq!(name(&p, "ls -la").as_deref(), Some("ls"));
        assert_eq!(name(&p, "ls -R").as_deref(), Some("ls-recursive"));
        assert_eq!(name(&p, "unknown-tool build"), None);
    }

    #[test]
    fn user_profiles_replace_builtin_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mine.toml"),
            r#"
[[profile]]
name = "cargo-build"
program = ["cargo"]
subcommand = ["build"]
class = "medium"
mem = "1G"

[[profile]]
name = "mytool"
program = ["mytool"]
class = "heavy"
"#,
        )
        .unwrap();
        let p = Profiles::load(Some(dir.path())).unwrap();
        let cargo = p.find(&argv("cargo build")).unwrap();
        assert_eq!((cargo.class, cargo.mem), (Class::Medium, Some(1 << 30)));
        assert_eq!(name(&p, "mytool").as_deref(), Some("mytool"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let bad =
            "[[profile]]\nname = \"x\"\nprogram = [\"x\"]\nclass = \"light\"\nmemory = \"1G\"\n";
        std::fs::write(dir.path().join("bad.toml"), bad).unwrap();
        assert!(Profiles::load(Some(dir.path())).is_err());
    }
}
