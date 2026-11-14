// The page's shift meter. It plays scripted sessions into the page in
// Chromium's headless shell, a frame at a time, and records where every line
// of text in the transcript stands on every frame, in device pixels. Every
// word the session makes is a word no other has (w1, w2, …), so each line is
// found again in the next frame by its first word that no other line has.
//
// Following, a line may move up as what comes at the end pushes it, and
// never down. Scrolled up, no line moves. The moves the rules allow are the
// reader's own (a scroll, End, the marker, sending, another view or width),
// the lines above a fold the reader toggles by less than a line, "Working…"
// and a note waiting at the end going down into the blank below the end as
// what comes above them takes it, and a taller view giving back what a
// shorter one took while following. A panel opened and closed with nothing
// between puts every line back on its pixel.
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

  ask(request) {
    const id = ++this.id;
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

// Counts the stream's events, holds the page's one-second clock and its time,
// and measures the transcript's lines.
const INSTRUMENT = `(() => {
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
    // "Working…", which rides on the end, and the notes waiting after the
    // rest.
    for (let at = children.length - 1; at >= 0; at--) {
      const row = children[at];
      if (row.matches('.user.pending')) endZone = row.getBoundingClientRect().top;
      else break;
    }
    const working = rows.querySelector(':scope > .working');
    if (working) endZone = Math.min(endZone ?? Infinity, working.getBoundingClientRect().top);
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
          line.words.push([match[0], Math.round(word.top * dpr), Math.round(word.left * dpr), word.top * dpr]);
        }
      }
      lines.push(...mine);
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
    if (own) out.set(`${key}|w:${clean(own[0])}`, { y: own[1], x: own[2], raw: own[3], text });
    else if (texts.get(`${key}|${text}`) === 1) {
      // (Where its first word, a marker, is drawn in another glyph.)
      const word = words.find(([word]) => /[A-Za-z0-9]/.test(word)) ?? words[0];
      out.set(`${key}|t:${text}`, { y: word ? word[1] : y, x: word ? word[2] : x, raw: word ? word[3] : y, text });
    }
  }
  return out;
}

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
    this.word = 0;
    this.last = null;
    this.calls = [];
    this.live = 'working';
    this.job = null;
    this.waiting = null;
    this.seq = 0;
    this.opened = null;
    this.owed = 0;
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

  async open() {
    const { targetId } = await this.cdp.send('Target.createTarget', { url: 'about:blank', enableBeginFrameControl: true });
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
    if (PAGE) {
      // The page's files as built in this tree, whatever the server was
      // built with.
      this.cdp.on(sessionId, 'Fetch.requestPaused', (event) => {
        const name = new URL(event.request.url).pathname.slice(1);
        let body = null;
        try {
          body = readFileSync(join(PAGE, name));
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
      await this.cdp.send('Fetch.enable', {
        patterns: ['arko-web.js', 'arko-web_bg.wasm', 'app.css', 'boot.js', 'themes.css'].map((name) => ({ urlPattern: `*/${name}*`, requestStage: 'Response' })),
      }, sessionId);
    }
    const { url } = await this.agent.ask({ do: 'session' });
    this.frameTime = 1000;
    await this.cdp.send('Page.navigate', { url }, sessionId);
    for (let at = 0; at < 600; at++) {
      await this.beginFrame();
      const ready = await this.evaluate(`!!(window.__meter && document.getElementById('rows') && !document.querySelector('.conn-row'))`).catch(() => false);
      if (ready) break;
      await sleep(10);
    }
    this.lineHeight = await this.evaluate('__meter.lineHeight()');
    await this.settle(null);
  }

  async close() {
    await this.cdp.send('Target.closeTarget', { targetId: this.target }).catch(() => {});
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
  /// below which lines aren't counted, and `viewport` says the transcript's
  /// height changes.
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
    if (before.choice === 'read' && after.choice === 'follow') this.owed = 0;
    const following = before.following && after.following;
    const changed = this.changeAt(before, after);
    const blank = Math.max(0, before.bottom - before.end);
    let rows = 0;
    let line = null;
    let room = 0;
    let toggle = 0;
    let pushed = 0;
    let highest = null;
    let up = 0;
    let sideways = 0;
    let sidewaysLine = null;
    for (const [id, { y: from, x, raw, text }] of before.ids) {
      const now = after.ids.get(id);
      if (!now) continue;
      // (By how far its rectangle moved, as a line on a half device pixel
      // rounds either way with float noise that moves nothing drawn.)
      const to = from + Math.round(now.raw - raw);
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
      // ("Working…" and the notes waiting at the end make room below.)
      if (after.endZone !== null && now.y >= after.endZone && (to > from || !following)) {
        room = Math.max(room, Math.abs(to - from));
        continue;
      }
      let moved = to > from || !following ? Math.abs(to - from) : 0;
      // (Lines below a change on the screen move with it. Following, they
      // go down into the blank below the end and no further.)
      if (from > changed && moved > 0 && (!following || moved <= blank)) {
        pushed = Math.max(pushed, moved);
        moved = 0;
      }
      // (The lines above a fold the reader toggled move by the space an open
      // item stands apart by, less than a line.)
      if (below !== null && from < below && moved > 0 && moved <= this.lineHeight) {
        toggle = Math.max(toggle, moved);
        moved = 0;
      }
      if (highest === null || from < highest.from) highest = { from, to };
      if (moved > rows) {
        rows = moved;
        line = { id, from, to, text };
      }
    }
    if (step === null || step === undefined) return 0;
    const key = (kind) => `${kind}|${step}|${this.view}`;
    if (room > 0) this.meter.tally(this.meter.allowed, key('the end makes room')).add(room);
    if (toggle > 0) this.meter.tally(this.meter.allowed, key("the reader's toggle")).add(toggle);
    if (pushed > 0) this.meter.tally(this.meter.allowed, key('below a change on the screen')).add(pushed);
    // While following, a shorter view moves the lines up, and a taller one
    // gives that back first. What is owed starts at none when following
    // starts, and a taller view while reading takes from it.
    const taller = after.clientHeight - before.clientHeight;
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
    for (const [id, { y, raw, text }] of before.ids) {
      const now = after.ids.get(id);
      if (!now) continue;
      const moved = Math.abs(Math.round(now.raw - raw));
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
    const at = await this.evaluate(`(() => { const j = document.querySelector('.jump.shown'); if (!j) return null; const r = j.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    if (at && this.rng.chance(50)) {
      await this.click(at[0], at[1]);
    } else {
      // End, with the focus out of the message box.
      await this.evaluate('document.activeElement && document.activeElement.blur()');
      await this.key('End', 'End');
    }
    this.owed = 0;
    await this.settle(null);
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

  async call() {
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
    await this.focusBox();
    await this.type(`Then ${this.words(3 + this.rng.below(8))}`);
    const before = await this.events();
    // (Ctrl+Enter sends on any device, as Enter makes a new line without a
    // fine pointer.)
    await this.key('Enter', 'Enter', '\r', 2);
    await this.delivered(before, 1500);
    await this.settle(null);
    this.owed = 0;
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
    await this.evaluate(`(() => { const rows = document.getElementById('rows'); const spacer = document.createElement('div'); spacer.id = 'case-spacer'; spacer.style.height = '${device / this.size.dpr}px'; rows.appendChild(spacer); })()`);
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
  const play = async (ratio, name, body) => {
    count++;
    const run = new Case({ cdp, agent, meter: new Meter(), size: { name: `cases @${ratio}`, width: 1000, height: 700, dpr: ratio, mobile: name.startsWith('touch') }, view: 'full', scrolled: false, seed: 7 });
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
  const follows = (shot) => shot.choice === 'follow' && shot.following;
  const blankOf = (shot) => shot.bottom - shot.end;
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
        for (let at = 0; at < 3; at++) {
          const { call } = await run.feed({ do: 'call', tool: 'shell', parameters: { command: `cargo test ${run.words(2)}` } });
          await run.feed({ do: 'result', call, tool: 'shell', output: `Exit code: 0\n${run.some(3, 3)} ok\n${run.some(3, 3)} ok` });
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
        return after.choice === before.choice ? null : `the choice went ${before.choice} → ${after.choice}`;
      });
    }
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
  }
  return { failures, count, skipped };
}

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
  const tables = `${meter.frames()} frames, ${meter.shifted()} moved a line, ${meter.tripsFailed()} round trips left a line elsewhere. Each cell is frames · frames that moved a line · the most device pixels / all device pixels.\n\n${meter.table()}\n${meter.sizeTable()}\n${meter.causeTable()}\nThe moves the meter allows. Each cell is frames · the most device pixels / all device pixels.\n\n${meter.allowedTable()}\nPanels, the message box and the window, opened and closed with nothing between. Each cell is round trips · those that left a line elsewhere · the most device pixels.\n\n${meter.tripTable()}`;
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
