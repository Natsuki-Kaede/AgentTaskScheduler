//! Redaction of secret-looking values before a command is stored or shown
//!.

const SECRET_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "credential",
    "auth",
];

fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w)) || lower.ends_with("_key") || lower == "key"
}

pub fn command_words(words: &[String]) -> Vec<String> {
    redact_words(words, 0)
}

fn redact_words(words: &[String], depth: usize) -> Vec<String> {
    let mut words = words.to_vec();
    redact_shell_arguments(&mut words, depth);
    let mut out = Vec::with_capacity(words.len());
    let mut hide_next = false;
    for word in &words {
        if hide_next {
            out.push("[redacted]".to_string());
            hide_next = false;
            continue;
        }
        if let Some(header) = redact_header(word) {
            out.push(header);
            continue;
        }
        if word.eq_ignore_ascii_case("bearer") {
            hide_next = true;
            out.push(word.clone());
            continue;
        }
        if let Some((name, _)) = word.split_once('=') {
            if looks_secret(name.trim_start_matches('-')) {
                out.push(format!("{name}=[redacted]"));
                continue;
            }
        } else if let Some(flag) = word.strip_prefix("--")
            && looks_secret(flag)
        {
            hide_next = true;
        }
        out.push(word.clone());
    }
    out
}

fn redact_shell_arguments(words: &mut [String], depth: usize) {
    for index in 0..words.len() {
        let program = words[index]
            .rsplit([';', '&', '|', '(', ')'])
            .next()
            .unwrap_or_default();
        let program = std::path::Path::new(program)
            .file_name()
            .and_then(|name| name.to_str());
        if !program.is_some_and(|p| ["sh", "bash", "zsh", "dash", "ksh"].contains(&p)) {
            continue;
        }
        let mut next = index + 1;
        while let Some(flag) = words.get(next) {
            if flag == "--" || !flag.starts_with('-') {
                break;
            }
            if flag == "--command" || (!flag.starts_with("--") && flag.contains('c')) {
                if let Some(source) = words.get_mut(next + 1) {
                    *source = shell_source_at(source, depth + 1);
                }
                break;
            }
            if let Some(source) = flag.strip_prefix("--command=") {
                words[next] = format!("--command={}", shell_source_at(source, depth + 1));
                break;
            }
            next += if ["-o", "-O", "--rcfile", "--init-file"].contains(&flag.as_str()) {
                2
            } else {
                1
            };
        }
    }
}

fn redact_header(word: &str) -> Option<String> {
    let (prefix, header) = if let Some(header) = word.strip_prefix("--header=") {
        ("--header=", header)
    } else if let Some(header) = word.strip_prefix("-H") {
        ("-H", header)
    } else {
        ("", word)
    };
    let (name, value) = header.split_once(':')?;
    ((name.trim().eq_ignore_ascii_case("authorization")
        || name.trim().eq_ignore_ascii_case("proxy-authorization"))
        && !value.trim().is_empty())
    .then(|| format!("{prefix}{name}: [redacted]"))
}

pub fn shell_source(source: &str) -> String {
    shell_source_at(source, 0)
}

