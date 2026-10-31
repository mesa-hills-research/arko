use super::*;
use crate::questions::QuestionState;

fn question(id: &str, choices: &[&str]) -> AgentQuestion {
    AgentQuestion {
        id: id.into(),
        question: format!("Question {id}?"),
        choices: choices.iter().map(|choice| choice.to_string()).collect(),
        multiple: false,
        default: Some("Spaces".into()),
        state: QuestionState::Open,
        waits: false,
        item: 7,
        number: None,
    }
}

fn sample() -> App {
    let feed = crate::remote::Feed::new(crate::agents::MAIN);
    let mut questions = [
        question("q1", &["Spaces", "Tabs"]),
        question(
            "q2",
            &[
                "Unit tests",
                "Integration tests",
                "Clippy",
                "Formatting",
                "Linux",
                "Windows",
            ],
        ),
        question(
            "q3",
            &[
                "Keep the current configuration and preserve all existing comments and ordering",
                "Write a separate example and let each project choose when to migrate",
                "Migrate automatically after making a backup of the original configuration",
            ],
        ),
        question("q4", &[]),
    ];
    questions[0].question = "Tabs or spaces?".into();
    questions[1].question = "Which checks?".into();
    questions[1].multiple = true;
    questions[2].question = "How should I update the configuration?".into();
    questions[3].question = "Anything else I should know?".into();
    feed.ask_questions(
        &questions[..3]
            .iter()
            .map(AgentQuestion::asked)
            .collect::<Vec<_>>(),
        true,
    );
    feed.ask_questions(&[questions[3].asked()], false);
    let mut app = super::super::app::tests::app();
    app.transcript.reset(feed.subscribe().0);
    app.set_reading(crate::remote::Reading {
        questions: feed.open_questions(),
        ..Default::default()
    });
    app
}

fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn capture(app: &mut App, width: u16, height: u16) -> (String, Position) {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| super::super::screen::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, terminal.get_cursor_position().unwrap())
}

#[test]
fn a_question_that_takes_one_answer_has_one_pick_and_keeps_the_words() {
    let question = question("q1", &["Spaces", "Tabs"]);
    let mut draft = Draft::default();
    pick(&mut draft, &question, Opt::YouDecide, false);
    assert!(draft.you_decide);
    pick(&mut draft, &question, Opt::Choice(1), false);
    assert_eq!((draft.chosen.clone(), draft.you_decide), (vec![1], false));
    draft.other = " four wide ".into();
    assert!(other_on(&draft));
    assert_eq!(
        answer_text(&draft, &question).as_deref(),
        Some("Tabs, plus “four wide”")
    );
    // Another pick keeps the words.
    pick(&mut draft, &question, Opt::Choice(0), false);
    assert_eq!(
        draft.answer(&question),
        Answer {
            choices: vec!["Spaces".into()],
            text: Some("four wide".into()),
            you_decide: false,
        }
    );
    pick(&mut draft, &question, Opt::YouDecide, false);
    assert_eq!(
        answer_text(&draft, &question).as_deref(),
        Some("You decide (Spaces), plus “four wide”")
    );
    assert_eq!(
        draft.answer(&question),
        Answer {
            choices: Vec::new(),
            text: Some("four wide".into()),
            you_decide: true,
        }
    );
    // The words alone, once nothing is picked.
    pick(&mut draft, &question, Opt::YouDecide, true);
    assert_eq!(answer_text(&draft, &question).as_deref(), Some("four wide"));
    draft.other.clear();
    assert_eq!(answer_text(&draft, &question), None);
    pick(&mut draft, &question, Opt::Choice(0), false);
    pick(&mut draft, &question, Opt::Choice(0), false);
    assert_eq!(draft.chosen, vec![0]);
    pick(&mut draft, &question, Opt::Choice(0), true);
    assert!(draft.chosen.is_empty());
}

#[test]
fn the_field_edits_and_wraps_at_its_cursor() {
    let mut draft = Draft::default();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    for c in "push it".chars() {
        field_key(&mut draft, key(KeyCode::Char(c)));
    }
    for _ in 0..2 {
        field_key(&mut draft, key(KeyCode::Left));
    }
    for c in "now ".chars() {
        field_key(&mut draft, key(KeyCode::Char(c)));
    }
    assert_eq!(draft.other, "push now it");
    field_key(
        &mut draft,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
    );
    assert_eq!(draft.other, "push it");
    field_paste(&mut draft, "a\nb");
    assert_eq!(draft.other, "push a bit");
    let text = "keep the branch local until the tests pass on every platform";
    let (rows, cursor) = field_rows(text, text.chars().count(), 20, Style::new());
    assert!(rows.len() >= 3, "{rows:?}");
    assert_eq!(cursor.0, rows.len() - 1);
}

