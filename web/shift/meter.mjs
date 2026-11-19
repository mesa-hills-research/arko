// The page's shift meter. It plays scripted sessions into the page in
// Chromium's headless shell, a frame at a time, and records where every line
// of text in the transcript stands on every frame, in device pixels. Every
// word the session makes is a word no other has (w1, w2, …), so each line is
// found again in the next frame by its first word that no other line has.
//
// Following, a line may move up as what comes at the end pushes it, and
// never down. Scrolled up, no line moves. A fold the reader opens or closes
// moves no line above it, up or down. The moves the rules allow are the
// reader's own (a scroll, End, the marker, sending, another view or width),
// "Working…" and a note waiting at the end going down into the blank below
// the end as what comes above them takes it, and a taller view giving back
// what a shorter one took while following, or hid while the reader read
// before going to the end (a phone's keyboard). A panel opened and closed with
// nothing between puts every line back on its pixel.
//
// Then it plays the pixel cases, each in a session of its own at ratios 1,
// 1.25, 1.5 and 2, with the inputs' times set.
//
// Arko's `remote_page_shift` test runs it with the session's side, which
// makes the feed's changes it asks for. Node 24 and its WebSocket are all it
// needs. `--page DIR` serves the page's files from DIR. SHIFT_METER_OUT
// names a file for the tables, and SHIFT_METER_FULL=1 plays every size and
// view with more turns. To look closer, SHIFT_METER_ONLY=1:quiet:up plays
// one session (size, view, following or up), SHIFT_METER_CASES=0 leaves the
// pixel cases out and SHIFT_METER_CASES_ONLY=1 plays only them,
// SHIFT_METER_CASE=<pattern> plays only the pixel cases whose names match it,
// SHIFT_METER_VERBOSE=1 prints each move, and SHIFT_METER_TRACE=<step>
// prints the lines of every frame of the steps it names and the page's
// console. SHIFT_METER_TABS is how many browsers play at once (3).

import net from 'node:net';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const argv = process.argv.slice(2);
const arg = (name) => {
  const at = argv.indexOf(`--${name}`);
  return at >= 0 ? argv[at + 1] : undefined;
};
const FULL = process.env.SHIFT_METER_FULL === '1';
const OUT = process.env.SHIFT_METER_OUT;
const ONLY = process.env.SHIFT_METER_ONLY; // "size:view:reader", for one session
const VERBOSE = process.env.SHIFT_METER_VERBOSE === '1';
const PAGE = arg('page');
const TRACE = process.env.SHIFT_METER_TRACE;
const CASES_ONLY = process.env.SHIFT_METER_CASES_ONLY === '1';
const CASE = process.env.SHIFT_METER_CASE ? new RegExp(process.env.SHIFT_METER_CASE) : null;

