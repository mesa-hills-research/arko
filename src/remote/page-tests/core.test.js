// Tests of the page's logic (core.js), outside the page: `node --test src/remote/page-tests/core.test.js`.
'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const core = require('../page/core.js');

let next = 0;
function item(kind, fields = {}) {
  next += 1;
  return { id: next, at: next * 1000, kind, agent: 'main', turn: 1, text: kind + ' ' + next, ...fields };
}

test('views keep what they say they keep', () => {
  next = 0;
  const items = [
    item('user', { turn: 1 }),
    item('text'),
    item('call', { status: 'ok' }),
    item('result'),
    item('worker_line', { worker: 'r1' }),
    item('call', { agent: 'r1', status: 'running' }),
    item('text', { agent: 'r1' }),
    item('call', { status: 'failed' }),
    item('notice', { level: 'info' }),
    item('notice', { level: 'error' }),
    item('reply'),
    item('turn'),
    item('call', { turn: 2, status: 'running' }),
  ];
  const kinds = (rows) => rows.map((row) => (row.count ? 'count' : row.item.kind + (row.item.agent === 'main' ? '' : ':' + row.item.agent)));
  assert.deepEqual(kinds(core.layout(items, 'full')), [
    'user', 'text', 'call', 'result', 'worker_line', 'call:r1', 'call', 'notice', 'notice', 'reply', 'turn', 'call',
  ]);
  assert.deepEqual(kinds(core.layout(items, 'quiet')), [
    'user', 'text', 'call', 'result', 'worker_line', 'call', 'notice', 'notice', 'reply', 'turn', 'call',
  ]);
  const messages = core.layout(items, 'messages');
  assert.deepEqual(kinds(messages), ['user', 'text', 'count', 'worker_line', 'notice', 'reply', 'turn', 'count']);
  assert.deepEqual(messages[2].count, { turn: 1, calls: 2, failed: 1, running: 0 });
  assert.equal(messages[2].id, 3, 'the count stands where the first call was');
  assert.deepEqual(messages[7].count, { turn: 2, calls: 1, failed: 0, running: 1 });
  assert.deepEqual(kinds(core.layout(items, 'full', 'r1')), ['call:r1', 'text:r1']);
});

test("what the user wrote to a worker shows in the worker's own view only", () => {
  next = 0;
  const items = [
    item('user'),
    item('call', { agent: 'r1', status: 'ok' }),
    item('user', { agent: 'r1' }),
    item('text', { agent: 'r1' }),
    item('text', { agent: 'r2' }),
  ];
  const kinds = (rows) => rows.map((row) => row.item.kind + (row.item.agent === 'main' ? '' : ':' + row.item.agent));
  assert.deepEqual(kinds(core.layout(items, 'full')), ['user', 'call:r1']);
  assert.deepEqual(kinds(core.layout(items, 'quiet')), ['user']);
  assert.deepEqual(kinds(core.layout(items, 'messages')), ['user']);
  assert.deepEqual(kinds(core.layout(items, 'full', 'r1')), ['call:r1', 'user:r1', 'text:r1']);
  assert.deepEqual(kinds(core.layout(items, 'full', 'r2')), ['text:r2']);
});

test('a result shows under its call, whatever came between', () => {
  next = 0;
  const items = [
    item('call', { status: 'ok' }),
    item('call', { agent: 'r1', status: 'running' }),
    item('text'),
    item('result', { call: 1 }),
    item('result', { agent: 'r1', call: 2 }),
  ];
  const ids = (rows) => rows.map((row) => row.id);
  assert.deepEqual(ids(core.layout(items, 'full')), [1, 4, 2, 3]);
  assert.deepEqual(ids(core.layout(items, 'quiet')), [1, 4, 3]);
  assert.deepEqual(ids(core.layout(items, 'full', 'r1')), [2, 5]);
  assert.equal(core.unseen(core.layout(items, 'full'), 2), 2);
});

