// Arko's remote page: follows the session's stream (Server-Sent Events) and
// sends messages, answers, stops and settings commands (POST). Text from
// the session is only ever set as text or built into elements, never
// parsed as markup. core.js holds the logic that has no page.
'use strict';

(() => {
  const C = window.FinCore;

  /** Who gets the extra rules for small models, as `/extra-rules` takes it. */
  const RULES = {
    small: { name: 'Small models', about: 'Agents on small models (Haiku, luna) get a few extra rules in their instructions.' },
    all: { name: 'Every agent', about: 'Every agent gets the extra rules for small models.' },
    off: { name: 'Off', about: 'No agent gets them.' },
  };

  /** Which of the project's instruction files the agents get, as
   * `/project-instructions` takes it. */
  const PROJECT = {
    one: { name: 'AGENTS.md, else CLAUDE.md', about: 'CLAUDE.md only when there is no AGENTS.md.' },
    both: { name: 'Both', about: 'AGENTS.md and CLAUDE.md, each under its own name.' },
    off: { name: 'Off', about: 'The agents get neither file.' },
  };
  const KEY = 'arko.remote.key';
  const $ = (id) => document.getElementById(id);
  const fine = window.matchMedia('(pointer: fine)');

  // ----- this device's choices (they live only in this browser) -----

  function load(name) {
    try {
      return window.localStorage.getItem(name);
    } catch {
      return null;
    }
  }
  function save(name, value) {
    try {
      if (value === null || value === '') window.localStorage.removeItem(name);
      else window.localStorage.setItem(name, value);
    } catch {
      // Private windows may refuse: the choice lasts as long as the page.
    }
  }
  function sessionGet(name) {
    try {
      return window.sessionStorage.getItem(name);
    } catch {
      return null;
    }
  }
  function sessionSet(name, value) {
    try {
      if (value) window.sessionStorage.setItem(name, value);
      else window.sessionStorage.removeItem(name);
    } catch {
      // As above.
    }
  }

  // The key: from the link's fragment into this tab's storage, and off the
  // address bar (and so out of bookmarks and shared links).
  const fragment = new URLSearchParams(location.hash.slice(1));
  if (fragment.get('key')) {
    sessionSet(KEY, fragment.get('key'));
    history.replaceState(null, '', location.pathname + location.search);
  }

  const store = new C.Store();
  let status = null;
  /** 'connecting', 'live' or 'offline'. */
  let connection = 'connecting';
  let view = C.VIEWS[load('arko.view')] ? load('arko.view') : 'full';
  /** The worker whose transcript is shown, or null for the main one. */
  let worker = null;
  /** Server time minus this device's, in ms, as the live items tell it. */
  let skew = null;

  const log = $('log');
  const list = $('rows');
  const text = $('text');
  const send = $('send');
  const stop = $('stop');

  function element(tag, className, content) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (content !== undefined && content !== null) node.textContent = content;
    return node;
  }

  function icon(path) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 20 20');
    svg.setAttribute('aria-hidden', 'true');
    const shape = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    shape.setAttribute('d', path);
    shape.setAttribute('fill', 'none');
    shape.setAttribute('stroke', 'currentColor');
    shape.setAttribute('stroke-width', '2');
    shape.setAttribute('stroke-linecap', 'round');
    shape.setAttribute('stroke-linejoin', 'round');
    svg.append(shape);
    return svg;
  }

  function serverNow() {
    return Date.now() - (skew || 0);
  }

  // ----- markdown, built with DOM calls -----

  function runs(nodes, parent) {
    for (const run of nodes) {
      switch (run.type) {
        case 'text':
          parent.append(run.text);
          break;
        case 'br':
          parent.append(document.createElement('br'));
          break;
        case 'code':
          parent.append(element('code', null, run.text));
          break;
        case 'strong':
        case 'em':
        case 'del': {
          const node = document.createElement(run.type === 'strong' ? 'strong' : run.type);
          runs(run.children, node);
          parent.append(node);
          break;
        }
        case 'link': {
          const link = document.createElement('a');
          link.href = run.href;
          link.target = '_blank';
          link.rel = 'noopener noreferrer';
          runs(run.children, link);
          parent.append(link);
          break;
        }
      }
    }
  }

  function markdown(blocks, parent) {
    for (const block of blocks) {
      switch (block.type) {
        case 'p': {
          const p = document.createElement('p');
          runs(block.children, p);
          parent.append(p);
          break;
        }
        case 'h': {
          const h = document.createElement('h' + block.level);
          runs(block.children, h);
          parent.append(h);
          break;
        }
        case 'code': {
          const pre = document.createElement('pre');
          const code = element('code', null, block.text);
          if (block.lang) pre.dataset.lang = block.lang;
          pre.append(code);
          parent.append(pre);
          break;
        }
        case 'quote': {
          const quote = document.createElement('blockquote');
          markdown(block.children, quote);
          parent.append(quote);
          break;
        }
        case 'list': {
          const list = document.createElement(block.ordered ? 'ol' : 'ul');
          if (block.ordered && block.start !== 1) list.start = block.start;
          for (const item of block.items) {
            const li = document.createElement('li');
            markdown(item, li);
            list.append(li);
          }
          parent.append(list);
          break;
        }
        case 'hr':
          parent.append(document.createElement('hr'));
          break;
        case 'table': {
          const wrap = element('div', 'table');
          const table = document.createElement('table');
          const head = table.createTHead().insertRow();
          for (const cell of block.head) {
            const th = document.createElement('th');
            runs(cell, th);
            head.append(th);
          }
          const body = table.createTBody();
          for (const row of block.rows) {
            const tr = body.insertRow();
            for (const cell of row) runs(cell, tr.insertCell());
          }
          wrap.append(table);
          parent.append(wrap);
          break;
        }
      }
    }
  }

  function md(source, className) {
    const box = element('div', 'md' + (className ? ' ' + className : ''));
    markdown(C.markdown(source), box);
    return box;
  }

  // ----- rows -----

  const DELIVERY = {
    note: 'added to the task',
    answer: 'answer',
    command: 'command',
    queued: 'waits for its turn',
    refresh: 'kept the cache warm',
  };

  const CHEVRON = 'M8 5l5 5-5 5';

  /** A worker's live line; `main` makes the sheet's "Main" entry. */
  function workerCard(id) {
    const summary = store.workers.get(id) || { id, title: '', state: 'running', tool_calls: 0 };
    const words = C.workerWords(summary, serverNow());
    const card = element('button', 'wcard ' + summary.state);
    card.type = 'button';
    card.dataset.worker = id;
    const title = element('span', 'wtitle');
    title.append(element('span', 'wid', id), words.title);
    const go = element('span', 'wgo');
    go.append(icon(CHEVRON));
    card.append(element('span', 'wdot'), title, go, element('span', 'wmeta', words.meta));
    if (words.last) card.append(element('span', 'wlast', words.last));
    card.setAttribute('aria-label', id + ': ' + words.title + ', ' + words.meta);
    return card;
  }

  function callLine(item, parent) {
    const line = element('span', 'call-line');
    line.append(element('span', 'mark', '▸'));
    if (item.agent !== C.MAIN && !worker) line.append(element('span', 'who', item.agent));
    line.append(element('span', 'what', item.text));
    const outcome = element('span', 'outcome');
    if (item.status === 'running') {
      outcome.append(element('span', 'running', '…'));
    } else if (item.status === 'ok' || item.status === 'failed') {
      outcome.append(element('span', item.status, item.status === 'ok' ? '✓' : '✗'));
      if (item.outcome) outcome.append(' ' + item.outcome);
      outcome.title = item.outcome || '';
    }
    if (item.status) line.append(outcome);
    // A long call can move to the background, and a job it made can stop.
    const actions = C.callActions(item, store, status && status.movable);
    if (actions.move) {
      const move = element('button', 'call-action', 'Move to background');
      move.type = 'button';
      move.addEventListener('click', (event) => {
        event.stopPropagation();
        move.disabled = true;
        post('/api/background', { item: item.id });
      });
      line.append(move);
    }
    if (actions.stop) {
      const stopJob = element('button', 'call-action', 'Stop');
      stopJob.type = 'button';
      stopJob.addEventListener('click', (event) => {
        event.stopPropagation();
        stopJob.disabled = true;
        post('/api/jobs/stop', { job: actions.stop });
      });
      line.append(stopJob);
    }
    parent.append(line);
  }

  function renderRow(row) {
    if (row.working) {
      // The agent waits on its model. The row stands in the thinking's
      // place, its word shimmering as the thinking's label does while it
      // streams.
      const box = element('div', 'row working');
      box.append(element('span', 'mark', '▸'), element('span', 'what', 'Working…'));
      return box;
    }
    if (row.count) {
      const box = element('div', 'row count');
      const button = element('button');
      button.type = 'button';
      const { calls, failed, running } = row.count;
      button.append('▸ ' + C.plural(calls, 'tool call', 'tool calls'));
      if (failed) button.append(' · ', element('span', 'failed', failed + ' failed'));
      if (running) button.append(' · running');
      button.title = 'Show them (Full view)';
      button.addEventListener('click', () => setView('full', row.id));
      box.append(button);
      return box;
    }
    const item = row.item;
    const box = element('div', 'row ' + item.kind + (item.level ? ' level-' + item.level : '') + (item.pending ? ' pending' : ''));
    box.dataset.id = item.id;
    box.title = new Date(item.at).toLocaleTimeString();
    switch (item.kind) {
      case 'user': {
        box.append(element('div', 'bubble', item.text));
        const tags = [];
        if (DELIVERY[item.delivery]) tags.push(DELIVERY[item.delivery]);
        // Out of its worker's own view, a message to a worker says to which.
        if (item.agent !== C.MAIN && !worker) tags.push('to ' + item.agent);
        if (item.source === 'terminal') tags.push('typed in the terminal');
        if (item.source === 'app') {
          const app = (status && status.info && status.info.app) || 'an app';
          tags.push('typed in ' + app);
        }
        if (tags.length) box.append(element('div', 'tag', tags.join(' · ')));
        break;
      }
      case 'text':
        box.append(md(item.text));
        break;
      case 'thinking': {
        // The agent's work, not what it says: folded to its first line,
        // and kept open across the text that streams in.
        const details = element('details', 'thinking');
        details.open = openThinking.has(item.id);
        details.addEventListener('toggle', () => {
          if (details.open) openThinking.add(item.id);
          else openThinking.delete(item.id);
        });
        const summary = document.createElement('summary');
        summary.append(element('span', 'mark', '▸'), element('span', 'what', 'Thinking'));
        const first = C.firstLine(item.text);
        if (first) summary.append(element('span', 'first', first));
        details.append(summary, md(item.text, 'thinking-text'));
        box.append(details);
        break;
      }
      case 'message': {
        // A worker's message says whose it is.
        const from = item.from && (item.from === C.MAIN ? 'the lead' : item.from);
        box.append(element('span', 'label', from ? 'Message from ' + from : 'Message'), md(item.text));
        break;
      }
      case 'reply':
        if (item.agent !== C.MAIN) {
          box.classList.add('report');
          box.append(element('span', 'label', 'Report'));
        }
        box.append(md(item.text));
        break;
      case 'question':
        box.append(element('span', 'label', 'The agent asks'), md(item.text));
        break;
      case 'call':
        // One line; a tap shows the whole of a long one.
        if (item.detail || item.text.length > 44) {
          const details = element('details', 'call');
          const summary = document.createElement('summary');
          callLine(item, summary);
          details.append(summary, element('pre', null, item.detail || item.text));
          box.append(details);
        } else {
          callLine(item, box);
        }
        break;
      case 'result':
        box.append(element('pre', null, item.detail || item.text));
        break;
      case 'output':
        box.append(element('pre', null, item.text));
        break;
      case 'turn': {
        const line = element('div', 'turn-line');
        line.append(element('span', null, item.text));
        box.append(line);
        // The plan's usage goes under the line whole, instead of wrapping into it.
        if (item.detail) box.append(element('div', 'turn-plan', item.detail));
        break;
      }
      case 'conversation': {
        // Where the conversation changes (/new, /resume): a rule with its name.
        const line = element('div', 'turn-line');
        line.append(element('span', null, item.text));
        box.append(line);
        break;
      }
      case 'worker_line':
        box.className = 'row wline';
        box.append(workerCard(item.worker));
        break;
      case 'notice':
        if (item.agent !== C.MAIN && !worker) box.append(element('span', 'label', item.agent));
        box.append(item.text);
        break;
      default:
        box.append(item.text);
    }
    if (item.detail && !['call', 'result', 'turn'].includes(item.kind)) {
      const details = element('details', 'more');
      details.append(element('summary', null, 'More'), element('pre', null, item.detail));
      box.append(details);
    }
    return box;
  }

  // ----- the transcript: layout, following, the reader's place -----

  /** The rows shown, and their elements by key. */
  let rows = [];
  /** The view's rows of items, without "Working…". */
  let laidOut = [];
  /** What shows the agent is under way, as last drawn. */
  let moving = { thinking: null, working: false };
  const nodes = new Map();
  /** Keys whose element must be built again (a change, more text). */
  const stale = new Set();
  /** The thinking items the reader opened. */
  const openThinking = new Set();
  let relayout = true;
  let frame = 0;
  /** The view follows new output while the reader is at the bottom. */
  let following = true;
  /** The newest item the reader has seen at the bottom. */
  let seen = 0;
  /** Text came below since the reader left the bottom. */
  let grew = false;
  /** A place to keep across the next draw: `{ id, offset }`. */
  let keep = null;
  /** The scroll position the page itself set last. */
  let ownTop = null;
  let lastTop = 0;
  let touching = false;

  function draw() {
    if (!frame) frame = requestAnimationFrame(flush);
  }

  /** The row the reader is looking at: the first one whose bottom shows
   * (of those in `keys`, when given: the rows the next layout keeps), and
   * how far its top is from the log's. */
  function place(keys) {
    const top = log.getBoundingClientRect().top;
    for (const row of rows) {
      if (row.working || (keys && !keys.has(row.key))) continue;
      const node = nodes.get(row.key);
      if (!node || !node.isConnected) continue;
      const box = node.getBoundingClientRect();
      if (box.bottom > top + 1) return { id: row.id, offset: box.top - top };
    }
    return null;
  }

  function atBottom() {
    return log.scrollHeight - log.clientHeight - log.scrollTop <= 4;
  }

  function setTop(top) {
    log.scrollTop = top;
    ownTop = log.scrollTop;
    lastTop = ownTop;
  }

  function flush() {
    frame = 0;
    let anchor = following ? null : keep;
    keep = null;
    if (relayout) {
      const next = C.layout(store.items, view, worker);
      // The reader's place: a row they can see that stays (a view that
      // hides the top one keeps the next one where it was).
      if (!following && !anchor) anchor = place(new Set(next.map((row) => row.key)));
      laidOut = next;
      relayout = false;
    } else if (!following && !anchor) {
      anchor = place();
    }
    moving = motionNow();
    rows = C.withWorking(laidOut, moving.working);
    const wanted = new Set();
    let cursor = list.firstChild;
    for (const row of rows) {
      wanted.add(row.key);
      let node = nodes.get(row.key);
      const sig = row.count ? JSON.stringify(row.count) : '';
      if (!node || stale.has(row.key) || (row.count && node.dataset.sig !== sig)) {
        const fresh = renderRow(row);
        if (node) {
          if (node === cursor) cursor = fresh;
          node.replaceWith(fresh);
        }
        node = fresh;
        nodes.set(row.key, node);
        node.dataset.key = row.key;
        if (row.count) node.dataset.sig = sig;
      }
      if (row.item && row.item.kind === 'thinking') node.classList.toggle('streaming', row.id === moving.thinking);
      if (node !== cursor) list.insertBefore(node, cursor);
      else cursor = cursor.nextSibling;
    }
    stale.clear();
    for (const [key, node] of nodes) {
      if (!wanted.has(key)) {
        node.remove();
        nodes.delete(key);
      }
    }
    $('empty').hidden = rows.length > 0;
    if (!rows.length) {
      // A worker of a resumed session ran before this history began.
      const summary = worker && store.workers.get(worker);
      $('empty').textContent = !worker
        ? 'Nothing here yet.'
        : summary && summary.report
          ? 'Its work came before this page’s history began. It reported “' + summary.report + '”'
          : 'Nothing from it yet.';
    }
    if (following) {
      setTop(log.scrollHeight);
      seen = newest();
    } else if (anchor) {
      const row = C.nearest(rows, anchor.id);
      const node = row && nodes.get(row.key);
      if (node) {
        const offset = node.getBoundingClientRect().top - log.getBoundingClientRect().top;
        setTop(log.scrollTop + offset - anchor.offset);
        // A shorter view can't keep the place: it ends at the bottom, and
        // the bottom follows.
        if (atBottom()) {
          following = true;
          grew = false;
          seen = newest();
        }
      }
    }
    pill();
  }

  /** What shows the agent is under way now, while the page is live. */
  function motionNow() {
    if (connection !== 'live') return { thinking: null, working: false };
    return C.motion(store, status, view, worker, Date.now());
  }

  /** Draws the transcript again if what shows the agent is under way
   * changed (its status, the connection, or text that went quiet). */
  function followMotion() {
    const now = motionNow();
    if (now.working !== moving.working || now.thinking !== moving.thinking) draw();
  }

  function newest() {
    const last = store.items[store.items.length - 1];
    return last ? last.id : 0;
  }

  /** "↓ N new" while the reader is up the transcript. */
  function pill() {
    const jump = $('jump');
    if (following) {
      jump.hidden = true;
      return;
    }
    const fresh = C.unseen(rows, seen);
    const far = log.scrollHeight - log.clientHeight - log.scrollTop > log.clientHeight;
    jump.hidden = !fresh && !grew && !far;
    jump.classList.toggle('quiet', !fresh && !grew);
    $('jump-text').textContent = fresh ? fresh + ' new' : grew ? 'New text' : 'Latest';
  }

  function follow() {
    following = true;
    grew = false;
    seen = newest();
    setTop(log.scrollHeight);
    pill();
  }

  /** The reader took the scroll: stop following at once. */
  function letGo() {
    if (following) {
      following = false;
      pill();
    }
  }

  log.addEventListener(
    'scroll',
    () => {
      const top = log.scrollTop;
      if (ownTop !== null && Math.abs(top - ownTop) < 2) {
        ownTop = null;
        lastTop = top;
        return;
      }
      ownTop = null;
      if (top < lastTop - 1) letGo();
      lastTop = top;
      if (!following && !touching && atBottom()) follow();
      else pill();
    },
    { passive: true },
  );
  log.addEventListener('wheel', (event) => event.deltaY < 0 && letGo(), { passive: true });
  log.addEventListener('touchstart', () => (touching = true), { passive: true });
  log.addEventListener('touchmove', letGo, { passive: true });
  log.addEventListener(
    'touchend',
    () => {
      touching = false;
      if (atBottom()) follow();
    },
    { passive: true },
  );
  document.addEventListener('keydown', (event) => {
    if (event.target === text || event.target instanceof HTMLInputElement || document.querySelector('dialog[open]')) return;
    if (['PageUp', 'ArrowUp', 'Home'].includes(event.key) || (event.key === ' ' && event.shiftKey)) letGo();
    // End: back to the latest, as the pill does.
    if (event.key === 'End' && !following) {
      event.preventDefault();
      follow();
    }
  });
  $('jump').addEventListener('click', follow);

  // ----- what the stream says -----

  function rebuildWorkerRows(id) {
    for (const row of rows) {
      if (row.item && row.item.kind === 'worker_line' && row.item.worker === id) stale.add(row.key);
    }
  }

  function onReset(data) {
    const before = newest();
    skew = null;
    if (!following) keep = place();
    store.reset(data.items, data.workers, data.jobs);
    nodes.clear();
    list.replaceChildren();
    relayout = true;
    // A reconnect keeps the reader's place; what came while away is new.
    if (!following && seen === 0) seen = before;
    onStatus(data.status);
    flush();
    renderWorkers();
  }

  function onItem(item) {
    // This device's clock against Arko's: the smallest gap a new item
    // shows (the rest is the network's delay).
    const sample = Date.now() - item.at;
    skew = skew === null ? sample : Math.min(skew, sample);
    store.add(item);
    relayout = true;
    draw();
    heard(item);
  }

  function onAppend(data) {
    if (!store.append(data.id, data.text)) return;
    if (!following && nodes.has('i' + data.id)) grew = true;
    stale.add('i' + data.id);
    draw();
  }

  function onChange(item) {
    if (!store.change(item)) return;
    stale.add('i' + item.id);
    // A call's status moves a Messages count.
    if (view === 'messages') relayout = true;
    draw();
  }

  /** The item `id` is gone. It held text a failed response streamed, or
   * it was a note the agent read, which comes again where the agent read
   * it. */
  function onRemove(data) {
    if (!store.remove(data.id)) return;
    relayout = true;
    draw();
  }

  function onWorker(summary) {
    store.worker(summary);
    rebuildWorkerRows(summary.id);
    draw();
    renderWorkers();
  }

  /** A background job changed: the line of the call that made it offers
   * Stop while it runs. */
  function onJob(summary) {
    store.job(summary);
    if (summary.call != null) stale.add('i' + summary.call);
    draw();
  }

  // ----- the status -----

  function onStatus(next) {
    const asked = status && status.question;
    const before = status && status.movable;
    status = next;
    // The calls that can move changed: their lines offer it, or no longer.
    C.movableChanged(before, status && status.movable, (id) => stale.add('i' + id), draw);
    renderStatus();
    if (JSON.stringify(asked) !== JSON.stringify(status.question)) renderAsk();
    renderSettings();
    followMotion();
  }

  function renderStatus() {
    const state = C.stateOf(status, connection);
    $('state').className = 'state ' + state.key;
    $('state-label').textContent = state.label;
    $('figures').textContent = C.figures(status).join(' · ');
    const settings = status && status.settings;
    $('figures').title =
      status && status.context_tokens != null
        ? 'Context ' + C.tokens(status.context_tokens) + ' tokens' + (settings && settings.handover_at ? ', handover size ' + C.tokens(settings.handover_at) : '')
        : '';
    $('outside').hidden = !(status && status.settings && status.settings.outside);
    const working = status && status.working && connection === 'live';
    stop.hidden = !working;
    tick();
    placeholder();
    document.title = 'Arko' + (state.key === 'asking' ? ' · asks you' : state.key === 'working' ? ' · working' : '');
  }

  /** The clocks: how long the turn has gone on, and the workers' times. */
  function tick() {
    const time = $('state-time');
    if (status && status.working && connection === 'live') {
      const { since } = store.currentTurn();
      time.textContent = since ? C.duration(serverNow() - since) : '';
    } else {
      time.textContent = '';
    }
    for (const node of document.querySelectorAll('.wcard.running .wmeta')) {
      const summary = store.workers.get(node.parentElement.dataset.worker);
      if (summary) node.textContent = C.workerWords(summary, serverNow()).meta;
    }
    if (worker) renderBanner();
    // Once the agent's text has been quiet a while, "Working…" comes.
    followMotion();
  }
  setInterval(tick, 1000);

  function placeholder() {
    if (asking()) text.placeholder = 'Your answer';
    else if (worker) text.placeholder = 'Message ' + worker;
    else text.placeholder = status && status.working ? 'Add to the task' : 'Message';
    send.title = asking() ? 'Answer' : 'Send';
    text.setAttribute('aria-label', text.placeholder);
  }

  function asking() {
    return Boolean(status && status.question);
  }

  // ----- the agent's question, docked above the composer -----

  let askedId = null;

  function renderAsk() {
    const ask = $('ask');
    const question = status && status.question;
    if (!question) {
      ask.hidden = true;
      askedId = null;
      placeholder();
      return;
    }
    if (question.id !== askedId) {
      askedId = question.id;
      ask.classList.remove('folded');
      $('ask-fold').textContent = 'Hide';
      $('ask-fold').setAttribute('aria-expanded', 'true');
      const body = $('ask-text');
      body.replaceChildren();
      markdown(C.markdown(question.text), body);
    }
    ask.hidden = false;
    placeholder();
  }

  $('ask-fold').addEventListener('click', () => {
    const folded = $('ask').classList.toggle('folded');
    $('ask-fold').textContent = folded ? 'Show' : 'Hide';
    $('ask-fold').setAttribute('aria-expanded', String(!folded));
  });

  // ----- views and the worker view -----

  function setView(next, at) {
    if (!C.VIEWS[next]) return;
    if (next !== view || at !== undefined) {
      if (at !== undefined) {
        following = false;
        keep = { id: at, offset: log.clientHeight / 3 };
      }
      view = next;
      save('arko.view', view);
      relayout = true;
      draw();
    }
    renderViews();
    closeMenu();
  }

  function renderViews() {
    $('view-name').textContent = C.VIEWS[view].name;
    for (const button of document.querySelectorAll('.views button')) {
      button.setAttribute('aria-checked', String(button.dataset.view === view));
    }
    for (const container of [$('view-menu'), $('view-options')]) {
      container.replaceChildren();
      for (const [key, about] of Object.entries(C.VIEWS)) {
        const option = element('button', container.id === 'view-menu' ? null : 'option');
        option.type = 'button';
        option.setAttribute('role', container.id === 'view-menu' ? 'menuitemradio' : 'radio');
        option.setAttribute('aria-checked', String(key === view));
        option.append(element('span', 'check', key === view ? '✓' : ''), element('span', 'name', about.name), element('span', 'about', about.about));
        option.addEventListener('click', () => setView(key));
        container.append(option);
      }
    }
  }

  for (const button of document.querySelectorAll('.views button')) {
    button.addEventListener('click', () => setView(button.dataset.view));
  }

  function closeMenu() {
    $('view-menu').hidden = true;
    $('view-button').setAttribute('aria-expanded', 'false');
  }

  $('view-button').addEventListener('click', (event) => {
    event.stopPropagation();
    const menu = $('view-menu');
    menu.hidden = !menu.hidden;
    $('view-button').setAttribute('aria-expanded', String(!menu.hidden));
  });
  document.addEventListener('click', (event) => {
    if (!$('view-menu').hidden && !$('view-menu').contains(event.target)) closeMenu();
  });

  /** Where the reader was in the main transcript, to come back to. */
  let mainPlace = null;

  /** Shows the worker `id`'s own transcript (null: the main one). A
   * worker's opens at its latest; going back returns to where the reader
   * was. */
  function showWorker(id, fromHistory) {
    if (id === worker) return;
    if (!worker) mainPlace = following ? null : place();
    worker = id;
    following = !(!id && mainPlace);
    keep = !id && mainPlace ? mainPlace : null;
    if (!id) mainPlace = null;
    if (!fromHistory) {
      if (id) history.pushState({ worker: id }, '');
      else if (history.state && history.state.worker) history.back();
    }
    $('banner').hidden = !id;
    if (id) renderBanner();
    relayout = true;
    nodes.clear();
    list.replaceChildren();
    placeholder();
    draw();
  }

  function renderBanner() {
    const summary = store.workers.get(worker) || { id: worker, state: 'running', tool_calls: 0 };
    const words = C.workerWords(summary, serverNow());
    $('banner-title').textContent = worker + ' · ' + words.title;
    $('banner-meta').textContent = words.meta;
  }

  $('back').addEventListener('click', () => showWorker(null));
  window.addEventListener('popstate', (event) => showWorker((event.state && event.state.worker) || null, true));

  list.addEventListener('click', (event) => {
    const card = event.target.closest('.wcard');
    if (card) showWorker(card.dataset.worker);
  });

  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape' || document.querySelector('dialog[open]')) return;
    if (!$('view-menu').hidden) closeMenu();
    else if (worker) showWorker(null);
  });

  // ----- sheets: the workers and the settings -----

  function openSheet(dialog) {
    closeMenu();
    if (typeof dialog.showModal === 'function') dialog.showModal();
    else dialog.setAttribute('open', '');
  }

  for (const dialog of document.querySelectorAll('dialog')) {
    dialog.addEventListener('click', (event) => {
      // A tap on the backdrop (the dialog itself, outside its content) closes it.
      if (event.target === dialog || event.target.closest('[data-close]')) dialog.close();
    });
  }

  function renderWorkers() {
    const ids = [...store.workers.keys()];
    const running = [...store.workers.values()].filter((w) => w.state === 'running' || w.state === 'queued').length;
    $('workers-button').hidden = ids.length === 0;
    $('workers-count').textContent = C.plural(ids.length, 'worker', 'workers');
    $('workers-button').classList.toggle('running', running > 0);
    $('workers-button').title = running ? running + ' running' : 'None running';
    if (!$('workers').open) return;
    const list = $('workers-list');
    list.replaceChildren();
    const main = element('button', 'wcard main');
    main.type = 'button';
    main.dataset.worker = '';
    const title = element('span', 'wtitle', 'Main agent');
    main.append(element('span', 'wdot'), title, element('span', 'wgo'), element('span', 'wmeta', worker ? 'Back to the main transcript' : 'Shown now'));
    main.lastChild.previousSibling.append(icon(CHEVRON));
    list.append(main);
    // Running and queued ones first, then the latest.
    const active = (id) => ['running', 'queued'].includes(store.workers.get(id).state);
    for (const id of ids.filter(active)) list.append(workerCard(id));
    for (const id of ids.filter((id) => !active(id)).reverse()) list.append(workerCard(id));
  }

  $('workers-button').addEventListener('click', () => {
    openSheet($('workers'));
    renderWorkers();
  });
  $('workers-list').addEventListener('click', (event) => {
    const card = event.target.closest('.wcard');
    if (!card) return;
    $('workers').close();
    showWorker(card.dataset.worker || null);
  });

  $('settings-button').addEventListener('click', () => {
    renderSettings();
    openSheet($('settings'));
    loadLinks();
  });

  /**
   * A command sent from the sheet (`/outside on`, `/handover-at 200k`,
   * `/handover`), waiting for Arko: `{ command, note, done(settings),
   * lines }`. The status shows a setting once it changed; the lines are
   * what Arko answered in the transcript (an error for a bad value), shown
   * under the setting so the reader needn't leave the sheet.
   */
  let pending = null;

  function renderSettings() {
    const settings = (status && status.settings) || { outside: false, handover_at: 0 };
    const info = (status && status.info) || {};
    const switcher = $('outside-switch');
    const turning = pending && pending.outside !== undefined;
    switcher.setAttribute('aria-checked', String(turning ? pending.outside : settings.outside));
    switcher.setAttribute('aria-busy', String(Boolean(turning)));
    $('outside-about').textContent = settings.outside
      ? 'The agents may work anywhere on this computer.'
      : 'The agents stay in ' + (info.workspace || 'the workspace') + '.';
    $('handover-value').textContent = settings.handover_at ? C.tokens(settings.handover_at) + ' tokens' : 'Not set';
    // The workers' size, in a session with workers: Set sets both, or the one chosen.
    const hasWorkers = Boolean(status && status.models && status.models.has_workers);
    $('workers-handover').hidden = !hasWorkers;
    $('handover-whose').hidden = !hasWorkers;
    $('workers-handover-value').textContent = settings.workers_handover_at ? C.tokens(settings.workers_handover_at) + ' tokens' : 'Not set';
    $('background-value').textContent = settings.background_after ? duration(settings.background_after) : 'Off';
    $('wakes-value').textContent = C.wakesWords(settings.monitor_wakes);
    // Text replies change only before the conversation starts.
    const replies = $('replies-switch');
    const replying = pending && pending.replies !== undefined;
    const started = Boolean(status && status.models && status.models.started);
    replies.setAttribute('aria-checked', String(replying ? pending.replies : Boolean(settings.text_replies)));
    replies.setAttribute('aria-busy', String(Boolean(replying)));
    replies.disabled = started && !replying;
    $('replies-about').textContent = (settings.text_replies
      ? 'The agent ends its turn with a plain reply.'
      : 'The agent ends its turn by calling complete_task.')
      + (started ? ' It keeps this for the session.' : ' It changes before the conversation starts.');
    // What agents on cc: models do at the handover size changes only before the conversation starts.
    const models = (status && status.models) || {};
    const compacting = pending && pending.compact !== undefined;
    const compacts = compacting ? pending.compact : settings.cc_compact !== false;
    const compact = $('cc-compact-switch');
    $('cc-compact').hidden = ![models.main, models.workers].some((model) => String(model || '').startsWith('cc:'));
    compact.setAttribute('aria-checked', String(compacts));
    compact.setAttribute('aria-busy', String(Boolean(compacting)));
    compact.disabled = started && !compacting;
    $('cc-compact-about').textContent = (compacts
      ? 'At the handover size, Claude Code compacts the conversation of the agents on cc: models.'
      : 'At the handover size, the agents on cc: models write a report and go on in a fresh conversation.')
      + (started ? ' The session keeps this.' : ' It changes before the conversation starts.');
    for (const node of document.querySelectorAll('.cc-compacts')) node.hidden = !compacts;
    // The extra rules change only before the conversation starts.
    const choosing = pending && pending.rules !== undefined;
    const chosen = choosing ? pending.rules : (settings.extra_rules || 'off');
    const rules = $('rules-options');
    rules.replaceChildren();
    for (const [key, about] of Object.entries(RULES)) {
      const option = element('button', 'option');
      option.type = 'button';
      option.setAttribute('role', 'radio');
      option.setAttribute('aria-checked', String(key === chosen));
      option.disabled = started && !choosing;
      option.append(element('span', 'check', key === chosen ? '✓' : ''), element('span', 'name', about.name), element('span', 'about', about.about));
      option.addEventListener('click', () => chooseRules(key));
      rules.append(option);
    }
    $('rules-about').textContent = started ? 'The session keeps this.' : 'It changes before the conversation starts.';
    // The project's instructions change at any time.
    const picking = pending && pending.project !== undefined;
    const files = picking ? pending.project : (settings.project_instructions || 'one');
    const project = $('project-options');
    project.replaceChildren();
    for (const [key, about] of Object.entries(PROJECT)) {
      const option = element('button', 'option');
      option.type = 'button';
      option.setAttribute('role', 'radio');
      option.setAttribute('aria-checked', String(key === files));
      option.append(element('span', 'check', key === files ? '✓' : ''), element('span', 'name', about.name), element('span', 'about', about.about));
      option.addEventListener('click', () => chooseProject(key));
      project.append(option);
    }
    $('project-workers').hidden = !(status && status.models && status.models.has_workers);
    const toWorkers = $('project-workers-switch');
    const giving = pending && pending.workers !== undefined;
    const workers = giving ? pending.workers : Boolean(settings.worker_project_instructions);
    toWorkers.setAttribute('aria-checked', String(workers));
    toWorkers.setAttribute('aria-busy', String(Boolean(giving)));
    $('project-workers-about').textContent = workers
      ? 'Each worker gets them once, in its first message.'
      : 'Only the main agent gets them.';
    if (pending && pending.done && pending.done(settings)) settle(pending, true);
    const rows = [
      ['Model', info.model],
      ['Workspace', info.workspace],
      ['Session', info.session],
    ];
    const dl = $('info');
    dl.replaceChildren();
    for (const [name, value] of rows) {
      if (!value) continue;
      dl.append(element('dt', null, name), element('dd', null, value));
    }
    $('workers-more').hidden = !(status && status.workers);
    $('workers-text').textContent = (status && status.workers) || '';
  }

  /** A time in seconds as the sheet says it: `10 min`, `90 s`, `1 h`. */
  function duration(seconds) {
    if (seconds % 3600 === 0) return seconds / 3600 + ' h';
    if (seconds % 60 === 0) return seconds / 60 + ' min';
    return seconds + ' s';
  }

  function note(id, message, error) {
    const node = $(id);
    node.textContent = message || '';
    node.hidden = !message;
    node.classList.toggle('error', Boolean(error));
  }

  /** The command is through: it worked (`ok`), or Arko said why not. */
  function settle(change, ok) {
    clearTimeout(change.timer);
    clearTimeout(change.wait);
    if (pending === change) pending = null;
    const said = change.lines.join('\n');
    if (ok) note(change.note, said);
    else note(change.note, said || 'Arko didn’t change it. The transcript says why.', true);
    renderSettings();
  }

  /** What Arko says after the sheet's command: its answer, for the sheet. */
  function heard(item) {
    const change = pending;
    if (!change) return;
    if (item.kind === 'user' && item.delivery === 'command' && item.text === change.command) {
      change.answered = true;
      return;
    }
    if (!change.answered || !['output', 'notice'].includes(item.kind)) return;
    change.lines.push(item.text);
    if (!change.done) {
      // A command that changes no setting (a handover) is through once Arko answers.
      settle(change, item.level !== 'error');
      return;
    }
    // An answer, and the status still the same a moment later: it didn't take.
    clearTimeout(change.wait);
    change.wait = setTimeout(() => pending === change && settle(change, false), 1500);
  }

  /** Sends `command` from the sheet; `done(settings)` says when the
   * status shows it took, and `shows` what the sheet shows meanwhile
   * (`{ outside: true }`). */
  async function command(text, noteId, done, shows) {
    if (pending) return false;
    note(noteId, '');
    const change = { command: text, note: noteId, done, ...shows, lines: [] };
    pending = change;
    renderSettings();
    if (!(await post('/api/message', { text }))) {
      clearTimeout(change.timer);
      pending = null;
      renderSettings();
      note(noteId, $('error').textContent || 'Not sent.', true);
      return false;
    }
    change.timer = setTimeout(() => pending === change && settle(change, false), 10000);
    return true;
  }

  $('outside-switch').addEventListener('click', () => {
    if (pending) return;
    const on = Boolean(status && status.settings && status.settings.outside);
    if (on) {
      // Turning it off is the safe way: no question asked.
      command('/outside off', 'outside-note', (settings) => !settings.outside, { outside: false });
    } else {
      $('outside-confirm').hidden = false;
      $('outside-allow').focus();
    }
  });
  $('outside-cancel').addEventListener('click', () => ($('outside-confirm').hidden = true));
  $('outside-allow').addEventListener('click', () => {
    $('outside-confirm').hidden = true;
    command('/outside on', 'outside-note', (settings) => settings.outside, { outside: true });
  });

  $('replies-switch').addEventListener('click', () => {
    if (pending) return;
    const on = !(status && status.settings && status.settings.text_replies);
    command('/text-replies ' + (on ? 'on' : 'off'), 'replies-note', (settings) => Boolean(settings.text_replies) === on, { replies: on });
  });

  $('cc-compact-switch').addEventListener('click', () => {
    if (pending) return;
    const on = !(status && status.settings && status.settings.cc_compact);
    command('/cc-compact ' + (on ? 'on' : 'off'), 'handover-note', (settings) => Boolean(settings.cc_compact) === on, { compact: on });
  });

  function chooseRules(key) {
    if (pending) return;
    const now = (status && status.settings && status.settings.extra_rules) || 'off';
    if (key === now) return;
    command('/extra-rules ' + key, 'rules-note', (settings) => settings.extra_rules === key, { rules: key });
  }

  function chooseProject(key) {
    if (pending) return;
    const now = (status && status.settings && status.settings.project_instructions) || 'one';
    if (key === now) return;
    command('/project-instructions ' + key, 'project-note', (settings) => settings.project_instructions === key, { project: key });
  }

  $('project-workers-switch').addEventListener('click', () => {
    if (pending) return;
    const on = !(status && status.settings && status.settings.worker_project_instructions);
    command('/project-instructions workers ' + (on ? 'on' : 'off'), 'project-note', (settings) => Boolean(settings.worker_project_instructions) === on, { workers: on });
  });

  $('handover-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const size = $('handover-input').value.trim();
    if (!size) {
      note('handover-note', 'Give a size, such as 200k.', true);
      return;
    }
    const now = (status && status.settings) || {};
    const before = [now.handover_at, now.workers_handover_at];
    const whose = $('handover-whose').hidden ? '' : $('handover-whose').value;
    // Arko reads the size ("200k", "200000") and says what's wrong with it.
    command('/handover-at ' + (whose ? whose + ' ' : '') + size, 'handover-note', (settings) => settings.handover_at !== before[0] || settings.workers_handover_at !== before[1]).then((sent) => {
      if (sent) $('handover-input').value = '';
    });
  });
  $('background-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const time = $('background-input').value.trim();
    if (!time) {
      note('background-note', 'Give a time, such as 10m, or off.', true);
      return;
    }
    const before = status && status.settings ? status.settings.background_after : 0;
    // Arko reads the time ("90s", "10m", "off") and says what's wrong with it.
    command('/background-after ' + time, 'background-note', (settings) => settings.background_after !== before).then((sent) => {
      if (sent) $('background-input').value = '';
    });
  });
  $('wakes-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const time = $('wakes-input').value.trim();
    if (!time) {
      note('wakes-note', 'Give a time, such as 2h, or always, or off.', true);
      return;
    }
    const before = status && status.settings ? status.settings.monitor_wakes : undefined;
    // Arko reads the time ("30m", "2h", "always", "off") and says what's wrong with it.
    command('/monitor-wakes ' + time, 'wakes-note', (settings) => settings.monitor_wakes !== before).then((sent) => {
      if (sent) $('wakes-input').value = '';
    });
  });
  $('handover-now').addEventListener('click', () => ($('handover-confirm').hidden = false));
  $('handover-cancel').addEventListener('click', () => ($('handover-confirm').hidden = true));
  $('handover-go').addEventListener('click', () => {
    $('handover-confirm').hidden = true;
    command('/handover', 'handover-note', null);
  });

  /** The links to the page, with their codes, when Arko offers them (the
   * section stays hidden when it doesn't). */
  async function loadLinks() {
    let links;
    try {
      const response = await fetch('/api/links', { credentials: 'same-origin' });
      if (!response.ok) throw new Error(String(response.status));
      links = await response.json();
    } catch {
      $('links-group').hidden = true;
      return;
    }
    if (!Array.isArray(links) || links.length === 0) {
      $('links-group').hidden = true;
      return;
    }
    $('links-group').hidden = false;
    $('links-first').replaceChildren(...linkCard(links[0]));
    const rest = $('links-rest');
    rest.replaceChildren();
    for (const link of links.slice(1)) {
      const card = element('div', 'link-card');
      card.append(...linkCard(link));
      rest.append(card);
    }
    $('links-more').hidden = links.length < 2;
    $('links-more-label').textContent = 'Other addresses (' + (links.length - 1) + ')';
  }

  const KINDS = {
    lan: 'Local network',
    tailscale: 'Tailscale',
    vpn: 'VPN',
    other: 'Other network',
    loopback: 'This computer only',
  };

  function linkCard(link) {
    const qr = element('div', 'qr');
    const svg = safeSvg(link.qr_svg);
    if (svg) qr.append(svg);
    const url = element('input', 'link-url');
    url.readOnly = true;
    url.value = link.url;
    url.setAttribute('aria-label', 'Link');
    url.addEventListener('focus', () => url.select());
    const words = element('div', 'link-text');
    words.append(element('div', 'link-where', link.interface || 'Link'), element('div', 'link-kind', KINDS[link.kind] || ''), url);
    return [qr, words];
  }

  /** Arko's QR code, as an element: only shapes, never scripts or
   * handlers, whatever the text says. */
  function safeSvg(source) {
    if (typeof source !== 'string') return null;
    const doc = new DOMParser().parseFromString(source, 'image/svg+xml');
    const svg = doc.documentElement;
    if (!svg || svg.nodeName !== 'svg') return null;
    const allowed = new Set(['svg', 'path', 'rect', 'g']);
    for (const node of [svg, ...svg.querySelectorAll('*')]) {
      if (!allowed.has(node.nodeName)) {
        node.remove();
        continue;
      }
      for (const attribute of [...node.attributes]) {
        if (/^on/i.test(attribute.name) || /href/i.test(attribute.name)) node.removeAttribute(attribute.name);
      }
    }
    return document.importNode(svg, true);
  }

  // ----- access -----

  /** Trades the key for a session cookie: 'ok', 'wrong', 'wait' or 'offline'. */
  async function login(key) {
    key = key || sessionGet(KEY);
    if (!key) return 'wrong';
    let response;
    try {
      response = await fetch('/api/session', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ key }),
      });
    } catch {
      return 'offline';
    }
    if (response.status === 204) {
      sessionSet(KEY, key);
      return 'ok';
    }
    if (response.status === 429) return 'wait';
    if (response.status === 401) sessionSet(KEY, null);
    return 'wrong';
  }

  function lock(why) {
    if (stream) stream.close();
    stream = null;
    $('lock-why').textContent =
      why === 'wait'
        ? 'Too many wrong keys. Wait a minute, then try again.'
        : 'Open the link Arko printed in its console, which holds the key. (A new session, or /remote new-key, makes a new one.)';
    $('lock').hidden = false;
    $('app').hidden = true;
  }

  function unlock() {
    $('lock').hidden = true;
    $('app').hidden = false;
  }

  $('key-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const result = await login($('key').value.trim());
    $('key').value = '';
    if (result === 'ok') {
      unlock();
      connect();
    } else {
      lock(result);
    }
  });

  // ----- the stream -----

  let stream = null;
  let retry = 1000;
  let retryTimer = 0;
  let failures = 0;

  async function start() {
    clearTimeout(retryTimer);
    retryTimer = 0;
    let response = null;
    try {
      response = await fetch('/api/session', { credentials: 'same-origin' });
    } catch {
      return offline();
    }
    if (response.status === 401) {
      const result = await login();
      if (result === 'offline') return offline();
      if (result !== 'ok') return lock(result);
    } else if (response.status === 429) {
      return lock('wait');
    } else if (!response.ok) {
      return offline();
    }
    unlock();
    connect();
  }

  function setConnection(next) {
    connection = next;
    renderStatus();
    followMotion();
    $('offline').hidden = next !== 'offline';
  }

  function offline() {
    failures += 1;
    // A dropped stream reconnects quietly at first: a phone that slept
    // comes back within a second or two.
    setConnection(failures > 1 ? 'offline' : 'connecting');
    $('offline-text').textContent =
      failures > 3 ? 'Arko isn’t reachable, and it may have quit. The page reconnects when it’s back.' : 'Not connected. Reconnecting…';
    retryTimer = setTimeout(start, retry);
    retry = Math.min(retry * 2, 15000);
  }

  function connect() {
    if (stream) stream.close();
    stream = new EventSource('/api/events');
    stream.addEventListener('reset', (event) => {
      retry = 1000;
      failures = 0;
      setConnection('live');
      onReset(JSON.parse(event.data));
    });
    stream.addEventListener('item', (event) => onItem(JSON.parse(event.data)));
    stream.addEventListener('append', (event) => onAppend(JSON.parse(event.data)));
    stream.addEventListener('change', (event) => onChange(JSON.parse(event.data)));
    stream.addEventListener('remove', (event) => onRemove(JSON.parse(event.data)));
    stream.addEventListener('worker', (event) => onWorker(JSON.parse(event.data)));
    stream.addEventListener('job', (event) => onJob(JSON.parse(event.data)));
    stream.addEventListener('status', (event) => onStatus(JSON.parse(event.data)));
    stream.onerror = () => {
      stream.close();
      stream = null;
      offline();
    };
  }

  $('retry').addEventListener('click', () => {
    retry = 1000;
    start();
  });

  // A phone back from sleep or another app reconnects at once.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible' && !stream && $('lock').hidden) {
      retry = 1000;
      start();
    }
  });

  // ----- actions -----

  function showError(message) {
    $('error').textContent = message;
    $('error').hidden = !message;
  }

  async function post(path, body) {
    for (let attempt = 0; attempt < 2; attempt++) {
      let response;
      try {
        response = await fetch(path, {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body || {}),
        });
      } catch {
        showError('Not sent, because Arko is out of reach. Your text is kept.');
        return false;
      }
      if (response.status === 401 && attempt === 0) {
        const result = await login();
        if (result === 'ok') continue;
        lock(result);
        return false;
      }
      if (!response.ok) {
        const why = await response.json().catch(() => ({}));
        if (why.error) showError('Not sent (' + why.error + ').');
        // A proxy in front of Arko (Tailscale Serve, say) answers for it.
        else if ([502, 503, 504].includes(response.status)) showError('Not sent, because Arko is out of reach. Your text is kept.');
        else showError('Not sent (' + response.status + ').');
        return false;
      }
      showError('');
      return true;
    }
    return false;
  }

  function grow() {
    text.style.height = 'auto';
    text.style.height = text.scrollHeight + 2 + 'px';
    send.disabled = sending || !text.value.trim();
    sessionSet('arko.draft', text.value);
  }

  let sending = false;

  async function submit() {
    const message = text.value;
    if (!message.trim() || sending) return;
    sending = true;
    send.disabled = true;
    const question = asking() ? status.question.id : null;
    // In a worker's own view it goes to that worker: the session tells its
    // commands from text, and the main agent's question takes the answer.
    const sent =
      question !== null
        ? await post('/api/answer', { question, text: message })
        : await post('/api/message', worker ? { text: message, to: worker } : { text: message });
    sending = false;
    if (sent) {
      text.value = '';
      // What one sends is what one wants to see next.
      follow();
    }
    grow();
  }

  $('compose').addEventListener('submit', (event) => {
    event.preventDefault();
    submit();
  });

  text.addEventListener('input', grow);
  text.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' || event.isComposing) return;
    // Enter sends with a keyboard; on a touch screen it starts a new line
    // (and Ctrl or Cmd+Enter sends).
    if (event.ctrlKey || event.metaKey || (fine.matches && !event.shiftKey && !event.altKey)) {
      event.preventDefault();
      submit();
    }
  });

  stop.addEventListener('click', async () => {
    stop.disabled = true;
    await post('/api/stop');
    // The status hides it when the turn ends; a second tap waits a moment.
    setTimeout(() => (stop.disabled = false), 1500);
  });

  // ----- start -----

  text.value = sessionGet('arko.draft') || '';
  renderViews();
  grow();
  start();
})();
