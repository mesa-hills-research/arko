use super::*;
use arko_agents::HandoverRecord;
use arko_ai::{Message, ToolResult};
use arko_workflows::AgentResult;
use serde_json::json;

fn config(model: &str, text_replies: bool) -> arko_agents::AgentConfig {
    let mut config = arko_agents::AgentConfig::builder(
        "main".into(),
        "Main".into(),
        arko_agents::AgentLLMConfig {
            model: crate::providers::resolve_model(model).unwrap(),
            generation_config: Default::default(),
        },
    )
    .role_description("A test.".into())
    .instructions("Test.".into())
    .build()
    .unwrap();
    config.tool_config.text_reply_ends_task = text_replies;
    config
}

/// A conversation of the main agent, built a message at a time.
struct Talk {
    messages: Vec<Message>,
    calls: usize,
}

impl Talk {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            calls: 0,
        }
    }

    fn user(&mut self, text: &str) -> &mut Self {
        self.messages.push(Message::new_user(text));
        self
    }

    fn text(&mut self, text: &str) -> &mut Self {
        self.messages.push(Message::new_assistant(text));
        self
    }

    /// A call of `tool` and its result.
    fn call(&mut self, tool: &str, arguments: Value, output: &str) -> &mut Self {
        self.called(tool, arguments, output, false)
    }

    fn called(&mut self, tool: &str, arguments: Value, output: &str, error: bool) -> &mut Self {
        self.calls += 1;
        let id = format!("call_{}", self.calls);
        let mut ask = Message::new_assistant("");
        ask.content.clear();
        ask.tool_calls = vec![
            serde_json::from_value(json!({"id": id, "name": tool, "arguments": arguments}))
                .unwrap(),
        ];
        self.messages.push(ask);
        let mut result = ToolResult::new(Some(id), tool, output);
        result.is_error = error;
        self.messages
            .push(Message::new_user_with_contents(vec![Content::ToolResult(
                result,
            )]));
        self
    }

    /// The agent's reply with `complete_task`.
    fn reply(&mut self, summary: &str) -> &mut Self {
        self.calls += 1;
        let mut done = Message::new_assistant("");
        done.content.clear();
        done.tool_calls = vec![
            serde_json::from_value(json!({
                "id": format!("call_{}", self.calls),
                "name": "complete_task",
                "arguments": {"summary": summary},
            }))
            .unwrap(),
        ];
        self.messages.push(done);
        self
    }

    fn task(&self, model: &str, text_replies: bool) -> arko_agents::Task {
        let prompt = self.messages.first().map_or(String::new(), |message| {
            message
                .content
                .iter()
                .filter_map(Content::as_text)
                .collect()
        });
        let mut task =
            arko_agents::Task::new(config(model, text_replies), prompt, "/work".into(), None);
        task.conversation.messages = self.messages.clone();
        task
    }
}

/// The words of turn `n` of the long conversation.
fn ask(n: usize) -> String {
    format!(
        "Turn {n} asks for step{n:03} of the plan, with a few more words so that the line runs long enough to be cut where the brief has little room for each of the user's messages."
    )
}

/// A long conversation: `turns` turns, each with the user's message, the
/// agent's text, a command with an output of 3,000 characters, an edit
/// every tenth turn, and a reply.
fn long_talk(turns: usize) -> Talk {
    long_talk_with(turns, |_| ["", "", ""])
}