test("a note the agent hasn't read waits at the end of its conversation", () => {
  next = 0;
  const items = [
    item('text'),
    item('user', { delivery: 'note', pending: true }),
    item('call', { status: 'ok' }),
    item('result', { call: 3 }),
    item('conversation'),
    item('user', { delivery: 'note', pending: true }),
    item('text'),
  ];
  const ids = (rows) => rows.map((row) => row.id);
  assert.deepEqual(ids(core.layout(items, 'full')), [1, 3, 4, 2, 5, 7, 6]);
  assert.deepEqual(ids(core.layout(items, 'quiet')), [1, 3, 4, 2, 5, 7, 6]);
  assert.deepEqual(ids(core.layout(items, 'messages')), [1, 3, 2, 7, 6]);

  // Once read, it comes again where the agent read it.
  const store = new core.Store();
  store.reset(items.slice(0, 4));
  assert.ok(store.remove(2));
  assert.ok(!store.remove(2));
  store.add({ ...items[1], id: 8, pending: undefined });
  assert.deepEqual(ids(core.layout(store.items, 'full')), [1, 3, 4, 8]);
  assert.equal(store.counts.main, 4);
});

test('the reader keeps their place at the nearest row', () => {
  const rows = [{ id: 2 }, { id: 5 }, { id: 9 }];
  assert.equal(core.nearest(rows, 5).id, 5);
  assert.equal(core.nearest(rows, 7).id, 5);
  assert.equal(core.nearest(rows, 1).id, 2);
  assert.equal(core.nearest(rows, 12).id, 9);
  assert.equal(core.nearest([], 3), null);
  assert.equal(core.nearest([{ id: 2 }, { id: 9 }, { id: 5 }], 7).id, 5, 'rows out of order');
});

test('the store keeps each transcript to its budget, and changes items in place', () => {
  const store = new core.Store();
  store.reset([], [{ id: 'r1', state: 'running' }]);
  for (let id = 1; id <= core.BUDGET.main + 5; id++) store.add({ id, agent: 'main', kind: 'text', text: '' });
  store.add({ id: 99999, agent: 'r1', kind: 'call', text: 'ls', status: 'running', outcome: 'x' });
  assert.equal(store.items.length, core.BUDGET.main + 1);
  assert.equal(store.items[0].id, 6);
  assert.ok(store.append(6, 'more'));
  assert.equal(store.byId.get(6).text, 'more');
  assert.ok(!store.append(1, 'gone'));
  store.change({ id: 99999, agent: 'r1', kind: 'call', text: 'ls', status: 'ok' });
  assert.equal(store.byId.get(99999).status, 'ok');
  assert.ok(!('outcome' in store.byId.get(99999)));
  assert.equal(store.workers.get('r1').state, 'running');
});

test('the current turn began with its first item', () => {
  const store = new core.Store();
  store.reset([
    { id: 1, at: 10, turn: null, agent: 'main' },
    { id: 2, at: 20, turn: 1, agent: 'main' },
    { id: 3, at: 30, turn: 2, agent: 'main' },
    { id: 4, at: 40, turn: 2, agent: 'r1' },
  ]);
  assert.deepEqual(store.currentTurn(), { turn: 2, since: 30 });
});

