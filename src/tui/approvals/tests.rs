use super::*;
use crate::approvals::{
    Approval, BillingApproval, ProfileApproval, Scope, ServerApproval, SkillApproval,
};
use crate::remote::Reading;

fn request(id: &str, approval: Approval) -> ApprovalRequest {
    ApprovalRequest {
        id: id.into(),
        batch: 1,
        approval,
    }
}

fn skill() -> Approval {
    Approval::Skill(SkillApproval {
        name: "release-notes".into(),
        scope: Scope::User,
        description: "How this project writes release notes.".into(),
        why: "The checklist changed.".into(),
        workers: true,
        ..SkillApproval::default()
    })
}

fn server() -> Approval {
    Approval::McpServer(ServerApproval {
        name: "github".into(),
        scope: Scope::Project,
        command: "npx -y @modelcontextprotocol/server-github".into(),
        why: "Read the issues.".into(),
        workers: true,
        ..ServerApproval::default()
    })
}

fn profile(instructions: &str) -> Approval {
    Approval::Profile(ProfileApproval {
        name: "3d-judge".into(),
        model: "sol".into(),
        effort: Some("xhigh".into()),
        scope: "every project".into(),
        description: "Judges 3D results".into(),
        limits: vec!["Workers with it get only the skill ponytail.".into()],
        folder: "Arko's profiles folder".into(),
        instructions: instructions.into(),
        ..ProfileApproval::default()
    })
}

fn billing() -> Approval {
    Approval::WorkerBilling(BillingApproval {
        model: "Opus 5.5".into(),
        input_price: Some(5.0),
        output_price: Some(25.0),
        spent: Some(3.2),
        budget: None,
        task: "Review the parser".into(),
    })
}

/// An app whose tray holds `requests`.
fn with(requests: Vec<ApprovalRequest>) -> App {
    let mut app = super::super::app::tests::app();
    app.set_reading(Reading {
        approvals: requests,
        ..Reading::default()
    });
    app
}

/// At 80 columns, an update that turns auto_load on says so in its row,
/// ahead of what it changes, and its question wraps whole however long the
/// skill's name.
#[test]
fn an_auto_loading_update_says_so_at_80_columns() {
    let name = "project-release-and-documentation-checks-for-all-conversations";
    let approval = Approval::Skill(SkillApproval {
        name: name.into(),
        scope: Scope::Project,
        auto_load: true,
        workers: true,
        description: "Release steps.".into(),
        update: Some(crate::approvals::SkillUpdate {
            saved: "1 Nov".into(),
            diff: "+ auto_load: true".into(),
            added: vec!["scripts/check.sh".into(), "docs/release.md".into()],
            ..Default::default()
        }),
        ..SkillApproval::default()
    });
    let mut app = with(vec![request("a1", approval)]);
    let rows = screen(&mut app, 80, 40);
    assert!(
        rows.lines()
            .any(|line| line.contains("this project  loads into every new")),
        "{rows}"
    );
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('v'));
    let rows = screen(&mut app, 80, 40);
    assert!(
        flat(&rows).contains(&format!(
            "Update the skill {name} for this project and load it into every new conversation?"
        )),
        "{rows}"
    );
}

fn three() -> App {
    with(vec![
        request("a1", skill()),
        request("a2", server()),
        request("a3", profile("")),
    ])
}

fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn alt_a(app: &mut App) {
    app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
    screen(app, 120, 40);
    let age = app.tray.now_ms() + 500;
    app.tray.clock = Some(Instant::now() - std::time::Duration::from_millis(age));
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| super::super::screen::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The screen's words with single spaces between them, for the keys, whose
/// caps pad them.
fn flat(rows: &str) -> String {
    rows.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The answers the view sent.
fn sent(app: &mut App) -> Vec<ApprovalAnswers> {
    app.take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Send(crate::tui::app::Sending {
                input: Input::AnswerApprovals(answers),
                ..
            }) => Some(answers),
            _ => None,
        })
        .collect()
}

fn choices(answers: &ApprovalAnswers) -> Vec<(&str, bool, bool)> {
    answers
        .choices
        .iter()
        .map(|choice| (choice.id.as_str(), choice.approve, choice.option))
        .collect()
}