/// [`long_talk`], with `extra(n)` added to turn n's message, command
/// output and reply.
fn long_talk_with(turns: usize, extra: impl Fn(usize) -> [&'static str; 3]) -> Talk {
    let mut talk = Talk::new();
    for n in 1..=turns {
        let [words, output, reply] = extra(n);
        if n == 1 {
            talk.user(&format!(
                "# Project instructions (AGENTS.md in the working directory)\n\nBe brief.\n\n# My message\n\n{}",
                ask(1)
            ));
        } else {
            talk.user(&format!("{}{words}", ask(n)));
        }
        talk.text(&format!("Looking at step{n:03}."));
        let output = format!("output of step{n:03}\n{}{output}", "x".repeat(3_000 - 18));
        talk.call(
            "shell",
            json!({"command": format!("make step{n:03}")}),
            &output,
        );
        if n % 10 == 0 {
            talk.call(
                "edit_file",
                json!({"path": format!("src/step{n:03}.rs"), "old_string": "a", "new_string": "b"}),
                "Edited.",
            );
        }
        talk.reply(&format!(
            "Step{n:03} is done.{reply} {}",
            "The reply says what changed and why, at some length. ".repeat(6)
        ));
    }
    talk
}

#[derive(Debug)]
struct Session {
    conversations: Vec<Vec<AgentResult>>,
    continued: Option<usize>,
}

impl Saved for Session {
    fn count(&self) -> usize {
        self.conversations.len()
    }

    fn runs(&self, number: usize) -> Result<Vec<AgentResult>, String> {
        Ok(self.conversations[number - 1].clone())
    }

    fn continued(&self, _run: uuid::Uuid) -> Option<usize> {
        self.continued
    }
}

fn session(conversations: Vec<Vec<arko_agents::Task>>) -> Session {
    Session {
        conversations: conversations
            .into_iter()
            .map(|runs| {
                runs.into_iter()
                    .map(|task| AgentResult::of_task("main".into(), task))
                    .collect()
            })
            .collect(),
        continued: Some(1),
    }
}

/// The tool's output for `params`, or its error.
fn call(session: &Session, params: Value) -> Result<String, String> {
    let params: HashMap<String, Value> = params.as_object().unwrap().clone().into_iter().collect();
    let request = Request::parse(&params).map_err(|error| error.to_string())?;
    request.answer(session, uuid::Uuid::new_v4())
}

fn no_workers() -> (Vec<Worker>, Open) {
    (Vec::new(), Open::default())
}

/// Where each of `headings` is in `text`, which must hold them all.
fn positions(text: &str, headings: &[&str]) -> Vec<usize> {
    headings
        .iter()
        .map(|heading| {
            text.find(heading)
                .unwrap_or_else(|| panic!("no {heading:?} in\n{text}"))
        })
        .collect()
}

#[test]
fn the_budget_is_4_percent_of_the_limit_between_6k_and_24k() {
    assert_eq!(budget(Some(100_000)), 6_000);
    assert_eq!(budget(Some(272_000)), 10_880);
    assert_eq!(budget(Some(400_000)), 16_000);
    assert_eq!(budget(Some(1_000_000)), 24_000);
    assert_eq!(budget(None), 12_000);
}

#[test]
fn a_small_window_gets_a_brief_within_its_budget() {
    let task = long_talk(300).task("opus", false);
    let workers = vec![
        Worker {
            handle: "r2".into(),
            state: "running".into(),
            task: "Fix the parser\nwith care".into(),
        },
        Worker {
            handle: "r3".into(),
            state: "done".into(),
            task: "Review the docs".into(),
        },
    ];
    let open = Open {
        questions: vec!["Tabs or spaces?".into()],
        unread: vec!["Also check the docs.".into()],
        ended: Some("stopped by the user".into()),
    };
    let brief = brief(&BriefParts {
        number: 1,
        model: "cc:opus",
        runs: &[&task],
        report: None,
        workers: &workers,
        open: &open,
        budget: 6_000,
    });
    assert!(tokens(&brief) <= 6_000, "{} tokens", tokens(&brief));
    assert!(
        brief.starts_with("# Earlier conversation\n\nConversation 1 on cc:opus, 300 turns, about "),
        "{brief}"
    );
    assert!(brief.contains("`earlier_conversation search`"));
    let at = positions(
        &brief,
        &[
            "## Goal",
            "## The user's messages",
            "## Files written or edited",
            "## Workers",
            "## Open",
            "## Last exchanges",
        ],
    );
    assert!(at.is_sorted(), "{brief}");
    assert!(!brief.contains("# Handover report"));
    assert!(!brief.contains("## Report"));
    // The goal is the user's own words, without Arko's sections.
    assert!(brief.contains("The user's first message in it.\n\nTurn 1 asks for step001"));
    assert!(!brief.contains("Be brief."));

    // The newest 60 of the user's messages, oldest first.
    let users = &brief[at[1]..at[2]];
    assert!(users.contains("The newest 60 of 300 are here"), "{users}");
    let lines: Vec<&str> = users
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    assert_eq!(lines.len(), 60);
    assert!(lines[0].starts_with("- 241 Turn 241 asks for step241"));
    assert!(lines[59].starts_with("- 300 Turn 300 asks"));

    // The newest files first, each with its turn.
    let files = &brief[at[2]..at[3]];
    assert!(files.contains("- src/step300.rs (turn 300)\n- src/step290.rs (turn 290)"));

    let workers = &brief[at[3]..at[4]];
    assert!(workers.contains("- r2, running. Fix the parser\n- r3, done. Review the docs"));
    let open = &brief[at[4]..at[5]];
    assert!(
        open.contains(
            "Questions the agent asked that wait for the user's answer\n- Tabs or spaces?"
        )
    );
    assert!(open.contains("which it never read\n- Also check the docs."));
    assert!(open.contains("The last turn was stopped by the user."));

    // The newest exchanges that fit, oldest first, ending with the last.
    let exchanges = &brief[at[5]..];
    let shown: Vec<usize> = exchanges
        .lines()
        .filter_map(|line| line.strip_prefix("### Turn "))
        .map(|number| number.parse().unwrap())
        .collect();
    assert!(!shown.is_empty());
    assert_eq!(*shown.last().unwrap(), 300);
    assert!(
        shown.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{shown:?}"
    );
    assert!(exchanges.ends_with(&format!(
        "Reply\nStep300 is done. {}",
        "The reply says what changed and why, at some length. "
            .repeat(6)
            .trim_end()
    )));
    // Without tool calls.
    assert!(!exchanges.contains("make step300"));
}

#[test]
fn a_report_comes_on_top_of_the_budget() {
    let task = long_talk(300).task("opus", false);
    let (workers, open) = no_workers();
    let report = format!("What was done. {}", "Details that matter. ".repeat(1_500));
    let runs = [&task];
    let parts = |report| BriefParts {
        number: 2,
        model: "gpt-6.1-sol",
        runs: &runs,
        report,
        workers: &workers,
        open: &open,
        budget: 6_000,
    };
    let without = brief(&parts(None));
    let with = brief(&parts(Some(&report)));
    let section = format!(
        "## Report\n\ngpt-6.1-sol wrote this report as conversation 2 ended.\n\n{}\n\n",
        report.trim()
    );
    assert!(with.contains(&section));
    assert_eq!(with.replacen(&section, "", 1), without);
    assert!(tokens(&with) > 6_000);
    let at = positions(&with, &["## Goal", "## Report", "## The user's messages"]);
    assert!(at.is_sorted());
}

fn handover(report: &str, at: &str) -> HandoverRecord {
    HandoverRecord {
        at: at.parse().unwrap(),
        input_tokens: 900_000,
        replaced_messages: 380,
        report: report.into(),
    }
}

/// A run that was handed over twice: it begins again with its first prompt
/// and the latest report.
fn handed_over() -> arko_agents::Task {
    let prompt = "# My message\n\nPort the parser to the new API.";
    let mut talk = Talk::new();
    talk.user(
        &arko_agents::HandoverSeed {
            task_prompt: prompt.into(),
            context: None,
        }
        .message("The second report."),
    );
    talk.text("Going on from the report.")
        .call("shell", json!({"command": "cargo test"}), "ok")
        .reply("Ported.")
        .user("Now the docs.")
        .reply("Done with the docs.");
    let mut task = talk.task("opus", false);
    task.initial_task_prompt = prompt.into();
    task.handovers = vec![
        handover("The first report.", "2026-10-28T09:00:00Z"),
        handover("The second report.", "2026-10-29T14:02:00Z"),
    ];
    task
}

#[test]
fn the_latest_handover_report_stands_in_for_a_report() {
    let task = handed_over();
    let (workers, open) = no_workers();
    let brief = brief(&BriefParts {
        number: 1,
        model: "opus",
        runs: &[&task],
        report: None,
        workers: &workers,
        open: &open,
        budget: 6_000,
    });
    assert!(brief.contains(
        "## Report\n\nThe agent wrote this report when its context was replaced on 2026-10-29 14:02 UTC, so later turns aren't in it.\n\nThe second report."
    ));
    assert!(!brief.contains("The first report."));
    assert!(!brief.contains("# Handover report"));
    // The goal is the first prompt, which the run began again with but
    // the user didn't send again.
    assert!(brief.contains("The user's first message in it.\n\nPort the parser to the new API."));
    assert!(brief.contains("Each line starts with its turn.\n\n- 2 Now the docs."));
}

#[test]
fn a_handover_shows_as_its_reports() {
    let saved = session(vec![vec![handed_over()]]);
    let outline = call(&saved, json!({"operation": "outline"})).unwrap();
    assert!(
        outline.starts_with("Conversation 1, turns 1 to 2 of 2."),
        "{outline}"
    );
    assert!(outline.contains("After 2 handovers. Their reports are 1.1 to 1.2.\n  Reply  Ported."));
    let read = call(&saved, json!({"operation": "read", "turn": "1"})).unwrap();
    assert!(read.contains(
        "1.1 Report of a handover on 2026-10-28 09:00 UTC, which replaced 380 messages (about 900k tokens)\nThe first report."
    ));
    assert!(read.contains("1.2 Report of a handover on 2026-10-29 14:02 UTC"));
    assert!(read.contains("1.3 Agent\nGoing on from the report."));
    // The first message, which the reports stand for, isn't shown again.
    assert!(!read.contains("Port the parser"));
    let found = call(
        &saved,
        json!({"operation": "search", "words": "first report"}),
    )
    .unwrap();
    assert!(found.contains("1.1 Report of a handover"), "{found}");
}

#[test]
fn outline_lists_50_turns_at_a_time() {
    let saved = session(vec![vec![long_talk(300).task("opus", false)]]);
    let outline = call(&saved, json!({"operation": "outline"})).unwrap();
    assert!(outline.starts_with("Conversation 1, turns 1 to 50 of 300."));
    assert!(outline.contains("Turn 1, 20"));
    assert!(
        outline.contains("Turn 12, about 888 tokens, 1 call\n  User  Turn 12 asks for step012"),
        "{outline}"
    );
    assert!(outline.contains("\n  Reply  Step012 is done."));
    assert!(outline.contains("Turn 50, about"));
    assert!(!outline.contains("Turn 51,"));
    assert!(outline.ends_with("`outline` with turn 51 goes on."));
    assert!(tokens(&outline) <= OUTPUT_TOKENS);
    let last = call(&saved, json!({"operation": "outline", "turn": 281})).unwrap();
    assert!(last.starts_with("Conversation 1, turns 281 to 300 of 300."));
    assert!(last.contains("Turn 300, about"));
    assert!(!last.contains("goes on"));
    // A small limit cuts the page.
    let small = call(&saved, json!({"operation": "outline", "limit": 1000})).unwrap();
    assert!(tokens(&small) <= 1_000);
    let next = small
        .rsplit_once("The output stops at its limit here. `outline` with turn ")
        .unwrap()
        .1;
    let next: usize = next.strip_suffix(" goes on.").unwrap().parse().unwrap();
    assert!(next > 1 && next < 50, "{small}");
    assert!(small.contains(&format!("Turn {}, about", next - 1)));
    assert!(!small.contains(&format!("Turn {next}, about")));
}

#[test]
fn read_cuts_long_parts_and_names_their_handles() {
    let mut talk = long_talk(30);
    talk.user("Write the notes.")
        .call(
            "write_file",
            json!({"path": "notes.md", "content": "n".repeat(5_000)}),
            "Wrote notes.md.",
        )
        .reply("Written.");
    let saved = session(vec![vec![talk.task("opus", false)]]);
    let read = call(&saved, json!({"operation": "read", "turn": 12})).unwrap();
    assert!(read.starts_with("Conversation 1, turn 12 of 31.\n\nTurn 12, about"));
    assert!(read.contains("12.1 User\nTurn 12 asks for step012"));
    assert!(read.contains("12.2 Agent\nLooking at step012."));
    assert!(
        read.contains(
            "12.3 Call shell\n{\"command\":\"make step012\"}\nResult\noutput of step012\n"
        )
    );
    assert!(read.contains("Cut. `result` 12.3 shows all 3,000 characters."));
    assert!(read.contains("12.4 Reply\nStep012 is done."));
    let shown = read.split("Result\n").nth(1).unwrap();
    assert_eq!(
        shown.split("\nCut.").next().unwrap().chars().count(),
        PART_SHOWN
    );

    // Parameters are cut to 300 characters, and `result` shows them.
    let write = call(&saved, json!({"operation": "read", "turn": 31})).unwrap();
    assert!(write.contains("31.2 Call write_file\n{\"content\":\"nnn"));
    let parameters = write.split("31.2 Call write_file\n").nth(1).unwrap();
    let parameters = parameters.lines().next().unwrap();
    assert_eq!(parameters.chars().count(), PARAMETERS_SHOWN + 1);
    assert!(parameters.ends_with('…'));
    assert!(write.contains("Result\nWrote notes.md.\nCut. `result` 31.2 shows all"));
    let whole = call(&saved, json!({"operation": "result", "turn": "31.2"})).unwrap();
    assert!(whole.contains(&"n".repeat(5_000)));
    assert!(whole.ends_with("\n\nResult\nWrote notes.md."));

    // Many turns stop at the cap, and say how to go on.
    let many = call(
        &saved,
        json!({"operation": "read", "turn": 1, "through": 30}),
    )
    .unwrap();
    assert!(tokens(&many) <= OUTPUT_TOKENS);
    let next = many
        .rsplit_once("The output stops at its limit here. `read` with turn ")
        .unwrap()
        .1;
    let next = next.strip_suffix(" and through 30 goes on.").unwrap();
    let at = At::parse(next).unwrap();
    // A higher limit shows more.
    let more = call(
        &saved,
        json!({"operation": "read", "turn": 1, "through": 30, "limit": 20000}),
    )
    .unwrap();
    assert!(tokens(&more) <= MAX_OUTPUT_TOKENS);
    assert!(more.len() > many.len() * 2);
    // Going on starts at the part where it stopped.
    let rest = call(
        &saved,
        json!({"operation": "read", "turn": next, "through": 30}),
    )
    .unwrap();
    let start = match at.part {
        Some(part) => format!("{}.{part} ", at.turn),
        None => format!("Turn {}, about", at.turn),
    };
    assert!(rest.contains(&start), "{start} in {rest}");
    if at.part.is_some_and(|part| part > 1) {
        assert!(rest.contains(&format!(", from part {next}")));
        assert!(!rest.contains(&format!("{}.1 User", at.turn)));
    }
}

#[test]
fn result_pages_a_long_result() {
    let mut talk = Talk::new();
    let output: String = (0..5_000).map(|n| format!("line {n:05}\n")).collect();
    talk.user("Dump it.")
        .call("shell", json!({"command": "dump"}), &output)
        .reply("Dumped.");
    let saved = session(vec![vec![talk.task("opus", false)]]);
    let first = call(&saved, json!({"operation": "result", "turn": "1.2"})).unwrap();
    assert!(first.starts_with("1.2 Call shell, characters 1 to 20,000 of 55,000.\n\nline 00000\n"));
    assert!(first.ends_with("`result` 1.2 with offset 20000 goes on."));
    let second = call(
        &saved,
        json!({"operation": "result", "turn": "1.2", "offset": 20000}),
    )
    .unwrap();
    assert!(second.starts_with("1.2 Call shell, characters 20,001 to 40,000 of 55,000."));
    let third = call(
        &saved,
        json!({"operation": "result", "turn": "1.2", "offset": 40000}),
    )
    .unwrap();
    assert!(third.starts_with("1.2 Call shell, characters 40,001 to 55,000 of 55,000."));
    assert!(!third.contains("goes on"));
    let page = |text: &str| text.split_once("\n\n").unwrap().1.to_string();
    let second = page(&second);
    let second = second.rsplit_once("\n\n`result`").unwrap().0;
    let first = page(&first);
    let first = first.rsplit_once("\n\n`result`").unwrap().0;
    assert_eq!(format!("{first}{second}{}", page(&third)), output);
    // A small limit makes the page smaller.
    let small = call(
        &saved,
        json!({"operation": "result", "turn": "1.2", "limit": 1000}),
    )
    .unwrap();
    assert!(tokens(&small) <= 1_000);
    assert!(
        small.contains("characters 1 to 3,600 of 55,000."),
        "{}",
        &small[..80]
    );
    // A short part is whole.
    let whole = call(&saved, json!({"operation": "result", "turn": "1.1"})).unwrap();
    assert_eq!(whole, "1.1 User, 8 characters.\n\nDump it.");
}

#[test]
fn search_ranks_turns_by_their_words() {
    // Turn 77's command output holds both words, turn 150's message one
    // of them twice, and turn 200's reply the other.
    let talk = long_talk_with(300, |n| match n {
        77 => [
            "",
            "\nbefore\nit calls Parse_Args with the config\nafter",
            "",
        ],
        150 => [" Then parse_args, and parse_args again.", "", ""],
        120 => ["", "", " The config, with care."],
        200 => ["", "", " It needed the config."],
        _ => ["", "", ""],
    });
    let saved = session(vec![vec![talk.task("opus", false)]]);
    let found = call(
        &saved,
        json!({"operation": "search", "words": "parse_args `config`"}),
    )
    .unwrap();
    assert!(
        found.starts_with(
            "4 turns of conversation 1 match “parse_args” or “config”, the best first."
        ),
        "{found}"
    );
    let hits: Vec<&str> = found
        .split("\n\n")
        .skip(1)
        .map(|hit| hit.lines().next().unwrap())
        .collect();
    assert_eq!(
        hits,
        [
            "77.3 Call shell",
            "150.1 User, 1 of 2 words",
            "200.5 Reply, 1 of 2 words",
            "120.5 Reply, 1 of 2 words"
        ]
    );
    // A three-line snippet around the match.
    assert!(
        found.contains("77.3 Call shell\n  before\n  it calls Parse_Args with the config\n  after")
    );
    assert!(found.contains("150.1 User, 1 of 2 words\n  …"));
    assert!(found.contains("Then parse_args, and parse_args again."));

    // A turn that holds the words as a phrase comes before those that
    // hold them apart.
    let phrase = call(
        &saved,
        json!({"operation": "search", "words": "with the config"}),
    )
    .unwrap();
    assert!(
        phrase.starts_with(
            "300 turns of conversation 1 match “with”, “the” or “config”, the best first, and here are the first 20.\n\n77.3 Call shell\n  before\n  it calls Parse_Args with the config\n"
        ),
        "{phrase}"
    );

    // Twenty hits at most, and newer turns first among equals.
    let many = call(&saved, json!({"operation": "search", "words": "make"})).unwrap();
    assert!(many.starts_with(
        "300 turns of conversation 1 match “make”, the best first, and here are the first 20."
    ));
    let turns: Vec<&str> = many
        .split("\n\n")
        .skip(1)
        .map(|hit| hit.split('.').next().unwrap())
        .collect();
    assert_eq!(turns.len(), 20);
    assert_eq!(turns[0], "300");
    assert_eq!(turns[19], "281");
    assert!(many.contains("300.3 Call shell\n  shell {\"command\":\"make step300\"}"));

    let none = call(&saved, json!({"operation": "search", "words": "nowhere"})).unwrap();
    assert_eq!(none, "No turn of conversation 1 mentions “nowhere”.");
}

#[tokio::test]
async fn a_call_without_a_number_reads_the_conversation_it_goes_on_from() {
    let mut first = Talk::new();
    first.user("The first conversation.").reply("One.");
    let mut second = Talk::new();
    second.user("The second conversation.").reply("Two.");
    let saved = Arc::new(session(vec![
        vec![first.task("opus", false)],
        vec![second.task("opus", false)],
    ]));
    #[derive(Debug)]
    struct Continues(Arc<Session>, uuid::Uuid);
    impl Saved for Continues {
        fn count(&self) -> usize {
            self.0.count()
        }
        fn runs(&self, number: usize) -> Result<Vec<AgentResult>, String> {
            self.0.runs(number)
        }
        fn continued(&self, run: uuid::Uuid) -> Option<usize> {
            (run == self.1).then_some(2)
        }
    }
    let run = uuid::Uuid::new_v4();
    let tool = EarlierConversation(Arc::new(Continues(saved, run)));
    let execute = |task_id| {
        let tool = &tool;
        async move {
            let context = TaskContext::new(task_id, "main", "Go.", Path::new("/work"));
            let params = [("operation".to_string(), json!("outline"))].into();
            tool.execute(&context, params).await
        }
    };
    let outline = execute(run).await.unwrap();
    assert!(
        outline.contains("User  The second conversation."),
        "{outline}"
    );
    let error = execute(uuid::Uuid::new_v4()).await.unwrap_err().to_string();
    assert!(
        error.contains(
            "Give `conversation`, a number from 1 to 2, since this conversation doesn't go on from an earlier one."
        ),
        "{error}"
    );
    let params = [
        ("operation".to_string(), json!("outline")),
        ("conversation".to_string(), json!(1)),
    ]
    .into();
    let context = TaskContext::new(run, "main", "Go.", Path::new("/work"));
    let named = tool.execute(&context, params).await.unwrap();
    assert!(named.contains("User  The first conversation."));
}

#[test]
fn mistakes_are_said_plainly() {
    let mut talk = Talk::new();
    talk.user("Go.")
        .text("On it.")
        .call("shell", json!({"command": "ls"}), "a")
        .reply("Done.");
    let saved = session(vec![vec![talk.task("opus", false)], vec![]]);
    let error = |params| call(&saved, params).unwrap_err();
    assert_eq!(
        error(json!({"operation": "outline", "conversation": 4})),
        "There is no conversation 4. This session has 2."
    );
    assert_eq!(
        error(json!({"operation": "read", "turn": 9})),
        "There is no turn 9. Conversation 1 has 1 turn."
    );
    assert_eq!(
        error(json!({"operation": "result", "turn": "1.9"})),
        "There is no part 1.9. Turn 1 has 4 parts."
    );
    assert_eq!(
        error(json!({"operation": "result", "turn": "1"})),
        "`result` needs a part's handle such as 1.1, not a turn alone."
    );
    assert_eq!(
        error(json!({"operation": "result", "turn": "1.3", "offset": 10})),
        "Part 1.3 has 1 character, so offset 10 is past its end."
    );
    assert!(
        error(json!({"operation": "list"}))
            .contains("takes outline, read, search or result, not \"list\"")
    );
    assert!(error(json!({"operation": "read", "turn": "twelve"})).contains("such as 12.3"));
    assert_eq!(
        error(json!({"operation": "read"})),
        "`read` needs `turn`, a turn's number such as 12."
    );
    assert_eq!(
        error(json!({"operation": "search", "words": " "})),
        "`search` needs `words`, such as a path, a name or an error."
    );
    assert_eq!(
        call(&saved, json!({"operation": "outline", "conversation": 2})).unwrap(),
        "Conversation 2 has no turns."
    );
    assert_eq!(
        call(&session(Vec::new()), json!({"operation": "outline"})).unwrap_err(),
        "This session has no conversation to read yet."
    );
}

#[test]
fn a_conversation_of_two_runs_reads_as_one() {
    let mut first = Talk::new();
    first
        .user("Start the port.")
        .reply("Started.")
        .user("Go on.")
        .call(
            "shell",
            json!({"command": "cargo build"}),
            "error: it failed",
        );
    let mut second = Talk::new();
    second
        .user("Try again.")
        .call("shell", json!({"command": "cargo build"}), "Finished")
        .reply("Built.");
    let saved = session(vec![vec![
        first.task("opus", false),
        second.task("opus", false),
    ]]);
    let outline = call(&saved, json!({"operation": "outline"})).unwrap();
    assert!(outline.starts_with("Conversation 1, turns 1 to 3 of 3."));
    assert!(outline.contains("Turn 2, about"));
    assert!(outline.contains("  User  Go on.\n  No reply"));
    assert!(outline.contains("Turn 3, 20"));
    assert!(outline.contains("  User  Try again.\n  Reply  Built."));
    let read = call(
        &saved,
        json!({"operation": "read", "turn": 2, "through": 3}),
    )
    .unwrap();
    assert!(
        read.contains("2.2 Call shell\n{\"command\":\"cargo build\"}\nResult\nerror: it failed")
    );
    assert!(read.contains("3.2 Call shell\n{\"command\":\"cargo build\"}\nResult\nFinished"));
    assert_eq!(
        call(&saved, json!({"operation": "result", "turn": "3.2"})).unwrap(),
        "3.2 Call shell, 8 characters.\n\nFinished"
    );
    let tasks: Vec<arko_agents::Task> = saved.conversations[0]
        .iter()
        .map(|result| result.task().unwrap().clone())
        .collect();
    let (workers, open) = no_workers();
    let brief = brief(&BriefParts {
        number: 1,
        model: "opus",
        runs: &[&tasks[0], &tasks[1]],
        report: None,
        workers: &workers,
        open: &open,
        budget: 6_000,
    });
    assert!(brief.contains("- 1 Start the port.\n- 2 Go on.\n- 3 Try again."));
    assert!(brief.contains("### Turn 2\n\nUser\nGo on.\n\nNo reply.\n\n### Turn 3"));
}

/// A Claude Code agent's conversation names Claude Code's tools as Claude
/// Code does, and Arko's own without their server's prefix. Its text and its
/// calls come in messages of their own, and its replies are text.
#[test]
fn a_claude_code_conversation_lists_the_files_it_changed() {
    let mut talk = Talk::new();
    talk.user("Fix it.")
        .text("I'll fix it.")
        .call(
            "Edit",
            json!({"file_path": "/work/src/a.rs", "old_string": "a", "new_string": "b"}),
            "The file /work/src/a.rs has been updated.",
        )
        .called(
            "Edit",
            json!({"file_path": "/work/src/failed.rs", "old_string": "x", "new_string": "y"}),
            "String to replace not found in file.",
            true,
        )
        .call("Bash", json!({"command": "cargo test"}), "ok")
        .text("Fixed.")
        .user("And the docs.")
        .call(
            "Write",
            json!({"file_path": "/work/docs/b.md", "content": "Docs."}),
            "File created successfully at: /work/docs/b.md",
        )
        .call(
            "MultiEdit",
            json!({"file_path": "/elsewhere/c.rs", "edits": []}),
            "Applied 2 edits.",
        )
        .call(
            "edit_file",
            json!({"path": "src/a.rs", "old_string": "b", "new_string": "c"}),
            "Edited.",
        )
        .text("The docs are done.");
    let task = talk.task("cc:opus", true);
    let (workers, open) = no_workers();
    let brief = brief(&BriefParts {
        number: 1,
        model: "cc:opus",
        runs: &[&task],
        report: None,
        workers: &workers,
        open: &open,
        budget: 6_000,
    });
    assert!(brief.contains(
        "## Files written or edited\n\n- src/a.rs (turns 1 and 2)\n- docs/b.md (turn 2)\n- /elsewhere/c.rs (turn 2)\n\n"
    ), "{brief}");
    assert!(!brief.contains("failed.rs"));
    assert!(brief.contains("### Turn 1\n\nUser\nFix it.\n\nReply\nFixed."));
    let saved = session(vec![vec![task]]);
    let outline = call(&saved, json!({"operation": "outline"})).unwrap();
    assert!(outline.contains("Turn 1, 20"));
    assert!(
        outline.contains(", 3 calls\n  User  Fix it.\n  Reply  Fixed."),
        "{outline}"
    );
    assert!(outline.contains("Turn 2, about"));
    assert!(outline.contains("  User  And the docs.\n  Reply  The docs are done."));
    let read = call(&saved, json!({"operation": "read", "turn": 1})).unwrap();
    assert!(read.contains("1.3 Call Edit\n{\"file_path\":\"/work/src/a.rs\""));
    assert!(read.contains("1.4 Call Edit\n{\"file_path\":\"/work/src/failed.rs\",\"new_string\":\"y\",\"old_string\":\"x\"}\nError\nString to replace not found in file."));
    assert!(read.contains("1.6 Agent\nFixed."));
}

#[test]
fn the_tool_only_reads_and_its_calls_can_overlap() {
    let definition = definition();
    assert_eq!(definition.name, TOOL_NAME);
    assert!(definition.read_only);
    assert!(definition.parallel_safe);
    let names: Vec<&str> = definition
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "operation",
            "conversation",
            "turn",
            "through",
            "words",
            "offset",
            "limit"
        ]
    );
    let required: Vec<&str> = definition
        .parameters
        .iter()
        .filter(|parameter| parameter.required)
        .map(|parameter| parameter.name.as_str())
        .collect();
    assert_eq!(required, ["operation"]);
    let tool = EarlierConversation(Arc::new(session(Vec::new())));
    assert_eq!(tool.recovery_mode(), ToolRecovery::Rerun);
    assert_eq!(Tool::definition(&tool).name, TOOL_NAME);
}
