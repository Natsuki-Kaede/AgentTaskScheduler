import assert from 'node:assert/strict';
import { test } from 'node:test';
import { apply, hook } from '../index.js';

function setup(call) {
  const events = new Map();
  const ctx = { on: (name, listener) => events.set(name, listener), effect() {}, logger: { warn() {} } };
  apply(ctx, { call });
  const agent = (id) => ({ id, session: { header: { id } }, injected: [], inject(message) { this.injected.push(message); } });
  return { events, a: agent('session-a'), b: agent('session-b') };
}

test('completion reminders enter the current step once per turn and session', async () => {
  const calls = [];
  const { events, a, b } = setup(async (event, input) => {
    calls.push([event, input.session_id]);
    return { hookSpecificOutput: { additionalContext: `completed: ${input.session_id}` } };
  });
  const next = async () => ({ kind: 'enter', messages: [] });
  const step = (agent, turn) => events.get('agent/pre-step')({ agent, turn }, next);
  assert.equal((await step(a, 1)).messages[0].content[0].text, 'completed: session-a');
  assert.equal((await step(a, 1)).messages.length, 0);
  assert.equal((await step(b, 1)).messages[0].content[0].text, 'completed: session-b');
  await step(a, 2);
  assert.deepEqual(calls.map((c) => c[1]), ['session-a', 'session-b', 'session-a']);
  events.get('agent/disposed')({ agent: a });
  await step(a, 2);
  assert.equal(calls.length, 4);
});

test('running reminders continue the same turn once and respect cancellation', async () => {
  const { events, a } = setup(async () => ({ decision: 'block', reason: 'wait or detach' }));
  const stopping = (turn, signal = new AbortController().signal) => events.get('agent/turn-stopping')({ agent: a, turn, signal });
  await stopping(1);
  await stopping(1);
  assert.equal(a.injected.length, 1);
  assert.equal(a.injected[0].content[0].text, 'wait or detach');
  await stopping(2, AbortSignal.abort());
  assert.equal(a.injected.length, 1);
  await stopping(3);
  assert.equal(a.injected.length, 2);
});

test('rejected steps do not consume reminders and failed hooks allow progress', async () => {
  let calls = 0;
  const { events, a } = setup(async () => { calls++; throw new Error('offline'); });
  const step = events.get('agent/pre-step');
  const rejected = { kind: 'reject' };
  assert.equal(await step({ agent: a, turn: 1 }, async () => rejected), rejected);
  assert.equal(calls, 0);
  const entered = { kind: 'enter', messages: [] };
  assert.equal(await step({ agent: a, turn: 1 }, async () => entered), entered);
  await events.get('agent/turn-stopping')({ agent: a, turn: 1, signal: new AbortController().signal });
  assert.equal(a.injected.length, 0);
  assert.deepEqual(await hook('/nonexistent/ats', 'stop', { session_id: 's' }), {});
});