fn shell_source_at(source: &str, depth: usize) -> String {
    if depth >= 8 {
        return "[redacted: nested shell source]".into();
    }
    match shlex::split(source) {
        Some(words) => redact_words(&words, depth).join(" "),
        None => "[redacted: unparsed shell source]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn redacts_secret_values() {
        let cases = [
            ("cargo build --release", "cargo build --release"),
            ("API_KEY=abc123 npm test", "API_KEY=[redacted] npm test"),
            ("tool --token=abc run", "tool --token=[redacted] run"),
            (
                "tool --password hunter2 run",
                "tool --password [redacted] run",
            ),
            (
                "curl -H Authorization: Bearer xyz",
                "curl -H Authorization: Bearer [redacted]",
            ),
            ("make GITHUB_TOKEN=t1", "make GITHUB_TOKEN=[redacted]"),
            ("env PATH=/bin ls", "env PATH=/bin ls"),
        ];
        for (input, want) in cases {
            assert_eq!(command_words(&w(input)).join(" "), want, "{input}");
        }
    }

    #[test]
    fn redacts_quoted_and_escaped_values_as_complete_words() {
        let cases = [
            (
                "API_TOKEN='alpha beta' ./build.sh",
                "API_TOKEN=[redacted] ./build.sh",
            ),
            (
                "tool --password 'first second' run",
                "tool --password [redacted] run",
            ),
            (
                "tool --password=\"first second\" run",
                "tool --password=[redacted] run",
            ),
            (
                r"tool --password first\ second run",
                "tool --password [redacted] run",
            ),
            (
                "tool '--password' 'first second' run",
                "tool --password [redacted] run",
            ),
            (
                "API_TOKEN='alpha\nbeta' ./build.sh",
                "API_TOKEN=[redacted] ./build.sh",
            ),
            (
                r"tool --password 'first'\'' second' run",
                "tool --password [redacted] run",
            ),
            (
                "tool --password 'unfinished value",
                "[redacted: unparsed shell source]",
            ),
        ];
        for (source, want) in cases {
            assert_eq!(shell_source(source), want, "{source}");
        }
        assert_eq!(
            command_words(&["tool".into(), "--password".into(), "first second".into()]),
            ["tool", "--password", "[redacted]"]
        );
    }

    #[test]
    fn redacts_credentials_inside_header_arguments() {
        for header in [
            "Authorization: Bearer secret",
            "authorization:Bearer secret==",
            "Proxy-Authorization: Basic secret",
            "--header=Authorization: Bearer secret",
            "-HAuthorization: Bearer secret",
        ] {
            let words = vec!["curl".into(), "-H".into(), header.into()];
            let redacted = command_words(&words).join(" ");
            assert!(!redacted.contains("secret"), "{header}: {redacted}");
            assert!(redacted.contains("[redacted]"));
        }
        assert_eq!(
            shell_source(
                "cargo build && curl -H 'Authorization: Bearer secret' https://example.invalid"
            ),
            "cargo build && curl -H Authorization: [redacted] https://example.invalid"
        );
        assert_eq!(
            command_words(&["Accept: application/json".into()]),
            ["Accept: application/json"]
        );
    }
}

#[cfg(test)]
mod nested_tests {
    use super::*;

    #[test]
    fn nested_shell_secrets_are_redacted_in_both_entry_forms() {
        for shell in ["sh", "/bin/bash", "zsh", "dash", "ksh"] {
            for flags in [vec!["-c"], vec!["-lc"], vec!["-o", "errexit", "-c"]] {
                let mut words = vec!["env".to_string(), shell.into()];
                words.extend(flags.into_iter().map(str::to_string));
                words.push("curl --token PRIVATE_TEST_VALUE https://example.invalid".into());
                let source = words
                    .iter()
                    .map(|w| crate::hook::quote(w))
                    .collect::<Vec<_>>()
                    .join(" ");
                for redacted in [command_words(&words).join(" "), shell_source(&source)] {
                    assert!(!redacted.contains("PRIVATE_TEST_VALUE"), "{redacted}");
                    assert!(redacted.contains("[redacted]"), "{redacted}");
                }
            }
        }
        assert!(
            !shell_source("echo ready;sh -c 'tool --token PRIVATE_TEST_VALUE'")
                .contains("PRIVATE_TEST_VALUE")
        );
        let source = r#"bash -c "sh -c 'tool --password PRIVATE_TEST_VALUE'""#;
        assert!(!shell_source(source).contains("PRIVATE_TEST_VALUE"));
        let words = vec![
            "bash".into(),
            "-c".into(),
            "tool --token 'PRIVATE_TEST_VALUE".into(),
        ];
        assert!(
            !command_words(&words)
                .join(" ")
                .contains("PRIVATE_TEST_VALUE")
        );
    }
}
