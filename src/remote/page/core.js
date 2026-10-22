// Arko's remote page, the part without a page: how the transcript is laid
// out for each view, markdown read into a tree (which app.js builds with
// DOM calls, never as markup), and the figures' wording. It runs in the
// browser (as window.FinCore) and under node for its tests.
'use strict';

(function (root, make) {
  const core = make();
  if (typeof module === 'object' && module.exports) module.exports = core;
  else root.FinCore = core;
})(typeof globalThis !== 'undefined' ? globalThis : this, () => {
  const MAIN = 'main';

  /** The transcript's views: everything, workers as one line, or just the
   * conversation. */
  const VIEWS = {
    full: { name: 'Full', about: 'Everything, with the workers’ calls inline.' },
    quiet: { name: 'Quiet', about: 'Each worker as one line that updates in place.' },
    messages: { name: 'Messages', about: 'Just the messages, with a count of each turn’s tool calls.' },
  };

  /** How many items the page keeps, as Arko does: the main transcript's
   * and the workers', each on its own. */
  const BUDGET = { main: 3000, workers: 3000 };

  // ----- the store -----

  /** The session's items in order, with the workers' summaries. */
  class Store {
    constructor() {
      this.items = [];
      this.byId = new Map();
      this.workers = new Map();
      /** The background jobs' summaries, by ID. */
      this.jobs = new Map();
      this.counts = { main: 0, workers: 0 };
      /** When each item came or last changed, in this device's time. */
      this.grown = new Map();
    }

    /** Starts over with `items`, `workers` and `jobs` (a `reset`). */
    reset(items, workers, jobs) {
      this.items = [];
      this.byId.clear();
      this.workers.clear();
      this.jobs.clear();
      this.grown.clear();
      this.counts = { main: 0, workers: 0 };
      for (const item of items) this.add(item);
      for (const worker of workers || []) this.workers.set(worker.id, worker);
      for (const job of jobs || []) this.jobs.set(job.id, job);
    }

    /** A background job's summary, whole. */
    job(summary) {
      this.jobs.set(summary.id, summary);
    }

    /** A new item, come at `now`; returns the items trimmed to keep the
     * budget. */
    add(item, now = Date.now()) {
      if (this.byId.has(item.id)) {
        this.change(item, now);
        return [];
      }
      this.items.push(item);
      this.byId.set(item.id, item);
      this.grown.set(item.id, now);
      const lane = item.agent === MAIN ? 'main' : 'workers';
      this.counts[lane] += 1;
      const trimmed = [];
      while (this.counts[lane] > BUDGET[lane]) {
        const index = this.items.findIndex((old) => (old.agent === MAIN) === (lane === 'main'));
        if (index < 0) break;
        const [old] = this.items.splice(index, 1);
        this.byId.delete(old.id);
        this.grown.delete(old.id);
        this.counts[lane] -= 1;
        trimmed.push(old);
      }
      return trimmed;
    }

    /** More text for the item `id`, come at `now`; false if it isn't kept. */
    append(id, text, now = Date.now()) {
      const item = this.byId.get(id);
      if (!item) return false;
      item.text += text;
      this.grown.set(id, now);
      return true;
    }

    /** Takes the item `id` out, or returns false if it isn't kept. */
    remove(id) {
      const item = this.byId.get(id);
      if (!item) return false;
      this.items.splice(this.items.indexOf(item), 1);
      this.byId.delete(id);
      this.grown.delete(id);
      this.counts[item.agent === MAIN ? 'main' : 'workers'] -= 1;
      return true;
    }

    /** The item with this one's ID is now this one, since `now`. */
    change(item, now = Date.now()) {
      const old = this.byId.get(item.id);
      if (!old) return false;
      Object.assign(old, item);
      // A field gone from the new one (an outcome) is gone.
      for (const key of Object.keys(old)) if (!(key in item)) delete old[key];
      this.grown.set(item.id, now);
      return true;
    }

    worker(summary) {
      this.workers.set(summary.id, summary);
    }

    /** The latest turn, and when it began (the time of its first item). */
    currentTurn() {
      let turn = null;
      let since = null;
      for (let i = this.items.length - 1; i >= 0; i--) {
        const item = this.items[i];
        if (item.turn == null) break;
        if (turn === null) turn = item.turn;
        if (item.turn !== turn) break;
        since = item.at;
      }
      return { turn, since };
    }
  }

  /**
   * What a call's line offers: `move` while it runs and the status lists
   * it among the calls that can move to the background (`movable`), and
   * `stop`, the ID of the background job it made while that job runs.
   */
  function callActions(item, store, movable) {
    const actions = { move: false, stop: null };
    if (item.kind !== 'call') return actions;
    actions.move = item.status === 'running' && (movable || []).includes(item.id);
    const job = item.job && store.jobs.get(item.job);
    if (job && job.state === 'running') actions.stop = job.id;
    return actions;
  }

  /**
   * After a status, the calls that can now move to the background, or no
   * longer can: `stale(id)` for each, whose line changes, then one
   * `redraw()`, so a running call offers Move to background as soon as it
   * can move. Returns whether any changed.
   */
  function movableChanged(before, after, stale, redraw) {
    const was = before || [];
    const is = after || [];
    let changed = false;
    for (const id of was.concat(is)) {
      if (was.includes(id) !== is.includes(id)) {
        stale(id);
        changed = true;
      }
    }
    if (changed) redraw();
    return changed;
  }

  // ----- views -----

  /** Whether a notice (or a failed call) is a warning or an error. */
  function serious(item) {
    return item.level === 'warn' || item.level === 'error';
  }

  /**
   * The rows a view shows, in order: `{ key, id, item }` for an item, and
   * `{ key, id, count: { turn, calls, failed, running } }` for a turn's
   * tool activity folded into one line (Messages). `worker` picks a
   * worker's own transcript instead of the main one.
   */
  function layout(all, view, worker) {
    const items = pendingLast(all);
    const rows = [];
    // A result shows under its call, which other items may have followed
    // by the time it came.
    const results = new Map();
    for (const item of items) {
      if (item.kind === 'result' && item.call != null) {
        if (!results.has(item.call)) results.set(item.call, []);
        results.get(item.call).push(item);
      }
    }
    const placed = new Set();
    const push = (item) => {
      rows.push({ key: 'i' + item.id, id: item.id, item });
      for (const result of results.get(item.id) || []) {
        rows.push({ key: 'i' + result.id, id: result.id, item: result });
        placed.add(result.id);
      }
    };
    if (worker) {
      for (const item of items) {
        if (item.agent === worker && !placed.has(item.id)) push(item);
      }
      return rows;
    }
    const counts = new Map();
    for (const item of items) {
      if (placed.has(item.id)) continue;
      const main = item.agent === MAIN;
      if (view === 'full') {
        // The workers' calls and troubles inline. Their text, their reports
        // and what the user wrote to them are in their own views.
        if (main) push(item);
        else if (item.kind === 'call' || (item.kind === 'notice' && serious(item))) rows.push({ key: 'i' + item.id, id: item.id, item });
        continue;
      }
      if (!main) continue;
      if (view === 'quiet') {
        push(item);
        continue;
      }
      if (shownInMessages(item)) {
        rows.push({ key: 'i' + item.id, id: item.id, item });
        continue;
      }
      if (item.kind !== 'call') continue;
      // Messages: the turn's calls, as one line where the first one was.
      const turn = item.turn == null ? 'none' : item.turn;
      let row = counts.get(turn);
      if (!row) {
        row = { key: 'c' + turn, id: item.id, count: { turn: item.turn, calls: 0, failed: 0, running: 0 } };
        counts.set(turn, row);
        rows.push(row);
      }
      row.count.calls += 1;
      if (item.status === 'failed') row.count.failed += 1;
      if (item.status === 'running') row.count.running += 1;
    }
    return rows;
  }

  /** `items` with each note the agent hasn't read yet at the end of its
   * conversation, below the agent's output, which goes on above it. */
  function pendingLast(items) {
    const out = [];
    let waiting = [];
    for (const item of items) {
      if (item.pending) {
        waiting.push(item);
        continue;
      }
      if (item.kind === 'conversation') {
        out.push(...waiting);
        waiting = [];
      }
      out.push(item);
    }
    return out.concat(waiting);
  }

  /** The first line of an agent's thinking, for its folded row: the first
   * one with text, without markdown's bold marks (a summary's title). */
  function firstLine(text) {
    const line = String(text || '').split('\n').map((l) => l.trim()).find((l) => l) || '';
    return line.replace(/\*\*/g, '');
  }

  /** What the Messages view keeps of the main transcript. */
  function shownInMessages(item) {
    switch (item.kind) {
      case 'user':
      case 'text':
      case 'message':
      case 'reply':
      case 'question':
      case 'output':
      case 'turn':
      case 'worker_line':
        return true;
      case 'notice':
        return serious(item);
      default:
        return false;
    }
  }

  // ----- what shows the agent is under way -----

  /** Text that hasn't grown for this long, while its agent works, no
   * longer shows that the agent is under way (it may be writing a long
   * call). */
  const QUIET_MS = 2000;

  /** What `agent`, which works (its turn began at `since`, in Arko's
   * time), is under way with, as its latest items say: `{ kind, id }`,
   * where `kind` is `model` (it waits on its model, and nothing the model
   * sent shows it yet), `thinking` or `text` (they stream, the item
   * `id`), `tool` (a call of its runs) or `done` (it replied). */
  function busy(items, agent, since) {
    // The calls of one response may run side by side: one that ended
    // doesn't mean the others did.
    let afterCall = false;
    for (let i = items.length - 1; i >= 0; i--) {
      const item = items[i];
      if (item.agent !== agent || item.pending) continue;
      switch (item.kind) {
        case 'call':
          if (item.status === 'running') return { kind: 'tool' };
          afterCall = true;
          continue;
        case 'result':
          afterCall = true;
          continue;
        // Arko's lines, a command's output and the workers' places.
        case 'notice':
        case 'output':
        case 'worker_line':
          continue;
        // What the user typed beside the agent's work.
        case 'user':
          if (item.delivery === 'command' || item.delivery === 'queued') continue;
          break;
      }
      if (afterCall) return { kind: 'model' };
      switch (item.kind) {
        case 'thinking':
        case 'text':
          return { kind: item.kind, id: item.id };
        case 'reply':
          return { kind: 'done' };
        // The end of a turn before this one began (a turn without a
        // message, when the workers' news came), or of this one.
        case 'turn':
          return { kind: item.at < since ? 'model' : 'done' };
        default:
          return { kind: 'model' };
      }
    }
    return { kind: 'model' };
  }

  /**
   * What shows that the agent of a view is under way, besides its calls:
   * `{ thinking, working }`. `thinking` is the ID of its thinking that
   * streams, whose label moves, and `working` says a "Working…" row stands
   * at the end, while the agent waits on its model with nothing else to
   * show. `now` is this device's time, as the store's `grown` has it.
   */
  function motion(store, status, view, worker, now) {
    const still = { thinking: null, working: false };
    let since = -Infinity;
    if (worker) {
      const summary = store.workers.get(worker);
      if (!summary || summary.state !== 'running') return still;
    } else {
      // (While the agent asks, it waits on the user, not on its model.)
      if (!status || !status.working || status.question) return still;
      if (status.since != null) since = status.since;
    }
    const doing = busy(store.items, worker || MAIN, since);
    switch (doing.kind) {
      case 'model':
        return { thinking: null, working: true };
      case 'text': {
        const grown = store.grown.has(doing.id) ? store.grown.get(doing.id) : now;
        return { thinking: null, working: now - grown >= QUIET_MS };
      }
      case 'thinking':
        // The Messages view leaves the agent's thinking out.
        if (!worker && view === 'messages') return { thinking: null, working: true };
        return { thinking: doing.id, working: false };
      default:
        return still;
    }
  }

  /** `rows` with the "Working…" row, when `working`, where the agent's
   * output comes next: at the end, above the notes it hasn't read. */
  function withWorking(rows, working) {
    if (!working) return rows;
    let at = rows.length;
    while (at > 0 && rows[at - 1].item && rows[at - 1].item.pending) at--;
    const out = rows.slice();
    out.splice(at, 0, { key: 'working', id: null, working: true });
    return out;
  }

  /** The row of `rows` to keep the reader's place at, for the item `id`
   * they were reading: that one, else the nearest one before it, else the
   * nearest one after. (A result shows under its call, so the rows'
   * IDs aren't all in order.) */
  function nearest(rows, id) {
    let before = null;
    let after = null;
    for (const row of rows) {
      if (row.working) continue;
      if (row.id === id) return row;
      if (row.id < id && (!before || row.id > before.id)) before = row;
      if (row.id > id && (!after || row.id < after.id)) after = row;
    }
    return before || after;
  }

  /** How many of `rows` came after the item `seen` ("Working…" isn't
   * new). */
  function unseen(rows, seen) {
    let count = 0;
    for (const row of rows) if (!row.working && row.id > seen) count++;
    return count;
  }

  // ----- wording -----

  /** 2m14s, 45s, 1h05m. */
  function duration(ms) {
    const s = Math.max(0, Math.floor(ms / 1000));
    if (s < 60) return s + 's';
    const m = Math.floor(s / 60);
    if (m < 60) return m + 'm' + String(s % 60).padStart(2, '0') + 's';
    return Math.floor(m / 60) + 'h' + String(m % 60).padStart(2, '0') + 'm';
  }

  /** 142k, 1.2M. */
  function tokens(n) {
    if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + 'M';
    if (n >= 1000) return Math.round(n / 1000) + 'k';
    return String(n);
  }

  function plural(n, one, many) {
    return n + ' ' + (n === 1 ? one : many);
  }

  /** How long after the user's last message a monitor's warning wakes the
   * agent, from the settings' `monitor_wakes` (seconds, 0 for never, or
   * null for always): "For 2 h after your last message", "Always", "Never". */
  function wakesWords(seconds) {
    if (seconds == null) return 'Always';
    if (seconds === 0) return 'Never';
    const time = seconds % 3600 === 0 ? seconds / 3600 + ' h' : seconds % 60 === 0 ? seconds / 60 + ' min' : seconds + ' s';
    return 'For ' + time + ' after your last message';
  }

  /** Who chooses the workers' models, as `/worker-models` takes it. The
   * names and sentences are the library's (`ModelChoice::label` and
   * `about`), which a Rust test compares with these. */
  const WORKER_MODELS = {
    agent: { name: "Main agent chooses", about: "The main agent chooses a profile, model and effort for each worker, and can save profiles." },
    profiles: { name: "Profiles only", about: "The main agent starts workers with a profile only. It can't choose a model or effort by itself." },
    session: { name: "Session's worker model", about: "Every worker runs the session's worker model." },
  };

  /** What the worker depth setting does, from whether workers can start
   * workers yet (`nests`, the settings' `nested_workers`). The library's
   * `workers::depth_about` has the same sentences. */
  function workerDepthAbout(nests) {
    if (nests) return "How many levels of workers there can be, from 1 to 4. 1 means only the main agent starts workers. 2 lets a worker the main agent starts begin workers of its own, when the main agent allows it.";
    return "How many levels of workers there can be, from 1 to 4. It takes effect once workers can start workers of their own, so for now only the main agent starts workers.";
  }

  /** How long a worker has run at `now`, in all its runs (also those
   * before the session was opened again), or null until it has run. */
  function runTime(summary, now) {
    const ran = summary.ran_ms || 0;
    if (summary.running_since != null) return ran + Math.max(0, now - summary.running_since);
    return ran > 0 ? ran : null;
  }

  /** What a worker's model calls cost, from the summary's `cost_billed`
   * and `cost_list` (dollars): "$0.12", or "≈ $0.31 at API prices" when a
   * plan covered the calls, and nothing under half a cent. */
  function costWords(billed, list) {
    if (!(list >= 0.005)) return '';
    if (list > billed + 0.005) return '≈ $' + list.toFixed(2) + ' at API prices';
    return '$' + billed.toFixed(2);
  }

  /** A worker's summary in words: `{ title, meta, last }`, where meta is
   * "18 calls · 2m14s · running · gpt-6.1-sol, high · profile judge · $0.12"
   * (the model, the profile and the cost once the summary has them). */
  function workerWords(summary, now) {
    const parts = [plural(summary.tool_calls, 'call', 'calls')];
    const ran = runTime(summary, now);
    if (ran != null) parts.push(duration(ran));
    parts.push(summary.state);
    if (summary.model) parts.push(summary.effort ? summary.model + ', ' + summary.effort : summary.model);
    if (summary.profile) parts.push('profile ' + summary.profile);
    const cost = costWords(summary.cost_billed || 0, summary.cost_list || 0);
    if (cost) parts.push(cost);
    let last = summary.last || '';
    if (summary.state === 'finished' && summary.report) last = summary.report.split('\n').find((l) => l.trim()) || last;
    return { title: summary.title || summary.id, meta: parts.join(' · '), last: last.trim() };
  }

  /** The header's state: `{ key, label }`. While a worker's own view shows,
   * `viewing` is its summary (or `{ id }` before the summary comes), and the
   * state is the worker's: "r2 working", "r2 finished". The main agent's
   * question stays, named, because the message box answers it. */
  function stateOf(status, connection, viewing) {
    if (connection === 'offline') return { key: 'offline', label: 'Offline' };
    if (connection !== 'live') return { key: 'connecting', label: 'Connecting' };
    if (!status) return { key: 'connecting', label: 'Connecting' };
    if (status.question) return { key: 'asking', label: viewing ? 'Main asks you' : 'Asks you' };
    if (viewing) return workerState(viewing);
    if (status.working) return { key: 'working', label: 'Working' };
    return { key: 'idle', label: 'Idle' };
  }

  /** A worker's state for the header: "r2 working" while it runs, else
   * its state in a word ("r2 queued", "r2 finished", "r2 failed"). */
  function workerState(summary) {
    if (!summary.state) return { key: 'idle', label: summary.id };
    const word = summary.state === 'running' ? 'working' : summary.state;
    const key = summary.state === 'running' ? 'working' : summary.state === 'failed' ? 'failed' : 'idle';
    return { key, label: summary.id + ' ' + word };
  }

  /** The figures the header shows for the agent whose transcript is shown:
   * the main agent's, or the worker `viewing`'s own, which the status gives
   * as `agents` (its context and the size it hands over at) and its summary
   * as its costs. */
  function agentFigures(status, viewing) {
    if (!viewing) {
      return {
        context: status.context_tokens,
        handover: status.settings && status.settings.handover_at,
        cost: status.cost,
        // What the usage on a plan would cost at API prices.
        api: status.api_price,
      };
    }
    const agent = (status.agents || []).find((one) => one.id === viewing.id) || {};
    const billed = viewing.cost_billed || 0;
    const list = viewing.cost_list || 0;
    return {
      context: agent.context_tokens,
      handover: agent.handover_at,
      cost: billed >= 0.005 ? billed : 0,
      api: list > billed + 1e-9 && list >= 0.005 ? list : 0,
    };
  }

  /** The header's figures: context against the handover size, then cost
   * (or the plan's usage on a subscription). While a worker's own view
   * shows (`viewing`, its summary), they are the worker's. */
  function figures(status, viewing) {
    const parts = [];
    if (!status) return parts;
    const { context, handover, cost, api } = agentFigures(status, viewing);
    if (context != null) {
      // As the terminal's status line says it: the context against the
      // size the agent hands over at.
      parts.push(tokens(context) + (handover ? '/' + tokens(handover) : ''));
    }
    // A plan's usage ("Claude plan 5 h 21%, 7 d 4%"), what was billed,
    // and what the usage on a plan would cost at API prices ("≈ $3.20").
    if (status.plan_usage) parts.push(status.plan_usage.replace(/^.*?\bplan\s+/i, ''));
    if (cost > 0) parts.push('$' + cost.toFixed(2));
    if (api > 0) parts.push('≈ $' + api.toFixed(2));
    return parts;
  }

  /** The tooltip of the header's figures: "Context 142k tokens, handover
   * size 250k", or nothing before a request reported a size. */
  function contextTitle(status, viewing) {
    if (!status) return '';
    const { context, handover } = agentFigures(status, viewing);
    if (context == null) return '';
    return 'Context ' + tokens(context) + ' tokens' + (handover ? ', handover size ' + tokens(handover) : '');
  }

  // ----- markdown -----

  const SAFE_LINK = /^(https?:|mailto:)/i;

  /**
   * Reads markdown into blocks: `{ type: 'p' | 'h' | 'code' | 'quote' |
   * 'list' | 'hr' | 'table', ... }`, with inline runs as
   * `{ type: 'text' | 'code' | 'strong' | 'em' | 'del' | 'link' | 'br' }`.
   * It is for an agent's replies: headings, emphasis, code, lists, quotes,
   * tables and links; anything else is text. An unclosed code fence (a
   * reply still streaming) runs to the end.
   */
  function markdown(source) {
    return blocks(String(source).replace(/\r\n?/g, '\n').split('\n'));
  }

  const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([\w+.#-]*)/;
  const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
  const RULE = /^ {0,3}([-*_])(\s*\1){2,}\s*$/;
  const QUOTE = /^ {0,3}>\s?/;
  const ITEM = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;
  const TABLE_RULE = /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/;

  function blocks(lines) {
    const out = [];
    let i = 0;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) {
        i++;
        continue;
      }
      let match = line.match(FENCE);
      if (match) {
        const fence = match[1];
        const body = [];
        i++;
        while (i < lines.length && !lines[i].trim().startsWith(fence)) body.push(lines[i++]);
        i++;
        out.push({ type: 'code', lang: match[2] || '', text: body.join('\n') });
        continue;
      }
      if ((match = line.match(HEADING))) {
        out.push({ type: 'h', level: match[1].length, children: inline(match[2]) });
        i++;
        continue;
      }
      if (RULE.test(line)) {
        out.push({ type: 'hr' });
        i++;
        continue;
      }
      if (QUOTE.test(line)) {
        const body = [];
        while (i < lines.length && QUOTE.test(lines[i])) body.push(lines[i++].replace(QUOTE, ''));
        out.push({ type: 'quote', children: blocks(body) });
        continue;
      }
      if (ITEM.test(line)) {
        const [list, next] = readList(lines, i);
        out.push(list);
        i = next;
        continue;
      }
      if (line.includes('|') && i + 1 < lines.length && TABLE_RULE.test(lines[i + 1])) {
        const head = cells(line);
        i += 2;
        const rows = [];
        while (i < lines.length && lines[i].includes('|') && lines[i].trim()) rows.push(cells(lines[i++]));
        out.push({ type: 'table', head: head.map(inline), rows: rows.map((row) => row.map(inline)) });
        continue;
      }
      const body = [];
      while (i < lines.length && lines[i].trim() && !startsBlock(lines, i)) body.push(lines[i++].trim());
      out.push({ type: 'p', children: inline(body.join('\n')) });
    }
    return out;
  }

  function startsBlock(lines, i) {
    const line = lines[i];
    return (
      FENCE.test(line) ||
      HEADING.test(line) ||
      RULE.test(line) ||
      QUOTE.test(line) ||
      ITEM.test(line) ||
      (line.includes('|') && i + 1 < lines.length && TABLE_RULE.test(lines[i + 1]))
    );
  }

  function cells(line) {
    let text = line.trim();
    if (text.startsWith('|')) text = text.slice(1);
    if (text.endsWith('|')) text = text.slice(0, -1);
    return text.split('|').map((cell) => cell.trim());
  }

  /** A list from line `start`: its items, each with its own blocks (a
   * deeper list among them). Returns [list, the line after it]. */
  function readList(lines, start) {
    const first = lines[start].match(ITEM);
    const indent = first[1].length;
    const ordered = /\d/.test(first[2]);
    const list = { type: 'list', ordered, start: ordered ? parseInt(first[2], 10) : 1, items: [] };
    let i = start;
    while (i < lines.length) {
      const match = lines[i].match(ITEM);
      if (!match || match[1].length !== indent || /\d/.test(match[2]) !== ordered) break;
      const body = [match[3]];
      i++;
      // The item's own lines: deeper ones, and lines that go on its text.
      while (i < lines.length) {
        const line = lines[i];
        if (!line.trim()) {
          const next = lines[i + 1];
          if (next !== undefined && next.trim() && leading(next) > indent) {
            body.push('');
            i++;
            continue;
          }
          break;
        }
        const nested = line.match(ITEM);
        if (nested && nested[1].length <= indent) break;
        if (!nested && leading(line) <= indent && startsBlock(lines, i)) break;
        body.push(line.slice(Math.min(leading(line), indent + 2)));
        i++;
      }
      list.items.push(blocks(body));
    }
    return [list, i];
  }

  function leading(line) {
    return line.length - line.trimStart().length;
  }

  /** Inline runs of `text`. */
  function inline(text) {
    const out = [];
    let plain = '';
    const flush = () => {
      if (plain) out.push({ type: 'text', text: plain });
      plain = '';
    };
    let i = 0;
    while (i < text.length) {
      const c = text[i];
      if (c === '\n') {
        flush();
        out.push({ type: 'br' });
        i++;
        continue;
      }
      if (c === '\\' && i + 1 < text.length && /[\\`*_{}\[\]()#+\-.!|~>]/.test(text[i + 1])) {
        plain += text[i + 1];
        i += 2;
        continue;
      }
      if (c === '`') {
        let ticks = 1;
        while (text[i + ticks] === '`') ticks++;
        const fence = '`'.repeat(ticks);
        const end = text.indexOf(fence, i + ticks);
        if (end > 0) {
          flush();
          out.push({ type: 'code', text: text.slice(i + ticks, end).replace(/^ (.+) $/, '$1') });
          i = end + ticks;
          continue;
        }
        plain += fence;
        i += ticks;
        continue;
      }
      if ((c === '*' || c === '_') && text[i + 1] === c) {
        const end = text.indexOf(c + c, i + 2);
        if (end > i + 2 && text[i + 2] !== ' ') {
          flush();
          out.push({ type: 'strong', children: inline(text.slice(i + 2, end)) });
          i = end + 2;
          continue;
        }
      }
      if (c === '~' && text[i + 1] === '~') {
        const end = text.indexOf('~~', i + 2);
        if (end > i + 2) {
          flush();
          out.push({ type: 'del', children: inline(text.slice(i + 2, end)) });
          i = end + 2;
          continue;
        }
      }
      if ((c === '*' || c === '_') && text[i + 1] && text[i + 1] !== ' ' && text[i + 1] !== c) {
        // `_` only at a word's edge: snake_case stays as it is.
        const opens = c === '*' || i === 0 || !/[\p{L}\p{N}]/u.test(text[i - 1]);
        let end = opens ? text.indexOf(c, i + 1) : -1;
        while (end > 0 && c === '_' && end + 1 < text.length && /[\p{L}\p{N}]/u.test(text[end + 1])) {
          end = text.indexOf(c, end + 1);
        }
        if (end > i + 1 && text[end - 1] !== ' ' && !text.slice(i + 1, end).includes('\n')) {
          flush();
          out.push({ type: 'em', children: inline(text.slice(i + 1, end)) });
          i = end + 1;
          continue;
        }
      }
      if (c === '[') {
        const close = text.indexOf('](', i + 1);
        const end = close > 0 ? text.indexOf(')', close + 2) : -1;
        if (close > i && end > close && !text.slice(i, end).includes('\n')) {
          const href = text.slice(close + 2, end).trim().split(/\s+/)[0];
          if (SAFE_LINK.test(href)) {
            flush();
            out.push({ type: 'link', href, children: inline(text.slice(i + 1, close)) });
            i = end + 1;
            continue;
          }
        }
      }
      if ((c === 'h' || c === 'H') && /^https?:\/\//i.test(text.slice(i, i + 8)) && (i === 0 || /[\s(<]/.test(text[i - 1]))) {
        let end = i;
        while (end < text.length && !/[\s<>]/.test(text[end])) end++;
        // Punctuation that ends a sentence isn't the link's.
        while (end > i && /[.,;:!?)'"]/.test(text[end - 1])) end--;
        const href = text.slice(i, end);
        flush();
        out.push({ type: 'link', href, children: [{ type: 'text', text: href }] });
        i = end;
        continue;
      }
      plain += c;
      i++;
    }
    flush();
    return out;
  }

  return {
    MAIN,
    VIEWS,
    BUDGET,
    Store,
    callActions,
    movableChanged,
    layout,
    QUIET_MS,
    motion,
    withWorking,
    nearest,
    unseen,
    duration,
    tokens,
    plural,
    wakesWords,
    costWords,
    WORKER_MODELS,
    workerDepthAbout,
    workerWords,
    stateOf,
    figures,
    contextTitle,
    runTime,
    firstLine,
    markdown,
    inline,
  };
});