/// Several requests show a row each, with a box all start checked, the
/// kind, the name, the scope and a short summary in columns, under a
/// heading that counts them. Unfocused, the heading says how to answer,
/// and the status line says approval waits.
#[test]
fn several_requests_show_a_row_each() {
    let mut app = three();
    let rows = screen(&mut app, 100, 24);
    let lines: Vec<&str> = rows.lines().collect();
    let heading = lines
        .iter()
        .position(|line| line.starts_with(" 3 approvals"))
        .unwrap_or_else(|| panic!("{rows}"));
    assert!(flat(lines[heading]).ends_with("Alt+A answer"), "{rows}");
    assert_eq!(
        lines[heading + 1..heading + 4],
        [
            "   [x] skill    release-notes  every project  How this project writes release notes.",
            "   [x] server   github         this project   npx -y @modelcontextprotocol/server-github",
            "   [x] profile  3d-judge       every project  sol xhigh",
        ]
    );
    assert!(rows.contains("● needs approval"), "{rows}");
    // Typing goes to the message box.
    for c in "ok".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    assert_eq!(app.editor.text(), "ok");
    assert!(sent(&mut app).is_empty());
}

/// Focused, the keys show in the heading and the cursor on its row. Space
/// unchecks a row, the approve key counts the selection, and Enter
/// approves the rows checked and rejects the others at once.
#[test]
fn enter_approves_the_selection_and_rejects_the_rest() {
    let mut app = three();
    alt_a(&mut app);
    assert!(app.tray.focused);
    let rows = screen(&mut app, 120, 24);
    assert!(
        flat(&rows)
            .contains("Space select A all Enter approve all R reject all V view Esc message"),
        "{rows}"
    );
    assert!(rows.contains(" › [x] skill    release-notes"), "{rows}");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char(' '));
    let rows = screen(&mut app, 120, 24);
    assert!(rows.contains(" › [ ] profile  3d-judge"), "{rows}");
    assert!(flat(&rows).contains("Enter approve 2, reject 1"), "{rows}");
    press(&mut app, KeyCode::Enter);
    let answers = sent(&mut app);
    assert_eq!(answers.len(), 1);
    assert_eq!(
        choices(&answers[0]),
        [
            ("a1", true, false),
            ("a2", true, false),
            ("a3", false, false)
        ]
    );
    assert_eq!(answers[0].words, None);
    assert!(!app.tray.focused, "the keys go back to the message box");
}

/// A selects none, then all again. R rejects them all, whatever is
/// selected.
#[test]
fn a_selects_all_or_none_and_r_rejects_all() {
    let mut app = three();
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('a'));
    assert_eq!(app.tray.unselected.len(), 3);
    assert!(flat(&screen(&mut app, 120, 24)).contains("Enter reject all"));
    press(&mut app, KeyCode::Char('a'));
    assert!(app.tray.unselected.is_empty());
    press(&mut app, KeyCode::Char('r'));
    let answers = sent(&mut app);
    assert_eq!(
        choices(&answers[0]),
        [
            ("a1", false, false),
            ("a2", false, false),
            ("a3", false, false)
        ]
    );
}

#[test]
fn answers_wait_for_rendering_and_leave_newer_requests_waiting() {
    for key in [KeyCode::Enter, KeyCode::Char('r')] {
        let mut app = with(vec![request("a1", skill())]);
        app.paste("Reason");
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
        app.tray.clock = Some(Instant::now() - std::time::Duration::from_secs(1));
        press(&mut app, key);
        assert!(sent(&mut app).is_empty(), "unrendered requests wait");
        assert_eq!(
            app.hint(),
            Some("Requests show for half a second before you can answer")
        );
        assert_eq!(app.editor.text(), "Reason");
        assert!(app.tray.focused);
        screen(&mut app, 120, 40);
        press(&mut app, key);
        assert!(sent(&mut app).is_empty(), "newly rendered requests wait");
        let age = app.tray.now_ms() + 500;
        app.tray.clock = Some(Instant::now() - std::time::Duration::from_millis(age));
        app.set_reading(Reading {
            approvals: vec![request("a1", skill()), request("a2", server())],
            ..Reading::default()
        });
        screen(&mut app, 120, 40);
        press(&mut app, key);
        let answers = sent(&mut app);
        assert_eq!(choices(&answers[0]), [("a1", key == KeyCode::Enter, false)]);
        assert_eq!(
            answers[0].words, None,
            "the newer request resets message intent"
        );
        app.set_reading(Reading {
            approvals: vec![request("a2", server())],
            ..Reading::default()
        });
        alt_a(&mut app);
        press(&mut app, key);
        assert_eq!(
            choices(&sent(&mut app)[0]),
            [("a2", key == KeyCode::Enter, false)]
        );
    }
}

