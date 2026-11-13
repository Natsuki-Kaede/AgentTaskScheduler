//! What to run: the two entry forms of `ats run`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "entry", rename_all = "lowercase")]
pub enum RunSpec {
    Argv { argv: Vec<String> },
    Sh { source: String, shell: String },
}

impl RunSpec {
    pub fn command(&self) -> std::process::Command {
        match self {
            RunSpec::Argv { argv } => {
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
            RunSpec::Sh { source, shell } => {
                let mut c = std::process::Command::new(shell);
                c.arg("-c").arg(source);
                c
            }
        }
    }

    pub fn default_shell() -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}