// ----- Chromium over the DevTools protocol -----

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    this.listeners = new Map();
    ws.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id !== undefined) {
        const waiting = this.pending.get(message.id);
        if (waiting) {
          this.pending.delete(message.id);
          if (message.error) waiting.reject(new Error(`${waiting.method}: ${message.error.message}`));
          else waiting.resolve(message.result);
        }
      } else {
        const key = `${message.sessionId ?? ''}:${message.method}`;
        for (const listen of this.listeners.get(key) ?? []) listen(message.params);
      }
    });
  }

  send(method, params = {}, sessionId, ms = 20000) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} took over ${ms} ms`));
      }, ms);
      const done = (then) => (value) => { clearTimeout(timer); then(value); };
      this.pending.set(id, { resolve: done(resolve), reject: done(reject), method });
      this.ws.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }

  on(sessionId, method, listen) {
    const key = `${sessionId ?? ''}:${method}`;
    if (!this.listeners.has(key)) this.listeners.set(key, []);
    this.listeners.get(key).push(listen);
  }
}

async function launch(path) {
  const profile = mkdtempSync(join(tmpdir(), 'arko-shift-'));
  const chrome = spawn(path, [
    '--remote-debugging-port=0',
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-sandbox',
    '--enable-begin-frame-control',
    '--run-all-compositor-stages-before-draw',
    '--disable-gpu',
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
    'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  const url = await new Promise((resolve, reject) => {
    let text = '';
    chrome.stderr.on('data', (data) => {
      text += data;
      const found = text.match(/ws:\/\/\S+/);
      if (found) resolve(found[0]);
    });
    chrome.on('exit', (code) => reject(new Error(`Chromium exited (${code}): ${text}`)));
  });
  const ws = new WebSocket(url);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve);
    ws.addEventListener('error', reject);
  });
  return {
    cdp: new Cdp(ws),
    close() {
      chrome.kill();
      rmSync(profile, { recursive: true, force: true });
    },
  };
}

// ----- the session's side (src/remote_shift.rs) -----

class Agent {
  constructor(address) {
    const [host, port] = address.split(':');
    this.socket = net.connect(Number(port), host);
    this.socket.setEncoding('utf8');
    this.id = 0;
    this.waiting = new Map();
    let buffer = '';
    this.socket.on('data', (data) => {
      buffer += data;
      let at;
      while ((at = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, at);
        buffer = buffer.slice(at + 1);
        const answer = JSON.parse(line);
        const resolve = this.waiting.get(answer.id);
        this.waiting.delete(answer.id);
        if (resolve) resolve(answer);
      }
    });
  }

  /// A step for the session. A step about a worker, a question or an
  /// approval names it in `id`, which the answer echoes, so that name is the
  /// request's own.
  ask(request) {
    const id = request.id ?? ++this.id;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, (answer) => (answer.error ? reject(new Error(answer.error)) : resolve(answer)));
      this.socket.write(`${JSON.stringify({ ...request, id })}\n`);
    });
  }

  close() {
    this.socket.end();
  }
}

// ----- in the page -----

// The table of gaps (src/remote/spacing.rs), said again here so the checks
// don't read the page's own answer. Each kind's step above and below it, the
// larger of two neighbours' (an open line and what it holds stack), in CSS
// px. The header and the composer stand where a row of the neighbour's kind
// would, so the first and the last row add the half-leading of their line,
// none where their edge is drawn (a bubble, a card, a report). `endOf` is
// the space under the last row, to the bottom of the column.
const TABLE = `(() => {
  const PX = { none: 0, stack: 4, apart: 12, turn: 24 };
  const ABOVE = { User: 'turn', Said: 'apart', Card: 'apart', TurnEnd: 'apart', Work: 'stack', Open: 'stack', Held: 'stack' };
  const BELOW = { User: 'apart', Said: 'apart', Card: 'apart', Open: 'apart', Held: 'apart', Work: 'stack', TurnEnd: 'turn' };
  const step = (a, b) => (a === 'Open' && b === 'Held' ? 'stack' : PX[ABOVE[b]] >= PX[BELOW[a]] ? ABOVE[b] : BELOW[a]);
  const isOpen = (row) => row.matches(':has(> details[open]), .thinking.open');
  const kindOf = (row) => {
    const c = row.classList;
    if (c.contains('user')) return 'User';
    if (c.contains('wline')) return 'Card';
    if (c.contains('turn') || c.contains('conversation')) return 'TurnEnd';
    if (c.contains('result')) return 'Held';
    if (c.contains('working') || c.contains('count') || c.contains('notice') || c.contains('thinking') || c.contains('call')) return isOpen(row) ? 'Open' : 'Work';
    return 'Said';
  };
  const framed = (row) => row.matches('.user, .wline, .report');
  const leadOf = (row) => {
    if (framed(row)) return 0;
    const s = getComputedStyle(row);
    return (parseFloat(s.lineHeight) - parseFloat(s.fontSize)) / 2;
  };
  const nextOf = (kind) => (kind === 'Held' ? 'Open' : kind);
  const endOf = (row) => PX[step(kindOf(row), nextOf(kindOf(row)))] + leadOf(row);
  return { PX, step, isOpen, kindOf, framed, leadOf, nextOf, endOf };
})()`;

// Counts the stream's events, holds the page's one-second clock and its time,
// and measures the transcript's lines.
const INSTRUMENT = `(() => {
  const table = ${TABLE};
  const meter = { events: 0, ahead: 0, ticks: [] };
  window.__meter = meter;
  const now = Date.now.bind(Date);
  Date.now = () => now() + meter.ahead;
  const every = window.setInterval.bind(window);
  window.setInterval = (callback, ms, ...rest) => {
    if (ms === 1000 && typeof callback === 'function') meter.ticks.push(callback);
    return every(callback, ms, ...rest);
  };
  meter.tick = (ms) => { meter.ahead += ms; for (const tick of meter.ticks) tick(); };
  const Source = window.EventSource;
  window.EventSource = class extends Source {
    addEventListener(type, listen, options) {
      const counted = function (event) {
        try { return listen.call(this, event); } finally { meter.events++; }
      };
      return super.addEventListener(type, counted, options);
    }
  };
  if (localStorage && !localStorage.getItem('arko.view.set')) {
    localStorage.setItem('arko.view', '__VIEW__');
    localStorage.setItem('arko.view.set', '1');
  }
  const visible = new WeakMap();
  const shows = (element) => {
    let known = visible.get(element);
    if (known === undefined) {
      const style = getComputedStyle(element);
      known = style.visibility !== 'hidden' && Number(style.opacity) > 0.05;
      visible.set(element, known);
    }
    return known;
  };
  // A word's font, and how far below its box's top its baseline is in it.
  // A word stands on its baseline, so a word drawn in another font on the
  // same baseline, such as a code span's as its closing mark comes, stays
  // where it is.
  const ascents = new Map();
  const canvas = document.createElement('canvas').getContext('2d');
  // (A face that loads draws the same font with other metrics.)
  document.fonts.addEventListener('loadingdone', () => ascents.clear());
  const ascent = (element) => {
    // (From the longhands, as the shorthand is empty where a font's other
    // properties, such as its numerals, have no place in it.)
    const style = getComputedStyle(element);
    const font = \`\${style.fontStyle} \${style.fontWeight} \${style.fontSize} \${style.fontFamily}\`;
    let known = ascents.get(font);
    if (known === undefined) {
      canvas.font = font;
      known = canvas.measureText('x').fontBoundingBoxAscent;
      if (!Number.isFinite(known)) throw new Error(\`no baseline for the font \${font}\`);
      ascents.set(font, known);
    }
    return [font, known];
  };
  meter.measure = () => {
    const log = document.getElementById('log');
    const rows = document.getElementById('rows');
    if (!log || !rows) return null;
    const dpr = devicePixelRatio;
    const box = log.getBoundingClientRect();
    const top = box.top;
    const bottom = box.top + log.clientHeight;
    const lines = [];
    let endZone = null;
    const children = rows.children;
    // The notes waiting after the rest, and "Working…" below them, which
    // ride on the end.
    for (let at = children.length - 1; at >= 0; at--) {
      const row = children[at];
      if (row.matches('.working, .user.pending')) endZone = row.getBoundingClientRect().top;
      else break;
    }
    let last = null;
    for (const row of children) {
      const rect = row.getBoundingClientRect();
      if (rect.height > 0) last = { bottom: rect.bottom + parseFloat(getComputedStyle(row).marginBottom || '0') };
      if (rect.bottom <= top || rect.top >= bottom) continue;
      const key = row.getAttribute('data-row') || (row.classList.contains('working') ? 'working' : row.className);
      // The row's lines as drawn. A word joins the line its middle is on,
      // and a line stands where its first word does.
      const mine = [];
      const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const text = node.nodeValue;
        if (!text || !text.trim()) continue;
        const parent = node.parentElement;
        if (!parent || !shows(parent)) continue;
        const [font, lift] = ascent(parent);
        const range = document.createRange();
        const pattern = /\\S+/g;
        let match;
        while ((match = pattern.exec(text))) {
          range.setStart(node, match.index);
          range.setEnd(node, match.index + match[0].length);
          const rects = range.getClientRects();
          if (!rects.length) continue;
          const word = rects[0];
          if (word.bottom <= top || word.top >= bottom || word.width === 0) continue;
          const middle = (word.top + word.bottom) / 2;
          let line = mine.find((line) => middle >= line.top && middle <= line.bottom);
          if (!line) {
            line = { top: word.top, bottom: word.bottom, y: Math.round(word.top * dpr), x: Math.round(word.left * dpr), key, words: [] };
            mine.push(line);
          }
          line.words.push([match[0], Math.round(word.top * dpr), Math.round(word.left * dpr), word.top * dpr, (word.top + lift) * dpr, font]);
        }
      }
      lines.push(...mine);
    }
    // The last rows' boxes (key, top, bottom, device px), which say what
    // changed at the end, and the spacing under the last one, as it is and
    // as the table gives it (device px, unrounded).
    let tailEnd = null;
    for (let at = children.length - 1; at >= 0; at--) {
      const row = children[at];
      const rect = row.getBoundingClientRect();
      if (rect.height <= 0) continue;
      // (Anything else at the end, such as a case's spacer, has none.)
      const rule = row.classList.contains('row') ? table.endOf(row) : 0;
      const actual = rows.getBoundingClientRect().bottom - rect.bottom;
      tailEnd = { actual: actual * dpr, rule: rule * dpr };
      break;
    }
    const tail = [];
    for (let at = children.length - 1; at >= 0 && tail.length < 6; at--) {
      const rect = children[at].getBoundingClientRect();
      if (rect.height <= 0) continue;
      const key = children[at].getAttribute('data-row') || (children[at].classList.contains('working') ? 'working' : children[at].className);
      tail.unshift([key, Math.round(rect.top * dpr), Math.round(rect.bottom * dpr)]);
    }
    const jump = document.querySelector('.jump');
    const marker = jump && jump.classList.contains('shown') ? jump.textContent.trim() : '';
    const following = log.classList.contains('pinned') || log.classList.contains('following');
    const dataset = log.dataset;
    return {
      lines: lines.map((line) => [line.y, line.words, line.x, line.key]),
      following,
      choice: dataset.choice || (log.classList.contains('pinned') ? 'follow' : 'read'),
      marker,
      scrollTop: log.scrollTop,
      scrollHeight: log.scrollHeight,
      clientHeight: log.clientHeight,
      top: Math.round(top * dpr),
      bottom: Math.round(bottom * dpr),
      end: last ? Math.round((last.bottom + parseFloat(getComputedStyle(rows).paddingBottom || '0')) * dpr) : null,
      endRaw: last ? (last.bottom + parseFloat(getComputedStyle(rows).paddingBottom || '0')) * dpr : null,
      tail,
      tailEnd,
      bottomRaw: bottom * dpr,
      endZone: endZone === null ? null : Math.round(endZone * dpr),
      start: children.length ? Math.round(children[0].getBoundingClientRect().top * dpr) >= Math.round(top * dpr) : true,
      dpr,
    };
  };
  meter.lineHeight = () => {
    const rows = document.getElementById('rows');
    const height = parseFloat(getComputedStyle(rows || document.body).lineHeight);
    return Math.round((Number.isFinite(height) ? height : 22) * devicePixelRatio);
  };
})();`;

// ----- what a frame says -----

/// The words the session made, w and a number, a line's own.
const sessionWord = (word) => /^w\d+$/.test(word.replace(/[^\w]/g, ''));
const clean = (word) => word.replace(/[^\w…↓]/g, '');

/// Each line of a frame, by its first word no other line has, else by its
/// text where no other line has that, with its device pixel row.
function lineIds(shot) {
  const seen = new Map();
  const texts = new Map();
  // (Within its row, as the same words can show in two rows, a worker's
  // task in its card and in its report.)
  // A line stands where its own word does, else where its first word
  // does, so a marker glyph that changes beside it doesn't move it.
  const all = shot.lines.map(([y, words, x, key]) => {
    const mine = [...new Set(words.map(([word]) => clean(word)).filter(sessionWord))];
    for (const word of mine) seen.set(`${key}|${word}`, (seen.get(`${key}|${word}`) ?? 0) + 1);
    const text = words.map(([word]) => word).join(' ');
    texts.set(`${key}|${text}`, (texts.get(`${key}|${text}`) ?? 0) + 1);
    return { y, x, key, words, text };
  });
  const out = new Map();
  for (const { y, x, key, words, text } of all) {
    const own = words.find(([word]) => sessionWord(clean(word)) && seen.get(`${key}|${clean(word)}`) === 1);
    if (own) out.set(`${key}|w:${clean(own[0])}`, { y: own[1], x: own[2], raw: own[3], base: own[4], font: own[5], text });
    else if (texts.get(`${key}|${text}`) === 1) {
      // (Where its first word, a marker, is drawn in another glyph.)
      const word = words.find(([word]) => /[A-Za-z0-9]/.test(word)) ?? words[0];
      out.set(`${key}|t:${text}`, { y: word ? word[1] : y, x: word ? word[2] : x, raw: word ? word[3] : y, base: word ? word[4] : y, font: word?.[5], text });
    }
  }
  return out;
}

/// How far a line moved from `was` to `now`, in device pixels (down is
/// more). By its box, unless its word is drawn in another font, when it's by
/// its baseline, as a word restyled in place stands on the same one.
const shift = (was, now) => Math.round(was.font === now.font ? now.raw - was.raw : now.base - was.base);

// ----- the tallies -----

class Tally {
  constructor() { this.frames = 0; this.shifted = 0; this.max = 0; this.total = 0; }
  add(moved) {
    this.frames++;
    if (moved > 0) { this.shifted++; this.max = Math.max(this.max, moved); this.total += moved; }
  }
  cell() {
    if (this.frames === 0) return '–';
    if (this.shifted === 0) return `${this.frames} · 0`;
    return `${this.frames} · **${this.shifted}** · ${this.max} / ${this.total}`;
  }
}

const VIEWS = ['full', 'quiet', 'messages'];

class Meter {
  constructor() {
    this.tallies = new Map(); // step|view|reader
    this.sizes = new Map(); // size|view|reader
    this.causes = new Map(); // cause|step|view
    this.allowed = new Map(); // kind|step|view
    this.examples = new Map(); // cause|step|view → [px, text]
    this.trips = new Map(); // what|view → { trips, failed, max }
    this.tripExamples = new Map();
    this.steps = new Set();
    this.blanks = new Map(); // step|view
  }

  tally(map, key) {
    if (!map.has(key)) map.set(key, new Tally());
    return map.get(key);
  }

  frames() { let n = 0; for (const tally of this.tallies.values()) n += tally.frames; return n; }
  shifted() { let n = 0; for (const tally of this.tallies.values()) n += tally.shifted; return n; }
  tripsFailed() { let n = 0; for (const trip of this.trips.values()) n += trip.failed; return n; }

  table() {
    const head = ['Step'];
    for (const view of VIEWS) for (const reader of ['following', 'scrolled up']) head.push(`${view}, ${reader}`);
    const rows = [head, head.map(() => '---')];
    const all = VIEWS.flatMap((view) => ['following', 'scrolled up'].map((reader) => ({ view, reader, tally: new Tally() })));
    for (const step of ORDER.filter((step) => this.steps.has(step))) {
      const row = [step];
      for (const column of all) {
        const tally = this.tallies.get(`${step}|${column.view}|${column.reader}`);
        row.push(tally ? tally.cell() : '–');
        if (tally) {
          column.tally.frames += tally.frames;
          column.tally.shifted += tally.shifted;
          column.tally.max = Math.max(column.tally.max, tally.max);
          column.tally.total += tally.total;
        }
      }
      rows.push(row);
    }
    rows.push(['All', ...all.map((column) => column.tally.cell())]);
    return markdown(rows);
  }

  sizeTable() {
    const rows = [['Size', 'View', 'Following', 'Scrolled up'], ['---', '---', '---', '---']];
    for (const size of SIZES.filter((size) => [...this.sizes.keys()].some((key) => key.startsWith(`${size.name}|`)))) {
      for (const view of VIEWS) {
        const get = (reader) => this.sizes.get(`${size.name}|${view}|${reader}`)?.cell() ?? '–';
        rows.push([size.name, view, get('following'), get('scrolled up')]);
      }
    }
    return markdown(rows);
  }

  causeTable() {
    const rows = [['Cause', 'Step', ...VIEWS], ['---', '---', ...VIEWS.map(() => '---')]];
    const keys = [...new Set([...this.causes.keys()].map((key) => key.split('|').slice(0, 2).join('|')))].sort();
    for (const key of keys) {
      const [cause, step] = key.split('|');
      const cells = VIEWS.map((view) => {
        const tally = this.causes.get(`${cause}|${step}|${view}`);
        return tally ? `${tally.shifted} · ${tally.max} / ${tally.total}` : '–';
      });
      rows.push([cause, step, ...cells]);
    }
    return markdown(rows);
  }

  allowedTable() {
    const rows = [['Move', 'Step', ...VIEWS], ['---', '---', ...VIEWS.map(() => '---')]];
    const keys = [...new Set([...this.allowed.keys()].map((key) => key.split('|').slice(0, 2).join('|')))].sort();
    for (const key of keys) {
      const [kind, step] = key.split('|');
      const cells = VIEWS.map((view) => {
        const tally = this.allowed.get(`${kind}|${step}|${view}`);
        return tally ? `${tally.shifted} · ${tally.max} / ${tally.total}` : '–';
      });
      rows.push([kind, step, ...cells]);
    }
    return markdown(rows);
  }

  tripTable() {
    const rows = [['Round trip', ...VIEWS], ['---', ...VIEWS.map(() => '---')]];
    const kinds = [...new Set([...this.trips.keys()].map((key) => key.split('|')[0]))].sort();
    for (const kind of kinds) {
      rows.push([kind, ...VIEWS.map((view) => {
        const trip = this.trips.get(`${kind}|${view}`);
        if (!trip) return '–';
        return trip.failed ? `${trip.trips} · **${trip.failed}** · ${trip.max}` : `${trip.trips} · 0`;
      })]);
    }
    return markdown(rows);
  }

  blankTable() {
    const rows = [['Step', ...VIEWS], ['---', ...VIEWS.map(() => '---')]];
    const steps = [...new Set([...this.blanks.keys()].map((key) => key.split('|')[0]))];
    for (const step of ORDER.filter((step) => steps.includes(step))) {
      rows.push([step, ...VIEWS.map((view) => {
        const tally = this.blanks.get(`${step}|${view}`);
        return tally ? `${tally.frames} · ${tally.max} / ${tally.total}` : '–';
      })]);
    }
    return markdown(rows);
  }

  exampleText() {
    let out = '';
    for (const [key, [, text]] of [...this.examples.entries()].sort()) {
      out += `### ${key.split('|').join(', ')}\n\n\`\`\`text\n${text}\`\`\`\n\n`;
    }
    for (const [key, text] of [...this.tripExamples.entries()].sort()) {
      out += `### round trip, ${key.split('|').join(', ')}\n\n\`\`\`text\n${text}\`\`\`\n\n`;
    }
    return out;
  }
}

function markdown(rows) {
  return rows.map((row) => `| ${row.join(' | ')} |`).join('\n') + '\n';
}

// The steps, in the order of a turn.
const ORDER = [
  'message → Working…',
  'Working… → call',
  'Working… → thought',
  'thought streams',
  'thought → call',
  'call joins a row of one file',
  'call joins a row of its kind',
  'call → its result',
  "code run's call starts",
  "code run's call ends",
  'result → reply',
  'thought → reply',
  'reply streams',
  'reply pauses → Working…',
  'Working… → reply goes on',
  'job running → finished',
  'notice',
  'message waits',
  'waiting message read',
  'fold opens near the end',
  'fold closes near the end',
  'approval tray opens',
  'approval tray closes',
  'question comes',
  'question answered',
  'answer panel opens',
  'answer panel closes',
  'slash menu opens',
  'slash menu closes',
  'message box grows',
  'message box shrinks',
  'window shorter',
  'window taller',
  'worker starts',
  'worker ends',
  'turn ends',
];

// Where blank may come below the end while following, as the end gets
// shorter (rule 1, and rows 23 and 25 of the table in DESIGN.md section 9).
// Blank in any other step fails ([`Run.compare`]), so the agent's calls
// coming and ending, a job ending, notices and the live line coming and
// going leave none.
// (Row 23's shrink, a Full command that ends with nothing to show, doesn't
// come up, as every command here shows output.)
function blankAllowed(step) {
  return BLANK_STEPS.has(step);
}
const BLANK_STEPS = new Set([
  // "Working…" or the thought that was the live line goes into the last row.
  'call joins a row of one file',
  'call joins a row of its kind',
  'thought → call',
  // A reply wraps again shorter, or the line before it folds.
  'reply streams',
  'result → reply',
  'thought → reply',
  'Working… → reply goes on',
  // The reader closes a fold.
  'fold closes near the end',
  // The last item shrinks or goes (row 25): a note read, a worker's row as
  // the worker starts and ends, "Working…" as the turn ends.
  'waiting message read',
  'worker starts',
  'worker ends',
  'turn ends',
]);

/// Whether the one change at the end between two frames is that "Working…"
/// went: the last rows of `before` are those of `after` and "Working…" after
/// them, each other row in the same place and of the same size, to the
/// device pixel, and the row now last has the spacing under it the end rule
/// gives it. The blank that comes is then the line "Working…" took (rule 1),
/// in whatever step it goes, as when the agent waits on the reader's answer,
/// and any other change at the end in that frame (a row or the end's
/// spacing a pixel shorter) still fails.
function onlyWorkingWent(before, after) {
  const was = before.tail ?? [];
  const now = after.tail ?? [];
  if (!was.length || was[was.length - 1][0] !== 'working' || now.some(([key]) => key === 'working')) return false;
  const kept = was.slice(0, -1);
  const tail = now.slice(-kept.length);
  if (!kept.length || tail.length !== kept.length) return false;
  if (!kept.every(([key, top, bottom], at) => tail[at][0] === key && tail[at][1] === top && tail[at][2] === bottom)) return false;
  // (And the row now last has the end's spacing the rule gives it, so
  // the blank is the line "Working…" took and nothing besides.)
  return !!after.tailEnd && Math.round(after.tailEnd.actual - after.tailEnd.rule) === 0;
}

// ----- sizes -----

const SIZES = [
  { name: 'desktop 1280×800 @1', width: 1280, height: 800, dpr: 1, mobile: false },
  { name: 'phone 390×844 @3', width: 390, height: 844, dpr: 3, mobile: true },
  { name: 'laptop 1440×900 @2', width: 1440, height: 900, dpr: 2, mobile: false },
  { name: 'window 900×560 @1.25', width: 900, height: 560, dpr: 1.25, mobile: false },
  { name: 'laptop 1280×720 @1.5', width: 1280, height: 720, dpr: 1.5, mobile: false },
  { name: 'phone 412×915 @2.625', width: 412, height: 915, dpr: 2.625, mobile: true },
];

// ----- a session in the page -----

class Rng {
  constructor(seed) { this.state = BigInt(seed) * 0x9E3779B97F4A7C15n | 1n; this.state &= (1n << 64n) - 1n; }
  below(n) {
    let x = this.state;
    x ^= (x << 13n) & ((1n << 64n) - 1n);
    x ^= x >> 7n;
    x ^= (x << 17n) & ((1n << 64n) - 1n);
    this.state = x;
    return Number(x % BigInt(Math.max(1, n)));
  }
  chance(percent) { return this.below(100) < percent; }
}

class Run {
  constructor({ cdp, agent, meter, size, view, scrolled, seed }) {
    Object.assign(this, { cdp, agent, meter, size, view, scrolled });
    this.rng = new Rng(seed);
    // The code runs' own numbers, which take the place of `rng` while one
    // plays, so the rest of the session makes the choices it would make
    // without them.
    this.runs = new Rng(seed * 7919 + 1);
    this.word = 0;
    this.last = null;
    this.calls = [];
    this.live = 'working';
    this.job = null;
    this.waiting = null;
    this.seq = 0;
    this.opened = null;
    this.owed = 0;
    this.shrunk = 0;
    this.hidden = 0;
    this.jumping = false;
    this.textOpen = false;
    this.workers = 0;
    this.failures = [];
    this.turnOffset = seed % 6;
  }

  words(n) {
    const out = [];
    for (let at = 0; at < Math.max(1, n); at++) out.push(`w${++this.word}`);
    return out.join(' ');
  }

  some(least, more) { return this.words(least + this.rng.below(more)); }

  name(kind) { return `${kind}${++this.word}`; }

  evaluate(expression) {
    return this.cdp.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }, this.session)
      .then((answer) => {
        if (answer.exceptionDetails) throw new Error(`${expression}: ${answer.exceptionDetails.text} ${answer.exceptionDetails.exception?.description ?? ''}`);
        return answer.result.value;
      });
  }

  /// Opens the page on a session of its own, or on `url`, an earlier
  /// page's. Each page has a browser context of its own, so nothing a page
  /// stored (the view a case picked, a draft) reaches a later one that the
  /// system gave the same port.
  async open({ url: earlier } = {}) {
    const { browserContextId } = await this.cdp.send('Target.createBrowserContext', {});
    this.context = browserContextId;
    const { targetId } = await this.cdp.send('Target.createTarget', { url: 'about:blank', enableBeginFrameControl: true, browserContextId });
    this.target = targetId;
    const { sessionId } = await this.cdp.send('Target.attachToTarget', { targetId, flatten: true });
    this.session = sessionId;
    await this.cdp.send('Page.enable', {}, sessionId);
    await this.cdp.send('Runtime.enable', {}, sessionId);
    // (Each session's page acts as the one with focus, as several play at
    // once.)
    await this.cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true }, sessionId);
    this.cdp.on(sessionId, 'Runtime.exceptionThrown', (event) => {
      this.failures.push(`page exception: ${event.exceptionDetails?.exception?.description ?? event.exceptionDetails?.text}`);
    });
    this.cdp.on(sessionId, 'Runtime.consoleAPICalled', (event) => {
      if (TRACE && event.type === 'log') console.log(`   console [${this.step}]: ${event.args.map((arg) => arg.value ?? arg.description).join(' ')}`);
      if (event.type === 'error') this.failures.push(`console: ${event.args.map((arg) => arg.value ?? arg.description).join(' ')}`);
    });
    await this.metrics(this.size.width, this.size.height);
    if (this.size.mobile) {
      await this.cdp.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 }, sessionId);
      await this.cdp.send('Emulation.setUserAgentOverride', {
        userAgent: 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Mobile Safari/537.36',
      }, sessionId);
    }
    await this.cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: INSTRUMENT.replace('__VIEW__', this.view) }, sessionId);
    // The page's files as built in this tree, whatever the server was built
    // with, and the requests a case holds back (`holdWholes`).
    this.patterns = PAGE
      ? ['arko-web.js', 'arko-web_bg.wasm', 'app.css', 'boot.js', 'themes.css'].map((name) => ({ urlPattern: `*/${name}*`, requestStage: 'Response' }))
      : [];
    this.heldBack = null;
    this.cdp.on(sessionId, 'Fetch.requestPaused', (event) => {
      if (event.responseStatusCode === undefined && this.heldBack) {
        this.heldBack.push(event.requestId);
        return;
      }
      let body = null;
      try {
        body = PAGE ? readFileSync(join(PAGE, new URL(event.request.url).pathname.slice(1))) : null;
      } catch {
        body = null;
      }
      if (body === null || event.responseStatusCode !== 200) {
        this.cdp.send('Fetch.continueRequest', { requestId: event.requestId }, sessionId).catch(() => {});
        return;
      }
      this.cdp.send('Fetch.fulfillRequest', {
        requestId: event.requestId,
        responseCode: 200,
        responseHeaders: event.responseHeaders.filter((header) => !/^(content-length|content-encoding|etag)$/i.test(header.name)),
        body: body.toString('base64'),
      }, sessionId).catch(() => {});
    });
    if (this.patterns.length) await this.cdp.send('Fetch.enable', { patterns: this.patterns }, sessionId);
    const { url } = earlier ? { url: earlier } : await this.agent.ask({ do: 'session' });
    this.url = url;
    this.frameTime = 1000;
    await this.cdp.send('Page.navigate', { url }, sessionId);
    for (let at = 0; at < 600; at++) {
      await this.beginFrame();
      const ready = await this.evaluate(`!!(window.__meter && document.getElementById('rows') && !document.querySelector('.conn-row'))`).catch(() => false);
      if (ready) break;
      await sleep(10);
    }
    // (A page in a view the run didn't ask for measures something else.)
    const view = await this.evaluate(`localStorage.getItem('arko.view')`);
    if (view !== this.view) this.failures.push(`the page opened in the view ${view}, not ${this.view}`);
    this.lineHeight = await this.evaluate('__meter.lineHeight()');
    await this.settle(null);
  }

  async close() {
    await this.cdp.send('Target.closeTarget', { targetId: this.target }).catch(() => {});
    await this.cdp.send('Target.disposeBrowserContext', { browserContextId: this.context }).catch(() => {});
  }

  async metrics(width, height) {
    this.width = width;
    this.height = height;
    await this.cdp.send('Emulation.setDeviceMetricsOverride', {
      width, height, deviceScaleFactor: this.size.dpr, mobile: this.size.mobile,
    }, this.session);
  }

  async beginFrame() {
    this.frameTime += 1000 / 60;
    await this.cdp.send('HeadlessExperimental.beginFrame', { frameTimeTicks: this.frameTime, interval: 1000 / 60, noDisplayUpdates: false }, this.session);
  }

  /// Draws a frame and counts how its lines moved from the last one's,
  /// under the step under way (none for the reader's own moves). Returns
  /// whether the screen stood still.
  async frame() {
    await this.beginFrame();
    const shot = await this.shot();
    if (!shot) return false;
    if (TRACE && String(this.step).startsWith(TRACE) && this.last && this.traced !== this.last) {
      console.log(`-- before, top ${this.last.scrollTop} end ${this.last.end} bottom ${this.last.bottom}`);
      for (const [y, words, x, key] of [...this.last.lines].sort((a, b) => a[0] - b[0])) console.log(`   ${y} ${x} ${key} ${words.map(([word]) => word).join(' ').slice(0, 60)}`);
    }
    if (TRACE && String(this.step).startsWith(TRACE)) {
      this.traced = shot;
      console.log(`-- ${this.step} top ${shot.scrollTop} range ${shot.scrollHeight}-${shot.clientHeight} end ${shot.end} bottom ${shot.bottom} following ${shot.following} choice ${shot.choice} marker "${shot.marker}"`);
      for (const [y, words, x, key] of [...shot.lines].sort((a, b) => a[0] - b[0])) console.log(`   ${y} ${x} ${key} ${words.map(([word]) => word).join(' ').slice(0, 60)}`);
    }
    const moved = this.last ? this.compare(this.last, shot) : 0;
    const same = this.last && moved === 0 && sameLines(this.last, shot) && this.last.scrollTop === shot.scrollTop;
    this.last = shot;
    return same;
  }

  /// Runs `input` (a DevTools call that may wait for a frame) while frames
  /// are drawn.
  async during(input) {
    let done = false;
    const sent = input().catch(() => {}).finally(() => { done = true; });
    for (let at = 0; at < 30 && !done; at++) await this.frame();
    // (An input the page never answers, such as a key a dialog took, is
    // let go after its frames.)
    await Promise.race([sent, sleep(200)]);
  }

  async shot() {
    const shot = await this.evaluate('__meter.measure()');
    if (shot) shot.ids = lineIds(shot);
    return shot;
  }

  /// Waits until the page took more of the stream's events than `before`,
  /// or a while.
  async delivered(before, ms = 400) {
    const until = Date.now() + ms;
    for (;;) {
      const now = await this.evaluate('__meter.events');
      if (now > before) {
        await sleep(8);
        return;
      }
      if (Date.now() > until) return;
      await sleep(2);
    }
  }

  async events() {
    return this.evaluate('__meter.events');
  }

  /// Asks the session for `request`, and waits until the page has it.
  async feed(request) {
    const before = await this.events();
    const answer = await this.agent.ask(request);
    await this.delivered(before);
    return answer;
  }

  /// Starts counting frames under `step` (none for the reader's own
  /// moves). `below` is the device pixel row of a fold the reader toggled,
  /// below which lines aren't counted and above which a line counts that
  /// moves at all, and `viewport` says the transcript's height changes.
  begin(step, { below = null, viewport = false } = {}) {
    this.step = step;
    this.below = below;
    this.viewport = viewport;
  }

  /// Draws frames until the screen is still, counted under `step`.
  async settle(step, options = {}) {
    if (step !== undefined) this.begin(step, options);
    let still = 0;
    for (let at = 0; at < 120; at++) {
      still = (await this.frame()) ? still + 1 : 0;
      if (still >= 3 && at >= 3) break;
    }
    this.begin(null);
  }

  /// The highest device pixel row of `before` at which a line changed, went
  /// or came between the two frames, inside the screen.
  changeAt(before, after) {
    // (Where the view scrolled, a line at an edge may only have come into
    // sight.)
    const still = before.scrollTop === after.scrollTop && before.clientHeight === after.clientHeight;
    const margin = still ? 0 : this.lineHeight;
    const inside = (y, shot) => y >= shot.top - this.lineHeight + margin && y < shot.bottom - margin;
    let at = Infinity;
    for (const [id, line] of before.ids) {
      const now = after.ids.get(id);
      if ((!now && inside(line.y, before)) || (now && now.text !== line.text)) at = Math.min(at, line.y);
    }
    const kept = [...after.ids.entries()].filter(([id]) => before.ids.has(id)).sort((a, b) => a[1].y - b[1].y);
    for (const [id, line] of after.ids) {
      if (before.ids.has(id) || !inside(line.y, after)) continue;
      // (Below the line above it that both frames show.)
      const above = kept.filter(([, other]) => other.y < line.y).pop();
      at = Math.min(at, above ? before.ids.get(above[0]).y + 1 : before.top);
    }
    return at;
  }

  /// Counts how the lines of `after` moved from `before`.
  compare(before, after) {
    const { step, below, viewport } = this;
    if (before.choice === 'read' && after.choice === 'follow') this.followsAgain();
    const following = before.following && after.following;
    const changed = this.changeAt(before, after);
    const blank = Math.max(0, before.bottom - before.end);
    let rows = 0;
    let line = null;
    let room = 0;
    let pushed = 0;
    let highest = null;
    let up = 0;
    let sideways = 0;
    let sidewaysLine = null;
    for (const [id, was] of before.ids) {
      const { y: from, x, text } = was;
      const now = after.ids.get(id);
      if (!now) continue;
      // (By how far it moved, as a line on a half device pixel rounds
      // either way with float noise that moves nothing drawn.)
      const to = from + shift(was, now);
      if (below !== null && from > below) continue;
      if (to < from) up = Math.max(up, from - to);
      // (A line that keeps its text and its row keeps its column too.)
      if (to === from && now.text === text && now.x !== x && !(after.endZone !== null && to >= after.endZone)) {
        if (Math.abs(now.x - x) > sideways) {
          sideways = Math.abs(now.x - x);
          sidewaysLine = { id, from: x, to: now.x, text };
        }
      }
      // (A line whose text changed is the change itself, such as a word
      // that becomes code as its closing mark comes.)
      if (now.text !== text) continue;
      // (In the frame of a toggle, the clicked line and every line above it
      // stay where they were, and the moves below don't apply.)
      const toggled = below !== null;
      // ("Working…" and the notes waiting at the end make room below.)
      if (!toggled && after.endZone !== null && now.y >= after.endZone && (to > from || !following)) {
        room = Math.max(room, Math.abs(to - from));
        continue;
      }
      // (Following, a line may go up as what comes at the end pushes it,
      // but not in the frame of a toggle.)
      let moved = to > from || !following || toggled ? Math.abs(to - from) : 0;
      // (Lines below a change on the screen move with it. Following, they
      // go down into the blank below the end and no further.)
      if (!toggled && from > changed && moved > 0 && (!following || moved <= blank)) {
        pushed = Math.max(pushed, moved);
        moved = 0;
      }
      if (highest === null || from < highest.from) highest = { from, to };
      if (moved > rows) {
        rows = moved;
        line = { id, from, to, text };
      }
    }
    if (step === null || step === undefined) return 0;
    // Blank left below the end while following, by step. It comes where the
    // end gets shorter (rule 1), and only in the steps [`blankAllowed`]
    // names, so blank in any other step fails.
    if (following && after.clientHeight === before.clientHeight && before.endRaw !== null && after.endRaw !== null) {
      // (Rounded as a line's move is, by how far the end itself moved.)
      const blankOf = (shot) => Math.max(0, shot.bottomRaw - shot.endRaw);
      const grew = Math.round(blankOf(after) - blankOf(before));
      if (grew > 0) {
        this.meter.tally(this.meter.blanks, `${step}|${this.view}`).add(grew);
        if (!blankAllowed(step) && !onlyWorkingWent(before, after)) {
          const tail = (shot) => (shot.tail ?? []).map(([key, top, bottom]) => `${key} ${top}-${bottom}`).join(', ');
          this.failures.push(`${grew} device px of blank came below the end at "${step}" (${this.view}); the last rows were ${tail(before)}, then ${tail(after)}`);
        }
      }
    }
    const key = (kind) => `${kind}|${step}|${this.view}`;
    if (room > 0) this.meter.tally(this.meter.allowed, key('the end makes room')).add(room);
    if (pushed > 0) this.meter.tally(this.meter.allowed, key('below a change on the screen')).add(pushed);
    // While following, a shorter view moves the lines up, and a taller one
    // gives that back first. What is owed starts, when following starts,
    // at what shorter views hid at the bottom while reading, and a taller
    // view while reading takes from both.
    const taller = after.clientHeight - before.clientHeight;
    this.shrunk = Math.max(0, this.shrunk - taller);
    if (taller > 0) this.hidden = Math.max(0, this.hidden - taller);
    else if (before.choice === 'read' && after.choice === 'read') {
      this.hidden += Math.max(0, -taller - blank / after.dpr);
    }
    if (viewport && following) {
      if (up > 0 && taller < 0) this.owed += up;
      if (rows > 0 && rows <= this.owed + 1) {
        this.meter.tally(this.meter.allowed, key('a taller view gives back')).add(rows);
        this.owed = Math.max(0, this.owed - rows);
        rows = 0;
      }
    } else if (taller > 0) {
      this.owed = Math.max(0, this.owed - taller * after.dpr);
    }
    if (sideways > rows) {
      rows = sideways;
      line = { ...sidewaysLine, sideways: true };
    }
    const reader = following ? 'following' : 'scrolled up';
    this.meter.steps.add(step);
    this.meter.tally(this.meter.tallies, `${step}|${this.view}|${reader}`).add(rows);
    this.meter.tally(this.meter.sizes, `${this.size.name}|${this.view}|${reader}`).add(rows);
    if (rows > 0 && line) {
      const what = /Working…/.test(line.text) ? 'Working…' : 'other';
      const how = line.sideways ? 'sideways'
        : below !== null ? 'toggled'
        : viewport ? 'the view changed'
        : !following ? 'scrolled up'
        : highest && highest.to > highest.from ? 'all down'
        : before.start && after.start ? 'short'
        : blank > 0 ? 'blank below the end'
        : 'at the end';
      const cause = `${how}, ${what}`;
      this.meter.tally(this.meter.causes, `${cause}|${step}|${this.view}`).add(rows);
      const exampleKey = `${cause}|${step}|${this.view}`;
      const known = this.meter.examples.get(exampleKey);
      if (!known || rows > known[0]) {
        this.meter.examples.set(exampleKey, [rows, describe(this, before, after, line, rows)]);
      }
      if (VERBOSE) console.log(`moved ${rows}px: ${step} ${this.view} ${this.size.name} ${cause} ${line.id} ${line.from}→${line.to} raw ${before.ids.get(line.id)?.raw} → ${after.ids.get(line.id)?.raw}`);
    }
    return rows;
  }

  /// A round trip, the screen after `back` against the one before `out`,
  /// with nothing between. A panel's is checked where the reader is as
  /// before, and `any` checks it whatever they are.
  async trip(kind, out, back, { any = false } = {}) {
    if (!this.last) return;
    const before = this.last;
    const wasFollowing = before.following;
    await out();
    await back();
    const after = this.last;
    const key = `${kind}|${this.view}`;
    if (!this.meter.trips.has(key)) this.meter.trips.set(key, { trips: 0, failed: 0, max: 0 });
    const trip = this.meter.trips.get(key);
    trip.trips++;
    let worst = 0;
    let worstLine = null;
    for (const [id, line] of before.ids) {
      const { y, text } = line;
      const now = after.ids.get(id);
      if (!now) continue;
      const moved = Math.abs(shift(line, now));
      if (moved > worst) { worst = moved; worstLine = { id, from: y, to: now.y, text }; }
    }
    if (worst > 0 && (any || wasFollowing === after.following)) {
      trip.failed++;
      trip.max = Math.max(trip.max, worst);
      if (!this.meter.tripExamples.has(key) || worst > (this.meter.tripMax?.[key] ?? 0)) {
        this.meter.tripMax = { ...(this.meter.tripMax ?? {}), [key]: worst };
        this.meter.tripExamples.set(key, describe(this, before, after, worstLine, worst));
      }
    }
  }

  // ----- the reader -----

  /// The wheel at the transcript's middle, `delta` CSS pixels (up is less
  /// than 0), a step at a time.
  async wheel(delta) {
    const box = await this.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    if (this.size.mobile) {
      // A finger, as the page sees it once the browser has scrolled.
      await this.evaluate(`document.getElementById('log').scrollBy({ top: ${delta}, behavior: 'instant' })`);
      return;
    }
    const steps = Math.max(1, Math.round(Math.abs(delta) / 100));
    for (let at = 0; at < steps; at++) {
      // (Chromium takes an emulated wheel's delta in device pixels.)
      await this.during(() => this.cdp.send('Input.dispatchMouseEvent', {
        type: 'mouseWheel', x: box[0], y: box[1], deltaX: 0, deltaY: (delta / steps) * this.size.dpr,
      }, this.session));
    }
  }

  /// A reader who keeps scrolled up and follows again scrolls up a few lines.
  async scrollUp() {
    if (!this.scrolled || !this.last?.following) return;
    const lines = 2 + this.rng.below(6);
    await this.wheel(-(lines * this.lineHeight) / this.size.dpr);
    await this.settle(null);
  }

  /// A press and a release at `x`, `y` (CSS pixels).
  async click(x, y) {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await this.during(() => this.cdp.send('Input.dispatchMouseEvent', {
        type, x, y, button: type === 'mouseMoved' ? 'none' : 'left', clickCount: 1,
      }, this.session));
    }
  }

  async key(key, code, text, modifiers = 0) {
    const keyCode = { Enter: 13, Escape: 27, Backspace: 8, End: 35, '/': 191 }[key] ?? 0;
    await this.during(() => this.cdp.send('Input.dispatchKeyEvent', {
      type: text ? 'keyDown' : 'rawKeyDown', key, code, text, unmodifiedText: text, windowsVirtualKeyCode: keyCode, modifiers,
    }, this.session));
    await this.during(() => this.cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key, code, windowsVirtualKeyCode: keyCode, modifiers }, this.session));
  }

  async type(text) {
    await this.during(() => this.cdp.send('Input.insertText', { text }, this.session));
  }

  /// The reader clicks the marker, which is End, and follows again.
  async toEnd() {
    this.jumping = true;
    const at = await this.evaluate(`(() => { const j = document.querySelector('.jump.shown'); if (!j) return null; const r = j.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    if (at && this.rng.chance(50)) {
      await this.click(at[0], at[1]);
    } else {
      // End, with the focus out of the message box.
      await this.evaluate('document.activeElement && document.activeElement.blur()');
      await this.key('End', 'End');
    }
    await this.settle(null);
    this.jumped();
  }

  /// Following starts, owing the lines shorter views hid while reading.
  /// (End, the marker and sending owe more, `jumped`.)
  followsAgain() {
    if (!this.jumping) this.owed = this.hidden * this.size.dpr;
    this.hidden = 0;
  }

  /// End, the marker or sending went to the end, owing all that shorter
  /// views took.
  jumped() {
    this.owed = Math.max(this.owed, this.shrunk * this.size.dpr);
    this.hidden = 0;
    this.jumping = false;
  }

  /// Toggles the lowest fold among the last lines on the screen, or toggles
  /// back the one the reader toggled.
  async toggle() {
    const opened = this.opened;
    this.opened = null;
    const fold = await this.evaluate(`(() => {
      const log = document.getElementById('log');
      const box = log.getBoundingClientRect();
      const bottom = box.top + log.clientHeight;
      const folds = [...document.querySelectorAll('#rows summary, #rows button.call-line')]
        .map((element) => ({ element, rect: element.getBoundingClientRect() }))
        .filter(({ rect }) => rect.height > 0 && rect.top >= box.top && rect.bottom <= bottom);
      const wanted = ${JSON.stringify(opened)};
      let found;
      if (wanted) {
        found = folds.find(({ element }) => element.closest('[data-row]')?.getAttribute('data-row') === wanted);
      } else {
        const near = folds.filter(({ rect }) => rect.top >= bottom - 8 * ${this.lineHeight / this.size.dpr});
        found = near[near.length - 1];
      }
      if (!found) return null;
      const details = found.element.closest('details');
      const open = details ? details.open : found.element.getAttribute('aria-expanded') === 'true';
      return {
        row: found.element.closest('[data-row]')?.getAttribute('data-row'),
        x: found.rect.left + Math.min(40, found.rect.width / 2),
        y: found.rect.top + found.rect.height / 2,
        top: Math.round(found.rect.top * devicePixelRatio),
        bottom: Math.round(found.rect.bottom * devicePixelRatio),
        open,
      };
    })()`);
    if (!fold) return;
    if (!opened) this.opened = fold.row;
    const step = fold.open ? 'fold closes near the end' : 'fold opens near the end';
    // (The clicked line is the summary's first line, its words above its
    // bottom.)
    this.begin(step, { below: fold.bottom - 1 });
    await this.click(fold.x, fold.y);
    await this.settle();
    await this.scrollUp();
  }

  // ----- the agent -----

  async meanwhile() {
    let notice = false;
    if (this.rng.chance(6)) {
      await this.feed({ do: 'notice', text: this.some(3, 12), warn: this.rng.chance(50) });
      await this.settle('notice');
      notice = true;
    }
    if (this.job && this.rng.chance(10)) {
      await this.feed({ do: 'job_ended', job: this.job });
      this.job = null;
      await this.settle('job running → finished');
    }
    if (!this.waiting && this.rng.chance(2)) {
      const seq = ++this.seq;
      await this.feed({ do: 'note', text: `Also ${this.some(2, 8)}`, seq });
      this.waiting = seq;
      await this.settle('message waits');
    }
    if (this.rng.chance(8)) await this.toggle();
    if (this.rng.chance(FULL ? 3 : 2)) await this.panel();
    return notice;
  }

  async readWaiting() {
    if (!this.waiting) return;
    await this.feed({ do: 'read', seq: this.waiting });
    this.waiting = null;
    await this.settle('waiting message read');
  }

  async think() {
    if (this.live === 'reply') return;
    await this.feed({ do: 'think', text: `${this.some(2, 4)} ` });
    this.live = 'thought';
    await this.settle('Working… → thought');
    for (let at = this.rng.below(5); at > 0; at--) {
      const n = 2 + this.rng.below(10);
      const more = this.rng.below(6) === 0 ? `\n\n${this.words(n)} ` : `${this.words(n)} `;
      await this.feed({ do: 'think', text: more });
      await this.settle('thought streams');
      await this.meanwhile();
    }
  }

  // A code run. Its call and its code come together, then each call its
  // code makes starts and ends on the run's row, a few of them refused, then
  // its result comes. Now and then its code is longer than a preview shows.
  async codeRun() {
    const purpose = this.words(3);
    const lines = this.rng.chance(30) ? 17 + this.rng.below(10) : 1 + this.rng.below(6);
    const code = Array.from({ length: lines }, () => this.some(2, 4)).join('\n');
    const step = this.live === 'thought' ? 'thought → call' : 'Working… → call';
    const { call: run } = await this.feed({ do: 'call', tool: 'run_code', parameters: { code, purpose } });
    this.calls.push({ kind: 'code', target: purpose });
    await this.settle(step);
    await this.meanwhile();
    for (let n = 1 + this.rng.below(4); n > 0; n--) {
      const target = `${this.folder}/${this.words(1)}.rs`;
      const { call } = await this.feed({ do: 'nested', run, tool: 'read_file', parameters: { path: target } });
      await this.settle("code run's call starts");
      await this.meanwhile();
      const refused = this.rng.chance(15);
      await this.feed({
        do: 'nested_end', run, call, tool: 'read_file',
        outcome: refused ? 'refused' : 'finished',
        output: refused ? 'Refused.' : `1 | ${this.some(2, 3)}\n2 | ${this.some(2, 3)}`,
      });
      await this.settle("code run's call ends");
      await this.meanwhile();
    }
    const output = Array.from({ length: 1 + this.rng.below(4) }, () => this.some(1, 4)).join('\n');
    await this.feed({ do: 'result', call: run, tool: 'run_code', output });
    this.live = 'result';
    await this.settle('call → its result');
    await this.meanwhile();
  }

  async call() {
    if (this.runs.chance(15)) {
      [this.rng, this.runs] = [this.runs, this.rng];
      await this.codeRun();
      [this.rng, this.runs] = [this.runs, this.rng];
    }
    const last = this.calls[this.calls.length - 1];
    const folder = this.folder;
    let kind;
    let target;
    let tool;
    let parameters;
    const pick = this.rng.below(20);
    if (pick <= 6) {
      kind = 'read';
      target = `${folder}/${this.name('f')}.rs`;
    } else if (pick <= 11) {
      if (last && ['read', 'edit'].includes(last.kind)) {
        kind = this.rng.chance(75) ? 'edit' : 'read';
        target = last.target;
      } else {
        kind = 'edit';
        target = `${folder}/${this.name('f')}.rs`;
      }
    } else if (pick <= 17 || this.job) {
      kind = 'command';
      target = `cargo test ${this.words(2)}`;
    } else {
      kind = 'job';
      target = `npm run ${this.words(2)}`;
    }
    switch (kind) {
      case 'read': tool = 'read_file'; parameters = { path: target }; break;
      case 'edit': tool = 'edit_file'; parameters = { path: target, old_string: this.words(2), new_string: this.words(3) }; break;
      default: tool = 'shell'; parameters = { command: target }; break;
    }
    const joins = last && kind !== 'job' && (last.kind === kind || (['read', 'edit'].includes(last.kind) && ['read', 'edit'].includes(kind) && last.target === target));
    const step = joins ? (last.kind === kind ? 'call joins a row of its kind' : 'call joins a row of one file')
      : this.live === 'thought' ? 'thought → call' : 'Working… → call';
    const { call } = await this.feed({ do: 'call', tool, parameters });
    this.calls.push({ kind, target });
    await this.settle(step);
    await this.meanwhile();
    const lines = 1 + this.rng.below(6);
    let output;
    switch (kind) {
      case 'read': output = Array.from({ length: lines }, (_, n) => `${n + 1}\t${this.some(2, 6)}`).join('\n'); break;
      case 'edit': output = `Made 1 edit to ${target}.`; break;
      case 'job': {
        const { job } = await this.agent.ask({ do: 'job', text: target, call });
        this.job = job;
        output = `Started ${target} in the background.`;
        break;
      }
      default: output = `Exit code: 0\n${Array.from({ length: lines }, () => `${this.some(2, 5)} ok`).join('\n')}`;
    }
    await this.feed({ do: 'result', call, tool, output });
    this.live = 'result';
    await this.settle('call → its result');
    await this.meanwhile();
  }

  piece() {
    const words = this.some(1, 6);
    if (this.code) {
      if (this.rng.below(5) === 0) { this.code = false; return `\n\`\`\`\n\n${words}`; }
      return `\n${words}`;
    }
    const pick = this.rng.below(100);
    if (pick <= 7) return `\n\n${words}`;
    if (pick <= 17) {
      this.bold = !this.bold;
      return this.bold ? ` **${words}` : ` ${words}**`;
    }
    if (pick <= 23 && !this.bold) return `\n${this.rng.chance(50) ? '-' : `${1 + this.rng.below(3)}.`} ${words}`;
    if (pick <= 26 && !this.bold) return `\n\n## ${words}\n\n`;
    if (pick <= 31) return ` \`${words}\``;
    if (pick <= 35 && !this.bold) { this.code = true; return `\n\n\`\`\`\n${words}`; }
    if (pick <= 37 && !this.bold) return `\n| ${words} | ${this.some(1, 3)} |`;
    return ` ${words}`;
  }

  closeOpen() {
    const close = this.code ? '\n```\n' : this.bold ? '**' : '';
    this.code = false;
    this.bold = false;
    return close;
  }

  async reply() {
    const step = this.live === 'result' ? 'result → reply' : this.live === 'thought' ? 'thought → reply' : 'Working… → reply goes on';
    await this.feed({ do: 'text', text: this.some(1, 4) });
    this.calls = [];
    this.live = 'reply';
    await this.settle(step);
    for (let at = 6 + this.rng.below(FULL ? 24 : 14); at > 0; at--) {
      const piece = this.piece();
      for (const chunk of this.chunks(piece)) {
        await this.feed({ do: 'text', text: chunk });
        await this.settle('reply streams');
      }
      if (await this.meanwhile()) {
        // (The text goes on in an item of its own.)
        this.closeOpen();
        await this.feed({ do: 'text', text: this.some(1, 4) });
        await this.settle('reply streams');
      }
      if (this.rng.chance(FULL ? 6 : 3)) {
        // It pauses, and Working… shows after a while.
        const close = this.closeOpen();
        if (close) {
          await this.feed({ do: 'text', text: close });
          await this.settle('reply streams');
        }
        await this.evaluate('__meter.tick(2600)');
        this.live = 'working';
        await this.settle('reply pauses → Working…');
        if (this.rng.chance(50)) {
          await this.call();
          await this.feed({ do: 'text', text: this.some(1, 4) });
          this.calls = [];
          this.live = 'reply';
          await this.settle('result → reply');
        } else {
          await this.feed({ do: 'text', text: ` ${this.some(1, 4)}` });
          this.live = 'reply';
          await this.settle('Working… → reply goes on');
        }
      }
    }
    const close = this.closeOpen();
    if (close) {
      await this.feed({ do: 'text', text: close });
      await this.settle('reply streams');
    }
  }

  chunks(piece) {
    const cuts = [];
    for (let at = 1; at < piece.length; at++) {
      if (!(/\d/.test(piece[at]) && /\d/.test(piece[at - 1]))) cuts.push(at);
    }
    const at = [...new Set(Array.from({ length: this.rng.below(3) }, () => cuts[this.rng.below(cuts.length)]).filter((cut) => cut !== undefined))].sort((a, b) => a - b);
    const out = [];
    let from = 0;
    for (const cut of at) { out.push(piece.slice(from, cut)); from = cut; }
    out.push(piece.slice(from));
    return out;
  }

  // ----- panels, the message box and the window -----

  async panel(kind) {
    const pick = kind ?? this.rng.below(6);
    if (pick === 0) {
      await this.trip('approval tray', async () => {
        const { id } = await this.feed({ do: 'approval', name: this.name('p') });
        this.approval = id;
        await this.settle('approval tray opens', { viewport: true });
      }, async () => {
        await this.feed({ do: 'approved', id: this.approval });
        await this.settle('approval tray closes', { viewport: true });
      });
    } else if (pick === 1) {
      const id = `q${++this.word}`;
      await this.feed({ do: 'ask', id, text: `${this.some(3, 5)}?`, waits: false });
      await this.settle('question comes', { viewport: true });
      await this.trip('answer panel', async () => {
        this.begin('answer panel opens', { viewport: true });
        await this.during(() => this.cdp.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'q', code: 'KeyQ', windowsVirtualKeyCode: 81, modifiers: 1 }, this.session));
        await this.during(() => this.cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'q', code: 'KeyQ', windowsVirtualKeyCode: 81, modifiers: 1 }, this.session));
        await this.settle();
      }, async () => {
        // (Esc closes the panel, where it opened, and otherwise follows,
        // which is the reader's own move.)
        const open = await this.evaluate(`!!document.querySelector('section.answers')`);
        this.begin(open ? 'answer panel closes' : null, { viewport: true });
        await this.key('Escape', 'Escape');
        await this.settle();
      });
      await this.feed({ do: 'answered', id, text: this.words(2) });
      await this.settle('question answered', { viewport: true });
    } else if (pick === 2) {
      await this.focusBox();
      await this.trip('slash menu', async () => {
        this.begin('slash menu opens', { viewport: true });
        await this.type('/');
        await this.settle();
      }, async () => {
        this.begin('slash menu closes', { viewport: true });
        await this.key('Backspace', 'Backspace');
        await this.settle();
      });
    } else if (pick === 3) {
      await this.focusBox();
      await this.trip('message box', async () => {
        this.begin('message box grows', { viewport: true });
        for (let line = 0; line < 3; line++) {
          await this.type(`${this.words(2)}\n`);
        }
        await this.settle();
      }, async () => {
        await this.evaluate(`(() => { const area = document.querySelector('.field textarea'); area.select(); })()`);
        this.begin('message box shrinks', { viewport: true });
        await this.key('Backspace', 'Backspace');
        await this.settle();
      });
    } else if (pick === 4) {
      // (Not below 510 px, where the page takes a landscape phone's
      // smaller chrome, which is another look.)
      const by = Math.max(0, Math.min(Math.round(this.height * 0.15), this.height - 510));
      await this.trip('window', async () => {
        await this.metrics(this.width, this.height - by);
        await this.settle('window shorter', { viewport: true });
      }, async () => {
        await this.metrics(this.width, this.height + by);
        await this.settle('window taller', { viewport: true });
      });
    } else {
      const id = `r${++this.workers}`;
      await this.feed({ do: 'worker', id, text: `Task ${this.words(4)}` });
      await this.settle('worker starts');
      await this.feed({ do: 'worker', id, change: 'finished', text: this.words(6) });
      await this.settle('worker ends');
    }
  }

  /// The reader goes through the other views and back, as Ctrl+O three
  /// times does in the terminal.
  async views() {
    const at = VIEWS.indexOf(this.view);
    const order = [...VIEWS.slice(at + 1), ...VIEWS.slice(0, at)];
    const reading = !this.last?.following;
    const blank = this.last ? this.last.bottom - this.last.end : 0;
    const go = async () => { for (const view of order) await this.pickView(view); };
    const back = async () => { await this.pickView(this.view); };
    if (reading || blank <= 0) await this.trip('views', go, back, { any: true });
    else { await go(); await back(); }
  }

  async pickView(view) {
    const index = VIEWS.indexOf(view);
    const menu = await this.evaluate(`(() => {
      const radio = document.querySelectorAll('.views button')[${index}];
      if (radio && radio.offsetParent) { radio.click(); return false; }
      document.querySelector('.view-pick button').click();
      return true;
    })()`);
    await this.settle(null);
    if (menu) {
      await this.evaluate(`document.querySelectorAll('.view-pick .menu button')[${index}].click()`);
      await this.settle(null);
    }
  }

  /// A worker starts and works, the reader opens its view and comes back,
  /// and the main view is as they left it.
  async worker() {
    const id = `r${++this.workers}`;
    await this.feed({ do: 'worker', id, text: `Task ${this.words(4)}` });
    await this.settle('worker starts');
    for (let at = 0; at < 3; at++) {
      await this.feed({ do: 'text', worker: id, text: `${this.some(4, 8)} ` });
    }
    await this.settle('worker starts');
    const card = await this.evaluate(`(() => {
      const cards = [...document.querySelectorAll('#rows .wcard')];
      const log = document.getElementById('log').getBoundingClientRect();
      const card = cards.reverse().find((card) => { const r = card.getBoundingClientRect(); return r.top >= log.top && r.bottom <= log.bottom; });
      if (!card) return null;
      const r = card.getBoundingClientRect();
      return [r.left + 20, r.top + r.height / 2];
    })()`);
    if (card) {
      await this.trip('worker view and back', async () => {
        await this.click(card[0], card[1]);
        await this.settle(null);
        await this.feed({ do: 'text', worker: id, text: `${this.some(4, 8)} ` });
        await this.settle(null);
      }, async () => {
        await this.key('Escape', 'Escape');
        await this.settle(null);
      }, { any: true });
    }
    await this.feed({ do: 'worker', id, change: 'finished', text: this.words(6) });
    await this.settle('worker ends');
  }

  /// The window gets narrower and wide again, which wraps the text again.
  async rewrap() {
    const by = Math.round(this.width * 0.2);
    await this.metrics(this.width - by, this.height);
    await this.settle(null);
    await this.metrics(this.width + by, this.height);
    await this.settle(null);
  }

  /// The reader sends a message, which goes to the end.
  async send() {
    this.jumping = true;
    await this.focusBox();
    await this.type(`Then ${this.words(3 + this.rng.below(8))}`);
    const before = await this.events();
    // (Ctrl+Enter sends on any device, as Enter makes a new line without a
    // fine pointer.)
    await this.key('Enter', 'Enter', '\r', 2);
    await this.delivered(before, 1500);
    await this.settle(null);
    this.jumped();
  }

  async focusBox() {
    // (As a tap or a click puts the focus there, without scrolling.)
    await this.evaluate(`(() => { const area = document.querySelector('.field textarea'); area && area.focus({ preventScroll: true }); })()`);
  }

  // ----- a turn -----

  async turn(at) {
    this.folder = `src/${this.name('d')}`;
    this.calls = [];
    if (this.rng.chance(40)) {
      // The reader sends it, which goes to the end, and scrolls up again.
      await this.send();
      await this.scrollUp();
      await this.feed({ do: 'started' });
    } else {
      await this.scrollUp();
      await this.feed({ do: 'user', text: `Please ${this.words(3 + this.rng.below(12))}` });
    }
    this.live = 'working';
    await this.settle('message → Working…');
    await this.meanwhile();
    // Each turn, one of the panels, and now and then the views, a worker's
    // view or another width.
    await this.panel((at + this.turnOffset) % 6);
    if (at === 0) await this.worker();
    if (at === 1) await this.views();
    if (at === 1) await this.rewrap();
    for (let part = 1 + this.rng.below(3); part > 0; part--) {
      if (this.rng.chance(60)) await this.think();
      for (let n = this.rng.below(6); n > 0; n--) {
        if (this.calls.length && this.rng.chance(40)) await this.think();
        await this.call();
      }
      if (this.rng.chance(30)) await this.readWaiting();
      if (this.rng.chance(20)) await this.think();
      await this.reply();
    }
    await this.readWaiting();
    await this.feed({ do: 'end' });
    await this.settle('turn ends');
    if (this.opened) await this.toggle();
    // A reader who scrolled up comes back to the end now and then.
    if (this.scrolled && at % 2 === 1) await this.toEnd();
  }
}