#[test]
fn labels_count_only_ready_rows_and_schedule_their_redraw() {
    let mut app = with(vec![request("a1", skill())]);
    alt_a(&mut app);
    app.set_reading(Reading {
        approvals: vec![request("a1", skill()), request("a2", server())],
        ..Reading::default()
    });
    let rows = flat(&screen(&mut app, 120, 40));
    assert!(rows.contains("Enter approve 1 R reject 1"), "{rows}");
    assert!(app.approvals_due().is_some());
    let age = app.tray.now_ms() + 500;
    app.tray.clock = Some(Instant::now() - Duration::from_millis(age));
    let rows = flat(&screen(&mut app, 120, 40));
    assert!(rows.contains("Enter approve all R reject all"), "{rows}");
    assert!(app.approvals_due().is_none());
}

#[test]
fn a_window_without_room_explains_why_answers_wait() {
    let mut app = with(vec![request("a1", skill())]);
    app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
    screen(&mut app, 80, 5);
    press(&mut app, KeyCode::Enter);
    assert!(sent(&mut app).is_empty());
    assert_eq!(app.hint(), Some("Make the window larger to answer"));
}

#[test]
fn unfocused_and_covered_requests_are_not_marked_seen() {
    use ratatui::{Terminal, backend::TestBackend};
    for covered in [false, true] {
        let mut app = with(vec![request("a1", skill())]);
        app.focused = covered;
        if covered {
            app.overlay = Some(crate::tui::overlay::Overlay::Remote {
                link: 0,
                since: Instant::now(),
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                draw_tray(frame, &mut app, Rect::new(0, 0, 100, 20));
            })
            .unwrap();
        app.tray.clock = Some(Instant::now() - Duration::from_secs(1));
        terminal
            .draw(|frame| {
                draw_tray(frame, &mut app, Rect::new(0, 0, 100, 20));
            })
            .unwrap();
        assert!(
            !app.tray
                .shown
                .can_answer("a1", app.tray.clock.unwrap().elapsed().as_millis() as u64)
        );
        assert!(app.approvals_due().is_none());
        app.focused = true;
        app.overlay = None;
        terminal
            .draw(|frame| {
                draw_tray(frame, &mut app, Rect::new(0, 0, 100, 20));
            })
            .unwrap();
        assert!(app.approvals_due().is_some());
    }
}

#[test]
fn a_message_begun_before_the_newest_request_stays() {
    for key in [KeyCode::Enter, KeyCode::Char('r')] {
        let mut app = super::super::app::tests::app();
        app.paste("Half-written message");
        app.set_reading(Reading {
            approvals: vec![request("a1", skill())],
            ..Reading::default()
        });
        assert!(!app.includes_message());
        assert!(screen(&mut app, 80, 24).contains("[ ] Include message"));
        alt_a(&mut app);
        press(&mut app, key);
        let answers = sent(&mut app);
        assert_eq!(answers[0].words, None);
        assert_eq!(answers[0].choices[0].approve, key == KeyCode::Enter);
        assert_eq!(app.editor.text(), "Half-written message");
    }
}

#[test]
fn edits_after_a_request_include_the_message_and_m_flips_it() {
    for key in [KeyCode::Enter, KeyCode::Char('r')] {
        for flips in 0..=2 {
            let mut app = three();
            app.paste("Reason");
            assert!(app.includes_message());
            alt_a(&mut app);
            for _ in 0..flips {
                press(&mut app, KeyCode::Char('m'));
            }
            let include = flips != 1;
            let mark = if include { "[x]" } else { "[ ]" };
            let rows = flat(&screen(&mut app, 80, 24));
            assert!(
                rows.contains(&format!("M {mark} Include message")),
                "{rows}"
            );
            press(&mut app, key);
            let answers = sent(&mut app);
            assert_eq!(answers[0].words.as_deref(), include.then_some("Reason"));
            assert_eq!(app.editor.text(), if include { "" } else { "Reason" });
        }
    }
    // Pre-existing words can be included explicitly too.
    let mut app = super::super::app::tests::app();
    app.paste("Draft");
    app.set_reading(Reading {
        approvals: vec![request("a1", skill())],
        ..Reading::default()
    });
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app)[0].words.as_deref(), Some("Draft"));
    assert!(app.editor.is_empty());
}

