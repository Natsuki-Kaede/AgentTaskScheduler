# DeepSeek Harness integration

ats supports DSH 0.2.2-rc.1 and later versions in the 0.2 series on macOS and Linux. The bundle has no npm dependencies. Its files are embedded in the ats binary, so installing ats does not require keeping a source checkout.

## Install and update

Initialize the intended DSH profile by starting it once, then run:

```sh
ats install --agent dsh --dry-run
ats install --agent dsh --yes
```

The default profile is `desktop`. Add `--dsh-profile headless`, `--dsh-profile web` or `--dsh-profile tui` for another initialized profile. Running `ats install` without `--agent` also detects the selected DSH profile alongside Claude Code and Codex.

`DSH_HOME` selects the DSH home directory (default `~/.dsh`). `DSH_BIN` selects the launcher executable; otherwise ats uses the standard macOS application launcher when present, or `dsh` on PATH. If `--agent dsh` is unavailable, update ats from current source:

```sh
cargo install --locked --git https://github.com/Natsuki-Kaede/AgentTaskScheduler --force
```

Installation previews the changes and backs up each changed file. It writes:

- `~/dsh-plugins/agent-task-scheduler/{package.json,cordis.patch.yml,index.js}`, with the absolute ats binary path in the installed plugin;
- a `link:` dependency and `@local/agent-task-scheduler` bundle entry in `$DSH_HOME/profiles/<profile>/package.json`;
- a compact managed section in `$DSH_HOME/AGENTS.md` and full command guidance in `$DSH_HOME/ATS.md`;
- the usual ats login service.

With a custom `DSH_HOME`, plugin files live under `$DSH_HOME/plugins/agent-task-scheduler/` so separate homes can be installed and removed independently.

The installer runs DSH's package manager with `--ignore-scripts` to refresh the profile's links and lockfile. Existing dependencies, bundle order and profile overrides are preserved. Reinstalling updates the embedded files and binary path. If linking fails after the files are written, retry installation.

After replacing the ats binary, follow the [upgrade steps](../../INSTALL.md#upgrade-an-existing-installation) and run `ats install --agent dsh --restart` for the selected profile. Restart that profile after installing or updating. Verify task history with `ats status`. DSH agents then send heavy commands through `ats run`; small commands remain direct.

## Reminders and session ownership

The integration follows the installed DSH 0.2.2 interfaces from the official [DeepSeek Harness source](https://github.com/deepseek-ai/deepseek-harness):

| DSH interface | ats behavior |
|---|---|
| Native `shellEnv.collect`, consumed by the Bash tool | Supplies `DSH_SESSION_ID`; ats assigns submissions to that session |
| `agent/pre-step` message waterfall | Adds completed-submission context to the first admitted step of each turn |
| `agent/turn-stopping` and `agent.inject` | Continues the current turn once when submissions still run, asking the agent to wait or detach |
| `agent/disposed` and plugin teardown | Clears the plugin's per-session turn markers |

`DSH_SESSION_ID` takes precedence over inherited Codex or Claude session variables because DSH rebuilds its managed environment for each native shell execution. Each agent, including an in-process subagent using the native shell, owns its own reminders. The adapter does not rewrite shell commands or change their approval policy.

The bundle invokes `ats hook dsh stop` and `ats hook dsh user-prompt-submit`. Final results and states requiring attention (`blocked`, `capacity_insufficient`) have separate delivery receipts. Detached work can still report an unacknowledged attention state at Stop or the next turn, once per uninterrupted state. Stop reminders are limited to once per turn. Detach does not wake a stopped agent; keep waiting when the current goal requires the result. Hook failures and an unavailable daemon allow the agent to continue. A canceled turn does not receive a stop reminder. Reminders require the native DSH shell's session environment; an unrelated external terminal needs an explicit `ats run --session <id>` to join that session.

## Remove

```sh
ats uninstall --agent dsh --dsh-profile desktop
```

The selected profile loses ats's dependency and bundle entry. Shared plugin files and managed instructions stay while another profile references the bundle. Removing the last configured profile removes these files and instructions. The login service remains when uninstalling a named agent. Plain `ats uninstall` removes the service and ats's links from all configured profiles in the current DSH home. If refreshing a profile's links fails, rerun the same uninstall command.

## Verification

`scripts/check.sh` runs Rust checks and Node adapter tests. Tests use temporary directories and do not change real ats data or DSH profiles. The optional runtime test loads an installed DSH core with a scripted model and its native Bash tool, shell environment registry and local subprocess provider. It uses a temporary ats daemon and needs no model credentials or network.

For a standalone runtime, set `DSH_RUNTIME_PACKAGE` to its `package.json` and run `node --test plugins/dsh/test/runtime.test.mjs` after `cargo build`. For the macOS desktop application:

```sh
ELECTRON_RUN_AS_NODE=1 \
DSH_RUNTIME_PACKAGE='/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/package.json' \
'/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness' \
  --test plugins/dsh/test/runtime.test.mjs
```

Set `ATS_BINARY` to verify another ats binary and `DSH_ATS_PLUGIN` to load an installed plugin entry point. This test checks native Bash session attribution and exit code 75, one same-turn running reminder, detach, and one completion reminder in the next turn. Its directory provider is fixed to a temporary directory. It does not make real model requests or drive the desktop UI.

The session assertion deliberately supplies a foreign inherited `CODEX_THREAD_ID`
and verifies that the native DSH session wins. The plugin receives `ATS_BINARY`
through its `binary` configuration so the same runtime test can exercise source
or an installed entry point against a candidate build. Local cross-version and
runtime results are recorded in [compatibility verification](../../docs/compatibility-verification.md).