function sameLines(a, b) {
  if (a.ids.size !== b.ids.size) return false;
  for (const [id, { y }] of a.ids) if (b.ids.get(id)?.y !== y) return false;
  return true;
}

function describe(run, before, after, line, px) {
  const head = `${run.size.name}, ${run.view}: ${line.id} went from ${line.sideways ? 'column' : 'row'} ${line.from} to ${line.to} (device px, ${px} px), scrollTop ${before.scrollTop} → ${after.scrollTop}, range ${before.scrollHeight}−${before.clientHeight} → ${after.scrollHeight}−${after.clientHeight}, the end at ${before.end} → ${after.end} and the bottom edge at ${before.bottom} → ${after.bottom}, marker "${before.marker}" → "${after.marker}"\n`;
  const show = ([y, words, x, key]) => `${String(y).padStart(5)} ${String(x).padStart(4)} ${key.slice(0, 6).padEnd(6)} ${words.map(([word]) => word).join(' ').slice(0, 50)}`;
  const rows = [...before.lines].sort((a, b) => a[0] - b[0]).map(show);
  const now = [...after.lines].sort((a, b) => a[0] - b[0]).map(show);
  let out = head;
  for (let at = 0; at < Math.max(rows.length, now.length); at++) {
    out += `${(rows[at] ?? '').padEnd(68)} │ ${now[at] ?? ''}\n`;
  }
  return out;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ----- the pixel cases -----

// The cases of FOLLOWING.md's section 10, each from a session of its own in
// a 1000×700 window, at pixel ratios 1, 1.25, 1.5 and 2. Deltas are in
// device pixels, and the inputs that wait carry their own times, so nothing
// sleeps.
const RATIOS = [1, 1.25, 1.5, 2];

/// The gap under the transcript's last item, to the composer's divider,
/// against the gap between two items of its kind (CSS px), em box to em box
/// for a row of text and frame to frame for a framed one. A kind's gap is
/// the table's step between two rows of its kind (an open row's next is an
/// open row) and its half-leading on both sides. `pair` is the gap of the
/// last two rows of its kind that stand together on the screen, if any.
const LAST_GAP = `(() => {
  const table = ${TABLE};
  const rows = document.getElementById('rows');
  const bottom = document.querySelector('.bottom');
  if (!rows || !bottom) return null;
  const divider = bottom.getBoundingClientRect().top;
  const shown = [...rows.children].filter((row) => row.getBoundingClientRect().height > 0);
  const last = shown[shown.length - 1];
  if (!last) return null;
  const kindOf = (row) => [...row.classList].filter((name) => name !== 'row' && name !== 'still' && name !== 'muted').sort().join(' ') + (table.isOpen(row) ? ' (open)' : '');
  const metrics = (row) => {
    const style = getComputedStyle(row);
    const number = (value) => parseFloat(value) || 0;
    const framed = table.framed(row);
    return {
      lead: table.leadOf(row),
      padTop: framed ? 0 : number(style.paddingTop) + number(style.borderTopWidth),
      padBottom: framed ? 0 : number(style.paddingBottom) + number(style.borderBottomWidth),
    };
  };
  const emTop = (row) => row.getBoundingClientRect().top + metrics(row).padTop + metrics(row).lead;
  const emBottom = (row) => row.getBoundingClientRect().bottom - metrics(row).padBottom - metrics(row).lead;
  const kind = kindOf(last);
  const own = table.kindOf(last);
  let pair = null;
  for (let at = shown.length - 2; at > 0 && pair === null; at--) {
    const [a, b] = [shown[at - 1], shown[at]];
    if (kindOf(a) === kind && kindOf(b) === kind) pair = emTop(b) - emBottom(a);
  }
  const standard = table.PX[table.step(own, table.nextOf(own))] + 2 * metrics(last).lead;
  return { kind, under: divider - emBottom(last), standard, pair, dpr: devicePixelRatio };
})()`;

/// A command whose line is longer than a phone's, as a push and a look at
/// its CI run make.
const LONG_COMMAND = 'cd /home/ada/proj/wt/release && git push -u origin release-1.4 2>&1 | tail -3; sleep 8; gh run list --repo example/proj --branch release-1.4 --limit 3 --json databaseId,headSha,status,conclusion,event';

class Case extends Run {
  constructor(options) {
    super(options);
    this.base = Date.now() / 1000;
    this.at = 0;
  }

  /// An input's time, `ms` after the case began.
  time(ms) {
    this.at = Math.max(this.at, ms);
    return this.base + ms / 1000;
  }

  async look() {
    const shot = await this.shot();
    this.last = shot;
    return shot;
  }

  /// A wheel event of `delta` device pixels (up is less than 0) at `ms`.
  async roll(delta, ms) {
    const box = await this.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    await this.during(() => this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseWheel', x: box[0], y: box[1], deltaX: 0, deltaY: delta, timestamp: this.time(ms),
    }, this.session));
    await this.settle(null);
    return this.look();
  }

  async press(key, code, ms, keyCode) {
    for (const type of ['rawKeyDown', 'keyUp']) {
      await this.during(() => this.cdp.send('Input.dispatchKeyEvent', { type, key, code, windowsVirtualKeyCode: keyCode, timestamp: this.time(ms) }, this.session));
    }
    await this.settle(null);
    return this.look();
  }

  async marker(ms) {
    const at = await this.evaluate(`(() => { const j = document.querySelector('.jump.shown'); if (!j) return null; const r = j.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    if (!at) return null;
    for (const type of ['mousePressed', 'mouseReleased']) {
      await this.during(() => this.cdp.send('Input.dispatchMouseEvent', { type, x: at[0], y: at[1], button: 'left', clickCount: 1, timestamp: this.time(ms) }, this.session));
    }
    await this.settle(null);
    return this.look();
  }

  /// Holds back the page's requests for the wholes of contents, as a slow
  /// connection would, until `letGo`.
  async holdWholes() {
    this.heldBack = [];
    await this.cdp.send('Fetch.enable', { patterns: [...this.patterns, { urlPattern: '*/api/result/*', requestStage: 'Request' }] }, this.session);
  }

  /// Lets the requests held back go, once one came, and holds no more.
  async letGo() {
    for (let at = 0; at < 200 && !this.heldBack.length; at++) await sleep(10);
    const held = this.heldBack;
    this.heldBack = null;
    for (const requestId of held) await this.cdp.send('Fetch.continueRequest', { requestId }, this.session);
    return held.length;
  }

  /// A transcript taller than the view, at its end and following.
  async fill(paragraphs = 24) {
    await this.feed({ do: 'user', text: `Please ${this.words(6)}` });
    for (let at = 0; at < paragraphs; at++) {
      await this.feed({ do: 'text', text: `${this.some(12, 8)}\n\n` });
    }
    await this.feed({ do: 'end' });
    await this.settle(null);
    return this.look();
  }

  /// Blank of `device` pixels below the end, made by an end that gets
  /// that much shorter while following.
  async blank(device) {
    // (The spacer isn't an item, so the last item keeps its gap, as a
    // closed call's content does.)
    await this.evaluate(`(() => { const rows = document.getElementById('rows'); const spacer = document.createElement('div'); spacer.id = 'case-spacer'; spacer.className = 'closed'; spacer.style.height = '${device / this.size.dpr}px'; rows.appendChild(spacer); })()`);
    await this.settle(null);
    this.grown = await this.look();
    await this.evaluate(`document.getElementById('case-spacer').remove()`);
    await this.settle(null);
    return this.look();
  }

  /// How far the lines moved from `before` to `after`, in device pixels
  /// (down is more), by the lines both show, or null where they differ.
  static moved(before, after) {
    let moved = null;
    for (const [id, line] of before.ids) {
      const now = after.ids.get(id);
      if (!now) continue;
      const by = Math.round(now.raw - line.raw);
      if (moved === null) moved = by;
      else if (moved !== by) return NaN;
    }
    return moved ?? 0;
  }
}

async function pixelCases(cdp, agent) {
  const failures = [];
  const skipped = [];
  let count = 0;
  const play = async (ratio, name, body, view = 'full') => {
    if (CASE && !CASE.test(name)) return;
    count++;
    const run = new Case({ cdp, agent, meter: new Meter(), size: { name: `cases @${ratio}`, width: 1000, height: 700, dpr: ratio, mobile: name.startsWith('touch') }, view, scrolled: false, seed: 7 });
    try {
      await run.open();
      const problem = await body(run, ratio);
      if (problem === 'skipped') skipped.push(`@${ratio} ${name}`);
      else if (problem) failures.push(`@${ratio} ${name}: ${problem}`);
    } catch (error) {
      failures.push(`@${ratio} ${name}: ${error.stack ?? error}`);
    }
    for (const failure of run.failures) failures.push(`@${ratio} ${name}: ${failure}`);
    await run.close();
  };
  // The blank guard's own case, on two frames made up: "Working…" goes from
  // under a row, and that row has the end's spacing the rule gives it, which
  // passes; or a device pixel less, which must fail; or the row moved.
  count++;
  {
    const before = { tail: [['i1', 0, 300], ['working', 312, 372]], tailEnd: { actual: 22.5, rule: 22.5 } };
    const right = { tail: [['i1', 0, 300]], tailEnd: { actual: 22.5, rule: 22.5 } };
    const short = { tail: [['i1', 0, 300]], tailEnd: { actual: 21.5, rule: 22.5 } };
    const moved = { tail: [['i1', 0, 299]], tailEnd: { actual: 22.5, rule: 22.5 } };
    if (!onlyWorkingWent(before, right)) failures.push('pixel case the blank guard: the line "Working…" took failed');
    if (onlyWorkingWent(before, short)) failures.push('pixel case the blank guard: a device pixel more blank passed');
    if (onlyWorkingWent(before, moved)) failures.push('pixel case the blank guard: a row that moved passed');
  }
  const follows = (shot) => shot.choice === 'follow' && shot.following;
  const blankOf = (shot) => shot.bottom - shot.end;
  // (Unrounded, so a change is rounded once, as a line's move is.)
  const rawBlank = (shot) => Math.max(0, shot.bottomRaw - shot.endRaw);
  // A page opened on an earlier page's address, as a case's is when the
  // system gives its session that page's port again, starts with nothing
  // the earlier page stored, and in the view it asks for.
  await play(1, 'a page on an earlier page\'s address starts clean', async (run) => {
    await run.evaluate(`localStorage.setItem('arko.view', 'messages'); localStorage.setItem('arko.draft', 'left over'); true`);
    const next = new Case({ cdp, agent, meter: new Meter(), size: run.size, view: 'full', scrolled: false, seed: 7 });
    await next.open({ url: run.url });
    try {
      const [view, draft] = await next.evaluate(`[localStorage.getItem('arko.view'), localStorage.getItem('arko.draft')]`);
      return view === 'full' && draft === null ? null : `it opened in the view ${view} with the draft ${JSON.stringify(draft)}`;
    } finally {
      await next.close();
    }
  }, 'quiet');
  for (const ratio of RATIOS) {
    await play(ratio, 'three scrolls up of 0.4 in one gesture', async (run) => {
      const a = await run.fill();
      const b = await run.roll(-0.4, 0);
      const c = await run.roll(-0.4, 16);
      if (Case.moved(a, c) !== 0 || !follows(c)) return `the first two moved ${Case.moved(a, c)} or stopped following`;
      const d = await run.roll(-0.4, 32);
      if (Case.moved(c, d) !== 1 || d.choice !== 'read') return `the third moved ${Case.moved(c, d)} and left the choice ${d.choice}`;
      return b && null;
    });
    await play(ratio, 'up 0.4 and down 0.4, repeated', async (run) => {
      const a = await run.fill();
      let shot = a;
      for (let at = 0; at < 10; at++) shot = await run.roll(at % 2 ? 0.4 : -0.4, at * 16);
      if (Case.moved(a, shot) !== 0 || !follows(shot)) return `moved ${Case.moved(a, shot)}, ${shot.choice}`;
      return null;
    });
    for (const back of [0.5, 1, 3]) {
      await play(ratio, `a flick down to the end, then ${back} the other way`, async (run) => {
        await run.fill();
        await run.roll(-300, 0);
        let shot;
        for (let at = 1; at <= 8; at++) shot = await run.roll(60, 200 + at * 16);
        if (!follows(shot)) return `the flick didn't follow (${shot.choice})`;
        const after = await run.roll(-back, 200 + 9 * 16);
        const moved = Case.moved(shot, after);
        const want = Math.floor(back);
        if (moved !== want) return `moved ${moved}, not ${want}`;
        if ((want > 0) !== (after.choice === 'read')) return `the choice is ${after.choice}`;
        return null;
      });
    }
    for (const past of [0.3, 300]) {
      await play(ratio, `a flick down that ends ${past} past the end`, async (run) => {
        const end = await run.fill();
        const up = await run.roll(-200, 0);
        const gap = up.end - up.bottom;
        await run.roll(gap + past, 200);
        let shot = await run.look();
        if (!follows(shot) || Math.abs(shot.end - shot.bottom) > 1) return `not at the end (${shot.end} against ${shot.bottom}), ${shot.choice}`;
        const before = shot;
        for (let at = 1; at <= 10; at++) shot = await run.roll(40, 200 + at * 16);
        if (Case.moved(before, shot) !== 0 || !follows(shot)) return `the coast moved ${Case.moved(before, shot)}`;
        return end && null;
      });
    }
    await play(ratio, 'a scroll down as far as it goes follows', async (run) => {
      await run.fill();
      await run.roll(-500, 0);
      const shot = await run.roll(5000, 400);
      return follows(shot) && !shot.marker ? null : `${shot.choice}, marker "${shot.marker}"`;
    });
    await play(ratio, 'with 40 of blank, up 10 reads with 30, and down 0.5 follows with 30', async (run) => {
      await run.fill();
      const a = await run.blank(40);
      if (blankOf(a) !== 40 || !follows(a)) return `blank ${blankOf(a)}, ${a.choice}`;
      const b = await run.roll(-10, 0);
      if (b.choice !== 'read' || blankOf(b) !== 30) return `after up 10: ${b.choice}, blank ${blankOf(b)}`;
      const c = await run.roll(0.5, 300);
      if (!follows(c) || blankOf(c) !== 30) return `after down 0.5: ${c.choice}, blank ${blankOf(c)}`;
      return null;
    });
    await play(ratio, 'the end gets 25 shorter while following', async (run) => {
      await run.fill();
      const b = await run.blank(25);
      const a = run.grown;
      if (Case.moved(a, b) !== 0) return `the lines moved ${Case.moved(a, b)}`;
      if (b.scrollHeight !== a.scrollHeight) return `the range went ${a.scrollHeight} → ${b.scrollHeight}`;
      return null;
    });
    await play(ratio, 'a flick up, the marker during its coast, and 30 more coast events', async (run) => {
      await run.fill();
      for (let at = 0; at < 8; at++) await run.roll(-120, at * 16);
      const marker = await run.marker(140);
      if (!marker) return 'no marker';
      let shot = marker;
      for (let at = 1; at <= 30; at++) shot = await run.roll(-60, 140 + at * 16);
      return follows(shot) && Case.moved(marker, shot) === 0 ? null : `${shot.choice}, moved ${Case.moved(marker, shot)}`;
    });
    await play(ratio, 'Home during a coast down stays at the start', async (run) => {
      await run.fill();
      await run.roll(-200, 0);
      for (let at = 1; at <= 4; at++) await run.roll(60, 200 + at * 16);
      await run.evaluate('document.activeElement && document.activeElement.blur()');
      await run.press('Home', 'Home', 280, 36);
      let shot;
      for (let at = 1; at <= 10; at++) shot = await run.roll(60, 280 + at * 16);
      return shot.scrollTop === 0 ? null : `at ${shot.scrollTop}`;
    });
    await play(ratio, 'a wheel event up 150 ms after the marker reads', async (run) => {
      await run.fill();
      await run.roll(-600, 0);
      const marker = await run.marker(100);
      if (!marker) return 'no marker';
      const shot = await run.roll(-30, 250);
      return shot.choice === 'read' && Case.moved(marker, shot) > 0 ? null : `${shot.choice}, moved ${Case.moved(marker, shot)}`;
    });
    await play(ratio, 'a flick down, End during its coast, and a wheel event up 20 ms later', async (run) => {
      await run.fill();
      await run.roll(-600, 0);
      for (let at = 1; at <= 3; at++) await run.roll(60, 300 + at * 16);
      await run.evaluate('document.activeElement && document.activeElement.blur()');
      const end = await run.press('End', 'End', 360, 35);
      const shot = await run.roll(-30, 380);
      return shot.choice === 'read' && Case.moved(end, shot) > 0 ? null : `${shot.choice}, moved ${Case.moved(end, shot)}`;
    });
    await play(ratio, 'PageUp leaves the top line two lines above the bottom edge', async (run) => {
      const a = await run.fill(60);
      await run.evaluate('document.activeElement && document.activeElement.blur()');
      const b = await run.press('PageUp', 'PageUp', 0, 33);
      // (The first line whole on the screen moves a screen less two lines.)
      const top = [...a.ids.entries()].filter(([, line]) => line.y >= a.top).sort((x, y) => x[1].y - y[1].y)[0];
      const now = b.ids.get(top[0]);
      if (!now) return 'the top line went';
      const line = await run.evaluate('__meter.lineHeight()');
      const want = Math.round(b.clientHeight * ratio - 2 * line);
      const moved = Math.round(now.raw - top[1].raw);
      return Math.abs(moved - want) <= 1 ? null : `it moved ${moved}, not ${want}`;
    });
    await play(ratio, 'six deltas in one frame and in six frames', async (run) => {
      await run.fill();
      const box = await run.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
      const sent = [];
      for (let at = 0; at < 6; at++) {
        sent.push(run.cdp.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: box[0], y: box[1], deltaX: 0, deltaY: -50, timestamp: run.time(at) }, run.session));
      }
      for (let at = 0; at < 8; at++) await run.frame();
      await Promise.all(sent);
      const one = await run.look();
      await run.roll(5000, 500);
      for (let at = 0; at < 6; at++) await run.roll(-50, 1000 + at * 16);
      const six = await run.look();
      return one.scrollTop === six.scrollTop && one.choice === six.choice ? null : `${one.scrollTop} ${one.choice} against ${six.scrollTop} ${six.choice}`;
    });
    await play(ratio, 'a scroll down to the old end in the frame a reply grows', async (run) => {
      await run.fill();
      const up = await run.roll(-100, 0);
      const gap = up.end - up.bottom;
      await run.feed({ do: 'user', text: `Then ${run.words(4)}` });
      await run.settle(null);
      const a = await run.look();
      const gapNow = a.end - a.bottom;
      const box = await run.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
      // (The reply's growth and the wheel come before the same frame.)
      await run.agent.ask({ do: 'text', text: `${run.some(30, 4)}\n\n` });
      await sleep(30);
      const wheel = run.cdp.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: box[0], y: box[1], deltaX: 0, deltaY: gapNow, timestamp: run.time(400) }, run.session);
      for (let at = 0; at < 6; at++) await run.frame();
      await wheel;
      const b = await run.look();
      const moved = -Case.moved(a, b);
      if (moved !== Math.round(gapNow)) return `moved up ${moved}, not the delta ${Math.round(gapNow)} (gap was ${gap})`;
      return b.choice === 'read' && b.end > b.bottom ? null : `${b.choice} with the end at ${b.end} against ${b.bottom}`;
    });
    await play(ratio, 'a row above the screen grows by 37.5 while reading', async (run) => {
      await run.fill();
      const a = await run.roll(-300, 0);
      await run.evaluate(`(() => { const row = document.querySelector('#rows > *'); row.style.paddingTop = '37.5px'; })()`);
      await run.settle(null);
      const b = await run.look();
      return Case.moved(a, b) === 0 ? null : `the lines moved ${Case.moved(a, b)}`;
    });
    for (const where of ['following', 'following with blank', 'reading']) {
      await play(ratio, `a fold clicked ${where} keeps the clicked line's row`, async (run) => {
        await run.feed({ do: 'user', text: `Please ${run.words(6)}` });
        for (let at = 0; at < 12; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
        // (Reads, closed in Full, so the click opens one under closed ones.)
        for (let at = 0; at < 3; at++) {
          const { call } = await run.feed({ do: 'call', tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
          await run.feed({ do: 'result', call, tool: 'read_file', output: `1\t${run.some(3, 3)}\n2\t${run.some(3, 3)}` });
        }
        await run.feed({ do: 'end' });
        await run.settle(null);
        if (where === 'following with blank') await run.blank(30);
        if (where === 'reading') await run.roll(-20, 0);
        const fold = await run.evaluate(`(() => {
          const log = document.getElementById('log').getBoundingClientRect();
          const all = [...document.querySelectorAll('#rows summary')].filter((s) => { const r = s.getBoundingClientRect(); return r.height > 0 && r.top >= log.top && r.bottom <= log.bottom; });
          const s = all[all.length - 1];
          if (!s) return null;
          const r = s.getBoundingClientRect();
          return { x: r.left + 30, y: r.top + r.height / 2, top: r.top * devicePixelRatio };
        })()`);
        if (!fold) return 'no fold on the screen';
        const before = await run.look();
        await run.click(fold.x, fold.y);
        await run.settle(null);
        const after = await run.look();
        const top = await run.evaluate(`(() => {
          const s = document.elementFromPoint(${fold.x}, ${fold.y})?.closest('summary');
          return s ? s.getBoundingClientRect().top * devicePixelRatio : null;
        })()`);
        if (top === null || Math.round(top - fold.top) !== 0) return `the line moved ${top === null ? 'away' : Math.round(top - fold.top)}`;
        for (const [id, line] of before.ids) {
          const now = after.ids.get(id);
          if (now && line.y < fold.top && Math.round(now.raw - line.raw) !== 0) return `"${line.text}" above it moved ${Math.round(now.raw - line.raw)}`;
        }
        return after.choice === before.choice ? null : `the choice went ${before.choice} → ${after.choice}`;
      });
    }
    // A command's output at the end, its first lines and the door to the
    // rest, which the page asks Arko for. Its whole comes after the click's
    // frame or before it, and either way the door opens below the content's
    // first lines and moves no line above it. Closing it again keeps those
    // lines where they show, and the door under the pointer where none of
    // them does (a long content closed from its end).
    for (const [order, long] of [['after', false], ['before', false], ['after', true]]) {
      const name = long
        ? 'a long door opened at the end while following and closed from its end keeps its lines, then the door, in place'
        : `a door opened at the end while following keeps the lines above it, its whole coming ${order} the click's frame`;
      await play(ratio, name, async (run) => {
        await run.feed({ do: 'user', text: `Please ${run.words(6)}` });
        for (let at = 0; at < 12; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `cargo build -p ${run.words(1)}` } });
        const output = Array.from({ length: long ? 90 : 30 }, () => `${run.some(2, 4)} ok`).join('\n');
        await run.feed({ do: 'result', call, tool: 'shell', output: `exit code 0, 41.0 s\n${output}` });
        await run.feed({ do: 'end' });
        await run.settle(null);
        const door = `(() => {
          const log = document.getElementById('log');
          const box = log.getBoundingClientRect();
          const door = [...document.querySelectorAll('#rows button.door')].pop();
          if (!door) return null;
          const r = door.getBoundingClientRect();
          const held = door.previousElementSibling;
          // (Its lines as the page draws them, one .line each, or as text
          // where it draws none.)
          const drawn = held.querySelectorAll('.line').length;
          const lines = drawn || (held.matches('pre') ? held.textContent.split('\\n').length : 0);
          return { x: r.left + 10, y: r.top + r.height / 2, top: r.top * devicePixelRatio, label: door.textContent, lines, below: r.bottom - (box.top + log.clientHeight), first: held.getBoundingClientRect().top - box.top };
        })()`;
        const shut = await run.evaluate(door);
        if (!shut || shut.below > 0) return `no door on the screen (${JSON.stringify(shut)})`;
        const before = await run.look();
        if (!follows(before)) return `not following before the click (${before.choice})`;
        await run.holdWholes();
        if (order === 'after') {
          await run.click(shut.x, shut.y);
          await run.settle(null);
          if (!(await run.letGo())) return 'the click asked for no whole';
        } else {
          // (A click and its answer with no frame between, as a browser that
          // draws less often may have them.)
          await run.evaluate(`[...document.querySelectorAll('#rows button.door')].pop().click()`);
          if (!(await run.letGo())) return 'the click asked for no whole';
          for (let at = 0; at < 200 && !(await run.evaluate(door)).label.startsWith('↑'); at++) await sleep(10);
        }
        await run.settle(null);
        const open = await run.evaluate(door);
        if (!open.label.startsWith('↑') || open.lines <= shut.lines) return `the door didn't open ("${open.label}", ${shut.lines} → ${open.lines} lines)`;
        const opened = await run.look();
        // (The lines of `a` from device pixel row `from` to `to` that `b`
        // shows too.)
        const still = (a, b, from, to) => {
          for (const [id, line] of a.ids) {
            const now = b.ids.get(id);
            if (now && line.y >= from && line.y < to && Math.round(now.raw - line.raw) !== 0) return `"${line.text}" moved ${Math.round(now.raw - line.raw)}`;
          }
          return null;
        };
        const moved = still(before, opened, -Infinity, shut.top);
        if (moved) return `opening: ${moved}`;
        // The reader scrolls to the door below and closes it.
        if (open.below > -20) await run.roll((open.below + 20) * ratio, 400);
        const reached = await run.evaluate(door);
        const ready = await run.look();
        if (long && reached.first + shut.lines * 18 > 0) return `the first lines show at the door (${reached.first})`;
        await run.click(reached.x, reached.y);
        await run.settle(null);
        const closed = await run.look();
        const again = await run.evaluate(door);
        if (again.label !== shut.label || again.lines !== shut.lines) return `the door didn't close ("${again.label}", ${again.lines} lines)`;
        if (long) {
          if (Math.round(again.top - reached.top) !== 0) return `closing moved the door ${Math.round(again.top - reached.top)}`;
          const below = still(ready, closed, reached.top, Infinity);
          return below ? `closing: ${below}` : null;
        }
        const back = still(ready, closed, -Infinity, reached.top);
        return back ? `closing: ${back}` : null;
      });
    }
    // A follower opens a fold that takes the end off the screen, and goes
    // through the other views and back. In a view where the end shows, the
    // frame follows, and the way back still comes to the block they left.
    const offTheEnd = async (run) => {
      await run.feed({ do: 'user', text: `Please ${run.words(6)}` });
      for (let at = 0; at < 24; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
      for (let at = 0; at < 3; at++) {
        const { call } = await run.feed({ do: 'call', tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
        await run.feed({ do: 'result', call, tool: 'read_file', output: Array.from({ length: 6 }, (_, n) => `${n + 1}\t${run.some(3, 3)}`).join('\n') });
      }
      await run.feed({ do: 'end' });
      await run.settle(null);
      const fold = await run.evaluate(`(() => {
        const log = document.getElementById('log').getBoundingClientRect();
        const all = [...document.querySelectorAll('#rows summary')].filter((s) => { const r = s.getBoundingClientRect(); return r.height > 0 && r.top >= log.top && r.bottom <= log.bottom; });
        const s = all[all.length - 1];
        if (!s) return null;
        const r = s.getBoundingClientRect();
        return { x: r.left + 30, y: r.top + r.height / 2 };
      })()`);
      if (!fold) return 'no fold on the screen';
      await run.click(fold.x, fold.y);
      await run.settle(null);
      const shot = await run.look();
      if (shot.choice !== 'follow' || shot.end <= shot.bottom) return `the fold left the end on the screen (${shot.choice})`;
      return shot;
    };
    // (Through the other views and back to `view`.)
    const views = async (run, view) => {
      const at = VIEWS.indexOf(view);
      for (const other of [...VIEWS.slice(at + 1), ...VIEWS.slice(0, at)]) await run.pickView(other);
      await run.pickView(view);
      return run.look();
    };
    const sameLines = (before, after) => {
      for (const [id, line] of before.ids) {
        const now = after.ids.get(id);
        if (!now) return `"${line.text}" didn't come back`;
        if (Math.round(now.raw - line.raw) !== 0) return `"${line.text}" came back ${Math.round(now.raw - line.raw)} from its row`;
      }
      return null;
    };
    for (const view of ['full', 'quiet']) {
      await play(ratio, `the views and back put every line back after a fold took a follower's end off the screen (${view})`, async (run) => {
        const before = await offTheEnd(run);
        if (typeof before === 'string') return before;
        return sameLines(before, await views(run, view));
      }, view);
    }
    // Choosing the end in another view, with a scroll down of less than a
    // device pixel, lets go of the block the follower left.
    await play(ratio, 'a scroll down of less than a device pixel at the end in another view follows on the way back', async (run) => {
      const before = await offTheEnd(run);
      if (typeof before === 'string') return before;
      await run.pickView('quiet');
      await run.pickView('messages');
      const there = await run.look();
      if (!follows(there)) return `the end didn't show in Messages (${there.choice}, following ${there.following})`;
      await run.roll(0.4, 0);
      await run.pickView('full');
      const back = await run.look();
      return follows(back) && Math.abs(back.end - back.bottom) <= 1 ? null : `back in Full: ${back.choice}, following ${back.following}, the end at ${back.end} against ${back.bottom}`;
    });
    // A reader goes through the views and back from the end of a long
    // content they opened, closes it from there, which keeps its door in
    // place, and goes through the views and back again. The way back comes
    // to where it closed, not to the block they left the first time.
    await play(ratio, 'a long door closed from its end between two trips through the views keeps the screen', async (run) => {
      await run.feed({ do: 'user', text: `Please ${run.words(6)}` });
      for (let at = 0; at < 4; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
      const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `cargo build -p ${run.words(1)}` } });
      const output = Array.from({ length: 90 }, () => `${run.some(2, 4)} ok`).join('\n');
      await run.feed({ do: 'result', call, tool: 'shell', output: `exit code 0, 41.0 s\n${output}` });
      for (let at = 0; at < 16; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
      await run.feed({ do: 'end' });
      await run.settle(null);
      const door = `(() => {
        const log = document.getElementById('log');
        const box = log.getBoundingClientRect();
        const door = document.querySelector('#rows button.door');
        if (!door) return null;
        const r = door.getBoundingClientRect();
        return { x: r.left + 10, y: r.top + r.height / 2, top: r.top * devicePixelRatio, label: door.textContent, middle: box.top + log.clientHeight / 2, first: door.previousElementSibling.getBoundingClientRect().top - box.top };
      })()`;
      const shut = await run.evaluate(door);
      if (!shut) return 'no door';
      await run.roll((shut.y - shut.middle) * ratio, 0);
      const at = await run.evaluate(door);
      await run.click(at.x, at.y);
      for (let wait = 0; wait < 200 && !(await run.evaluate(door)).label.startsWith('↑'); wait++) await sleep(10);
      await run.settle(null);
      const open = await run.evaluate(door);
      if (!open.label.startsWith('↑')) return `the door didn't open ("${open.label}")`;
      await run.roll((open.y - open.middle) * ratio, 400);
      const end = await run.evaluate(door);
      const before = await run.look();
      if (before.choice !== 'read' || end.first > -300) return `not reading at the end of the content (${before.choice}, its first lines at ${end.first})`;
      const first = sameLines(before, await views(run, 'full'));
      if (first) return `the first trip: ${first}`;
      await run.click(end.x, end.y);
      await run.settle(null);
      const closed = await run.look();
      const again = await run.evaluate(door);
      if (again.label.startsWith('↑') || Math.round(again.top - end.top) !== 0) return `closing: "${again.label}", the door moved ${Math.round(again.top - end.top)}`;
      const second = sameLines(closed, await views(run, 'full'));
      return second ? `the second trip: ${second}` : null;
    });
    await play(ratio, 'the thumb dragged up reads, and to the bottom follows', async (run) => {
      await run.fill();
      await run.evaluate(`document.getElementById('log').scrollBy({ top: -40, behavior: 'instant' })`);
      await run.settle(null);
      const up = await run.look();
      if (up.choice !== 'read') return `up: ${up.choice}`;
      await run.evaluate(`(() => { const l = document.getElementById('log'); l.scrollTo({ top: l.scrollHeight, behavior: 'instant' }); })()`);
      await run.settle(null);
      const down = await run.look();
      return follows(down) ? null : `down: ${down.choice}`;
    });
    await play(ratio, 'with 40 of blank the thumb sits at the bottom, and a drag up leaves the blank behind', async (run) => {
      await run.fill();
      const a = await run.blank(40);
      if (a.scrollTop + a.clientHeight < a.scrollHeight - 1) return `the thumb isn't at the bottom (${a.scrollTop} + ${a.clientHeight} of ${a.scrollHeight})`;
      await run.evaluate(`document.getElementById('log').scrollBy({ top: -20, behavior: 'instant' })`);
      await run.settle(null);
      const b = await run.look();
      if (b.choice !== 'read') return `${b.choice}`;
      return b.scrollTop + b.clientHeight >= b.scrollHeight - 1 ? null : `the range kept the blank that left (${b.scrollTop} + ${b.clientHeight} of ${b.scrollHeight})`;
    });
    await play(ratio, 'a selection dragged past the top edge reads', async (run) => {
      await run.fill();
      // (On a word of a line in the middle of the screen.)
      const shot = await run.look();
      const middle = [...shot.ids.values()].sort((x, y) => x.y - y.y)[Math.floor(shot.ids.size / 2)];
      const top = await run.evaluate(`document.getElementById('log').getBoundingClientRect().top`);
      const box = [middle.x / ratio + 4, middle.y / ratio + 6, top];
      await run.during(() => run.cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: box[0], y: box[1], button: 'left', clickCount: 1 }, run.session));
      await run.during(() => run.cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: box[0], y: box[2] - 30, button: 'left', buttons: 1 }, run.session));
      for (let at = 0; at < 30; at++) await run.frame();
      // (Chromium's headless shell, drawing frames on demand, doesn't
      // scroll a selection dragged past an edge, so there's nothing to
      // check there.)
      const moved = await run.evaluate(`document.getElementById('log').scrollTop`);
      if (!(await run.evaluate('String(getSelection()).length')) || moved === shot.scrollTop) return 'skipped';
      await run.during(() => run.cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: box[0], y: box[2] - 30, button: 'left', clickCount: 1 }, run.session));
      await run.settle(null);
      const after = await run.look();
      return after.choice === 'read' ? null : `${after.choice}`;
    });
    await play(ratio, 'the count stays with no output', async (run) => {
      await run.fill();
      await run.roll(-400, 0);
      await run.feed({ do: 'text', text: `${run.some(30, 4)}\n\n` });
      await run.settle(null);
      const a = await run.look();
      for (let at = 0; at < 10; at++) await run.frame();
      const b = await run.look();
      return a.marker && a.marker === b.marker ? null : `"${a.marker}" → "${b.marker}"`;
    });
    // A phone's keyboard takes the bottom of the view and gives it back.
    // (180 px keeps the view above 510 px, where the page takes a landscape
    // phone's smaller chrome.)
    await play(ratio, 'the keyboard comes and goes while reading, and nothing moves', async (run) => {
      await run.fill();
      const a = await run.roll(-200, 0);
      await run.metrics(run.width, run.height - 180);
      await run.settle(null);
      await run.metrics(run.width, run.height + 180);
      await run.settle(null);
      const b = await run.look();
      if (b.choice !== 'read') return `the choice is ${b.choice}`;
      return Case.moved(a, b) === 0 ? null : `the lines moved ${Case.moved(a, b)}`;
    });
    await play(ratio, 'the keyboard takes only blank while reading, a message is sent, and it goes', async (run) => {
      await run.fill();
      await run.blank(300 * ratio);
      await run.roll(-10, 0);
      await run.metrics(run.width, run.height - 180);
      await run.settle(null);
      await run.send();
      const sent = await run.look();
      if (!follows(sent) || Math.abs(blankOf(sent)) > 1) return `sent: ${sent.choice}, blank ${blankOf(sent)}`;
      await run.metrics(run.width, run.height + 180);
      await run.settle(null);
      const b = await run.look();
      if (!follows(b)) return `the choice is ${b.choice}`;
      return blankOf(b) === blankOf(sent) ? null : `blank ${blankOf(b)} below the end, ${blankOf(sent)} as it was sent`;
    });
    await play(ratio, 'following with blank, the keyboard comes, a message is sent, and it goes', async (run) => {
      await run.fill();
      const a = await run.blank(100 * ratio);
      if (!follows(a)) return `the choice is ${a.choice}`;
      await run.metrics(run.width, run.height - 180);
      await run.settle(null);
      await run.send();
      const sent = await run.look();
      if (!follows(sent) || Math.abs(blankOf(sent)) > 1) return `sent: ${sent.choice}, blank ${blankOf(sent)}`;
      await run.metrics(run.width, run.height + 180);
      await run.settle(null);
      const b = await run.look();
      if (!follows(b)) return `the choice is ${b.choice}`;
      return blankOf(b) === blankOf(sent) ? null : `blank ${blankOf(b)} below the end, ${blankOf(sent)} as it was sent`;
    });
    // The keyboard comes while reading, the reader opens a worker's view
    // and comes back, and sends. Another view starts what the window took
    // afresh, so the keyboard going afterwards leaves its height blank and
    // moves no line, and going in the worker's view leaves nothing owed.
    for (const away of [false, true]) {
      const where = away ? 'in the worker\'s view' : 'after sending';
      await play(ratio, `the keyboard comes while reading, a worker's view and back, and it goes ${where}`, async (run) => {
        await run.fill();
        await run.feed({ do: 'worker', id: 'r1', text: `Task ${run.words(4)}` });
        for (let at = 0; at < 6; at++) await run.feed({ do: 'text', text: `${run.some(12, 8)}\n\n` });
        await run.settle(null);
        await run.roll(-40, 0);
        await run.metrics(run.width, run.height - 180);
        await run.settle(null);
        // (The card above the end, in sight. Scrolled to where it isn't,
        // which keeps the reader reading.)
        const where = `(() => {
          const log = document.getElementById('log').getBoundingClientRect();
          const card = [...document.querySelectorAll('#rows .wcard')].pop();
          if (!card) return null;
          const r = card.getBoundingClientRect();
          return { x: r.left + 20, y: r.top + r.height / 2, above: log.top + 20 - r.top, below: r.bottom + 20 - log.bottom };
        })()`;
        let card = await run.evaluate(where);
        if (!card) return 'no worker card';
        if (card.above > 0) await run.roll(-card.above * ratio, 200);
        else if (card.below > 0) await run.roll(card.below * ratio, 200);
        card = await run.evaluate(where);
        if (card.above > 0 || card.below > 0) return `the worker card is out of sight (${JSON.stringify(card)})`;
        if ((await run.look()).choice !== 'read') return 'the scroll to the card followed';
        await run.click(card.x, card.y);
        await run.settle(null);
        if (away) {
          await run.metrics(run.width, run.height + 180);
          await run.settle(null);
        }
        await run.key('Escape', 'Escape');
        await run.settle(null);
        const back = await run.look();
        if (back.choice !== 'read') return `back, the choice is ${back.choice}`;
        await run.send();
        const sent = await run.look();
        if (!follows(sent) || Math.abs(blankOf(sent)) > 1) return `sent: ${sent.choice}, blank ${blankOf(sent)}`;
        if (!away) {
          await run.metrics(run.width, run.height + 180);
          await run.settle(null);
        }
        const b = await run.look();
        if (!follows(b)) return `the choice is ${b.choice}`;
        if (Case.moved(sent, b) !== 0) return `the lines moved ${Case.moved(sent, b)}`;
        const want = blankOf(sent) + (away ? 0 : Math.round(180 * ratio));
        return Math.abs(blankOf(b) - want) <= 1 ? null : `blank ${blankOf(b)} below the end, not ${want}`;
      });
    }
    // A worker's view, with a call, opened and left again before the next
    // frame, so the rows it drew go before the frames they asked for. The
    // page stopping fails the case, as any error in its console does.
    for (const view of ['full', 'quiet']) {
      await play(ratio, `a worker's view opened and left before the next frame, ${view}`, async (run) => {
        await run.fill();
        await run.feed({ do: 'worker', id: 'r1', text: `Task ${run.words(4)}` });
        const { call } = await run.feed({ do: 'call', worker: 'r1', tool: 'shell', parameters: { command: `ls ${run.words(2)}` } });
        await run.feed({ do: 'result', worker: 'r1', call, tool: 'shell', output: 'exit code 0, 0.1 s' });
        await run.settle(null);
        const left = await run.evaluate(`(async () => {
          const card = [...document.querySelectorAll('#rows .wcard')].pop();
          if (!card) return false;
          card.click();
          await new Promise((resolve) => setTimeout(resolve, 0));
          window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
          return true;
        })()`);
        if (!left) return 'no worker card';
        await run.settle(null);
        return null;
      }, view);
    }
    await play(ratio, 'the keyboard takes only blank while reading, a scroll down follows, and it goes', async (run) => {
      await run.fill();
      await run.blank(300 * ratio);
      const a = await run.roll(-10, 0);
      if (a.choice !== 'read') return `the choice is ${a.choice}`;
      await run.metrics(run.width, run.height - 180);
      await run.settle(null);
      const b = await run.roll(0.5, 300);
      if (!follows(b)) return `the scroll down left ${b.choice}`;
      await run.metrics(run.width, run.height + 180);
      await run.settle(null);
      const c = await run.look();
      if (Case.moved(a, c) !== 0) return `the lines moved ${Case.moved(a, c)}`;
      return blankOf(c) === blankOf(a) ? null : `blank ${blankOf(c)}, not ${blankOf(a)}`;
    });
    for (const narrower of [0, 200]) {
      const width = narrower ? `, the window ${narrower} narrower meanwhile` : '';
      await play(ratio, `a message sent from above the end, then the keyboard goes, leaves no blank${width}`, async (run) => {
        await run.fill();
        await run.roll(-200, 0);
        await run.metrics(run.width, run.height - 180);
        await run.settle(null);
        if (narrower) {
          await run.metrics(run.width - narrower, run.height);
          await run.settle(null);
        }
        await run.send();
        const sent = await run.look();
        // (At a ratio of 1.5 the end may stand a device pixel above the
        // edge as sending puts it there.)
        if (!follows(sent) || Math.abs(blankOf(sent)) > 1) return `sent: ${sent.choice}, blank ${blankOf(sent)}`;
        await run.metrics(run.width, run.height + 180);
        await run.settle(null);
        const b = await run.look();
        if (!follows(b)) return `the choice is ${b.choice}`;
        return blankOf(b) === blankOf(sent) ? null : `blank ${blankOf(b)} below the end, ${blankOf(sent)} as it was sent`;
      });
    }
    for (const lines of [0, 1, 5]) {
      await play(ratio, `the message box grows 3 lines and shrinks with ${lines} lines of blank`, async (run) => {
        await run.fill();
        const line = await run.evaluate('__meter.lineHeight()');
        const a = lines ? await run.blank(lines * line) : await run.look();
        await run.focusBox();
        for (let at = 0; at < 3; at++) await run.type(`${run.words(2)}\n`);
        await run.settle(null);
        await run.evaluate(`document.querySelector('.field textarea').select()`);
        await run.key('Backspace', 'Backspace');
        await run.settle(null);
        const b = await run.look();
        const moved = Case.moved(a, b);
        return moved === 0 ? null : `the lines are ${moved} from their rows`;
      });
    }
    // A row whose words and spinner leave little room still shows its
    // button whole, on one line, inside the row, and its words give way,
    // down to 8 characters. A Quiet row of several calls says what runs
    // first, with nothing after it.
    for (const view of ['quiet', 'full']) {
      await play(ratio, `touch: a running row with Move to background fits a phone (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.settle(null);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        const dir = run.words(3).split(' ').join('/');
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `ls ${dir}` } });
        await run.feed({ do: 'result', call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        const path = run.words(4).split(' ').join('/');
        await run.feed({ do: 'call', tool: 'shell', parameters: { command: `sleep 40; tail -3 ../${path}/target/debug/build/output` } });
        await run.feed({ do: 'movable' });
        const measure = `(() => {
          const line = [...document.querySelectorAll('#rows .call-line')].find((line) => line.querySelector('.call-action'));
          if (!line) return null;
          const row = line.closest('.row').getBoundingClientRect();
          const button = line.querySelector('.call-action');
          const what = line.querySelector('.what');
          const right = Math.max(...[...line.querySelectorAll('*')].map((each) => each.getBoundingClientRect().right));
          const zero = document.createElement('span');
          zero.style.cssText = 'position: absolute; display: inline-block; white-space: nowrap; visibility: hidden';
          zero.style.font = getComputedStyle(what).font;
          zero.textContent = '00000000';
          document.body.appendChild(zero);
          const eight = zero.getBoundingClientRect().width;
          zero.remove();
          return {
            past: right - row.right,
            line: line.getBoundingClientRect().height,
            button: button.getBoundingClientRect().height,
            clipped: button.scrollWidth > button.clientWidth,
            what: what.getBoundingClientRect().width,
            cut: what.scrollWidth > what.clientWidth,
            eight,
            note: [...line.querySelectorAll('.outcome')].some((each) => each.textContent.includes('running')),
          };
        })()`;
        // (The status, which says the call may move, comes every 2 s.)
        let shot = null;
        for (let at = 0; at < 40 && !shot; at++) {
          await run.settle(null);
          shot = await run.evaluate(measure);
          if (!shot) await new Promise((done) => setTimeout(done, 100));
        }
        if (!shot) return 'no Move to background';
        if (view === 'quiet' && shot.note) return 'the row says what runs after its words, not first';
        if (shot.past > 0.5) return `the row runs ${shot.past} px past its right edge`;
        if (shot.clipped || shot.button > 24.5) return `the button wraps or is cut (${shot.button} px tall)`;
        if (shot.line > 20.5) return `the line is ${shot.line} px tall`;
        if (!shot.cut) return 'the words fit whole, so nothing gave way';
        if (shot.what < shot.eight - 1) return `the words are ${shot.what} px, under 8 characters (${shot.eight} px)`;
        return null;
      }, view);
    }
    // A long command at a phone's width. The live line is one line in each
    // view, a row keeps its height as its button comes and goes, and the
    // end keeps its place as the call ends (no blank comes below it).
    for (const view of ['quiet', 'full', 'messages']) {
      await play(ratio, `touch: a long command's live line is one line, and its end keeps its place (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(8);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        await run.feed({ do: 'text', text: `${run.some(8, 4)}.` });
        await run.settle(null);
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: LONG_COMMAND } });
        await run.settle('Working… → call');
        const live = `(() => {
          const working = document.querySelector('#rows > .working > .what');
          const line = [...document.querySelectorAll('#rows .call-line')].pop();
          return {
            working: working ? working.getBoundingClientRect().height : 0,
            row: line ? line.closest('.row').getBoundingClientRect().height : 0,
            button: !!(line && line.querySelector('.call-action')),
          };
        })()`;
        const before = await run.evaluate(live);
        await run.feed({ do: 'movable' });
        // (The status, which says the call may move, comes every 2 s.)
        let after = before;
        for (let at = 0; at < 40 && view !== 'messages' && !after.button; at++) {
          await run.settle(null);
          after = await run.evaluate(live);
          if (!after.button) await sleep(100);
        }
        // (Each check is made, and the call ends, before the first problem
        // is said, so the meter's own check of the blank sees the end too.)
        const problems = [];
        if (view !== 'messages' && !after.button) problems.push('no Move to background');
        if (Math.max(before.working, after.working) > 20.5) problems.push(`the live line is ${Math.max(before.working, after.working)} px tall`);
        if (Math.abs(after.row - before.row) > 0.5) problems.push(`the row went from ${before.row} to ${after.row} px as its button came`);
        const running = await run.look();
        await run.feed({ do: 'result', call, tool: 'shell', output: 'exit code 0, 9.0 s\nok' });
        await run.feed({ do: 'movable' });
        await run.settle('call → its result');
        const ended = await run.look();
        const grew = Math.round(rawBlank(ended) - rawBlank(running));
        if (grew > 0) problems.push(`${grew} device px of blank came below the end as the call ended`);
        return problems[0] ?? null;
      }, view);
    }
    // A fold above a running call opens, the call ends meanwhile, and the
    // fold closes. No line is lower than before, and no blank is left.
    for (const view of ['quiet', 'full']) {
      await play(ratio, `touch: a fold above a running call, which ends while it's open (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(8);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        const ls = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `ls ${run.words(2).split(' ').join('/')}` } });
        const listing = Array.from({ length: 4 }, () => run.words(2)).join('\n');
        await run.feed({ do: 'result', call: ls.call, tool: 'shell', output: `exit code 0, 0.1 s\n${listing}` });
        await run.feed({ do: 'text', text: `${run.some(8, 4)}.` });
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: LONG_COMMAND } });
        await run.feed({ do: 'movable' });
        for (let at = 0; at < 40; at++) {
          await run.settle(null);
          if (await run.evaluate(`!!document.querySelector('#rows .call-action')`)) break;
          await sleep(100);
        }
        const a = await run.look();
        if (!follows(a)) return `the case began ${a.choice}`;
        const fold = `(() => {
          const line = [...document.querySelectorAll('#rows .call-line')].find((line) => /\\bls\\b/.test(line.textContent) && !line.querySelector('.call-action'));
          if (!line) return null;
          const r = line.getBoundingClientRect();
          return [r.left + 30, r.top + r.height / 2];
        })()`;
        const at = await run.evaluate(fold);
        if (!at) return 'no fold above the running call';
        await run.click(at[0], at[1]);
        await run.settle(null);
        await run.feed({ do: 'result', call, tool: 'shell', output: 'exit code 0, 9.0 s\nok' });
        await run.feed({ do: 'movable' });
        await run.settle(null);
        const again = await run.evaluate(fold);
        if (!again) return 'the fold went';
        await run.click(again[0], again[1]);
        await run.settle(null);
        const c = await run.look();
        for (const [id, line] of a.ids) {
          const now = c.ids.get(id);
          if (now && Math.round(now.raw - line.raw) > 0) return `${id} is ${Math.round(now.raw - line.raw)} device px lower`;
        }
        const left = Math.round(rawBlank(c) - rawBlank(a));
        if (left > 0) return `${left} device px of blank left below the end`;
        return null;
      }, view);
    }
    // A command that runs stands on a row of its own after its group of
    // ended commands, and joins it as it ends, "Working…" taking its line:
    // no line moves down and no blank comes below the end, in either step.
    for (const view of ['quiet', 'full']) {
      await play(ratio, `touch: a call that runs on its own row joins its group as it ends (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(8);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        const ls = () => `ls ${run.words(2).split(' ').join('/')}`;
        for (let n = 0; n < 2; n++) {
          const ended = await run.feed({ do: 'call', tool: 'shell', parameters: { command: ls() } });
          await run.feed({ do: 'result', call: ended.call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        }
        await run.settle(null);
        const before = await run.look();
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: ls() } });
        await run.feed({ do: 'movable' });
        await run.settle('Working… → call');
        const running = await run.look();
        const rows = `[...document.querySelectorAll('#rows > .row')].filter((row) => row.getBoundingClientRect().height > 0).map((row) => row.className + ' ' + row.textContent.trim().replace(/\\s+/g, ' ').slice(0, 60))`;
        const runningRows = await run.evaluate(rows);
        if (view === 'quiet' && !/Running/.test(runningRows[runningRows.length - 1])) return `the running call isn't on the last row: ${runningRows.slice(-2).join(' | ')}`;
        if (view === 'quiet' && /Ran 2/.test(runningRows[runningRows.length - 1])) return 'the running call is in its group';
        await run.feed({ do: 'result', call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        await run.feed({ do: 'movable' });
        await run.settle('call → its result');
        const ended = await run.look();
        const endedRows = await run.evaluate(rows);
        if (view === 'quiet' && !endedRows.some((row) => /Ran 3 commands/.test(row))) return `it didn't join its group: ${endedRows.slice(-2).join(' | ')}`;
        for (const [from, to] of [[before, running], [running, ended]]) {
          for (const [id, line] of from.ids) {
            const now = to.ids.get(id);
            if (now && now.text === line.text && Math.round(now.raw - line.raw) > 0) return `${id} went ${Math.round(now.raw - line.raw)} device px down`;
          }
          const grew = Math.round(rawBlank(to) - rawBlank(from));
          if (grew > 0) return `${grew} device px of blank came below the end`;
        }
        return null;
      }, view);
    }
    // A command that runs, a row below it the reader opened, and the command
    // ends: nothing regroups or closes, and no line moves down.
    for (const view of ['quiet', 'full']) {
      await play(ratio, `touch: a call that runs ends while a row below it is open (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(8);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        const ls = () => `ls ${run.words(2).split(' ').join('/')}`;
        const ended = await run.feed({ do: 'call', tool: 'shell', parameters: { command: ls() } });
        await run.feed({ do: 'result', call: ended.call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: ls() } });
        const read = await run.feed({ do: 'call', tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
        await run.feed({ do: 'result', call: read.call, tool: 'read_file', output: '1 | fn main() {}\n2 | }' });
        await run.settle(null);
        const below = await run.evaluate(`(() => {
          const line = [...document.querySelectorAll('#rows .call-line')].filter((line) => /\\.rs/.test(line.textContent)).pop();
          if (!line) return null;
          const r = line.getBoundingClientRect();
          return [r.left + 30, r.top + r.height / 2];
        })()`);
        if (!below) return 'no row below the running call';
        await run.click(below[0], below[1]);
        await run.settle(null);
        // (The row the reader opened, by its key.)
        const readOpen = `(() => {
          const line = [...document.querySelectorAll('#rows .call-line')].filter((line) => /\\.rs/.test(line.textContent)).pop();
          return !!line && !!line.closest('details[open]');
        })()`;
        if (!(await run.evaluate(readOpen))) return "the row below didn't open";
        const before = await run.look();
        await run.feed({ do: 'result', call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        await run.settle('call → its result');
        const after = await run.look();
        if (!(await run.evaluate(readOpen))) return 'the row below closed as the call ended';
        for (const [id, line] of before.ids) {
          const now = after.ids.get(id);
          if (now && now.text === line.text && Math.round(now.raw - line.raw) > 0) return `${id} went ${Math.round(now.raw - line.raw)} device px down`;
        }
        return null;
      }, view);
    }
    // An open code run whose words fit on its line alone keeps its lines,
    // and so does a row after it, as its first call starts. The count of
    // calls and the call that runs come and take room on its line, so its
    // words whole don't come in above its code, and the call shows nothing
    // in the run yet, so the run doesn't grow. (The window is as narrow as
    // makes the count cut the words on the line.)
    await play(ratio, "an open code run keeps its lines as its count of calls comes (quiet)", async (run) => {
      await run.metrics(560, 800);
      await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
      const code = Array.from({ length: 4 }, () => run.some(2, 3)).join('\n');
      const { call } = await run.feed({ do: 'call', tool: 'run_code', parameters: { code, purpose: 'rename the setting in every crate' } });
      await run.settle(null);
      const line = `(() => {
        const row = [...document.querySelectorAll('#rows .row.quiet')].pop();
        if (!row) return null;
        const summary = row.querySelector('summary');
        const what = summary.querySelector('.what');
        const r = summary.getBoundingClientRect();
        return { x: r.left + 30, y: r.top + r.height / 2, open: row.querySelector('details').open, cut: what.scrollWidth > what.clientWidth + 0.5 };
      })()`;
      const shut = await run.evaluate(line);
      if (!shut) return 'no Quiet row';
      if (shut.cut) return 'its words are cut before its calls come';
      await run.click(shut.x, shut.y);
      await run.settle(null);
      // (A row after it.)
      await run.feed({ do: 'notice', text: run.some(3, 3) });
      await run.settle(null);
      if (!(await run.evaluate(line)).open) return "the row didn't open";
      const before = await run.look();
      await run.feed({ do: 'nested', run: call, tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
      await run.settle(null);
      const after = await run.look();
      // (Else the count never took the room this case is about.)
      if (!(await run.evaluate(line)).cut) return "the count of calls didn't cut its words on the line";
      const moved = Case.moved(before, after);
      return moved === 0 ? null : `lines moved (${Number.isNaN(moved) ? 'some, not all' : `all, ${moved} device px`}) as its first call started`;
    }, 'quiet');
    // A long command's details show the whole command once: in Full, where
    // a command is open from the start, and in a merged Quiet row whose
    // member the reader opens.
    for (const view of ['full', 'quiet']) {
      await play(ratio, `touch: a long command's details show the command once (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(4);
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        const first = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `ls ${run.words(2).split(' ').join('/')}` } });
        await run.feed({ do: 'result', call: first.call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        const long = await run.feed({ do: 'call', tool: 'shell', parameters: { command: LONG_COMMAND } });
        await run.feed({ do: 'result', call: long.call, tool: 'shell', output: 'exit code 0, 9.0 s\nok' });
        await run.settle(null);
        const click = async (selector) => {
          const at = await run.evaluate(`(() => { const all = [...document.querySelectorAll(${JSON.stringify(selector)})]; const el = all[all.length - 1]; if (!el) return null; const r = el.getBoundingClientRect(); return [r.left + 30, r.top + r.height / 2]; })()`);
          if (!at) return false;
          await run.click(at[0], at[1]);
          await run.settle(null);
          return true;
        };
        if (view === 'quiet') {
          if (!(await click('#rows .row.quiet summary'))) return 'no Quiet row';
          if (!(await click('#rows .member-line'))) return 'no member line in the open row';
        }
        const copies = await run.evaluate(`[...document.querySelectorAll('#rows .held')].filter((held) => held.textContent.includes('databaseId,headSha')).length`);
        return copies === 1 ? null : `the whole command shows ${copies} times`;
      }, view);
    }
    // The composer stands where the next item of the last one's kind would,
    // em box to em box (a line box less its half-leading): the gap under the
    // last item, to the composer's divider, is the gap between two items of
    // its kind, by their margins and line metrics, and the gap two such rows
    // on the screen have. For a call that runs and ends, "Working…", text as
    // it streams and the turn's line, and again after a fold above opens and
    // closes.
    for (const view of ['quiet', 'full', 'messages']) {
      await play(ratio, `touch: the composer stands where the next item of the last one's kind would (${view})`, async (run) => {
        await run.metrics(390, 844);
        await run.fill(8);
        const problems = [];
        let at = 0;
        const check = async (what) => {
          await run.settle(null);
          // (End first, as blank an end that got shorter left (rule 1) is
          // not the gap.)
          await run.press('End', 'End', (at += 100), 35);
          const gap = await run.evaluate(LAST_GAP);
          if (!gap) return problems.push(`${what}: nothing to measure`);
          const device = (px) => Math.round(px * gap.dpr);
          if (device(gap.under) !== device(gap.standard)) problems.push(`${what}: ${gap.kind} has ${gap.under} px under it to the composer, and two of its kind ${gap.standard}`);
          if (gap.pair !== null && device(gap.pair) !== device(gap.standard)) problems.push(`${what}: two ${gap.kind} rows on the screen are ${gap.pair} px apart, not ${gap.standard}`);
          return null;
        };
        await run.feed({ do: 'user', text: `Please ${run.words(4)}` });
        await check('"Working…"');
        await run.feed({ do: 'text', text: `${run.some(8, 4)}.` });
        await check('text as it streams');
        const read = await run.feed({ do: 'call', tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
        await check('a call that runs');
        await run.feed({ do: 'result', call: read.call, tool: 'read_file', output: '1 | fn main() {}' });
        await check('"Working…" after a call');
        // A note waiting at the end, the user's message, whose top margin
        // (24 px) is more than its bottom one (12 px).
        await run.feed({ do: 'note', text: `And ${run.words(3)}`, seq: 1 });
        await check('a note waiting at the end');
        await run.feed({ do: 'read', seq: 1 });
        const ls = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `ls ${run.words(2).split(' ').join('/')}` } });
        await check('a second call that runs');
        // (A fold above, opened and closed, changes nothing at the end.)
        const fold = await run.evaluate(`(() => {
          const line = [...document.querySelectorAll('#rows .call-line')].find((line) => /\\.rs/.test(line.textContent));
          if (!line) return null;
          const r = line.getBoundingClientRect();
          return [r.left + 30, r.top + r.height / 2];
        })()`);
        if (fold) {
          await run.click(fold[0], fold[1]);
          await run.settle(null);
          await run.click(fold[0], fold[1]);
          await check('a call that runs, after a fold above opened and closed');
        }
        await run.feed({ do: 'result', call: ls.call, tool: 'shell', output: 'exit code 0, 0.1 s\na.rs' });
        await check('"Working…" after two calls');
        await run.feed({ do: 'text', text: `${run.some(8, 4)}.` });
        await check('the reply');
        await run.feed({ do: 'end' });
        await check('the turn line');
        // A read last, its content closed (Full keeps it in the column,
        // hidden), as a worker that finished leaves it.
        const last = await run.feed({ do: 'call', tool: 'read_file', parameters: { path: `src/${run.words(1)}.rs` } });
        await run.feed({ do: 'result', call: last.call, tool: 'read_file', output: '1 | fn main() {}' });
        await check('a read with its content closed');
        return problems[0] ?? null;
      }, view);
    }
    await play(ratio, 'touch: a finger lifted without a drag catches up to the end', async (run) => {
      await run.fill();
      const box = await run.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + 100, r.top + 100]; })()`);
      await run.during(() => run.cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: box[0], y: box[1] }] }, run.session));
      await run.feed({ do: 'text', text: `${run.some(40, 4)}\n\n${run.some(40, 4)}` });
      await run.settle(null);
      const held = await run.look();
      if (held.ids.size && held.end <= held.bottom) return 'the output didn\'t take the end off the screen';
      await run.during(() => run.cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] }, run.session));
      await run.settle(null);
      const shot = await run.look();
      return follows(shot) ? null : `${shot.choice}, end ${shot.end} against ${shot.bottom}`;
    });
    await play(ratio, 'touch: a cancelled touch and a lost release end the hold', async (run) => {
      await run.fill();
      const box = await run.evaluate(`(() => { const r = document.getElementById('log').getBoundingClientRect(); return [r.left + 100, r.top + 100]; })()`);
      await run.during(() => run.cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: box[0], y: box[1] }] }, run.session));
      await run.settle(null);
      if ((await run.evaluate(`document.getElementById('log').dataset.held`)) !== 'true') return 'a finger down doesn\'t hold';
      await run.during(() => run.cdp.send('Input.dispatchTouchEvent', { type: 'touchCancel', touchPoints: [] }, run.session));
      await run.settle(null);
      if ((await run.evaluate(`document.getElementById('log').dataset.held`)) !== 'false') return 'a cancelled touch holds';
      await run.during(() => run.cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: box[0], y: box[1], button: 'left', clickCount: 1 }, run.session));
      await run.settle(null);
      await run.evaluate(`window.dispatchEvent(new FocusEvent('blur'))`);
      await run.settle(null);
      return (await run.evaluate(`document.getElementById('log').dataset.held`)) === 'false' ? null : 'a lost release holds';
    });
    // The space between the rows is the shared table's in each view, folds
    // closed and open, the header's and the composer's included.
    for (const view of VIEWS) {
      await play(ratio, `every gap is the table's step (${view})`, async (run) => {
        await spacedSession(run);
        const problems = [];
        const check = async (when) => {
          for (const off of await run.evaluate(SPACING)) problems.push(`${when}: ${off}`);
        };
        await check('closed');
        await run.evaluate(OPEN_FOLDS);
        await run.settle(null);
        await check('open');
        // (Quiet's rows of several calls open to a rail of their lines.)
        await run.evaluate(OPEN_MEMBERS);
        await run.settle(null);
        // (A line open on a rail with a line shown after it, so the gap
        // under an open line is measured.)
        const opened = await run.evaluate(`[...document.querySelectorAll('#rows .quiet-calls > :is(.quiet-member.open, .row.thinking.open)')].filter((el) => { const next = el.nextElementSibling; return next && next.getBoundingClientRect().height > 0; }).length`);
        if (view === 'quiet' && !opened) problems.push('no line on a rail opened with one after it');
        await check('its lines open');
        return problems.length ? problems.slice(0, 6).join('; ') : null;
      }, view);
    }
    // A framed block holds what it says alike above and below.
    await play(ratio, 'every framed block holds its content alike top and bottom', async (run) => {
      await spacedSession(run);
      const problems = [];
      await run.evaluate(OPEN_FOLDS);
      await run.settle(null);
      problems.push(...(await run.evaluate(FRAMES)).map((off) => `main: ${off}`));
      // Each worker's view, with its report.
      for (const worker of ['r1', 'r2']) {
        const card = await run.evaluate(`(() => {
          const card = [...document.querySelectorAll('#rows .wcard')].find((card) => card.textContent.includes('${worker}'));
          if (!card) return null;
          card.scrollIntoView({ block: 'center' });
          const r = card.getBoundingClientRect();
          return { x: r.left + 20, y: r.top + r.height / 2 };
        })()`);
        if (!card) return `no card of ${worker}`;
        await run.settle(null);
        await run.click(card.x, card.y);
        await run.settle(null);
        if (!(await run.evaluate(`!!document.querySelector('#rows .row.report')`))) return `no report in ${worker}'s view`;
        problems.push(...(await run.evaluate(FRAMES)).map((off) => `${worker}'s view: ${off}`));
        await run.key('Escape', 'Escape');
        await run.settle(null);
      }
      return problems.length ? problems.slice(0, 6).join('; ') : null;
    }, 'full');
  }
  // A code span that closes while a reply streams draws its words in the
  // code font, on the baselines they stood on. On a phone, following, with
  // the span's first line full, its last words keep a line of their own,
  // which doesn't move.
  await play(3, 'a code span that closes on a wrapped line moves none of its words', async (run) => {
    await run.metrics(390, 844);
    await run.fill();
    await run.feed({ do: 'user', text: `Then ${run.words(4)}` });
    await run.feed({ do: 'text', text: 'w901 w902 w903 w904 w905 w906 `w907 w908 w909' });
    await run.settle(null);
    const open = await run.look();
    if (!follows(open)) return `not following (${open.choice})`;
    await run.feed({ do: 'text', text: '`' });
    await run.settle('reply streams');
    const closed = await run.look();
    const id = [...open.ids.keys()].find((key) => key.endsWith('|w:w908'));
    const was = open.ids.get(id);
    const now = closed.ids.get(id);
    if (!was || !now || was.text !== now.text) return `the span's last words didn't keep a line of their own ("${was?.text}" → "${now?.text}")`;
    if (!(await run.evaluate(`[...document.querySelectorAll('#rows code')].some((code) => code.textContent.includes('w908'))`))) return 'the span didn\'t close';
    const moved = shift(was, now);
    if (moved !== 0) return `"${now.text}" moved ${moved}`;
    if (run.meter.shifted() !== 0) return `${run.meter.shifted()} frames moved a line`;
    // A device pixel's move of the span or of plain text above it counts,
    // in the case's own tallies.
    for (const [what, words, select] of [['code', 'w908', '#rows code'], ['plain text', 'w901', '#rows p']]) {
      const before = await run.look();
      const shifted = run.meter.shifted();
      await run.evaluate(`(() => {
        const element = [...document.querySelectorAll('${select}')].find((each) => each.textContent.includes('${words}'));
        element.style.position = 'relative';
        element.style.top = \`\${1 / devicePixelRatio}px\`;
      })()`);
      await run.settle('a line moves a device pixel');
      const after = await run.look();
      const key = [...before.ids.keys()].find((key) => key.endsWith(`|w:${words}`));
      const by = key && after.ids.get(key) ? shift(before.ids.get(key), after.ids.get(key)) : null;
      if (by !== 1 || run.meter.shifted() !== shifted + 1) return `the ${what} moved a device pixel and the meter measured ${by} in ${run.meter.shifted() - shifted} frames`;
    }
    // So does a span that closes in the frame its row moves a device pixel.
    await run.feed({ do: 'text', text: '\n\nw911 w912 w913 w914 w915 w916 `w917 w918 w919' });
    await run.settle(null);
    const unclosed = await run.look();
    const shifted = run.meter.shifted();
    const events = await run.events();
    await run.agent.ask({ do: 'text', text: '`' });
    await run.delivered(events);
    await run.evaluate(`(() => {
      const row = [...document.querySelectorAll('#rows > *')].find((row) => row.textContent.includes('w918'));
      row.style.position = 'relative';
      row.style.top = \`\${1 / devicePixelRatio}px\`;
    })()`);
    await run.settle('a line moves a device pixel');
    const both = await run.look();
    const key = [...unclosed.ids.keys()].find((key) => key.endsWith('|w:w918'));
    const [from, to] = [unclosed.ids.get(key), both.ids.get(key)];
    if (!from || !to || from.font === to.font) return `the second span's last words didn't keep a line of their own in another font ("${from?.text}" → "${to?.text}")`;
    const by = shift(from, to);
    return by === 1 && run.meter.shifted() === shifted + 1 ? null : `a span closed as its row moved a device pixel, and the meter measured ${by} in ${run.meter.shifted() - shifted} frames`;
  });
  return { failures, count, skipped };
}