#[test]
fn the_switch_follows_new_requests_but_not_refreshes_or_cursor_moves() {
    let mut app = three();
    app.paste("Draft");
    app.set_reading(Reading {
        approvals: vec![request("a1", skill()), request("a4", server())],
        ..Reading::default()
    });
    assert!(!app.includes_message());
    press(&mut app, KeyCode::Left);
    assert!(!app.includes_message());
    press(&mut app, KeyCode::Char('x'));
    assert!(
        !app.includes_message(),
        "continuing the older draft keeps it out"
    );
    app.set_reading(Reading {
        approvals: vec![request("a1", skill())],
        ..Reading::default()
    });
    assert!(
        !app.includes_message(),
        "removing a request keeps the choice"
    );
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Esc);
    app.newline();
    assert!(
        app.includes_message(),
        "the manual choice wins over later edits"
    );
}

#[test]
fn history_and_cancelled_dictation_do_not_attach_an_older_draft() {
    for dictation in [false, true] {
        let mut app = super::super::app::tests::app();
        app.editor.set("Earlier message");
        app.editor.take();
        app.paste("Draft");
        app.set_reading(Reading {
            approvals: vec![request("a1", skill())],
            ..Reading::default()
        });
        if dictation {
            use crate::dictation::{DictationEvent, DictationUpdate};
            app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
            app.dictation_opened(7);
            app.dictation_event(DictationUpdate {
                id: 7,
                event: DictationEvent::Listening,
            });
            app.dictation_event(DictationUpdate {
                id: 7,
                event: DictationEvent::Text {
                    settled: "More".into(),
                    tentative: String::new(),
                },
            });
            press(&mut app, KeyCode::Esc);
        } else {
            assert!(app.editor.history_back());
            assert!(app.editor.history_forward());
        }
        assert_eq!(app.editor.text(), "Draft");
        assert!(!app.includes_message());
        alt_a(&mut app);
        press(&mut app, KeyCode::Enter);
        assert_eq!(sent(&mut app)[0].words, None);
        assert_eq!(app.editor.text(), "Draft");
    }
}

#[test]
fn a_request_during_a_refused_send_does_not_take_its_restored_draft() {
    let mut app = super::super::app::tests::app();
    app.paste("Draft");
    press(&mut app, KeyCode::Enter);
    let sending = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Send(sending) => Some(sending),
            _ => None,
        })
        .expect("sent");
    app.set_reading(Reading {
        approvals: vec![request("a1", skill())],
        ..Reading::default()
    });
    screen(&mut app, 80, 24);
    app.verdict(
        sending,
        crate::Verdict::refused(crate::RefusalKind::TooLong, "Shorten the message."),
    );
    assert_eq!(app.editor.text(), "Draft");
    assert!(!app.includes_message());
    alt_a(&mut app);
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app)[0].words, None);
    assert_eq!(app.editor.text(), "Draft");
}

#[test]
fn a_refused_send_restores_the_switch_with_its_draft() {
    for manual in [false, true] {
        let mut app = super::super::app::tests::app();
        app.paste("Draft");
        app.set_reading(Reading {
            approvals: vec![request("a1", skill())],
            ..Reading::default()
        });
        if manual {
            alt_a(&mut app);
            press(&mut app, KeyCode::Char('m'));
            press(&mut app, KeyCode::Esc);
        }
        press(&mut app, KeyCode::Enter);
        let sending = app
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send(sending) => Some(sending),
                _ => None,
            })
            .expect("sent");
        screen(&mut app, 80, 24);
        app.verdict(
            sending,
            crate::Verdict::refused(crate::RefusalKind::TooLong, "Shorten the message."),
        );
        assert_eq!(app.editor.text(), "Draft");
        assert_eq!(app.includes_message(), manual);
    }
}