test('figures and workers read briefly', () => {
  assert.equal(core.duration(45_000), '45s');
  assert.equal(core.duration(134_000), '2m14s');
  assert.equal(core.duration(3_900_000), '1h05m');
  assert.deepEqual(
    core.figures({ context_tokens: 142_400, cost: 3.2, settings: { handover_at: 250_000 } }),
    ['142k/250k', '$3.20'],
  );
  assert.deepEqual(core.figures({ context_tokens: 900, cost: 0, plan_usage: 'ChatGPT plan 5 h 12%' }), ['900', '5 h 12%']);
  assert.deepEqual(
    core.figures({ cost: 0, api_price: 3.2, plan_usage: 'Claude plan 5 h 21%, 7 d 4%' }),
    ['5 h 21%, 7 d 4%', '≈ $3.20'],
  );
  // A mixed session: the main agent on a plan, the workers on a key.
  assert.deepEqual(
    core.figures({ cost: 1.25, api_price: 4.5, plan_usage: 'Claude plan 5 h 21%' }),
    ['5 h 21%', '$1.25', '≈ $4.50'],
  );
  const words = core.workerWords(
    { id: 'r2', title: 'Fix flaky test', state: 'running', tool_calls: 18, started_at: 0, ended_at: null, ran_ms: 0, running_since: 0, last: '$ cargo test' },
    134_000,
  );
  assert.deepEqual(words, { title: 'Fix flaky test', meta: '18 calls · 2m14s · running', last: '$ cargo test' });
  const done = core.workerWords({ id: 'r1', title: '', state: 'finished', tool_calls: 1, started_at: 0, ended_at: 5000, ran_ms: 5000, running_since: null, report: '\nAll good.\nMore.' }, 9e9);
  assert.deepEqual(done, { title: 'r1', meta: '1 call · 5s · finished', last: 'All good.' });
  // The time counts every run, also those before the session was opened
  // again, and not only the latest.
  const again = { id: 'r3', state: 'running', tool_calls: 40, started_at: 3_600_000, ended_at: null, ran_ms: 3_600_000, running_since: 3_600_000 };
  assert.equal(core.workerWords(again, 3_900_000).meta, '40 calls · 1h05m · running');
  const ended = { ...again, state: 'finished', ended_at: 3_900_000, ran_ms: 3_900_000, running_since: null };
  assert.equal(core.workerWords(ended, 9e9).meta, '40 calls · 1h05m · finished');
  const waiting = { id: 'r4', state: 'queued', tool_calls: 0, started_at: null, ended_at: null, ran_ms: 0, running_since: null };
  assert.equal(core.workerWords(waiting, 9e9).meta, '0 calls · queued');
  assert.equal(core.stateOf({ working: true, question: { id: 1 } }, 'live').label, 'Asks you');
  assert.equal(core.stateOf({ working: true }, 'live').key, 'working');
  assert.equal(core.stateOf(null, 'offline').key, 'offline');
});

test('markdown becomes a tree of blocks and runs', () => {
  const tree = core.markdown(
    [
      '# Done',
      '',
      'The **parser** handles `snake_case` and *emphasis*; see [docs](https://example.com) or https://x.dev/a.',
      'Next line.',
      '',
      '- one',
      '- two',
      '  - deeper',
      '1. first',
      '',
      '> quoted',
      '',
      '| a | b |',
      '|---|---|',
      '| 1 | 2 |',
      '',
      '```rust',
      'fn main() {}',
      '```',
      '---',
      '[bad](javascript:alert(1)) <b>not html</b>',
    ].join('\n'),
  );
  assert.deepEqual(tree.map((block) => block.type), ['h', 'p', 'list', 'list', 'quote', 'table', 'code', 'hr', 'p']);
  const runs = tree[1].children.map((run) => run.type);
  assert.deepEqual(runs, ['text', 'strong', 'text', 'code', 'text', 'em', 'text', 'link', 'text', 'link', 'text', 'br', 'text']);
  assert.equal(tree[1].children[9].href, 'https://x.dev/a');
  assert.equal(tree[2].items.length, 2);
  assert.equal(tree[2].items[1][1].type, 'list', 'a deeper list is in its item');
  assert.ok(tree[3].ordered);
  assert.equal(tree[5].rows[0][1][0].text, '2');
  assert.deepEqual(tree[6], { type: 'code', lang: 'rust', text: 'fn main() {}' });
  // A link that isn't http(s) or mailto is text; markup is text.
  assert.ok(tree[8].children.every((run) => run.type === 'text'));
  assert.equal(tree[8].children.map((run) => run.text).join(''), '[bad](javascript:alert(1)) <b>not html</b>');
});

test('markdown that is still streaming reads sensibly', () => {
  const tree = core.markdown('Here:\n```js\nlet a = 1;');
  assert.deepEqual(tree[1], { type: 'code', lang: 'js', text: 'let a = 1;' });
  const runs = core.inline('a **half-open and a_b_c');
  assert.equal(runs.map((run) => run.text).join(''), 'a **half-open and a_b_c');
});