// A session with every kind of row: the user's messages and a note waiting,
// text, a list and a code block, calls run and running, a thought, a notice,
// a question answered, a worker's card and its report, and turns' lines.
async function spacedSession(run) {
  const call = async (tool, parameters, output, worker) => {
    const who = worker ? { worker } : {};
    const { call: id } = await run.feed({ do: 'call', tool, parameters, ...who });
    if (output !== undefined) await run.feed({ do: 'result', call: id, tool, output, ...who });
  };
  const lines = (n) => Array.from({ length: n }, (_, at) => `${String(at + 1).padStart(4)} | ${run.words(4)}`).join('\n');
  await run.feed({ do: 'user', text: `Please ${run.words(6)}` });
  await run.feed({ do: 'text', text: run.some(8, 6) });
  await call('shell', { command: `git status ${run.words(1)}` }, 'exit code 0, 0.1 s\n M Cargo.toml');
  await call('read_file', { path: '/home/ada/proj/src/main.rs' }, lines(12));
  // (Quiet merges the two reads into a row whose rail has a line for each.)
  await call('read_file', { path: '/home/ada/proj/src/lib.rs' }, lines(6));
  await run.feed({ do: 'think', text: run.some(10, 6) });
  await call('shell', { command: `cargo check ${run.words(1)}` }, 'exit code 0, 0.2 s\n');
  await run.feed({ do: 'notice', text: run.some(5, 4) });
  await run.feed({ do: 'text', text: `${run.some(6, 4)}\n\n- ${run.words(5)}\n- ${run.words(4)}` });
  await run.feed({ do: 'worker', id: 'r1', text: `Task ${run.words(4)}` });
  await run.feed({ do: 'text', text: run.some(6, 4), worker: 'r1' });
  await call('shell', { command: `cargo test ${run.words(1)}` }, 'exit code 0, 0.3 s\nok', 'r1');
  // (Its report ends with a code block, and r2's with a quote, drawn edges.)
  await run.feed({ do: 'worker', id: 'r1', change: 'finished', text: `${run.some(8, 4)}\n\n- ${run.words(5)}\n- ${run.words(4)}\n\n\`\`\`sh\ncargo test\n\`\`\`` });
  await run.feed({ do: 'worker', id: 'r2', text: `Task ${run.words(4)}` });
  await run.feed({ do: 'worker', id: 'r2', change: 'finished', text: `${run.some(8, 4)}\n\n> ${run.words(6)}` });
  // (r3 runs on with no calls yet, so its card has no last line.)
  await run.feed({ do: 'worker', id: 'r3', text: `Task ${run.words(4)}` });
  await run.feed({ do: 'ask', id: 'q1', text: `${run.some(4, 4)}?` });
  await run.feed({ do: 'answered', id: 'q1', text: run.words(3) });
  await run.feed({ do: 'text', text: `${run.some(6, 4)}\n\n\`\`\`rust\nfn main() {\n    println!("{}", 1);\n}\n\`\`\`` });
  await run.feed({ do: 'end' });
  await run.feed({ do: 'user', text: `And ${run.words(5)}` });
  await run.feed({ do: 'text', text: run.some(6, 4) });
  await call('shell', { command: `cargo build ${run.words(1)}` });
  await run.feed({ do: 'note', text: `Also ${run.words(4)}`, seq: 1 });
  await run.evaluate('__meter.tick(2600)');
  await run.settle(null);
}