#[test]
fn emptying_the_box_resets_its_manual_choice() {
    for how in 0..4 {
        let mut app = three();
        app.paste("Reason");
        alt_a(&mut app);
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Esc);
        app.paste(" more");
        assert!(!app.includes_message());
        match how {
            0 => app.editor.clear(),
            1 => {
                app.editor.take();
            }
            2 => app.editor.set(""),
            _ => {
                while !app.editor.is_empty() {
                    press(&mut app, KeyCode::Backspace);
                }
            }
        }
        app.paste("Another reason");
        assert!(app.includes_message());
        alt_a(&mut app);
        press(&mut app, KeyCode::Enter);
        assert_eq!(sent(&mut app)[0].words.as_deref(), Some("Another reason"));
    }
}

#[test]
fn the_message_switch_stays_visible_while_rows_scroll() {
    let mut app = with(
        (0..20)
            .map(|n| request(&format!("a{n}"), skill()))
            .collect(),
    );
    app.paste("Reason");
    alt_a(&mut app);
    for _ in 0..19 {
        press(&mut app, KeyCode::Down);
    }
    let rows = flat(&screen(&mut app, 50, 24));
    assert!(rows.contains("[x] Include message"), "{rows}");
    assert!(rows.contains("R reject all"), "{rows}");
}

#[test]
fn an_empty_message_has_no_switch() {
    let mut app = three();
    assert!(!screen(&mut app, 80, 24).contains("Include message"));
    app.paste(" ");
    assert!(!screen(&mut app, 80, 24).contains("Include message"));
    app.editor.clear();
    alt_a(&mut app);
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app)[0].words, None);
}

/// Included words leave the box and come back on refusal.
#[test]
fn the_message_box_gives_the_answer_its_words() {
    let mut app = three();
    for c in "Name them better.".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('r'));
    let answers = sent(&mut app);
    assert_eq!(answers[0].words.as_deref(), Some("Name them better."));
    assert!(app.editor.is_empty());

    // Words the session refuses come back to the box, with why.
    let mut app = three();
    let long = "y".repeat(crate::approvals::MAX_APPROVAL_WORDS + 1);
    app.editor.set(&long);
    alt_a(&mut app);
    press(&mut app, KeyCode::Enter);
    let sending = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Send(sending) => Some(sending),
            _ => None,
        })
        .expect("sent");
    assert!(app.editor.is_empty());
    let words = "Your words are too long to send. Shorten them to 16 KB.";
    app.verdict(
        sending,
        crate::Verdict::refused(crate::RefusalKind::TooLong, words),
    );
    assert_eq!(app.editor.text(), long);
    assert_eq!(
        app.hint.as_ref().map(|(text, _)| text.as_str()),
        Some(words)
    );
}

/// One request is one row, without a box, and its keys approve or reject
/// it.
#[test]
fn one_request_is_one_row_with_approve_and_reject() {
    let mut app = with(vec![request("a1", profile(""))]);
    alt_a(&mut app);
    let rows = screen(&mut app, 100, 24);
    assert!(rows.contains(" 1 approval"), "{rows}");
    assert!(
        flat(&rows).contains("Enter approve R reject V view Esc message"),
        "{rows}"
    );
    assert!(
        rows.contains(" › profile  3d-judge  every project  sol xhigh"),
        "{rows}"
    );
    assert!(!rows.contains("[x]"), "{rows}");
    // Space and A do nothing with one.
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('a'));
    assert!(app.tray.unselected.is_empty());
    press(&mut app, KeyCode::Enter);
    assert_eq!(choices(&sent(&mut app)[0]), [("a1", true, false)]);
}