test("an agent's thinking shows (folded) in Full and Quiet and in its own view, not in Messages", () => {
  next = 0;
  const items = [
    item('user'),
    item('thinking', { text: '**Planning**\nI should read the tests first.' }),
    item('text'),
    item('call', { status: 'ok' }),
    item('thinking', { agent: 'r1' }),
    item('call', { agent: 'r1', status: 'ok' }),
    item('reply'),
  ];
  const kinds = (rows) => rows.map((row) => (row.count ? 'count' : row.item.kind + (row.item.agent === 'main' ? '' : ':' + row.item.agent)));
  assert.deepEqual(kinds(core.layout(items, 'full')), ['user', 'thinking', 'text', 'call', 'call:r1', 'reply']);
  assert.deepEqual(kinds(core.layout(items, 'quiet')), ['user', 'thinking', 'text', 'call', 'reply']);
  assert.deepEqual(kinds(core.layout(items, 'messages')), ['user', 'text', 'count', 'reply']);
  assert.deepEqual(kinds(core.layout(items, 'full', 'r1')), ['thinking:r1', 'call:r1']);
  // The folded row's line: the first one with text, without bold marks.
  assert.equal(core.firstLine(items[1].text), 'Planning');
  assert.equal(core.firstLine('\n\n  Checking the parser.  \nMore.'), 'Checking the parser.');
  assert.equal(core.firstLine(''), '');
});

test('"Working…" shows while the agent waits on its model, and its thinking moves as it streams', () => {
  next = 0;
  const store = new core.Store();
  const still = { thinking: null, working: false };
  const working = { thinking: null, working: true };
  const status = { working: true, since: 0, question: null };
  const add = (kind, fields) => store.add(item(kind, fields), 1000);
  const set = (id, fields) => store.change({ ...store.byId.get(id), ...fields }, 1000);
  const at = (now = 1000, view = 'full', what = status) => core.motion(store, what, view, null, now);
  add('user', { delivery: 'turn' });
  assert.deepEqual(at(), working, 'a turn began');
  assert.deepEqual(at(1000, 'full', { working: false }), still, 'idle');
  assert.deepEqual(at(1000, 'full', { ...status, question: { id: 1, text: 'Tabs?' } }), still, 'it asks');
  add('thinking');
  assert.deepEqual(at(), { thinking: 2, working: false });
  assert.deepEqual(at(1000, 'messages'), working, 'Messages leaves thinking out');
  add('call', { status: 'running' });
  assert.deepEqual(at(), still);
  set(3, { status: 'ok' });
  assert.deepEqual(at(), working, 'the result went back');
  // Calls side by side: while one runs, it shows the agent works.
  add('call', { status: 'running' });
  add('call', { status: 'ok' });
  assert.deepEqual(at(), still);
  set(4, { status: 'ok' });
  assert.deepEqual(at(), working);
  // Its text streams, then goes quiet (it writes a long call).
  add('text');
  assert.deepEqual(at(), still);
  assert.deepEqual(at(1000 + core.QUIET_MS), working);
  store.append(6, ' more', 5000);
  assert.deepEqual(at(5000), still);
  add('reply');
  const turn = item('turn');
  store.add(turn, 1000);
  assert.deepEqual(at(), still, 'it replied');
  // A turn that begins without a message (the workers' news came).
  add('notice', { level: 'info' });
  assert.deepEqual(at(1000, 'full', { working: true, since: turn.at + 1 }), working);
});

test("a worker's own view says it works while it runs", () => {
  next = 0;
  const store = new core.Store();
  store.workers.set('r1', { id: 'r1', state: 'running' });
  const at = (view = 'full') => core.motion(store, { working: false }, view, 'r1', 1000);
  assert.deepEqual(at(), { thinking: null, working: true }, 'from its start');
  store.add(item('thinking', { agent: 'r1' }), 1000);
  assert.deepEqual(at(), { thinking: 1, working: false });
  assert.deepEqual(at('messages'), { thinking: 1, working: false }, 'its own view shows thinking');
  store.add(item('result', { agent: 'r1' }), 1000);
  assert.deepEqual(at(), { thinking: null, working: true });
  store.workers.set('r1', { id: 'r1', state: 'finished' });
  assert.deepEqual(at(), { thinking: null, working: false });
});

test('"Working…" stands above the notes the agent hasn\'t read, and isn\'t new', () => {
  next = 0;
  const items = [
    item('user'),
    item('call', { status: 'ok' }),
    item('user', { delivery: 'note', pending: true }),
  ];
  const rows = core.layout(items, 'full');
  assert.equal(core.withWorking(rows, false), rows);
  const shown = core.withWorking(rows, true);
  assert.deepEqual(shown.map((row) => row.key), ['i1', 'i2', 'working', 'i3']);
  assert.equal(rows.length, 3, 'the rows themselves stay as they were');
  assert.equal(core.unseen(shown, 1), 2);
  assert.equal(core.nearest(shown, 99).key, 'i3');
  assert.equal(core.nearest(shown, 0).key, 'i1');
});

