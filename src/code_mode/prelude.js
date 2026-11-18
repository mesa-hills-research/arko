// What every run of code starts with: the `tools` object, `console`, and
// the entry points the host calls (`__run`, `__settle`, `__rethrow`,
// `__setup`). Standard ECMAScript only, so any engine behind the seam runs
// it. The host's native function `__host` takes one string and answers
// with one, and this takes it off the global object first.
"use strict";
(() => {
  const host = globalThis.__host;
  delete globalThis.__host;
  const { stringify, parse } = JSON;
  const { freeze, defineProperty } = Object;
  const { apply } = Reflect;
  const then = Promise.prototype.then;
  const isArray = Array.isArray;
  const send = (message) => host(stringify(message));

  // Calls not answered yet, by ID.
  const pending = new Map();
  // Calls that failed, in order, and the promises the code read.
  const failed = [];
  const read = new WeakSet();
  let next = 0;
  let started = false;
  let returned = null;
  let finished = false;
  let failure;

  // A call's promise. Awaiting it, or reading it any other way, calls its
  // `then`, so a failure the code never read can be reported.
  class ToolCall extends Promise {
    static get [Symbol.species]() {
      return Promise;
    }
    then(onFulfilled, onRejected) {
      read.add(this);
      return apply(then, this, [onFulfilled, onRejected]);
    }
  }

  // A value as the result or a log line shows it: a string as it is,
  // anything else as JSON where it has one.
  const render = (value) => {
    if (value === undefined) return null;
    if (typeof value === "string") return value;
    try {
      const text = stringify(value);
      if (text !== undefined) return text;
    } catch {}
    return String(value);
  };

  const fail = (entry, message, refused) => {
    entry.error.message = message;
    entry.error.tool = entry.name;
    if (refused) entry.error.refused = true;
    failed.push(entry);
    entry.reject(entry.error);
  };

  const call = (name, args) => {
    // Made here, so its stack names the code's line of the call.
    const entry = { name, error: new Error() };
    entry.promise = new ToolCall((resolve, reject) => {
      entry.resolve = resolve;
      entry.reject = reject;
    });
    if (args === undefined) args = {};
    if (args === null || typeof args !== "object" || isArray(args)) {
      fail(entry, `tools.${name} takes one object of arguments, such as { path }.`, false);
      return entry.promise;
    }
    const id = ++next;
    let text;
    try {
      text = stringify({ t: "call", id, tool: name, args });
    } catch (error) {
      fail(entry, `The arguments of tools.${name} aren't JSON: ${error.message}`, false);
      return entry.promise;
    }
    pending.set(id, entry);
    // An empty answer means the call went out, and any other refuses it.
    const answer = host(text);
    if (answer !== "") {
      pending.delete(id);
      fail(entry, answer, true);
    }
    return entry.promise;
  };

  // Reports the result once the code returned and its calls have answers.
  const finish = () => {
    if (finished || returned === null || pending.size !== 0) return;
    finished = true;
    const unread = [];
    for (const entry of failed) {
      if (!read.has(entry.promise)) unread.push(`${entry.name}: ${entry.error.message}`);
    }
    send({ t: "done", value: returned.value, unread });
  };

  const fault = (error) => {
    failure = error;
    let refused = false;
    let tool = null;
    try {
      refused = error?.refused === true;
      tool = typeof error?.tool === "string" ? error.tool : null;
    } catch {}
    send({ t: "failed", refused, tool });
  };

  const run = (body) => {
    if (started) throw new TypeError("The code runs once.");
    started = true;
    apply(then, body(), [
      (value) => {
        try {
          returned = { value: render(value) };
          finish();
        } catch (error) {
          fault(error);
        }
      },
      fault,
    ]);
  };

  const settle = (text) => {
    const { id, ok, text: result, refused } = parse(text);
    const entry = pending.get(id);
    if (entry === undefined) return;
    pending.delete(id);
    if (ok) entry.resolve(result);
    else fail(entry, result, refused === true);
    finish();
  };

  const rethrow = () => {
    throw failure;
  };

  const log = (...args) => {
    let line = "";
    for (const arg of args) {
      const text = typeof arg === "string" ? arg : (render(arg) ?? "undefined");
      line = line === "" ? text : `${line} ${text}`;
    }
    send({ t: "log", line });
  };

  const fixed = (name, value) =>
    defineProperty(globalThis, name, { value, writable: false, enumerable: false, configurable: false });

  fixed("console", freeze({ log, info: log, warn: log, error: log, debug: log }));
  fixed("__run", run);
  fixed("__settle", settle);
  fixed("__rethrow", rethrow);
  // The tools, by their names, once.
  defineProperty(globalThis, "__setup", {
    value: (text) => {
      delete globalThis.__setup;
      const tools = {};
      for (const name of parse(text)) tools[name] = (args) => call(name, args);
      fixed("tools", freeze(tools));
    },
    writable: false,
    enumerable: false,
    configurable: true,
  });
})();
