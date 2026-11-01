//! A SIGTERM ends `arko` with the MCP servers it started, which run in
//! process groups of their own and would otherwise outlive it.

#![cfg(all(unix, feature = "tui"))]

use std::path::{Path, PathBuf};
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

/// Kills, when dropped, `arko` and the stub server whose process ID is in
/// `pid_file`, unless the test saw the server end. So a run that fails
/// leaves neither running.
struct Reaper {
    arko: std::process::Child,
    pid_file: PathBuf,
    script: PathBuf,
    ended: bool,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.arko.kill();
        let _ = self.arko.wait();
        if self.ended {
            return;
        }
        let pid = std::fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        if let Some(pid) = pid
            && is_server(pid, &self.script)
        {
            // SAFETY: a signal to the stub server this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Whether `pid` is the stub server running `script`. Where /proc tells
/// the command line, it must be the stub's, in case the ID was reused.
fn is_server(pid: i32, script: &Path) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(line) => String::from_utf8_lossy(&line).contains(&*script.to_string_lossy()),
        Err(_) => !Path::new("/proc/self").exists() && alive(pid),
    }
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
    let arko = Command::new(env!("CARGO_BIN_EXE_arko"))
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
    let mut reaper = Reaper {
        arko,
        pid_file: pid_file.clone(),
        script: script.clone(),
        ended: false,
    };
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
    let input = reaper.arko.stdin.take();
    // SAFETY: a signal to the child this test started.
    assert_eq!(
        unsafe { libc::kill(reaper.arko.id() as i32, libc::SIGTERM) },
        0
    );
    let status = reaper.arko.wait().unwrap();
    wait_for("the server's end", || !alive(server));
    reaper.ended = true;
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    drop(input);
}