test('a running call that can move offers it, and a running job offers Stop', () => {
  next = 0;
  const store = new core.Store();
  const running = item('call', { status: 'running' });
  const moved = item('call', { status: 'ok', job: 'j1' });
  const other = item('text');
  store.reset([running, moved, other], [], [{ id: 'j1', state: 'running', call: moved.id }]);
  assert.deepEqual(core.callActions(running, store, [running.id]), { move: true, stop: null });
  // Not listed as movable: a call of a tool that can't move.
  assert.deepEqual(core.callActions(running, store, []), { move: false, stop: null });
  assert.deepEqual(core.callActions(running, store, undefined), { move: false, stop: null });
  assert.deepEqual(core.callActions(moved, store, [moved.id]), { move: false, stop: 'j1' });
  assert.deepEqual(core.callActions(other, store, [other.id]), { move: false, stop: null });
  // Once the job ends, its call no longer offers Stop.
  store.job({ id: 'j1', state: 'finished', call: moved.id });
  assert.deepEqual(core.callActions(moved, store, []), { move: false, stop: null });
});

test('a call that can move, or no longer can, has its line drawn again at once', () => {
  const stale = [];
  let draws = 0;
  const changed = (before, after) =>
    core.movableChanged(before, after, (id) => stale.push(id), () => {
      draws += 1;
    });
  // A call drawn before it could move offers it from the status that says so.
  assert.equal(changed(undefined, [3]), true);
  assert.deepEqual(stale, [3]);
  assert.equal(draws, 1);
  assert.equal(changed([3], [3, 5]), true);
  assert.deepEqual(stale, [3, 5]);
  assert.equal(draws, 2);
  // One that moved or ended no longer does.
  assert.equal(changed([3, 5], [5]), true);
  assert.deepEqual(stale, [3, 5, 3]);
  assert.equal(draws, 3);
  // A status that changes nothing draws nothing.
  assert.equal(changed([5], [5]), false);
  assert.equal(changed(undefined, []), false);
  assert.equal(draws, 3);
});

test("the header shows the worker's own figures while its view shows", () => {
  const status = {
    working: true,
    context_tokens: 142_400,
    cost: 3.2,
    api_price: 4.5,
    plan_usage: 'Claude plan 5 h 21%',
    settings: { handover_at: 250_000 },
    agents: [
      { id: 'r1', context_tokens: 88_000, context_window: 1_000_000, handover_at: 190_000, compacts: false },
      { id: 'r3', context_tokens: 12_300, context_window: null, handover_at: null, compacts: false },
    ],
  };
  const running = { id: 'r1', state: 'running', ran_ms: 0, running_since: 0, cost_billed: 0.1234, cost_list: 0.1234 };
  // The main agent's, as before.
  assert.deepEqual(core.figures(status), ['142k/250k', '5 h 21%', '$3.20', '≈ $4.50']);
  assert.equal(core.contextTitle(status), 'Context 142k tokens, handover size 250k');
  assert.deepEqual(core.stateOf(status, 'live'), { key: 'working', label: 'Working' });
  // A running worker: its context against its own handover size, its cost.
  assert.deepEqual(core.figures(status, running), ['88k/190k', '5 h 21%', '$0.12']);
  assert.equal(core.contextTitle(status, running), 'Context 88k tokens, handover size 190k');
  assert.deepEqual(core.stateOf(status, 'live', running), { key: 'working', label: 'r1 working' });
  assert.equal(core.runTime(running, 134_000), 134_000);
  // A worker whose calls a plan covered shows what they would cost.
  const plan = { id: 'r1', state: 'finished', ran_ms: 5000, running_since: null, cost_billed: 0, cost_list: 0.31 };
  assert.deepEqual(core.figures(status, plan), ['88k/190k', '5 h 21%', '≈ $0.31']);
  assert.deepEqual(core.stateOf(status, 'live', plan), { key: 'idle', label: 'r1 finished' });
  assert.equal(core.runTime(plan, 9e9), 5000);
  // Ended workers keep their figures, a failure is marked.
  assert.deepEqual(core.stateOf(status, 'live', { ...plan, state: 'failed' }), { key: 'failed', label: 'r1 failed' });
  assert.equal(core.stateOf(status, 'live', { ...plan, state: 'stopped' }).label, 'r1 stopped');
  // No figures yet: none of the main agent's stand in for them. A context
  // without a handover size shows alone.
  const waiting = { id: 'r2', state: 'queued', ran_ms: 0, running_since: null };
  assert.deepEqual(core.figures(status, waiting), ['5 h 21%']);
  assert.equal(core.contextTitle(status, waiting), '');
  assert.deepEqual(core.stateOf(status, 'live', waiting), { key: 'idle', label: 'r2 queued' });
  assert.equal(core.runTime(waiting, 9e9), null);
  assert.deepEqual(core.figures(status, { id: 'r3', state: 'running', ran_ms: 0, running_since: 0 }), ['12k', '5 h 21%']);
  assert.equal(core.contextTitle(status, { id: 'r3', state: 'running' }), 'Context 12k tokens');
  // Before the summary comes, the worker is only named.
  assert.deepEqual(core.stateOf(status, 'live', { id: 'r4' }), { key: 'idle', label: 'r4' });
  assert.deepEqual(core.figures({ ...status, plan_usage: null, agents: undefined }, { id: 'r4' }), []);
  // The main agent's question stays in the worker's view, named.
  const asking = { ...status, question: { id: 7, text: 'Tabs or spaces?' } };
  assert.deepEqual(core.stateOf(asking, 'live', running), { key: 'asking', label: 'Main asks you' });
  assert.equal(core.stateOf(asking, 'live').label, 'Asks you');
  // A connection that isn't live says so, whoever is shown.
  assert.equal(core.stateOf(status, 'offline', running).key, 'offline');
  assert.equal(core.stateOf(status, 'connecting', running).key, 'connecting');
  // An older Arko sends no worker figures.
  const older = { ...status };
  delete older.agents;
  assert.deepEqual(core.figures(older, running), ['5 h 21%', '$0.12']);
});