// Opens every fold the transcript shows, from the last up, so an open row's
// neighbours are measured too.
const OPEN_FOLDS = `(() => {
  const folds = [...document.querySelectorAll('#rows > .row > details:not([open]) > summary, #rows > .row.count > button[aria-expanded="false"], #rows > .row.thinking:not(.open) > .thought')];
  for (const fold of folds.reverse()) fold.click();
})()`;

// Opens the first line on each open row's rail, a call's or a thought's,
// which a line follows. (One a rail, as a line opened may draw its rail
// again.)
const OPEN_MEMBERS = `(() => {
  for (const rail of [...document.querySelectorAll('#rows .quiet-calls')].reverse()) {
    const first = rail.querySelector(':scope > .quiet-member:not(.open) > .member-line, :scope > .row.thinking:not(.open) > .thought');
    if (first) first.click();
  }
})()`;

// Each gap against the table (TABLE), in CSS px between the rows' boxes, the
// header's and the composer's included. A list of what's off, empty when
// every gap is the table's.
const SPACING = `(() => {
  const { PX, step, kindOf: kind, leadOf: lead, endOf } = ${TABLE};
  const rows = [...document.querySelectorAll('#rows > .row')].filter((row) => row.getBoundingClientRect().height > 0);
  const column = document.getElementById('rows').getBoundingClientRect();
  const off = [];
  const say = (row) => kind(row) + ' "' + row.textContent.trim().replace(/\\s+/g, ' ').slice(0, 24) + '"';
  const test = (what, want, got) => { if (Math.abs(want - got) > 0.5) off.push(what + ' wants ' + want + ', got ' + Math.round(got * 10) / 10); };
  if (!rows.length) return off;
  const first = rows[0];
  const shut = (k) => (k === 'Open' || k === 'Held' ? 'Work' : k);
  test('the header above ' + say(first), PX[step(shut(kind(first)), shut(kind(first)))] + lead(first), first.getBoundingClientRect().top - column.top);
  for (let at = 1; at < rows.length; at++) {
    const a = rows[at - 1];
    const b = rows[at];
    test(say(a) + ' above ' + say(b), PX[step(kind(a), kind(b))], b.getBoundingClientRect().top - a.getBoundingClientRect().bottom);
  }
  const last = rows[rows.length - 1];
  test(say(last) + ' above the composer', endOf(last), column.bottom - last.getBoundingClientRect().bottom);
  // The lines on an open Quiet row's rail, spaced as rows, which the rail
  // starts with.
  const member = (el) => (el.matches('.quiet-member.open, .row.thinking.open') ? 'Open' : el.classList.contains('row') ? kind(el) : 'Work');
  for (const rail of document.querySelectorAll('#rows .quiet-calls')) {
    const members = [...rail.children].filter((el) => el.getBoundingClientRect().height > 0);
    if (!members.length) continue;
    test('the rail above ' + say(members[0]), 0, members[0].getBoundingClientRect().top - rail.getBoundingClientRect().top);
    for (let at = 1; at < members.length; at++) {
      const a = members[at - 1];
      const b = members[at];
      test('on a rail, ' + say(a) + ' above ' + say(b), PX[step(member(a), member(b))], b.getBoundingClientRect().top - a.getBoundingClientRect().bottom);
    }
  }
  return off;
})()`;

