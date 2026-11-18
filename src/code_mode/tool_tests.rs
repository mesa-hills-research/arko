//! `run_code`'s definition, its arguments, and how a run's end reads.

use super::*;

/// The description is the only place that teaches the model the API, so
/// it changes only on purpose. `UPDATE_GOLDENS=1` writes it again.
#[test]
fn the_description_is_its_golden() {
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/code_mode/run_code-description.txt");
    let definition = RunCodeTool::new(&super::super::CALLABLE).definition();
    let mut shown = definition.description.clone();
    for parameter in &definition.parameters {
        shown.push_str(&format!(
            "\n\n{} ({}{}): {}",
            parameter.name,
            match parameter.param_type {
                ToolParameterType::Integer => "integer",
                _ => "string",
            },
            if parameter.required { ", required" } else { "" },
            parameter.description
        ));
    }
    shown.push('\n');
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &shown).unwrap();
    }
    // (A checkout on Windows may end its lines with \r\n.)
    let expected = std::fs::read_to_string(&golden).unwrap().replace("\r\n", "\n");
    assert_eq!(shown, expected);
    assert!(!definition.read_only && !definition.parallel_safe);
}

#[test]
fn the_description_names_only_the_tools_the_session_has() {
    let tool = RunCodeTool::new(&["read_file", "shell", "terminal_start"]);
    assert!(
        tool.description()
            .contains("The tools it can call are read_file and shell.")
    );
}

#[test]
fn the_run_keeps_its_own_time_limit_and_stops_when_asked() {
    let tool = RunCodeTool::new(&super::super::CALLABLE);
    assert!(!tool.uses_host_timeout());
    assert!(tool.interruptible());
    assert_eq!(tool.background_mode(), arko_agents::ToolBackground::Never);
    assert_eq!(tool.recovery_mode(), arko_agents::ToolRecovery::InDoubt);
}

#[test]
fn timeout_seconds_takes_the_shells_range() {
    let params = |value: Value| HashMap::from([("timeout_seconds".to_string(), value)]);
    assert_eq!(
        timeout_seconds(&HashMap::new()).unwrap(),
        DEFAULT_TIMEOUT_SECONDS
    );
    assert_eq!(timeout_seconds(&params(serde_json::json!(60))).unwrap(), 60);
    assert_eq!(
        timeout_seconds(&params(serde_json::json!("90"))).unwrap(),
        90
    );
    for bad in [
        serde_json::json!(0),
        serde_json::json!(MAX_TIMEOUT_SECONDS + 1),
        serde_json::json!("soon"),
    ] {
        assert!(timeout_seconds(&params(bad)).is_err());
    }
}

#[tokio::test]
async fn code_too_large_or_missing_is_refused_before_a_child_starts() {
    let tool = RunCodeTool::new(&super::super::CALLABLE);
    let workspace = std::env::temp_dir();
    let context = TaskContext::new(uuid::Uuid::new_v4(), "agent", "", &workspace);
    let call = |code: String| {
        HashMap::from([
            ("code".to_string(), Value::String(code)),
            ("purpose".to_string(), Value::String("test".into())),
        ])
    };
    let error = tool
        .execute(&context, call("x".repeat(MAX_CODE_BYTES + 1)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("over the 100 KB"), "{error}");
    let error = tool.execute(&context, call(" ".into())).await.unwrap_err();
    assert!(matches!(error, ToolError::MissingParameter { .. }));
}

#[cfg(feature = "code-mode")]
mod words_of_a_run {
    use super::super::super::child::{Changes, Outcome, Run};
    use super::super::super::protocol::{Done, Ended};
    use super::super::words::{changed, cut, result};

    fn done(ended: Ended, log: &[&str]) -> Run {
        Run {
            outcome: Outcome::Done(Done {
                ended,
                log: log.iter().map(|line| line.to_string()).collect(),
                log_left_out: 0,
                value_left_out: 0,
                calls: 0,
                heap_bytes: 0,
                compute_ms: 0,
            }),
            changes: Changes::default(),
            calls: 0,
            most_held: 0,
            took: Default::default(),
        }
    }

    #[test]
    fn a_result_is_its_log_then_its_value() {
        let run = done(
            Ended::Returned {
                value: Some("[1,2]".into()),
                unread: vec!["read_file: no such file".into()],
            },
            &["a line"],
        );
        assert_eq!(
            result(&run, 300).unwrap(),
            "a line\n[1,2]\nA call the code didn't await failed:\nread_file: no such file"
        );
        let run = done(
            Ended::Returned {
                value: None,
                unread: Vec::new(),
            },
            &[],
        );
        assert_eq!(result(&run, 300).unwrap(), "The code returned nothing.");
    }

    #[test]
    fn a_failure_says_where_and_what_had_changed() {
        let mut run = done(
            Ended::Threw {
                message: "Error: boom".into(),
                line: Some(3),
                compiling: false,
                refused: false,
                tool: None,
            },
            &["before"],
        );
        run.changes = Changes {
            files_written: 0,
            files_edited: 2,
            commands: 1,
        };
        assert_eq!(
            result(&run, 300).unwrap_err(),
            "The code threw on line 3: Error: boom Before it ended, the code edited 2 files and ran 1 command.\nIts log so far:\nbefore"
        );
        let run = done(
            Ended::Threw {
                message: "SyntaxError: unexpected token".into(),
                line: Some(1),
                compiling: true,
                refused: false,
                tool: None,
            },
            &[],
        );
        assert_eq!(
            result(&run, 300).unwrap_err(),
            "The code has a syntax error on line 1 (SyntaxError: unexpected token). It runs as JavaScript."
        );
        let mut run = done(Ended::ComputeLimit, &[]);
        run.outcome = Outcome::TimedOut;
        assert_eq!(
            result(&run, 45).unwrap_err(),
            "The run took longer than timeout_seconds (45 s) and was stopped."
        );
    }

    #[test]
    fn what_changed_reads_as_one_sentence() {
        assert_eq!(changed(&Changes::default()), None);
        assert_eq!(
            changed(&Changes {
                files_written: 1,
                files_edited: 0,
                commands: 3,
            })
            .unwrap(),
            "Before it ended, the code wrote 1 file and ran 3 commands."
        );
    }

    #[test]
    fn a_long_result_is_cut_with_a_line_saying_how_much() {
        let long = "é".repeat(30_005);
        let shown = cut(&long, 0);
        assert!(
            shown.ends_with("\n[5 more characters left out]"),
            "{}",
            &shown[shown.len() - 40..]
        );
        assert_eq!(cut("short", 0), "short");
        // What the runner left out of the value counts too.
        assert_eq!(
            cut("short", 1_234),
            "short\n[1,234 more characters left out]"
        );
    }
}