test('the monitor wakes setting reads as words', () => {
  assert.equal(core.wakesWords(7200), 'For 2 h after your last message');
  assert.equal(core.wakesWords(1800), 'For 30 min after your last message');
  assert.equal(core.wakesWords(null), 'Always');
  assert.equal(core.wakesWords(0), 'Never');
});

test('a worker says its model, profile and cost once the summary has them', () => {
  const base = { id: 'r2', title: 'Judge', state: 'running', tool_calls: 4, started_at: 0, ended_at: null, ran_ms: 0, running_since: 0 };
  assert.equal(core.workerWords(base, 60_000).meta, '4 calls · 1m00s · running');
  const known = { ...base, model: 'gpt-6.1-sol', effort: 'high', profile: '3d-judge', cost_billed: 0.1234, cost_list: 0.1234 };
  assert.equal(core.workerWords(known, 60_000).meta, '4 calls · 1m00s · running · gpt-6.1-sol, high · profile 3d-judge · $0.12');
  // A model that doesn't reason names no effort, and a plan covers the calls.
  const plan = { ...base, model: 'cc:claude-haiku-5-5', cost_billed: 0, cost_list: 0.31 };
  assert.equal(core.workerWords(plan, 60_000).meta, '4 calls · 1m00s · running · cc:claude-haiku-5-5 · ≈ $0.31 at API prices');
  assert.equal(core.costWords(0, 0.004), '');
  assert.equal(core.costWords(undefined, undefined), '');
});

test('a worker that a worker started says whose it is and what the family cost', () => {
  const base = { id: 'r3', title: 'Count', state: 'running', tool_calls: 4, started_at: 0, ended_at: null, ran_ms: 0, running_since: 0 };
  const nested = { ...base, parent: 'r1', depth: 2, model: 'gpt-6.1-sol', cost_billed: 0.1, cost_list: 0.1, family_cost_billed: 0.1, family_cost_list: 0.1 };
  assert.equal(core.workerWords(nested, 60_000).meta, '4 calls · 1m00s · running · under r1 · gpt-6.1-sol · $0.10');
  // A worker with workers under it says what they cost together, once that is more.
  const parent = { ...base, id: 'r1', cost_billed: 0.1, cost_list: 0.1, family_cost_billed: 0.6, family_cost_list: 0.6 };
  assert.equal(core.workerWords(parent, 60_000).meta, '4 calls · 1m00s · running · $0.10 · with its workers $0.60');
  const plan = { ...parent, cost_billed: 0, cost_list: 0.2, family_cost_billed: 0, family_cost_list: 0.9 };
  assert.equal(core.workerWords(plan, 60_000).meta, '4 calls · 1m00s · running · ≈ $0.20 at API prices · with its workers ≈ $0.90 at API prices');
  const alone = { ...parent, family_cost_billed: 0.1, family_cost_list: 0.1 };
  assert.equal(core.workerWords(alone, 60_000).meta, '4 calls · 1m00s · running · $0.10');
});

