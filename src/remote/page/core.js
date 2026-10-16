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
      this.counts = { main: 0, workers: 0 };
    }

    /** Starts over with `items` and `workers` (a `reset`). */
    reset(items, workers) {
      this.items = [];
      this.byId.clear();
      this.workers.clear();
      this.counts = { main: 0, workers: 0 };
      for (const item of items) this.add(item);
      for (const worker of workers || []) this.workers.set(worker.id, worker);
    }

    /** A new item; returns the items trimmed to keep the budget. */
    add(item) {
      if (this.byId.has(item.id)) {
        this.change(item);
        return [];
      }
      this.items.push(item);
      this.byId.set(item.id, item);
      const lane = item.agent === MAIN ? 'main' : 'workers';
      this.counts[lane] += 1;
      const trimmed = [];
      while (this.counts[lane] > BUDGET[lane]) {
        const index = this.items.findIndex((old) => (old.agent === MAIN) === (lane === 'main'));
        if (index < 0) break;
        const [old] = this.items.splice(index, 1);
        this.byId.delete(old.id);
        this.counts[lane] -= 1;
        trimmed.push(old);
      }
      return trimmed;
    }

    /** More text for the item `id`; false if it isn't kept. */
    append(id, text) {
      const item = this.byId.get(id);
      if (!item) return false;
      item.text += text;
      return true;
    }

    /** The item with this one's ID is now this one. */
    change(item) {
      const old = this.byId.get(item.id);
      if (!old) return false;
      Object.assign(old, item);
      // A field gone from the new one (an outcome) is gone.
      for (const key of Object.keys(old)) if (!(key in item)) delete old[key];
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
  function layout(items, view, worker) {
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
        // The workers' calls and troubles inline; their text and reports
        // are in their own views.
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

  /** The row of `rows` to keep the reader's place at, for the item `id`
   * they were reading: that one, else the nearest one before it, else the
   * nearest one after. (A result shows under its call, so the rows'
   * IDs aren't all in order.) */
  function nearest(rows, id) {
    let before = null;
    let after = null;
    for (const row of rows) {
      if (row.id === id) return row;
      if (row.id < id && (!before || row.id > before.id)) before = row;
      if (row.id > id && (!after || row.id < after.id)) after = row;
    }
    return before || after;
  }

  /** How many of `rows` came after the item `seen`. */
  function unseen(rows, seen) {
    let count = 0;
    for (const row of rows) if (row.id > seen) count++;
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

  /** A worker's summary in words: `{ title, meta, last }`, where meta is
   * "18 calls · 2m14s · running". */
  function workerWords(summary, now) {
    const parts = [plural(summary.tool_calls, 'call', 'calls')];
    if (summary.started_at != null) {
      const end = summary.ended_at != null ? summary.ended_at : now;
      parts.push(duration(end - summary.started_at));
    }
    parts.push(summary.state);
    let last = summary.last || '';
    if (summary.state === 'finished' && summary.report) last = summary.report.split('\n').find((l) => l.trim()) || last;
    return { title: summary.title || summary.id, meta: parts.join(' · '), last: last.trim() };
  }

  /** The header's state: `{ key, label }`. */
  function stateOf(status, connection) {
    if (connection === 'offline') return { key: 'offline', label: 'Offline' };
    if (connection !== 'live') return { key: 'connecting', label: 'Connecting' };
    if (!status) return { key: 'connecting', label: 'Connecting' };
    if (status.question) return { key: 'asking', label: 'Asks you' };
    if (status.working) return { key: 'working', label: 'Working' };
    return { key: 'idle', label: 'Idle' };
  }

  /** The header's figures: context against the handover size, then cost
   * (or the plan's usage on a subscription). */
  function figures(status) {
    const parts = [];
    if (!status) return parts;
    const handover = status.settings && status.settings.handover_at;
    if (status.context_tokens != null) {
      // As the terminal's status line says it: the context against the
      // size the agent hands over at.
      parts.push(tokens(status.context_tokens) + (handover ? '/' + tokens(handover) : ''));
    }
    // A plan's usage ("Claude plan 5 h 21%, 7 d 4%"), what was billed,
    // and what the usage on a plan would cost at API prices ("≈ $3.20").
    if (status.plan_usage) parts.push(status.plan_usage.replace(/^.*?\bplan\s+/i, ''));
    if (status.cost > 0) parts.push('$' + status.cost.toFixed(2));
    if (status.api_price > 0) parts.push('≈ $' + status.api_price.toFixed(2));
    return parts;
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
    layout,
    nearest,
    unseen,
    duration,
    tokens,
    plural,
    workerWords,
    stateOf,
    figures,
    markdown,
    inline,
  };
});
