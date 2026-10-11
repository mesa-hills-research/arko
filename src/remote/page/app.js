// fin's remote page: follows the session's stream (Server-Sent Events) and
// sends messages, answers and stops (POST). Text from the session is only
// ever set as text, never as markup.
'use strict';

(() => {
  const KEY = 'fin.remote.key';
  const MAX_ITEMS = 3000;
  const $ = (id) => document.getElementById(id);

  const log = $('log');
  const state = $('state');
  const figures = $('figures');
  const text = $('text');
  const send = $('send');
  const stop = $('stop');
  const question = $('question');
  const answer = $('answer');
  const jump = $('jump');
  const error = $('error');

  /** The text node holder of each streaming item, by ID. */
  const bodies = new Map();
  let stream = null;
  let retry = 1000;
  let asked = null;
  let following = true;

  // The key: from the link's fragment into this tab's storage, and off the
  // address bar (and so out of bookmarks and shared links).
  const fragment = new URLSearchParams(location.hash.slice(1));
  if (fragment.get('key')) {
    sessionStorage.setItem(KEY, fragment.get('key'));
    history.replaceState(null, '', location.pathname + location.search);
  }

  function element(tag, className, content) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (content !== undefined && content !== null) node.textContent = content;
    return node;
  }

  // ----- access -----

  /** Trades the key for a session cookie: 'ok', 'wrong', 'wait' or 'offline'. */
  async function login(key) {
    key = key || sessionStorage.getItem(KEY);
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
      sessionStorage.setItem(KEY, key);
      return 'ok';
    }
    if (response.status === 429) return 'wait';
    if (response.status === 401) sessionStorage.removeItem(KEY);
    return 'wrong';
  }

  function lock(why) {
    if (stream) stream.close();
    stream = null;
    $('lock-why').textContent =
      why === 'wait'
        ? 'Too many wrong keys: wait a minute, then try again.'
        : 'Open the link fin printed in its console: it holds the key. (A new session, or /remote new-key, makes a new one.)';
    $('lock').hidden = false;
    $('top').hidden = true;
    $('bottom').hidden = true;
    log.hidden = true;
  }

  function unlock() {
    $('lock').hidden = true;
    $('top').hidden = false;
    $('bottom').hidden = false;
    log.hidden = false;
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

  async function start() {
    let response = null;
    try {
      response = await fetch('/api/session', { credentials: 'same-origin' });
    } catch {
      offline();
      return later(start);
    }
    if (response.status === 401) {
      const result = await login();
      if (result === 'offline') {
        offline();
        return later(start);
      }
      if (result !== 'ok') return lock(result);
    } else if (response.status === 429) {
      return lock('wait');
    } else if (!response.ok) {
      offline();
      return later(start);
    }
    unlock();
    connect();
  }

  function later(then) {
    setTimeout(then, retry);
    retry = Math.min(retry * 2, 15000);
  }

  function offline() {
    state.className = 'pill offline';
    state.textContent = 'offline';
    stop.hidden = true;
  }

  function connect() {
    if (stream) stream.close();
    stream = new EventSource('/api/events');
    stream.addEventListener('reset', (event) => {
      retry = 1000;
      const data = JSON.parse(event.data);
      log.replaceChildren();
      bodies.clear();
      for (const item of data.items) add(item, false);
      status(data.status);
      following = true;
      toBottom();
    });
    stream.addEventListener('item', (event) => add(JSON.parse(event.data), true));
    stream.addEventListener('append', (event) => {
      const data = JSON.parse(event.data);
      const body = bodies.get(data.id);
      if (body) {
        body.append(data.text);
        follow();
      }
    });
    stream.addEventListener('status', (event) => status(JSON.parse(event.data)));
    stream.onerror = () => {
      stream.close();
      stream = null;
      offline();
      later(start);
    };
  }

  // ----- the log -----

  const DELIVERY = {
    note: 'added to the task',
    answer: 'answer',
    command: 'command',
    queued: 'waits for its turn',
  };

  function render(item) {
    const box = element('div', 'item ' + item.kind + (item.level ? ' ' + item.level : ''));
    box.title = new Date(item.at).toLocaleTimeString();
    let detailOpen = false;
    switch (item.kind) {
      case 'user': {
        box.classList.add('mine');
        box.append(element('div', 'bubble', item.text));
        const tags = [];
        if (DELIVERY[item.delivery]) tags.push(DELIVERY[item.delivery]);
        if (item.source === 'terminal') tags.push('typed in the terminal');
        if (tags.length) box.append(element('div', 'tag', tags.join(' · ')));
        break;
      }
      case 'text':
      case 'reply':
      case 'message': {
        const body = element('div', 'body', item.text);
        bodies.set(item.id, body);
        box.append(body);
        break;
      }
      case 'output':
        box.append(element('pre', null, item.text));
        break;
      case 'result':
        box.append(element('span', 'line', item.text));
        detailOpen = true;
        break;
      case 'worker':
        box.append(element('span', 'who', item.who + ': '), item.text);
        break;
      default:
        box.append(item.text);
    }
    if (item.detail) {
      if (detailOpen) {
        box.append(element('pre', null, item.detail));
      } else {
        const details = element('details');
        details.append(element('summary', null, 'more'), element('pre', null, item.detail));
        box.append(details);
      }
    }
    return box;
  }

  function add(item, live) {
    log.append(render(item));
    while (log.childElementCount > MAX_ITEMS) log.firstElementChild.remove();
    if (live) follow();
  }

  function nearBottom() {
    return window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 120;
  }

  function toBottom() {
    window.scrollTo(0, document.documentElement.scrollHeight);
    jump.hidden = true;
  }

  function follow() {
    if (following) toBottom();
    else jump.hidden = false;
  }

  window.addEventListener('scroll', () => {
    following = nearBottom();
    if (following) jump.hidden = true;
  }, { passive: true });

  jump.addEventListener('click', () => {
    following = true;
    toBottom();
  });

  // ----- the status -----

  function status(s) {
    const asking = Boolean(s.question);
    state.className = 'pill ' + (asking ? 'asking' : s.working ? 'working' : 'idle');
    state.textContent = asking ? 'asks you' : s.working ? 'working' : 'idle';
    stop.hidden = !s.working;
    text.placeholder = s.working ? 'Add to the task' : 'Message';

    const parts = [];
    if (s.context_tokens != null) parts.push('context ' + Math.round(s.context_tokens / 1000) + 'k');
    if (s.cost > 0) parts.push('$' + s.cost.toFixed(2));
    if (s.workers_running && s.workers_running.length) {
      const n = s.workers_running.length;
      parts.push(n + (n === 1 ? ' worker' : ' workers') + ' running');
    }
    if (s.plan_usage) parts.push(s.plan_usage);
    figures.textContent = parts.join(' · ');

    const info = s.info || {};
    $('info').textContent = [info.model, info.workspace, info.session && 'session ' + info.session]
      .filter(Boolean)
      .join(' · ');
    $('workers').textContent = s.workers || 'No workers.';

    if (asking) {
      if (!asked || asked.id !== s.question.id) answer.value = '';
      asked = s.question;
      $('question-text').textContent = s.question.text;
      question.hidden = false;
    } else {
      asked = null;
      question.hidden = true;
    }
  }

  $('more').addEventListener('click', () => {
    const details = $('details');
    details.hidden = !details.hidden;
    $('more').setAttribute('aria-expanded', String(!details.hidden));
  });

  // ----- actions -----

  function showError(message) {
    error.textContent = message;
    error.hidden = !message;
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
        showError('Not sent: fin is out of reach.');
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
        showError(why.error ? 'Not sent: ' + why.error + '.' : 'Not sent (' + response.status + ').');
        return false;
      }
      showError('');
      return true;
    }
    return false;
  }

  function grow() {
    text.style.height = 'auto';
    text.style.height = text.scrollHeight + 'px';
  }

  async function submit() {
    const message = text.value;
    if (!message.trim()) return;
    send.disabled = true;
    const sent = await post('/api/message', { text: message });
    send.disabled = false;
    if (sent) {
      text.value = '';
      grow();
      following = true;
    }
  }

  $('compose').addEventListener('submit', (event) => {
    event.preventDefault();
    submit();
  });

  text.addEventListener('input', grow);
  text.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' || event.isComposing) return;
    // Enter sends with a keyboard; on a touch screen it starts a new line
    // (and Ctrl or Cmd-Enter sends).
    const keyboard = window.matchMedia('(pointer: fine)').matches;
    if (event.ctrlKey || event.metaKey || (keyboard && !event.shiftKey)) {
      event.preventDefault();
      submit();
    }
  });

  question.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!asked || !answer.value.trim()) return;
    if (await post('/api/answer', { question: asked.id, text: answer.value })) answer.value = '';
  });

  stop.addEventListener('click', async () => {
    stop.disabled = true;
    await post('/api/stop');
    stop.disabled = false;
  });

  start();
})();
