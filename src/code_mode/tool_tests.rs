//! `run_code`'s definition, its arguments, and how a run's end reads.

use super::*;

use super::super::arko_definitions;

/// A definition named `name` and nothing more.
fn named(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: String::new(),
        output_based_example: None,
        parameters: Vec::new(),
        parallel_safe: false,
        read_only: false,
        tool_type_details: ToolTypeDetails::DeveloperDefined,
        raw_json_schema_override: None,
    }
}

/// The main agent's tool configuration under `tools`, with the search
/// index tools.
fn tool_config(tools: super::super::Tools) -> AgentToolConfig {
    let model = crate::agents::ModelSettings {
        model: crate::agents::resolve_model("opus").unwrap().id,
        effort: crate::agents::Effort::High,
        fast: false,
        cache_1h: false,
        handover_at: None,
    };
    let settings = crate::agents::AgentSettings {
        tools,
        index_tools: crate::index::TOOL_NAMES,
        ..crate::agents::AgentSettings::for_test(&model, &model)
    };
    crate::agents::main_config(&settings).unwrap().tool_config
}

/// `definition` as a golden shows it, its description and then each
/// parameter.
fn shown(definition: &ToolDefinition) -> String {
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
    shown
}

/// `shown` is the golden file `name`. `UPDATE_GOLDENS=1` writes it again.
fn assert_golden(name: &str, shown: &str) {
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/code_mode")
        .join(name);
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, shown).unwrap();
    }
    // (A checkout on Windows may end its lines with \r\n.)
    let expected = std::fs::read_to_string(&golden)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(shown, expected);
}

/// Under `--tools code` the description is the only place that teaches the
/// model the API and the tools, which it declares whole as the agent's
/// configuration shows them, so it changes only on purpose.
#[test]
fn under_code_the_description_declares_the_tools_and_is_its_golden() {
    let tool = RunCodeTool::new(arko_definitions());
    let definition = tool.offered_definition(&tool_config(super::super::Tools::Code));
    let description = &definition.description;
    for name in super::super::CALLABLE
        .iter()
        .filter(|name| **name != arko_agents::builtins::EDIT_FILE_TOOL_NAME)
    {
        assert!(description.contains(&format!("\n{name}(args")), "{name}");
    }
    assert!(!description.contains("edit_file(args"));
    // As the agent's configuration shows them, line ranges and the hint
    // that points write_file's callers to apply_diff included.
    assert!(description.contains("  end_line?: number;"));
    assert!(description.contains("To change part of a file, use `apply_diff`."));
    // index_status takes no argument it needs.
    assert!(description.contains("\nindex_status(args?: {"));
    assert_golden("run_code-code-description.txt", &shown(&definition));
}

/// Under the other settings the agents are offered the tools code calls,
/// so `run_code` describes none of them, and a run's description follows
/// its own configuration, not the session's.
#[test]
fn the_description_follows_the_configuration() {
    let tool = RunCodeTool::new(arko_definitions());
    for tools in [super::super::Tools::Classic, super::super::Tools::Both] {
        assert_eq!(
            tool.offered_definition(&tool_config(tools)).description,
            tool.definition().description,
            "{tools:?}"
        );
    }
    assert_ne!(
        tool.offered_definition(&tool_config(super::super::Tools::Code))
            .description,
        tool.definition().description
    );
}

#[test]
fn a_declaration_holds_the_whole_description_and_no_comment_end() {
    let mut definition = named("probe");
    definition.description = "First line.\n\nSee `a */ b`.".into();
    definition.parameters = vec![
        parameter("text", "What to say.", true),
        ToolParameterDefinition {
            param_type: ToolParameterType::Bool,
            ..parameter("loud", "Whether to shout.", false)
        },
    ];
    assert_eq!(
        declaration(&definition),
        "/**\n * First line.\n *\n * See `a *\\/ b`.\n */\nprobe(args: {\n  /** What to say. */\n  text: string;\n  /** Whether to shout. */\n  loud?: boolean;\n}): Promise<string>;"
    );
}

/// The description is the only place that teaches the model the API, so
/// it changes only on purpose. `UPDATE_GOLDENS=1` writes it again.
#[test]
fn the_description_is_its_golden() {
    let definition = RunCodeTool::new(arko_definitions()).definition();
    assert_golden("run_code-description.txt", &shown(&definition));
    assert!(!definition.read_only && !definition.parallel_safe);
}

#[test]
fn the_description_names_only_the_tools_the_session_has() {
    let tool = RunCodeTool::new(["read_file", "shell", "terminal_start"].map(named));
    assert!(
        tool.description()
            .contains("The tools it can call are read_file and shell.")
    );
}

#[test]
fn the_run_keeps_its_own_time_limit_and_stops_when_asked() {
    let tool = RunCodeTool::new(arko_definitions());
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
    let tool = RunCodeTool::new(arko_definitions());
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
            files_maybe_changed: 0,
        };
        assert_eq!(
            result(&run, 300).unwrap_err(),
            "The code threw on line 3: Error: boom\nBefore it ended, the code edited 2 files and ran 1 command.\nIts log so far:\nbefore"
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
        let run = done(
            Ended::Threw {
                message: "null".into(),
                line: None,
                compiling: false,
                refused: false,
                tool: None,
            },
            &[],
        );
        assert_eq!(
            result(&run, 300).unwrap_err(),
            "The code threw: null\nThe engine also throws null when the heap passes the 64 MB a run may hold."
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
                files_maybe_changed: 0,
            })
            .unwrap(),
            "Before it ended, the code wrote 1 file and ran 3 commands."
        );
        assert_eq!(
            changed(&Changes {
                files_written: 1,
                files_edited: 0,
                commands: 0,
                files_maybe_changed: 2,
            })
            .unwrap(),
            "Before it ended, the code wrote 1 file. 2 writes or edits that failed or were stopped may have changed their files."
        );
        assert_eq!(
            changed(&Changes {
                files_maybe_changed: 1,
                ..Changes::default()
            })
            .unwrap(),
            "A write or edit that failed or was stopped may have changed its file."
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
