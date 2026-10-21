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

test("what the user wrote to a worker shows in every view, and in the worker's own", () => {
  next = 0;
  const items = [
    item('user'),
    item('call', { agent: 'r1', status: 'ok' }),
    item('user', { agent: 'r1' }),
    item('text', { agent: 'r1' }),
  ];
  const kinds = (rows) => rows.map((row) => row.item.kind + (row.item.agent === 'main' ? '' : ':' + row.item.agent));
  assert.deepEqual(kinds(core.layout(items, 'full')), ['user', 'call:r1', 'user:r1']);
  assert.deepEqual(kinds(core.layout(items, 'quiet')), ['user', 'user:r1']);
  assert.deepEqual(kinds(core.layout(items, 'messages')), ['user', 'user:r1']);
  assert.deepEqual(kinds(core.layout(items, 'full', 'r1')), ['call:r1', 'user:r1', 'text:r1']);
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