// Each framed block's inner space above and below what it holds, to the
// ink, a line of text by its em box and anything drawn by its box, with what
// lies over its corner left out. A list of the blocks that hold it unevenly,
// by more than a pixel (two for code, whose monospaced glyph box sits about
// 0.85 px off its line's middle), and of a bubble or a card that holds it
// other than 12 px from its frame.
const FRAMES = `(() => {
  const KINDS = [['bubble', '.row.user .bubble'], ['report', '.row.report'], ['worker card', '.wcard'], ['question record', '.record-box'], ['code block', '.md pre'], ['output', '.row.output pre']];
  const shows = (el, frame) => {
    for (let e = el; e && e !== frame.parentElement; e = e.parentElement) {
      const s = getComputedStyle(e);
      if (s.display === 'none' || s.visibility === 'hidden' || Number(s.opacity) === 0) return false;
      if (e !== frame && s.position === 'absolute') return false;
      const up = e.parentElement;
      if (up && up.tagName === 'DETAILS' && !up.open && e.tagName !== 'SUMMARY') return false;
    }
    return true;
  };
  const drawn = (s) => (s.backgroundColor && !/rgba\\(.*,\\s*0\\)$/.test(s.backgroundColor) && s.backgroundColor !== 'transparent') || ['Top', 'Bottom', 'Left', 'Right'].some((side) => parseFloat(s['border' + side + 'Width']) > 0);
  const off = [];
  for (const [name, selector] of KINDS) {
    for (const frame of document.querySelectorAll('#rows ' + selector)) {
      const box = frame.getBoundingClientRect();
      if (box.height <= 0) continue;
      const fs = getComputedStyle(frame);
      const inner = { top: box.top + parseFloat(fs.borderTopWidth), bottom: box.bottom - parseFloat(fs.borderBottomWidth) };
      let top = Infinity;
      let bottom = -Infinity;
      const walker = document.createTreeWalker(frame, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
      walker.nextNode();
      for (let node = walker.currentNode; node; node = walker.nextNode()) {
        if (node.nodeType === Node.TEXT_NODE) {
          if (!node.textContent.trim() || !shows(node.parentElement, frame)) continue;
          const size = parseFloat(getComputedStyle(node.parentElement).fontSize);
          const range = document.createRange();
          range.selectNodeContents(node);
          for (const r of range.getClientRects()) {
            const mid = (r.top + r.bottom) / 2;
            if (r.width <= 0.01 || mid < inner.top || mid > inner.bottom) continue;
            top = Math.min(top, mid - size / 2);
            bottom = Math.max(bottom, mid + size / 2);
          }
        } else if (node !== frame && shows(node, frame)) {
          const r = node.getBoundingClientRect();
          if (r.height > 0 && (['svg', 'img'].includes(node.tagName.toLowerCase()) || drawn(getComputedStyle(node)))) {
            top = Math.min(top, Math.max(r.top, inner.top));
            bottom = Math.max(bottom, Math.min(r.bottom, inner.bottom));
          }
        }
      }
      if (top === Infinity) continue;
      const above = top - inner.top;
      const below = inner.bottom - bottom;
      const code = name === 'code block' || name === 'output';
      const said = name + ' "' + frame.textContent.trim().slice(0, 20) + '" holds it ' + Math.round(above * 10) / 10 + ' above and ' + Math.round(below * 10) / 10 + ' below';
      if (Math.abs(above - below) > (code ? 2 : 1)) off.push(said);
      // (A bubble and the cards hold it 12 px from their frame.)
      else if (!code && (Math.abs(above - 12) > 1 || Math.abs(below - 12) > 1)) off.push(said + ', not 12');
    }
  }
  return off;
})()`;