#[test]
fn questions_and_summary_keep_their_height_and_long_fields_stay_visible() {
    for (width, height) in [(80, 24), (140, 40)] {
        let mut app = sample();
        let mut first_height = None;
        for at in 0..4 {
            app.focus_on(at);
            capture(&mut app, width, height);
            assert_eq!(
                *first_height.get_or_insert(app.panel.height.1),
                app.panel.height.1,
                "question {at}"
            );
        }
        press(&mut app, KeyCode::Tab);
        let (summary, _) = capture(&mut app, width, height);
        assert!(
            summary.contains("Your answers") && summary.contains("Not answered"),
            "{summary}"
        );
        assert_eq!(first_height, Some(app.panel.height.1));
        press(&mut app, KeyCode::BackTab);
        let text = format!(
            "{}VISIBLE_END",
            "Keep all configuration comments and run the tests before publishing. ".repeat(45)
        );
        app.panel_paste(&text);
        let (rows, cursor) = capture(&mut app, width, height);
        assert!(rows.contains("VISIBLE_END"), "{width}x{height}\n{rows}");
        assert!(cursor.x < width && cursor.y < height, "{cursor:?}");
        press(&mut app, KeyCode::Home);
        let (rows, _) = capture(&mut app, width, height);
        assert!(rows.contains("Keep all configuration"), "{rows}");
    }
}

#[test]
fn picks_advance_and_choose_any_waits_then_the_summary_can_edit_and_send() {
    let mut app = sample();
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(app.focus_index(), 1);
    assert_eq!(app.panel.drafts["q1"].chosen, [1]);
    press(&mut app, KeyCode::Char('1'));
    press(&mut app, KeyCode::Char('3'));
    assert_eq!(app.panel.drafts["q2"].chosen, [0, 2]);
    assert_eq!(app.focus_index(), 1);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.focus_index(), 2);
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.focus_index(), 1);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    app.panel_paste("Keep it local");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.panel.review, Some(4));
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter);
    assert!(app.panel.review.is_none());
    assert_eq!(app.focus_index(), 3);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.panel.review, Some(4));
    press(&mut app, KeyCode::Enter);
    assert!(app.take_effects().iter().any(|effect| matches!(effect,
        Effect::Input(Input::AnswerQuestions(answers)) if answers.answers.iter().any(|a| a.answer.text.as_deref() == Some("Keep it local")))));
}

#[test]
fn the_words_typed_go_with_the_choices_picked_or_you_decide() {
    let mut app = sample();
    let typing = |app: &mut App, text: &str| {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    };
    // One choice, then words beside it: the choice stays picked.
    press(&mut app, KeyCode::Char('2'));
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    typing(&mut app, "keep it narrow");
    assert_eq!(app.panel.drafts["q1"].chosen, [1]);
    press(&mut app, KeyCode::Enter);
    // Several choices, with words.
    press(&mut app, KeyCode::Char('1'));
    press(&mut app, KeyCode::Char('3'));
    typing(&mut app, "on CI");
    press(&mut app, KeyCode::Enter);
    // "You decide", with words: typing doesn't take it back.
    press(&mut app, KeyCode::Char('4'));
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    typing(&mut app, "ask Bob first");
    assert!(app.panel.drafts["q3"].you_decide);
    press(&mut app, KeyCode::Enter);
    // Words alone.
    typing(&mut app, "nothing");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.panel.review, Some(4));
    let (summary, _) = capture(&mut app, 140, 40);
    for answer in [
        "Tabs, plus “keep it narrow”",
        "Unit tests, Clippy, plus “on CI”",
        "You decide (Spaces), plus “ask Bob first”",
        "nothing",
    ] {
        assert!(summary.contains(answer), "{answer}\n{summary}");
    }
    press(&mut app, KeyCode::Enter);
    let answer = |choices: &[&str], text: &str, you_decide: bool| Answer {
        choices: choices.iter().map(|choice| choice.to_string()).collect(),
        text: Some(text.to_string()),
        you_decide,
    };
    let sent: Vec<Answers> = app
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Input(Input::AnswerQuestions(answers)) => Some(answers),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        [Answers {
            answers: vec![
                QuestionAnswer::new("q1", answer(&["Tabs"], "keep it narrow", false)),
                QuestionAnswer::new("q2", answer(&["Unit tests", "Clippy"], "on CI", false)),
                QuestionAnswer::new("q3", answer(&[], "ask Bob first", true)),
                QuestionAnswer::new("q4", answer(&[], "nothing", false)),
            ],
        }]
    );
}

