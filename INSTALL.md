# Install ats with an AI agent

This document is for an AI coding agent. The user asked you to install Agent Task Scheduler (`ats`). Do the steps in sequence.

Some steps change files outside the current project. Before each of these steps, show the user the changes and ask. Do not continue without the agreement of the user.

## 1. Check the machine

Run `uname -sm`. Find the target for the output in this table:

| `uname -sm` | Target |
|---|---|
| `Darwin arm64` | `aarch64-apple-darwin` |
| `Darwin x86_64` | `x86_64-apple-darwin` |
| `Linux aarch64` | `aarch64-unknown-linux-musl` |
| `Linux x86_64` | `x86_64-unknown-linux-musl` |

If the output is not in the table, stop. Tell the user that ats runs on macOS and Linux, on 64-bit Arm and x86.

## 2. Install the binary

If `ats --version` prints a version, go to step 3.

If `brew` is available, run this command:

```sh
brew install natsuki-kaede/tap/ats
```

If `brew` is not available, download the binary. Replace `<target>` with the target from step 1:

```sh
mkdir -p ~/.local/bin
curl -fsSL https://github.com/Natsuki-Kaede/AgentTaskScheduler/releases/latest/download/ats-<target>.tar.gz | tar -xz -C ~/.local/bin --strip-components 1 ats-<target>/ats
```

If the download fails and `cargo` is available, run this command:

```sh
cargo install --locked --git https://github.com/Natsuki-Kaede/AgentTaskScheduler
```

Run `ats --version`. If the shell does not find `ats`, tell the user to add the install directory to `PATH`: `$(brew --prefix)/bin`, `~/.local/bin` or `~/.cargo/bin`.

## 3. Connect your agent

`ats install` writes nothing when you give it `--dry-run`. It writes without a question when you give it `--yes`.

### Claude Code or Codex

1. Run `ats install --agent claude --dry-run`. For Codex, use `--agent codex`. The command shows the changes as diffs.
2. Show the diffs to the user and ask.
3. If the user agrees, run the same command with `--yes` in place of `--dry-run`.

This adds the instructions for the model, two reminder hooks, and a login service that starts the daemon. The login service is a LaunchAgent on macOS and a systemd user unit on Linux.

For Codex, full guidance goes in `~/.codex/ATS.md`; the managed section in `~/.codex/AGENTS.md` contains only the scheduling trigger and a reference to read for queued jobs or errors. Reinstalling updates these files, and uninstalling removes the managed section and its reference file.

### DeepSeek Harness (DSH)

Requires DSH 0.2.2-rc.1 or newer in the 0.2 series and an initialized profile. If `ats install --help` does not list `dsh`, install ats from current source with `cargo install --locked --git https://github.com/Natsuki-Kaede/AgentTaskScheduler --force`.

1. Run `ats install --agent dsh --dry-run`. The default profile is `desktop`; use `--dsh-profile <name>` for another initialized profile. `DSH_HOME` overrides `~/.dsh`.
2. Show the diff and get permission if installation has not already been authorized.
3. Run the same command with `--yes` instead of `--dry-run`.

This adds a compact managed section to `$DSH_HOME/AGENTS.md`, full guidance to `$DSH_HOME/ATS.md`, and a dependency-free reminder bundle in `~/dsh-plugins/agent-task-scheduler/`. It adds a `link:` dependency and bundle entry to the selected profile's `package.json`, runs DSH's package manager with lifecycle scripts disabled, and installs the ats login service. Each changed file is backed up before writing. A custom `DSH_HOME` stores its plugin under `$DSH_HOME/plugins/agent-task-scheduler/` to keep separate homes independent.

The installer finds `dsh` on PATH or in the standard macOS app location. Set `DSH_BIN` to an executable launcher path for another installation. Restart the selected profile to load its new bundle. DSH's native Bash tool supplies `DSH_SESSION_ID`; the plugin delivers reminders within that same session.

To remove the integration, use `ats uninstall --agent dsh --dsh-profile <name>`. Shared plugin files and instructions stay until the last configured profile removes ats. The daemon service stays when `--agent` is specified. See [DSH integration](plugins/dsh/README.md) for verification and interface details.

### A different agent

1. Run `ats install --service --dry-run`. Show the diff to the user and ask. If the user agrees, run `ats install --service --yes`. This adds the login service.
2. Run `ats instructions`. It prints a section between `<!-- ats:begin -->` and `<!-- ats:end -->`.
3. Find the file with your global instructions, for example `~/.config/opencode/AGENTS.md` for OpenCode or `~/.gemini/GEMINI.md` for Gemini CLI. Tell the user that you will add the section at the end of this file, and ask. If the user agrees, add the section.

If `launchctl` or `systemctl` fails because your sandbox blocks it, tell the user to run the same `ats install` command in a terminal.

If the Linux machine has no systemd, for example in a container, `systemctl` fails. Then start the daemon with `nohup ats daemon >/dev/null 2>&1 &`. Tell the user that the daemon does not start again after a restart of the machine.

## 4. Check the result

Run `ats status`. It must print `no jobs` or a list of jobs. If it prints an error, show the error to the user.

Tell the user these items:

- which files changed;
- that new agent sessions read the instructions, and sessions that run now do not;
- for DSH: restart the selected profile to load the reminder bundle;
- that Codex asks to trust the changed hooks when it starts the next time;
- on Linux: systemd stops the daemon when the user closes the last login session. If the user works over SSH, `loginctl enable-linger` keeps it running;
- that `ats uninstall` removes the changes of `ats install`. For a different agent, the user also removes the section between the two markers from the instructions file.
