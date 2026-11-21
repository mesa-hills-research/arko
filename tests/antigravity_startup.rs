#![cfg(all(unix, feature = "tui"))]
//! Starting another model does not check Antigravity.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::net::TcpListener;
use tokio::process::Command;

const ARKO: &str = env!("CARGO_BIN_EXE_arko");

/// Runs `arko -m sol -p hi` and returns how long it took to reach the model
/// server, and the fake agy's log.
async fn first_model_request(with_agy: bool, agy_delay: u32) -> (Duration, Vec<Value>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (home, bin, data, work) = (
        root.join("home"),
        root.join("bin"),
        root.join("arko"),
        root.join("proj"),
    );
    for dir in [&home, &bin, &work] {
        std::fs::create_dir(dir).unwrap();
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agy_agent.py");
    let python = std::env::var("ARKO_TEST_PYTHON").unwrap_or_else(|_| "python3".to_string());
    if with_agy {
        let agy = bin.join("agy");
        std::fs::write(
            &agy,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{{\"event\":\"launch\"}}' >> \"$FAKE_AGY_LOG\"\n[ \"$1\" = \"--version\" ] && sleep {agy_delay}\nexec \"$TEST_PYTHON\" \"$TEST_FIXTURE\" \"$@\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // A Claude Code that writes down how it was run, for comparison.
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho '2.1.400 (Claude Code)'\n",
            root.join("claude.txt").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = root.join("fake.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let started = Instant::now();
    let mut child = Command::new(ARKO)
        .env_clear()
        .env("HOME", &home)
        .env("ARKO_HOME", &data)
        .env(
            "PATH",
            std::env::join_paths([&bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap(),
        )
        .env("TEST_PYTHON", &python)
        .env("TEST_FIXTURE", &fixture)
        .env("FAKE_AGY_LOG", &log)
        .env("OPENAI_API_KEY", "sk-test")
        .env("ARKO_TEST_MODEL_URL", &url)
        .args(["-m", "sol", "--openai-auth", "api-key", "-p", "Say hi."])
        .current_dir(&work)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let accepted = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await;
    let elapsed = started.elapsed();
    let _ = child.kill().await;
    assert!(accepted.is_ok(), "arko never reached the model server");
    assert!(!root.join("claude.txt").exists());
    let events = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (elapsed, events)
}

#[tokio::test]
async fn a_session_on_another_model_does_not_start_antigravity() {
    let (without, _) = first_model_request(false, 0).await;
    let (with, events) = first_model_request(true, 6).await;
    let launches: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "launch")
        .collect();
    assert!(
        launches.is_empty(),
        "an `-m sol` session started agy {launches:?}. First request took {without:?} without agy and {with:?} with agy."
    );
}