test('a family is listed with each worker\'s workers after it', () => {
  const worker = (id, parent, state = 'running') => ({ id, parent, state });
  const list = [worker('r1'), worker('r2'), worker('r3', 'r1'), worker('r4', 'r3'), worker('r5', 'r1'), worker('r6', 'r2'), worker('r7', 'r9')];
  assert.deepEqual(core.familyOrder(list).map((w) => w.id), ['r1', 'r3', 'r4', 'r5', 'r2', 'r6', 'r7']);
  // The sheet: the families with a worker going first, then the others, the latest first.
  const ended = [
    worker('r1', undefined, 'finished'),
    worker('r2', undefined, 'finished'),
    worker('r3', 'r1', 'finished'),
    worker('r4', undefined, 'running'),
    worker('r5', 'r4', 'finished'),
    worker('r6', 'r2', 'running'),
  ];
  assert.deepEqual(core.sheetOrder(ended).map((w) => w.id), ['r2', 'r6', 'r4', 'r5', 'r1', 'r3']);
  assert.deepEqual(core.sheetOrder([]), []);
});

test('the workers settings read as words', () => {
  assert.deepEqual(Object.keys(core.WORKER_MODELS), ['agent', 'profiles', 'session']);
  assert.equal(core.WORKER_MODELS.agent.name, 'Main agent chooses');
  assert.equal(core.WORKER_MODELS.profiles.name, 'Profiles only');
  assert.equal(core.WORKER_MODELS.session.name, "Session's worker model");
  assert.match(core.workerDepthAbout(), /^How many levels of workers there can be, from 1 to 4\. 1 means only the main agent starts workers\./);
  assert.doesNotMatch(core.workerDepthAbout(), /takes effect/);
});

// ----- the prompt cache's timer: the words the terminal and Arko Studio use -----

const cache = (minutes, refreshedAt) => ({
  lifetime: minutes * 60000,
  refreshed_at: refreshedAt,
  tokens: 180000,
  miss_cost: 0.9,
});

test('the cache timer counts down while idle and warns in the last minute', () => {
  const idle = cache(5, 0);
  const timer = (now, running = false) => core.cacheTimer(idle, running, now);
  // 3:12 left.
  assert.equal(core.cacheLabel(timer(300000 - 192000)), 'cache 3:12');
  assert.equal(core.cacheWarns(timer(300000 - 192000)), false);
  // Half a second left reads 0:01, and it warns.
  assert.equal(core.cacheLabel(timer(299500)), 'cache 0:01');
  assert.equal(core.cacheWarns(timer(299500)), true);
  // The last minute warns from its first moment.
  assert.equal(core.cacheWarns(timer(240000)), true);
  assert.equal(core.cacheWarns(timer(239999)), false);
  assert.equal(core.cacheLabel(timer(300000)), 'cache expired');
  assert.equal(core.cacheWarns(timer(300000)), false);
  assert.equal(core.cacheLabel(timer(9e9)), 'cache expired');
  // Hidden while requests run, and where the lifetime or last use isn't
  // known.
  assert.equal(timer(1000, true), null);
  assert.equal(core.cacheTimer({ ...idle, lifetime: undefined }, false, 1000), null);
  assert.equal(core.cacheTimer({ ...idle, refreshed_at: undefined }, false, 1000), null);
  assert.equal(core.cacheTimer(undefined, false, 1000), null);
});

