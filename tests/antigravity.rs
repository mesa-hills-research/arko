#![cfg(all(unix, feature = "tui"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

const ARKO: &str = env!("CARGO_BIN_EXE_arko");

async fn run(command: &mut Command) -> Output {
    tokio::time::timeout(Duration::from_secs(45), command.kill_on_drop(true).output())
        .await
        .expect("Arko exited")
        .unwrap()
}

#[tokio::test]
async fn antigravity_reads_through_arko_in_a_saved_session() {
    let temp = tempfile::tempdir().unwrap();
    let temp_path = temp.path().canonicalize().unwrap();
    let home = temp_path.join("home");
    let bin = temp_path.join("bin");
    let data = temp_path.join("arko");
    let work = temp_path.join("proj");
    for dir in [&home, &work, &bin] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(work.join("note.txt"), "The relay read this line.\n").unwrap();
    let agy = bin.join("agy");
    let python = std::env::var("ARKO_TEST_PYTHON").unwrap_or_else(|_| "python3".to_string());
    std::fs::write(
        &agy,
        "#!/bin/sh\nexec \"$TEST_PYTHON\" \"$TEST_FIXTURE\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = temp_path.join("fake.jsonl");
    let script = temp_path.join("turns.json");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agy_agent.py");
    std::fs::write(
        &script,
        json!({
            "models": [
                {"value": "gemini-4-neon", "name": "Gemini 4 Neon", "levels": ["low", "high"]},
                {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash", "levels": ["low", "medium"]}
            ],
            "turns": [
                {"match": "Read the note.", "steps": [
                    {"tool": "read_file", "input": {"path": "note.txt"}},
                    {"tool": "start_agent", "input": {"agent": "worker", "task": "Check the note as well."}},
                    {"tool": "complete_task", "input": {"summary": "I read the note."}}
                ]},
                {"match": "Check the note as well.", "steps": [
                    {"tool": "read_file", "input": {"path": "note.txt"}},
                    {"tool": "complete_task", "input": {"summary": "The worker read the note."}}
                ]},
                {"match": "The worker read the note.", "steps": [
                    {"tool": "complete_task", "input": {"summary": "I read the note."}}
                ]},
                {"match": "Carry on.", "steps": [
                    {"tool": "complete_task", "input": {"summary": "I can still read the note."}}
                ]}
            ]
        }).to_string(),
    ).unwrap();
    let command = || {
        let mut command = Command::new(ARKO);
        command
            .env_clear()
            .env("HOME", &home)
            .env("ARKO_HOME", &data)
            .env(
                "PATH",
                std::env::join_paths([&bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap(),
            )
            .env("TEST_PYTHON", &python)
            .env("TEST_FIXTURE", &fixture)
            .env("FAKE_AGY_SCRIPT", &script)
            .env("FAKE_AGY_LOG", &log)
            .current_dir(&work);
        command
    };
    let first = run(command().args([
        "--tools",
        "classic",
        "-m",
        "AGY:Neon",
        "--worker-model",
        "agy:flash",
        "--worker-billing",
        "plans",
        "-p",
        "Read the note.",
    ]))
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("I read the note."));

    let events: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let event = |name: &'static str| events.iter().filter(move |event| event["event"] == name);
    let profile = event("profile")
        .find(|entry| entry["agent"] == "arko")
        .unwrap();
    assert_eq!(profile["default_agent"], false);
    assert_eq!(profile["problems"], json!([]));
    assert!(
        profile["system_prompt"]
            .as_str()
            .unwrap()
            .contains("You are Arko")
    );
    let pid = &profile["pid"];
    let launch = event("argv").find(|entry| &entry["pid"] == pid).unwrap();
    assert_eq!(launch["env"]["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
    let args = launch["argv"].as_array().unwrap();
    let at = args.iter().position(|arg| arg == "--gemini-dir").unwrap();
    let directory = Path::new(args[at + 1].as_str().unwrap());
    assert!(directory.starts_with(data.join("sessions")));
    let level = event("config")
        .find(|entry| &entry["pid"] == pid && entry["params"]["configId"] == "thought_level")
        .unwrap();
    assert_eq!(level["params"]["value"], "high");
    let relay = event("mcp_started")
        .find(|entry| &entry["pid"] == pid)
        .unwrap();
    assert_eq!(relay["command"][1], "agy-helper");
    assert_eq!(relay["command"][2], "relay");
    let worker = event("config")
        .find(|entry| {
            &entry["pid"] != pid
                && entry["params"]["value"] == "gemini-3.8-flash"
                && event("profile")
                    .any(|profile| profile["pid"] == entry["pid"] && profile["agent"] == "arko")
        })
        .expect("worker ran Flash in a private profile");
    let worker_pid = &worker["pid"];
    let worker_launch = event("argv")
        .find(|entry| &entry["pid"] == worker_pid)
        .unwrap();
    let worker_args = worker_launch["argv"].as_array().unwrap();
    let at = worker_args
        .iter()
        .position(|arg| arg == "--gemini-dir")
        .unwrap();
    let worker_directory = Path::new(worker_args[at + 1].as_str().unwrap());
    assert!(worker_directory.starts_with(data.join("sessions")));
    assert_ne!(directory, worker_directory);
    for pid in [pid, worker_pid] {
        let read = event("tool_result")
            .find(|entry| &entry["pid"] == pid && entry["tool"] == "read_file")
            .unwrap();
        assert_eq!(read["isError"], false);
        assert!(
            read["text"]
                .as_str()
                .unwrap()
                .contains("The relay read this line.")
        );
    }
    for launch in event("argv")
        .chain(event("version"))
        .chain(event("usage_command"))
    {
        assert_eq!(launch["env"]["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
    }
    assert!(event("permission").all(|entry| entry["allowed"] == true));

    let second = run(command().args(["-c", "-p", "Carry on."])).await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("I can still read the note."));

    let outdated = run(command().env("FAKE_AGY_VERSION", "1.5.13").args([
        "-m",
        "agy:neon",
        "-p",
        "Read the note.",
    ]))
    .await;
    assert!(!outdated.status.success());
    assert!(String::from_utf8_lossy(&outdated.stderr).contains("Antigravity CLI 1.5.13 is too old for Arko, which needs 1.5.14. Update it with `agy update`."));
    std::fs::write(&script, json!({"signed_out": true}).to_string()).unwrap();
    let signed_out = run(command().args(["-m", "agy:neon", "-p", "Read the note."])).await;
    assert!(!signed_out.status.success());
    assert!(String::from_utf8_lossy(&signed_out.stderr).contains("Run `agy` to sign in."));
}

#[tokio::test]
async fn antigravity_helper_is_not_in_public_help() {
    let help = run(Command::new(ARKO).arg("--help")).await;
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("agy:neon"));
    assert!(!help.contains("agy-helper"));
}