#[test]
fn the_empty_field_always_shows_the_same_muted_placeholder() {
    let mut app = sample();
    for at in [0, 3] {
        app.focus_on(at);
        for selected in [0, options(&app.open_questions()[at]).len() - 1] {
            app.panel.row = selected;
            let card = steps_card(&app, 75);
            let text = card
                .body
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Type your own answer"), "{text}");
            assert!(!text.contains("Other"), "{text}");
            let placeholder = card
                .body
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "Type your own answer")
                .unwrap();
            assert_eq!(placeholder.style, app.theme.dim());
        }
    }
}

#[test]
fn chat_dismiss_and_reopen_keep_their_keys() {
    let mut app = sample();
    press(&mut app, KeyCode::Esc);
    assert!(!app.panel.open);
    assert_eq!(app.panel.attached.len(), 4);
    app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT));
    assert!(app.panel.open);
    app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(app.take_effects().iter().any(|effect| matches!(effect,
        Effect::Input(Input::DismissQuestions(ids)) if ids.len() == 4)));
}

/// "Answer in chat" attaches the questions open then. One asked after it
/// waits in a chip of its own beside them, even while the box is empty,
/// and stays open when the message goes. Ctrl+N dismisses it alone.
#[test]
fn a_question_asked_after_answer_in_chat_stays_out_of_the_message() {
    let reading = |questions: &[&AgentQuestion]| crate::remote::Reading {
        questions: questions.iter().map(|&question| question.clone()).collect(),
        ..Default::default()
    };
    let first = question("q1", &["Spaces", "Tabs"]);
    let mut second = question("q2", &[]);
    second.item = 8;
    let mut app = super::super::app::tests::app();
    app.set_reading(reading(&[&first]));
    assert!(app.panel.open);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel.attached, ["q1"]);
    app.panel.last_key = None;
    app.set_reading(reading(&[&first, &second]));
    assert!(!app.panel.open);
    assert_eq!(app.panel.attached, ["q1"]);
    let (screen, _) = capture(&mut app, 80, 12);
    assert!(
        screen.contains(
            "Arko asks 1 question    Alt+Q  answer   Ctrl+N  not now\n  Replying to 1 question ×"
        ),
        "{screen}"
    );
    for c in "Spaces.".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    let sent: Vec<Input> = app
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Input(input) => Some(input),
            Effect::Send(sending) => Some(sending.input),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        [Input::Message(crate::ui::Message {
            text: "Spaces.".into(),
            answers: vec!["q1".into()],
            ..Default::default()
        })]
    );
    app.set_reading(reading(&[&second]));
    let (screen, _) = capture(&mut app, 80, 12);
    assert!(screen.contains("1 open question"), "{screen}");

    // Ctrl+N beside the questions attached dismisses only the new one.
    let mut app = super::super::app::tests::app();
    app.set_reading(reading(&[&first]));
    press(&mut app, KeyCode::Esc);
    app.set_reading(reading(&[&first, &second]));
    app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(app.take_effects().iter().any(|effect| matches!(effect,
        Effect::Input(Input::DismissQuestions(ids)) if *ids == ["q2"])));
    assert_eq!(app.panel.attached, ["q1"]);
}

#[test]
fn answer_panel_goldens() {
    for (width, height) in [(80, 24), (140, 40)] {
        let mut app = sample();
        let (first, _) = capture(&mut app, width, height);
        app.focus_on(2);
        let (long, _) = capture(&mut app, width, height);
        app.focus_on(3);
        app.panel_paste("Keep the branch local until the tests pass on Linux and Windows. Preserve the configuration comments and show the migration diff before writing any files.");
        let (other, _) = capture(&mut app, width, height);
        press(&mut app, KeyCode::Enter);
        let (summary, _) = capture(&mut app, width, height);
        let actual = format!(
            "{first}\n\n--- Long choices ---\n{long}\n\n--- Written answer ---\n{other}\n\n--- Summary ---\n{summary}\n"
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/answer-panel/{width}x{height}.txt"));
        if std::env::var_os("UPDATE_GOLDENS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            actual,
            "{}",
            path.display()
        );
    }
}
