//! A SIGTERM ends `arko` with the MCP servers it started, which run in
//! process groups of their own and would otherwise outlive it.

#![cfg(all(unix, feature = "tui"))]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A stub MCP server that writes its process ID to the file it is given,
/// answers the handshake and the tool list, and keeps running when its
/// input closes, as a server that doesn't watch its input would.
const SERVER: &str = r#"
import json, os, sys, time
with open(sys.argv[1], "w") as file:
    file.write(str(os.getpid()))
def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    if message.get("method") == "initialize":
        reply(message["id"], {"protocolVersion": message["params"]["protocolVersion"],
              "capabilities": {"tools": {}}, "serverInfo": {"name": "stub", "version": "1"}})
    elif message.get("method") == "tools/list":
        reply(message["id"], {"tools": []})
    else:
        reply(message["id"], {})
while True:
    time.sleep(1)
"#;

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "{what} in time");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_sigterm_ends_the_mcp_servers_with_arko() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = temp.path().join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    let pid_file = temp.path().join("server.pid");
    let python = std::env::var("ARKO_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let config = temp.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({"mcpServers": {"stub": {
            "command": python,
            "args": [script.display().to_string(), pid_file.display().to_string()],
        }}})
        .to_string(),
    )
    .unwrap();
    let mut arko = Command::new(env!("CARGO_BIN_EXE_arko"))
        .args(["--repl", "-m", "luna", "--mcp-config"])
        .arg(&config)
        .current_dir(&work)
        .env("ARKO_HOME", temp.path().join("arko"))
        .env("OPENAI_API_KEY", "test-key")
        .env("ARKO_OPENAI_AUTH", "api-key")
        .env_remove("ARKO_CODEX_AUTH")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the server's start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| !text.is_empty())
    });
    let server: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(server));
    // The input stays open until Arko has ended, so that its end doesn't
    // close the session first.
    let input = arko.stdin.take();
    // SAFETY: a signal to the child this test started.
    assert_eq!(unsafe { libc::kill(arko.id() as i32, libc::SIGTERM) }, 0);
    let status = arko.wait().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    wait_for("the server's end", || !alive(server));
    drop(input);
}
