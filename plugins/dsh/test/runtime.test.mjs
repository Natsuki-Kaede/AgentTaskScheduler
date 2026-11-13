// Tests reminders against an installed DSH runtime.

import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
const { apply, hook } = await import(process.env.DSH_ATS_PLUGIN ?? new URL('../index.js', import.meta.url));

const execute = promisify(execFile);
const quote = (text) => `'${text.replaceAll("'", "'\\''")}'`;

test('installed DSH runs native bash, resumes a pending turn and delivers completion once',
  { skip: !process.env.DSH_RUNTIME_PACKAGE, timeout: 30_000 }, async (t) => {
    const require = createRequire(process.env.DSH_RUNTIME_PACKAGE);
    const { Context, Service } = require('@deepseek-ai/cordis');
    const { SessionStore } = require('@deepseek-ai/dsh-session');
    const { AgentRegistry } = require('@deepseek-ai/dsh-agent');
    const { SessionProjectionRegistry } = require('@deepseek-ai/dsh-session-projection');
    const { SystemPrompt } = require('@deepseek-ai/dsh-system-prompt');
    const { ToolsService } = require('@deepseek-ai/dsh-tools');
    const { apply: nativeTools } = require('@deepseek-ai/dsh-tool-executor-native');
    const { apply: bashTool } = require('@deepseek-ai/dsh-tool-bash');
    const { LocalBashExecutor } = require('@deepseek-ai/dsh-bash-local');
    const { LocalSubprocessRuntime } = require('@deepseek-ai/dsh-subprocess-local');
    const { ShellEnvRegistry } = require('@deepseek-ai/dsh-shell-env');
    const { AgentLoop } = require('@deepseek-ai/dsh-agent-loop');
    const { createUserMessage } = require('@deepseek-ai/dsh-llm');
    const root = await mkdtemp(join(tmpdir(), 'ats-dsh-runtime-'));
    const binary = process.env.ATS_BINARY ?? fileURLToPath(new URL('../../../target/debug/ats', import.meta.url));
    const prior = { ATS_HOME: process.env.ATS_HOME, ATS_JOB_ID: process.env.ATS_JOB_ID,
      CODEX_THREAD_ID: process.env.CODEX_THREAD_ID };
    process.env.ATS_HOME = join(root, 'ats');
    delete process.env.ATS_JOB_ID;
    process.env.CODEX_THREAD_ID = 'inherited-foreign-codex-session';
    const ctx = new Context();
    const daemon = spawn(binary, ['daemon'], { stdio: 'ignore' });
    t.after(async () => {
      try {
        await ctx.fiber.dispose();
      } finally {
        try {
          if (daemon.exitCode === null && daemon.signalCode === null) {
            const exit = once(daemon, 'exit');
            daemon.kill();
            await exit;
          }
        } finally {
          for (const [key, value] of Object.entries(prior)) {
            if (value === undefined) delete process.env[key]; else process.env[key] = value;
          }
          await rm(root, { recursive: true, force: true });
        }
      }
    });
    const status = async () => JSON.parse((await execute(binary, ['status', '--json'])).stdout);
    for (let n = 0; ; n++) {
      try { await status(); break; } catch (error) {
        if (n > 100) throw error;
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
    }
    new SessionProjectionRegistry(ctx);
    new SessionStore(ctx);
    const registry = new AgentRegistry(ctx);
    await registry[Service.init]();
    new SystemPrompt(ctx, { includeHarnessIdentity: false, includeRuntimeContext: true, personaPrefix: '', personaSuffix: '' });
    const requests = [];
    let submission;
    ctx.provide('llm', {
      async prepareCall(config) { return { config, stream: (request) => this.stream(request) }; },
      async *stream(request) {
        requests.push(request);
        let command;
        if (requests.length === 1) command = `${quote(binary)} run --no-wait --sh ${quote('perl -e "sleep 2"')}`;
        if (requests.length === 3) command = `${quote(binary)} detach ${submission}`;
        if (command) yield { type: 'block-end', index: 0, block: { type: 'tool-call', id: `c${requests.length}`,
          name: 'bash', arguments: JSON.stringify({ description: 'Verify ats integration', command }) } };
        else yield { type: 'text-delta', index: 0, text: 'done' };
        yield { type: 'finish', reason: { kind: 'completed' } };
      },
    });
    new ToolsService(ctx);
    nativeTools(ctx);
    new LocalSubprocessRuntime(ctx);
    const config = Object.fromEntries(Object.entries({ cwd: root, timeoutMs: 10_000, maxTimeoutMs: 10_000,
      maxOutputBytes: 64_000, maxSpillBytes: 1_000_000, graceMs: 500 }).map(([key, value]) => [key, { get: () => value }]));
    new LocalBashExecutor(ctx, config);
    new ShellEnvRegistry(ctx, { dshHome: join(root, 'dsh') });
    ctx.provide('workingDirectory', { ensure: async () => root });
    bashTool(ctx, { enableRunInBackground: false });
    const exits = [];
    ctx.on('tools/result', (_exec, result) => {
      exits.push(result.value?.exitCode);
      const text = result.value?.stderr?.text ?? '';
      submission ??= text.match(/\[ats\] submission (s_\w+)/)?.[1];
    });
    const loop = new AgentLoop(ctx, { agents: [], maxParallelToolCalls: { get: () => 10 } });
    await loop[Service.init]();
    apply(ctx, { binary });
    const handle = await ctx.agents.create({ sessionId: 'session-11111111-2222-3333-4444-555555555555',
      agentOptions: { provider: 'scripted', model: 'scripted' }, meta: { cwd: root } });
    const turn = async () => {
      handle.agent.followup(createUserMessage({ source: { kind: 'user' }, content: [{ type: 'text', text: 'Check the job' }] }));
      await handle.agent.whenIdle();
    };
    await turn();
    assert.ok(submission);
    const attributed = JSON.parse((await execute(binary, ['status', submission, '--json'])).stdout);
    assert.equal(attributed.session_id, handle.agent.id, 'native DSH session must override inherited Codex session');
    assert.deepEqual(exits, [75, 0]);
    assert.equal(requests.length, 4);
    assert.match(JSON.stringify(requests[2].messages), /still running/);
    assert.equal(handle.agent.session.snapshotEvents().filter((e) => e.type === 'turn/start').length, 1);
    for (let n = 0; ; n++) {
      if ((await status()).every((job) => job.state === 'completed')) break;
      assert.ok(n < 100, 'job did not finish');
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    await turn();
    assert.match(JSON.stringify(requests[4].messages), /ended since your last turn/);
    const fresh = await hook(binary, 'user-prompt-submit', { session_id: handle.agent.id });
    assert.deepEqual(fresh, {});
    assert.equal(handle.agent.session.snapshotEvents().filter((e) => e.type === 'turn/start').length, 2);
    const completed = await status();
    assert.equal(completed.length, 1);
    assert.equal(completed[0].exit_code, 0);
    t.diagnostic(JSON.stringify({ atsBinary: binary, bashExitCodes: exits, sessionOwned: true,
      turns: 2, runningReminder: true, completionReminderConsumed: true, exitCode: 0 }));
    await handle.dispose();
  });
