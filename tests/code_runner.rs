//! `arko code-runner`, as Arko starts it for a run of `run_code`: it reads
//! the run as a frame on its input and writes how it ended.

#![cfg(feature = "code-mode")]
// The measurement prints its numbers. (No full screen runs here.)
#![allow(clippy::disallowed_macros)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};

const MAGIC: &[u8] = b"\0arko-code-runner 1\n";

fn frame(message: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap();
    let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(&body);
    bytes
}

/// Runs `code` in `arko code-runner`, and gives its `Done`.
fn run(code: &str) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arko"))
        .arg("code-runner")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = serde_json::json!({
        "type": "start",
        "code": code,
        "tools": [],
        "limits": {
            "compute_ms": 5000,
            "heap_bytes": 64 * 1024 * 1024,
            "stack_bytes": 512 * 1024,
            "calls": 500,
            "os_memory_bytes": 512u64 * 1024 * 1024,
            "os_address_space_bytes": 1024u64 * 1024 * 1024,
            "cpu_seconds": 60,
        },
    });
    // Its input stays open while it runs, as Arko keeps it, since the
    // runner ends when its input does.
    let mut input = child.stdin.take().unwrap();
    input.write_all(&frame(&start)).unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut magic = vec![0; MAGIC.len()];
    output.read_exact(&mut magic).unwrap();
    assert_eq!(magic, MAGIC);
    let mut length = [0; 4];
    output.read_exact(&mut length).unwrap();
    let mut body = vec![0; u32::from_le_bytes(length) as usize];
    output.read_exact(&mut body).unwrap();
    drop(input);
    assert!(child.wait().unwrap().success());
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn the_program_runs_code_when_started_as_the_code_runner() {
    let done = run("console.log('hello');\nreturn 6 * 7;");
    assert_eq!(done["type"], "done");
    assert_eq!(done["ended"]["kind"], "returned");
    assert_eq!(done["ended"]["value"], "42");
    assert_eq!(done["log"], serde_json::json!(["hello"]));
}

/// How long a run takes in this program, from its start to its end, for a
/// run that returns at once. Prints the median of 20.
#[test]
#[ignore = "a measurement, run with --ignored --nocapture"]
fn start_time() {
    let mut times: Vec<_> = (0..20)
        .map(|_| {
            let began = std::time::Instant::now();
            assert_eq!(run("return 1;")["ended"]["value"], "1");
            began.elapsed()
        })
        .collect();
    times.sort();
    println!(
        "arko code-runner, `return 1`, 20 runs: median {:?}, fastest {:?}, slowest {:?}",
        times[10], times[0], times[19]
    );
}