// ----- the runs -----

async function main() {
  const control = arg('control');
  const chromium = arg('chromium');
  if (!control || !chromium) {
    console.error('usage: meter.mjs --control HOST:PORT --chromium PATH');
    process.exit(2);
  }
  const meter = new Meter();
  const sizes = FULL ? SIZES : SIZES.slice(0, 5);
  const turns = FULL ? 4 : 2;
  const plays = [];
  for (const [at, size] of sizes.entries()) {
    for (const [index, view] of VIEWS.entries()) {
      for (const scrolled of [false, true]) {
        const key = `${at}:${view}:${scrolled ? 'up' : 'following'}`;
        if ((ONLY && ONLY !== key) || CASES_ONLY) continue;
        plays.push({ size, view, scrolled, seed: 1 + at * 20 + index * 4 + (scrolled ? 1 : 0) });
      }
    }
  }
  const failures = [];
  const started = Date.now();
  const parallel = Number(process.env.SHIFT_METER_TABS ?? 3);
  let next = 0;
  // (Each plays in a browser of its own, as frames drawn on demand don't
  // share one well.)
  async function worker() {
    const agent = new Agent(control);
    const browser = await launch(chromium);
    while (next < plays.length) {
      const play = plays[next++];
      const run = new Run({ cdp: browser.cdp, agent, meter, ...play });
      try {
        await run.open();
        for (let turn = 0; turn < turns; turn++) await run.turn(turn);
      } catch (error) {
        failures.push(`${play.size.name} ${play.view} ${play.scrolled ? 'scrolled up' : 'following'}: ${error.stack ?? error}`);
      }
      for (const failure of run.failures) failures.push(`${play.size.name} ${play.view}: ${failure}`);
      await run.close();
      if (VERBOSE) console.log(`played ${play.size.name} ${play.view} ${play.scrolled ? 'scrolled up' : 'following'} (${Math.round((Date.now() - started) / 1000)} s)`);
    }
    agent.close();
    browser.close();
  }
  await Promise.all(Array.from({ length: Math.min(parallel, plays.length) }, worker));
  // The pixel cases, in a browser of their own.
  let caseCount = 0;
  if (process.env.SHIFT_METER_CASES !== '0' && !ONLY) {
    const agent = new Agent(control);
    const browser = await launch(chromium);
    const { failures: missed, count, skipped } = await pixelCases(browser.cdp, agent);
    caseCount = count;
    for (const skip of skipped) console.log(`pixel case skipped: ${skip}`);
    for (const failure of missed) failures.push(`pixel case ${failure}`);
    agent.close();
    browser.close();
  }
  const tables = `${meter.frames()} frames, ${meter.shifted()} moved a line, ${meter.tripsFailed()} round trips left a line elsewhere. Each cell is frames · frames that moved a line · the most device pixels / all device pixels.\n\n${meter.table()}\n${meter.sizeTable()}\n${meter.causeTable()}\nThe moves the meter allows. Each cell is frames · the most device pixels / all device pixels.\n\n${meter.allowedTable()}\nPanels, the message box and the window, opened and closed with nothing between. Each cell is round trips · those that left a line elsewhere · the most device pixels.\n\n${meter.tripTable()}\nBlank that came below the end while following, by step (rule 1). Each cell is frames · the most device pixels / all device pixels.\n\n${meter.blankTable()}`;
  if (OUT) writeFileSync(OUT, `${tables}\n## The largest move of each cause, step and view\n\n${meter.exampleText()}`);
  console.log(tables);
  if (caseCount) console.log(`${caseCount} pixel cases, ${failures.filter((failure) => failure.startsWith('pixel case')).length} failed.`);
  for (const failure of failures) console.error(failure);
  const ok = failures.length === 0 && (CASES_ONLY || meter.frames() > 500) && meter.shifted() === 0 && meter.tripsFailed() === 0;
  if (!ok && !OUT) console.error(meter.exampleText());
  process.exit(ok ? 0 : 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