/// V opens a row's details in place, under it, with a profile's model, its
/// description and its instructions in a box, which PgDn scrolls.
#[test]
fn v_opens_the_details_in_place() {
    let instructions = (1..=20)
        .map(|n| format!("Check point {n} of the render."))
        .collect::<Vec<_>>()
        .join("\n");
    let mut app = with(vec![
        request("a1", profile(&instructions)),
        request("a2", server()),
    ]);
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('v'));
    let rows = screen(&mut app, 100, 40);
    for shown in [
        "Save the worker profile 3d-judge for every project?",
        "Runs on sol at xhigh effort.",
        "Judges 3D results",
        "Workers with it get only the skill ponytail.",
        "Instructions",
        "│ Check point 1 of the render.",
        "PgDn more",
        "V close",
    ] {
        assert!(flat(&rows).contains(shown), "{shown}\n{rows}");
    }
    assert!(!rows.contains("Check point 9 "), "{rows}");
    // The details sit between the rows.
    let opened = rows.find("Runs on sol").unwrap();
    let next = rows.find("[x] server").unwrap();
    assert!(opened < next, "{rows}");
    for _ in 0..4 {
        press(&mut app, KeyCode::PageDown);
    }
    let rows = screen(&mut app, 100, 40);
    assert!(rows.contains("Check point 20 of the render."), "{rows}");
    assert!(flat(&rows).contains("PgUp more"), "{rows}");
    press(&mut app, KeyCode::Char('v'));
    assert!(!screen(&mut app, 100, 40).contains("Runs on sol"));
}

/// A server's details give its command as code and the warning, and a
/// worker billed per token its price, the day's spend and the option that
/// allows its model for the session, which O turns on and Approve covers.
#[test]
fn a_rows_option_goes_with_its_approval() {
    let mut app = with(vec![request("a1", server()), request("a2", billing())]);
    let rows = screen(&mut app, 120, 30);
    assert!(
        rows.contains(
            "   [x] worker   Opus 5.5  billed per token  $5/$25 per million tokens, $3.20 spent today"
        ),
        "{rows}"
    );
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('v'));
    let rows = screen(&mut app, 120, 30);
    assert!(
        rows.contains("Runs npx -y @modelcontextprotocol/server-github."),
        "{rows}"
    );
    assert!(
        rows.contains("It runs with your permissions, outside the command guard."),
        "{rows}"
    );
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('v'));
    let rows = screen(&mut app, 120, 30);
    for shown in [
        "It costs $5 per million input tokens and $25 per million output tokens.",
        "Today’s API spend is $3.20.",
        "The task is “Review the parser”.",
        "[ ] Allow Opus 5.5 for this session",
        "O option",
    ] {
        assert!(flat(&rows).contains(shown), "{shown}\n{rows}");
    }
    press(&mut app, KeyCode::Char('o'));
    assert!(screen(&mut app, 120, 30).contains("[x] Allow Opus 5.5 for this session"));
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        choices(&sent(&mut app)[0]),
        [("a1", true, false), ("a2", true, true)]
    );
}

/// Alt+A and Esc give the keys back to the message box, and other letters
/// are kept from it while the tray has them. A request that joins the tray
/// starts checked, and the tray goes once its requests are answered
/// elsewhere.
#[test]
fn the_tray_follows_its_requests() {
    let mut app = with(vec![request("a1", skill())]);
    alt_a(&mut app);
    press(&mut app, KeyCode::Char('x'));
    assert!(app.editor.is_empty());
    press(&mut app, KeyCode::Esc);
    assert!(!app.tray.focused);
    alt_a(&mut app);
    alt_a(&mut app);
    assert!(!app.tray.focused);
    alt_a(&mut app);
    press(&mut app, KeyCode::Char(' '));
    app.set_reading(Reading {
        approvals: vec![request("a1", skill()), request("a2", server())],
        ..Reading::default()
    });
    press(&mut app, KeyCode::Char(' '));
    assert!(app.tray.unselected.contains("a1"), "{:?}", app.tray);
    assert!(!app.tray.unselected.contains("a2"));
    app.set_reading(Reading::default());
    assert!(!app.tray.focused);
    assert!(!screen(&mut app, 80, 24).contains("approval"));
    alt_a(&mut app);
    assert_eq!(
        app.hint.as_ref().map(|(text, _)| text.as_str()),
        Some("No approvals wait")
    );
}