test('the cache timer never shows more than the lifetime', () => {
  assert.equal(core.cacheLabel(core.cacheTimer(cache(60, 0), false, 1000)), 'cache 59:59');
  // The last use is dated ten minutes after now.
  assert.equal(core.cacheLabel(core.cacheTimer(cache(5, 600000), false, 0)), 'cache 5:00');
  // An hour or more reads with hours.
  assert.equal(core.cacheLabel({ expired: false, left: 3723000 - 999 }), 'cache 1:02:03');
});

test('the cache timer\'s tooltip says when it lapses and what a miss re-caches', () => {
  const at = new Date(2026, 9, 22, 14, 0, 0).getTime();
  // Used at 13:55 with five minutes: it lapses at 14:00.
  const used = cache(5, at - 5 * 60000);
  const counting = core.cacheTimer(used, false, at - 120000);
  assert.equal(
    core.cacheTitle(counting, used, at - 120000),
    'The prompt cache keeps this conversation until 14:00. Letting it expire re-caches about 180k tokens, ≈ $0.90.',
  );
  const expired = core.cacheTimer(used, false, at + 1000);
  assert.equal(core.cacheTitle(expired, used, at + 1000), 'The next message re-caches about 180k tokens, ≈ $0.90.');
  const plan = { ...used, miss_cost: undefined, on_plan: 'your Claude plan' };
  assert.equal(
    core.cacheTitle(expired, plan, at + 1000),
    'The next message re-caches about 180k tokens of your Claude plan.',
  );
  const unsized = { ...used, tokens: undefined };
  assert.equal(core.cacheTitle(expired, unsized, at + 1000), 'The prompt cache has expired.');
  assert.equal(
    core.cacheTitle(counting, unsized, at - 120000),
    'The prompt cache keeps this conversation until 14:00.',
  );
  // Another day, within a week and beyond it.
  const tomorrow = { ...used, refreshed_at: at + 86400000 - 300000 };
  assert.match(core.cacheTitle(core.cacheTimer(tomorrow, false, at), tomorrow, at), /until [A-Z][a-z]{2} 14:00\./);
  const later = { ...used, refreshed_at: new Date(2026, 10, 21, 14, 0, 0).getTime() - 300000 };
  assert.match(core.cacheTitle(core.cacheTimer(later, false, at), later, at), /until Nov 21\./);
  assert.equal(core.CACHE_WARNING_MS, 60000);
});

test('the cache timer\'s token counts are short', () => {
  const counted = (tokens) =>
    core.cacheTitle({ expired: true }, { ...cache(5, 0), tokens, miss_cost: undefined }, 0).replace(/^.*about | tokens\.$/g, '');
  for (const [count, text] of [[820, '820'], [8200, '8.2k'], [9960, '10k'], [142000, '142k'], [999600, '1M'], [1020000, '1.02M']]) {
    assert.equal(counted(count), text);
  }
});

test('a user message is tagged only for what is unusual', () => {
  const tag = (delivery, pending) => core.deliveryTag({ kind: 'user', delivery, pending });
  // A note waits as "pending", and once the agent read it, it is a message
  // like any other.
  assert.equal(tag('note', true), 'pending');
  assert.equal(tag('note', false), null);
  assert.equal(tag('note', undefined), null);
  assert.equal(tag('answer', false), 'answer');
  assert.equal(tag('queued', false), 'queued');
  assert.equal(tag('refresh', false), 'kept the cache warm');
  // A turn, a command, and a delivery a later Arko adds have none.
  for (const delivery of ['turn', 'command', 'later', undefined]) assert.equal(tag(delivery, false), null);
});

test('text beside a wait and a message from the agent show in every view, in order', () => {
  next = 0;
  const items = [
    item('user'),
    item('text', { text: 'Looking at it.' }),
    item('message', { text: 'Halfway.' }),
    item('text', { text: 'Waiting for the check.' }),
    item('call', { status: 'running', tool: 'wait_for_agents', text: 'waiting for r1' }),
  ];
  const kinds = (rows) => rows.map((row) => (row.count ? 'count' : row.item.kind));
  assert.deepEqual(kinds(core.layout(items, 'full')), ['user', 'text', 'message', 'text', 'call']);
  assert.deepEqual(kinds(core.layout(items, 'quiet')), ['user', 'text', 'message', 'text', 'call']);
  assert.deepEqual(kinds(core.layout(items, 'messages')), ['user', 'text', 'message', 'text', 'count']);
});
