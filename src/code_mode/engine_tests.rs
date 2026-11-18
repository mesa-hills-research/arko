//! The engine tests: the spike's cases with their outcomes asserted, run on
//! QuickJS-ng through the host loop, in this process. A port of the engine
//! passes these before it replaces [`QuickJs`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::host::{self, Calls};
use super::protocol::{Answer, Call, Done, Ended, Limits, Start};
use super::quickjs::QuickJs;
use super::{HEAP_BYTES, STACK_BYTES, THREAD_STACK_BYTES};

const MB: u64 = 1024 * 1024;

/// An echo host on a thread of its own. `echo` answers with its arguments,
/// `slow` the same after 300 ms, `fail` fails and `refuse` is refused.
struct Echo {
    calls: mpsc::Sender<Call>,
    answers: mpsc::Receiver<Answer>,
}

impl Calls for Echo {
    fn send(&mut self, call: Call) -> bool {
        self.calls.send(call).is_ok()
    }

    fn answer(&mut self) -> Option<Answer> {
        self.answers.recv().ok()
    }
}

fn limits(compute: Duration) -> Limits {
    Limits {
        compute_ms: compute.as_millis() as u64,
        heap_bytes: HEAP_BYTES,
        stack_bytes: STACK_BYTES,
        calls: super::MAX_CALLS,
        os_memory_bytes: 0,
        os_address_space_bytes: 0,
        cpu_seconds: 0,
    }
}

/// What a run of `code` gave, how long it took, and the calls it sent.
struct Ran {
    done: Done,
    took: Duration,
    calls: u32,
}

fn run_with(code: &str, limits: Limits, stop: Arc<AtomicBool>) -> Ran {
    let (calls, incoming) = mpsc::channel::<Call>();
    let (answer, answers) = mpsc::channel();
    let start = Start {
        code: code.into(),
        tools: ["echo", "slow", "fail", "refuse"]
            .map(String::from)
            .to_vec(),
        limits,
    };
    let sent = Arc::new(AtomicU32::new(0));
    let host = std::thread::spawn({
        let sent = sent.clone();
        move || {
            for call in incoming {
                sent.fetch_add(1, Ordering::SeqCst);
                let text = call.args.to_string();
                let reply = match call.tool.as_str() {
                    "fail" => Answer {
                        id: call.id,
                        ok: false,
                        text: "the tool broke".into(),
                        refused: false,
                    },
                    "refuse" => Answer {
                        id: call.id,
                        ok: false,
                        text: "Refused: not here.".into(),
                        refused: true,
                    },
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(300));
                        Answer {
                            id: call.id,
                            ok: true,
                            text,
                            refused: false,
                        }
                    }
                    _ => Answer {
                        id: call.id,
                        ok: true,
                        text,
                        refused: false,
                    },
                };
                if answer.send(reply).is_err() {
                    break;
                }
            }
        }
    });
    let began = Instant::now();
    let done = std::thread::Builder::new()
        .name("arko-code".into())
        .stack_size(THREAD_STACK_BYTES)
        .spawn(move || host::run::<QuickJs>(&start, Echo { calls, answers }, stop))
        .unwrap()
        .join()
        .unwrap();
    let took = began.elapsed();
    host.join().unwrap();
    Ran {
        done,
        took,
        calls: sent.load(Ordering::SeqCst),
    }
}

fn run(code: &str) -> Ran {
    run_with(code, limits(Duration::from_secs(5)), Arc::default())
}

fn run_for(code: &str, compute: Duration) -> Ran {
    run_with(code, limits(compute), Arc::default())
}

