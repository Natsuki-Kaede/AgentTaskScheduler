// Delivers ats session reminders through DSH lifecycle hooks.

import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';

export const name = 'agent-task-scheduler';
export const inject = ['agents'];
const DEFAULT_ATS = 'ats';

export function hook(binary, event, input) {
  return new Promise((resolve) => {
    const child = spawn(binary, ['hook', 'dsh', event], { stdio: ['pipe', 'pipe', 'pipe'] });
    let output = '';
    const timer = setTimeout(() => { child.kill(); resolve({}); }, 2500);
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (text) => {
      output += text;
      if (output.length > 256 * 1024) child.kill();
    });
    child.stderr.resume();
    child.stdin.on('error', () => {});
    child.on('error', () => { clearTimeout(timer); resolve({}); });
    child.on('close', (code) => {
      clearTimeout(timer);
      try { resolve(code === 0 ? JSON.parse(output) : {}); } catch { resolve({}); }
    });
    child.stdin.end(JSON.stringify(input));
  });
}

function message(text) {
  return { id: randomUUID(), role: 'user', source: { kind: 'ats' }, content: [{ type: 'text', text }] };
}

export function apply(ctx, config = {}) {
  const call = config.call ?? ((event, input) => hook(config.binary ?? DEFAULT_ATS, event, input));
  const sessions = new Map();
  const state = (agent) => {
    let record = sessions.get(agent.id);
    if (!record) { record = {}; sessions.set(agent.id, record); }
    return record;
  };
  const remind = async (event, agent) => {
    try { return await call(event, { session_id: agent.session.header.id, stop_hook_active: false }); }
    catch (error) { ctx.logger.warn(`ats reminder: ${error.message}`); return {}; }
  };
  ctx.on('agent/pre-step', async ({ agent, turn }, next) => {
    const decision = await next();
    if (decision.kind !== 'enter') return decision;
    const record = state(agent);
    if (record.promptTurn === turn) return decision;
    record.promptTurn = turn;
    const result = await remind('user-prompt-submit', agent);
    const text = result?.hookSpecificOutput?.additionalContext;
    return text ? { ...decision, messages: [...decision.messages, message(text)] } : decision;
  });
  ctx.on('agent/turn-stopping', async ({ agent, turn, signal }) => {
    if (signal.aborted) return;
    const record = state(agent);
    if (record.stopTurn === turn) return;
    record.stopTurn = turn;
    const result = await remind('stop', agent);
    if (!signal.aborted && result?.decision === 'block' && result.reason) {
      agent.inject(message(result.reason));
    }
  });
  ctx.on('agent/disposed', ({ agent }) => sessions.delete(agent.id));
  ctx.effect(() => () => sessions.clear(), 'ats.dsh');
}