fn returned(ran: &Ran) -> String {
    match &ran.done.ended {
        Ended::Returned { value, .. } => value.clone().unwrap_or_default(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_code_returns_a_value() {
    assert_eq!(returned(&run("return 6 * 7;")), "42");
    assert_eq!(returned(&run("return 'two\\nlines';")), "two\nlines");
    assert_eq!(returned(&run("return 10n;")), "10");
    let ran = run("return { m: new Map([[1, 2]]), d: new Date(0), u: undefined };");
    assert_eq!(returned(&ran), r#"{"m":{},"d":"1970-01-01T00:00:00.000Z"}"#);
    assert!(matches!(
        run("const x = 1;").done.ended,
        Ended::Returned { value: None, .. }
    ));
}

#[test]
fn a_thrown_error_names_its_line_in_the_code() {
    let ran = run("const a = 1;\nconst b = 2;\nthrow new Error('boom');");
    assert_eq!(
        ran.done.ended,
        Ended::Threw {
            message: "Error: boom".into(),
            line: Some(3),
            compiling: false,
            refused: false,
            tool: None,
        }
    );
    for (thrown, message) in [
        ("'a string'", "a string"),
        ("null", "null"),
        ("{ n: 1 }", r#"{"n":1}"#),
    ] {
        let ran = run(&format!("throw {thrown};"));
        assert!(
            matches!(&ran.done.ended, Ended::Threw { message: got, .. } if got == message),
            "{thrown}: {:?}",
            ran.done.ended
        );
    }
}

#[test]
fn a_syntax_error_and_typescript_fail_to_compile() {
    let ran = run("const n = 1;\nconst m: number = 2;\nreturn n;");
    match ran.done.ended {
        Ended::Threw {
            message,
            line,
            compiling,
            ..
        } => {
            assert!(message.starts_with("SyntaxError"), "{message}");
            assert_eq!(line, Some(2));
            assert!(compiling);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_deadline_stops_a_loop_inside_try_in_a_regex_and_after_an_await() {
    for code in [
        "while (true) {}",
        "try { while (true) {} } catch (e) { return 'caught'; } finally { return 'finally'; }",
        "const r = /^(a+)+$/.test('a'.repeat(40) + 'b'); return r;",
        "await tools.echo({}); while (true) {}",
        "await Promise.resolve(); for (;;) { try { while (true) {} } catch {} }",
    ] {
        let ran = run_for(code, Duration::from_millis(200));
        assert_eq!(ran.done.ended, Ended::ComputeLimit, "{code}");
        assert!(ran.took < Duration::from_secs(2), "{code}: {:?}", ran.took);
    }
}

#[test]
fn waiting_for_a_tool_isnt_computing() {
    let ran = run_for(
        "await tools.slow({}); await tools.slow({}); return 'waited';",
        Duration::from_millis(200),
    );
    assert_eq!(returned(&ran), "waited");
    assert!(ran.took >= Duration::from_millis(600));
}

#[test]
fn the_heap_cap_ends_a_run_and_code_may_catch_it() {
    let mut small = limits(Duration::from_secs(20));
    small.heap_bytes = 32 * MB;
    let ran = run_with(
        "const all = []; while (true) { all.push(new Array(100000).fill(7)); }",
        small,
        Arc::default(),
    );
    // QuickJS-ng throws null where it has no memory left for its Error, as
    // on macOS, whose allocator counts allocations larger.
    assert!(
        matches!(&ran.done.ended, Ended::OutOfMemory { .. })
            || matches!(&ran.done.ended, Ended::Threw { message, .. } if message == "null"),
        "{:?}",
        ran.done.ended
    );
    assert!(ran.done.heap_bytes <= 32 * MB);
    let ran = run_with(
        "try { return new Array(1e9).fill(0).length; } catch (e) { return 'threw ' + e.message; }",
        small,
        Arc::default(),
    );
    assert_eq!(returned(&ran), "threw out of memory");
}

#[test]
fn the_stack_cap_ends_endless_recursion() {
    let ran = run("function f(n) { return f(n + 1) + 1; }\nreturn f(0);");
    match ran.done.ended {
        Ended::Threw { message, line, .. } => {
            assert_eq!(message, "RangeError: Maximum call stack size exceeded");
            assert_eq!(line, Some(1));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_promise_nothing_settles_ends_the_run() {
    let ran = run("await new Promise(() => {}); return 1;");
    assert_eq!(ran.done.ended, Ended::Unsettled);
}

/// What the global object holds: the language's own names, what
/// QuickJS-ng adds (`performance`, `DOMException`, `atob`, `btoa`,
/// `queueMicrotask`), and the prelude's. An engine update that adds a name
/// fails here.
#[test]
fn the_global_object_holds_only_the_allowed_names() {
    let names = returned(&run(
        "return Object.getOwnPropertyNames(globalThis).sort().join(' ');",
    ));
    assert_eq!(
        names,
        "AggregateError Array ArrayBuffer AsyncDisposableStack Atomics BigInt BigInt64Array \
         BigUint64Array Boolean DOMException DataView Date DisposableStack Error EvalError \
         FinalizationRegistry Float16Array Float32Array Float64Array Function Infinity \
         Int16Array Int32Array Int8Array InternalError Iterator JSON Map Math NaN Number Object \
         Promise Proxy RangeError ReferenceError Reflect RegExp Set SharedArrayBuffer String \
         SuppressedError Symbol SyntaxError TypeError URIError Uint16Array Uint32Array \
         Uint8Array Uint8ClampedArray WeakMap WeakRef WeakSet __rethrow __run __settle atob btoa \
         console decodeURI decodeURIComponent encodeURI encodeURIComponent escape eval \
         globalThis isFinite isNaN parseFloat parseInt performance queueMicrotask tools \
         undefined unescape"
    );
    let probes = returned(&run(
        "return [typeof require, typeof fetch, typeof std, typeof os, typeof process, typeof setTimeout, typeof print, typeof WebAssembly, typeof __host, typeof __setup].join(' ');",
    ));
    assert_eq!(probes, "undefined ".repeat(10).trim_end());
}

#[test]
fn import_rejects_and_atomics_wait_throws() {
    let ran = run("try { await import('fs'); return 'loaded'; } catch (e) { return 'rejected'; }");
    assert_eq!(returned(&ran), "rejected");
    let ran = run(
        "try { return Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0); } catch (e) { return 'threw'; }",
    );
    assert_eq!(returned(&ran), "threw");
}

#[test]
fn the_entry_points_and_tools_cant_be_replaced() {
    let ran = run("const hits = [];\n\
         for (const replace of [\n\
           () => { globalThis.__settle = 5; },\n\
           () => { delete globalThis.__run; },\n\
           () => { globalThis.tools = {}; },\n\
           () => { tools.echo = () => 1; },\n\
           () => { console.log = () => 1; },\n\
           () => { Object.defineProperty(globalThis, '__rethrow', { value: 1 }); },\n\
         ]) {\n\
           try { replace(); hits.push('replaced'); } catch (e) { hits.push(e.constructor.name); }\n\
         }\n\
         hits.push(await tools.echo({ a: 1 }));\n\
         return hits.join(' ');");
    assert_eq!(
        returned(&ran),
        r#"TypeError TypeError TypeError TypeError TypeError TypeError {"a":1}"#
    );
}

#[test]
fn the_call_cap_counts_calls_as_they_are_made() {
    let ran = run("for (let i = 0; i < 600; i++) tools.echo({ i }); return 'done';");
    assert_eq!(ran.done.ended, Ended::CallLimit);
    assert_eq!(ran.calls, 500);
    assert_eq!(ran.done.calls, 500);
}

#[test]
fn a_failed_call_rejects_with_the_tools_message_and_can_be_caught() {
    let ran = run("const seen = [];\n\
         for (const name of ['fail', 'refuse']) {\n\
           try { await tools[name]({}); } catch (e) { seen.push(`${e.message} ${e.refused === true} ${e.tool}`); }\n\
         }\n\
         return seen.join(' | ');");
    assert_eq!(
        returned(&ran),
        "the tool broke false fail | Refused: not here. true refuse"
    );
}

#[test]
fn an_uncaught_failed_call_names_the_line_of_the_call() {
    let ran = run("const x = 1;\nawait tools.refuse({ x });");
    assert_eq!(
        ran.done.ended,
        Ended::Threw {
            message: "Error: Refused: not here.".into(),
            line: Some(2),
            compiling: false,
            refused: true,
            tool: Some("refuse".into()),
        }
    );
}

#[test]
fn calls_not_awaited_finish_first_and_their_failures_are_reported() {
    let ran = run("tools.slow({ n: 1 });\n\
         tools.fail({});\n\
         try { await tools.fail({}); } catch {}\n\
         return 'went on';");
    assert_eq!(ran.calls, 3);
    assert_eq!(
        ran.done.ended,
        Ended::Returned {
            value: Some("went on".into()),
            unread: vec!["fail: the tool broke".into()],
        }
    );
    // A bad argument is a failure of the call, and the run goes on.
    let ran = run("try { await tools.echo('src'); } catch (e) { return e.message; }");
    assert_eq!(
        returned(&ran),
        "tools.echo takes one object of arguments, such as { path }."
    );
}

#[test]
fn console_lines_come_before_the_value_and_survive_a_failure() {
    let ran =
        run("console.log('a', 1, { b: 2 }, [3], undefined);\nconsole.error('e');\nreturn [1, 2];");
    assert_eq!(ran.done.log, ["a 1 {\"b\":2} [3] undefined", "e"]);
    assert_eq!(returned(&ran), "[1,2]");
    let ran = run_for(
        "console.log('before');\nwhile (true) {}",
        Duration::from_millis(100),
    );
    assert_eq!(ran.done.ended, Ended::ComputeLimit);
    assert_eq!(ran.done.log, ["before"]);
    // (A slow machine's debug build takes seconds over 100,000 lines.)
    let ran = run_for(
        "for (let i = 0; i < 100000; i++) console.log('x'.repeat(10));\nreturn 1;",
        Duration::from_secs(120),
    );
    assert_eq!(returned(&ran), "1");
    assert_eq!(ran.done.log.len(), super::MAX_LOG_BYTES / 10);
    assert_eq!(
        ran.done.log_left_out,
        100_000 - (super::MAX_LOG_BYTES / 10) as u64
    );
}

#[test]
fn a_stop_from_outside_ends_a_run() {
    let stop = Arc::new(AtomicBool::new(false));
    let setter = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop.store(true, Ordering::SeqCst);
        })
    };
    let ran = run_with("while (true) {}", limits(Duration::from_secs(20)), stop);
    setter.join().unwrap();
    assert!(
        matches!(ran.done.ended, Ended::Failed { ref message } if message == "The run was stopped."),
        "{:?}",
        ran.done.ended
    );
    assert!(ran.took < Duration::from_secs(2));
}

#[test]
fn many_calls_one_after_another() {
    let ran = run(
        "let n = 0; for (let i = 0; i < 400; i++) { n += JSON.parse(await tools.echo({ i })).i; } return n;",
    );
    assert_eq!(returned(&ran), "79800");
    assert_eq!(ran.calls, 400);
}
