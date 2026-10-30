use super::*;

fn skill() -> SkillApproval {
    SkillApproval {
        name: "release-notes".into(),
        scope: Scope::User,
        description: "How this project writes release notes.".into(),
        source: SkillSource::Downloaded {
            url: "https://example.com/release-notes.zip".into(),
            bytes: 14 * 1024,
        },
        workers: true,
        models: vec!["haiku".into(), "cc:haiku".into()],
        scripts: vec!["scripts/release.sh".into()],
        update: None,
        why: "The release checklist changed.".into(),
        files: vec![
            SkillFile {
                path: "SKILL.md".into(),
                bytes: 3300,
                text: Some(format!(
                    "---\nname: release-notes\n---\n{}",
                    "x".repeat(3182)
                )),
            },
            SkillFile {
                path: "checklist.md".into(),
                bytes: 1200,
                text: Some("- Tag it".into()),
            },
            SkillFile {
                path: "scripts/release.sh".into(),
                bytes: 300,
                text: None,
            },
        ],
    }
}

fn server() -> ServerApproval {
    ServerApproval {
        name: "github".into(),
        scope: Scope::Project,
        command: "npx -y @modelcontextprotocol/server-github".into(),
        env: vec![
            EnvName {
                name: "GITHUB_TOKEN".into(),
                set: true,
            },
            EnvName {
                name: "SENTRY_DSN".into(),
                set: false,
            },
        ],
        workers: false,
        ignored: true,
        shared: None,
        replaces: None,
        why: "Read and file this repository's issues.".into(),
    }
}

fn lines(approval: &Approval) -> Vec<(Look, String)> {
    approval
        .details()
        .into_iter()
        .map(|detail| (detail.look, detail.text))
        .collect()
}

/// A skill's row and details say what the card in the design says, in its
/// order, with SKILL.md in the box and the other files a line each.
#[test]
fn a_skill_reads_as_its_card() {
    let approval = Approval::Skill(skill());
    assert_eq!(
        (
            approval.kind_word(),
            approval.name(),
            approval.scope(),
            approval.summary()
        ),
        (
            "Skill",
            "release-notes",
            "every project",
            "How this project writes release notes.".to_string()
        )
    );
    assert_eq!(
        approval.title(),
        "Save the skill release-notes for every project?"
    );
    assert_eq!(approval.option(), None);
    let shown = lines(&approval);
    let text = |at: usize| shown[at].1.as_str();
    assert_eq!(text(0), "How this project writes release notes.");
    assert_eq!(
        text(1),
        "Downloaded from https://example.com/release-notes.zip (14 KB)."
    );
    assert_eq!(
        text(2),
        "Workers get it too. Listed for agents on haiku and cc:haiku."
    );
    assert_eq!(
        text(3),
        "SKILL.md is 3,210 characters (4 lines), with 2 more files. It has scripts an agent may run (scripts/release.sh)."
    );
    assert_eq!(shown[4], (Look::Heading, "Why".into()));
    assert_eq!(text(5), "The release checklist changed.");
    assert_eq!(shown[6], (Look::Heading, "Files".into()));
    assert_eq!(shown[7].0, Look::Box);
    assert!(text(7).starts_with("---\nname: release-notes"));
    assert!(approval.every_project());
    assert_eq!(
        shown[8..],
        [
            (Look::File, "checklist.md (1 KB)".to_string()),
            (Look::File, "scripts/release.sh (300 bytes)".to_string()),
        ]
    );
    // Written by the agent, kept to the main agent, for this project.
    let mut own = skill();
    own.source = SkillSource::Agent;
    own.workers = false;
    own.models.clear();
    own.scope = Scope::Project;
    let own = Approval::Skill(own);
    assert_eq!(own.scope(), "this project");
    let shown = lines(&own);
    assert_eq!(shown[1].1, "Written by the agent.");
    assert_eq!(shown[2].1, "Only the main agent gets it.");
    assert!(!own.every_project());
}

/// An update of a skill an agent saved reads as one, in its row, its
/// question and its details, which say what changes before the reason, with
/// SKILL.md's changed lines in a box of their own.
#[test]
fn an_update_reads_as_what_it_changes() {
    let mut update = skill();
    update.update = Some(SkillUpdate {
        saved: "26 Oct".into(),
        diff: "  ---\n- Tag it.\n+ Tag it and push.".into(),
        added: vec!["checklist.md".into()],
        removed: vec!["old.md".into()],
        changed: vec!["scripts/release.sh".into()],
    });
    let approval = Approval::Skill(update);
    assert_eq!(
        (approval.kind_word(), approval.summary()),
        ("Update", "changes SKILL.md and 3 more files".to_string())
    );
    assert_eq!(
        approval.title(),
        "Update the skill release-notes for every project?"
    );
    let shown = lines(&approval);
    assert_eq!(
        shown[4..8],
        [
            (Look::Heading, "Changes".to_string()),
            (
                Look::Text,
                "It changes SKILL.md and scripts/release.sh, adds checklist.md and removes old.md. An agent saved the skill on 26 Oct.".to_string()
            ),
            (Look::Box, "  ---\n- Tag it.\n+ Tag it and push.".to_string()),
            (Look::Heading, "Why".to_string()),
        ]
    );
    // Files only, and SKILL.md the same.
    let files = SkillUpdate {
        saved: "26 Oct".into(),
        added: vec!["a.md".into()],
        ..SkillUpdate::default()
    };
    assert_eq!(files.short(), "1 file");
    assert_eq!(files.sentence(), "It adds a.md.");
}

/// A server's details give its command, the names of its environment and
/// whether each is set (never a value), who can use it, where it is kept,
/// the warning in the warning style, and the agent's reasons.
#[test]
fn a_server_reads_as_its_card() {
    let approval = Approval::McpServer(server());
    assert_eq!(
        (approval.kind_word(), approval.scope(), approval.summary()),
        (
            "Server",
            "this project",
            "npx -y @modelcontextprotocol/server-github".to_string()
        )
    );
    assert_eq!(
        approval.title(),
        "Add the MCP server github for this project?"
    );
    assert_eq!(
        lines(&approval),
        [
            (
                Look::Text,
                "Runs `npx -y @modelcontextprotocol/server-github`.".to_string()
            ),
            (
                Look::Text,
                "Gets GITHUB_TOKEN and SENTRY_DSN from your environment. SENTRY_DSN isn't set."
                    .to_string()
            ),
            (Look::Text, "Only the main agent can use it.".to_string()),
            (
                Look::Text,
                "Kept in this project's .arko/mcp.json, which git ignores.".to_string()
            ),
            (
                Look::Warning,
                "It runs with your permissions, outside the command guard.".to_string()
            ),
            (Look::Heading, "Why".to_string()),
            (
                Look::Text,
                "Read and file this repository's issues.".to_string()
            ),
        ]
    );
    let mut shared = server();
    shared.scope = Scope::User;
    shared.workers = true;
    shared.env.clear();
    shared.shared = Some("It keeps no state between calls.".into());
    shared.replaces = Some("26 Oct".into());
    let text: Vec<String> = lines(&Approval::McpServer(shared))
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    for line in [
        "Workers can use it too.",
        "Kept in Arko's mcp.json.",
        "Sessions open at the same time share one copy.",
        "It replaces the server github an agent added on 26 Oct.",
        "Why shared",
        "It keeps no state between calls.",
    ] {
        assert!(text.iter().any(|shown| shown == line), "{line}: {text:?}");
    }
}

/// A profile's details give its model, what it is for, what it narrows
/// for its workers, and its instructions in a box.
#[test]
fn a_profile_reads_as_its_card() {
    let approval = Approval::Profile(ProfileApproval {
        name: "judge".into(),
        model: "sol".into(),
        effort: Some("high".into()),
        scope: "every project".into(),
        description: "Judges renders".into(),
        limits: vec!["Workers with it get only the skill ponytail.".into()],
        folder: "Arko's profiles folder".into(),
        instructions: "Compare it with the reference.".into(),
        ..ProfileApproval::default()
    });
    assert_eq!(approval.summary(), "sol high");
    assert_eq!(
        lines(&approval),
        [
            (Look::Text, "Runs on sol at high effort.".to_string()),
            (Look::Text, "Judges renders".to_string()),
            (
                Look::Text,
                "Workers with it get only the skill ponytail.".to_string()
            ),
            (Look::Text, "Saved in Arko's profiles folder.".to_string()),
            (Look::Heading, "Instructions".to_string()),
            (Look::Box, "Compare it with the reference.".to_string()),
        ]
    );
}

/// Its model line says fast mode and the prompt cache when the profile sets
/// them, and a line says its workers can start workers.
#[test]
fn a_profile_says_its_mode_cache_and_nesting() {
    let approval = Approval::Profile(ProfileApproval {
        name: "judge".into(),
        model: "sol".into(),
        fast: true,
        cache: Some("5m".into()),
        nested: true,
        scope: "this project".into(),
        description: "Judges renders".into(),
        ..ProfileApproval::default()
    });
    assert_eq!(
        lines(&approval)[..3],
        [
            (
                Look::Text,
                "Runs on sol in fast mode with a 5m prompt cache.".to_string()
            ),
            (Look::Text, "Judges renders".to_string()),
            (
                Look::Text,
                "A worker started with it can start workers of its own.".to_string()
            ),
        ]
    );
}

/// The tray's words: its heading, and the approve button by how many are
/// selected.
#[test]
fn the_buttons_count_the_selection() {
    assert_eq!(approvals_heading(1), "1 approval");
    assert_eq!(approvals_heading(3), "3 approvals");
    assert_eq!(approve_label(1, 1), "Approve");
    assert_eq!(approve_label(0, 1), "Reject");
    assert_eq!(approve_label(3, 3), "Approve all");
    assert_eq!(approve_label(2, 3), "Approve 2, reject 1");
    assert_eq!(approve_label(0, 3), "Reject all");
    assert_eq!(reject_label(1), "Reject");
    assert_eq!(reject_label(2), "Reject all");
    assert_eq!(
        approval_words_refusal(&"y".repeat(MAX_APPROVAL_WORDS)),
        None
    );
    assert_eq!(
        approval_words_refusal(&"y".repeat(MAX_APPROVAL_WORDS + 1)).as_deref(),
        Some("Your words are too long to send. Shorten them to 16 KB.")
    );
}

/// What the page sends reads back as the server wrote it, each kind with
/// its tag, and a request the page reads refuses fields it doesn't know.
#[test]
fn requests_and_answers_cross_the_wire() {
    let kinds = [
        Approval::Profile(ProfileApproval {
            name: "judge".into(),
            model: "sol".into(),
            effort: Some("high".into()),
            scope: "every project".into(),
            description: "Judges".into(),
            limits: vec!["Workers with it get only the skill ponytail.".into()],
            folder: "Arko's profiles folder".into(),
            instructions: String::new(),
            fast: true,
            cache: Some("5m".into()),
            nested: true,
        }),
        Approval::Skill(skill()),
        Approval::McpServer(server()),
        Approval::WorkerBilling(BillingApproval {
            model: "Opus 5.5".into(),
            input_price: Some(5.0),
            output_price: Some(25.0),
            spent: Some(0.5),
            budget: None,
            task: "Review".into(),
        }),
    ];
    for (approval, kind) in
        kinds
            .into_iter()
            .zip(["profile", "skill", "mcp_server", "worker_billing"])
    {
        let request = ApprovalRequest {
            id: "a1".into(),
            batch: 1,
            approval,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["approval"]["kind"], kind);
        let back: ApprovalRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
    }
    let mut extra = serde_json::to_value(ApprovalRequest {
        id: "a1".into(),
        batch: 1,
        approval: Approval::McpServer(server()),
    })
    .unwrap();
    extra["approval"]["values"] = "secret".into();
    assert!(serde_json::from_value::<ApprovalRequest>(extra).is_err());
    let answers: ApprovalAnswers = serde_json::from_str(
        r#"{"choices": [{"id": "a1", "approve": true, "option": true}, {"id": "a2", "approve": false}], "words": "Fine."}"#,
    )
    .unwrap();
    assert_eq!(
        answers,
        ApprovalAnswers {
            choices: vec![
                ApprovalChoice {
                    id: "a1".into(),
                    approve: true,
                    option: true
                },
                ApprovalChoice {
                    id: "a2".into(),
                    approve: false,
                    option: false
                },
            ],
            words: Some("Fine.".into()),
        }
    );
}

/// No one to answer (`-p`, no terminal): nothing waits, and the answer is
/// that none came.
#[tokio::test]
async fn no_one_to_answer_asks_nothing() {
    let feed = Feed::new(crate::agents::MAIN);
    let alone = Approvals::new(Some(feed.clone()), false, None);
    assert!(!alone.can_answer());
    assert!(!Approvals::none().can_answer());
    let answer = alone
        .request_for(Uuid::new_v4(), None, Approval::McpServer(server()))
        .await;
    assert_eq!(answer, Answer::Unanswered);
    assert!(feed.approvals().is_empty());
}

/// The web page, while it is served, is someone to answer, also without a
/// terminal (`-p --remote`, or input piped in). Every copy of the tray
/// follows it.
#[test]
fn the_web_page_can_answer_while_it_is_served() {
    let feed = Feed::new(crate::agents::MAIN);
    let alone = Approvals::new(Some(feed), false, None);
    let copy = alone.clone();
    alone.set_page_served(true);
    assert!(alone.can_answer() && copy.can_answer());
    alone.set_page_served(false);
    assert!(!copy.can_answer());
    // Without a feed there is no tray to put a request in.
    let trayless = Approvals::none();
    trayless.set_page_served(true);
    assert!(!trayless.can_answer());
}

/// A request waits until its answer comes, and leaves the tray when its
/// asker stops waiting.
#[tokio::test]
async fn a_request_waits_for_its_answer() {
    let feed = Feed::new(crate::agents::MAIN);
    let approvals = Approvals::new(Some(feed.clone()), true, None);
    let waiting = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .request_for(Uuid::new_v4(), None, Approval::Skill(skill()))
                .await
        }
    });
    let request = loop {
        if let Some(request) = feed.approvals().into_iter().next() {
            break request;
        }
        tokio::task::yield_now().await;
    };
    feed.answer_approvals(&ApprovalAnswers::of(
        std::slice::from_ref(&request),
        |_| false,
        |_| false,
        "Not this one.",
    ));
    assert_eq!(
        waiting.await.unwrap(),
        Answer::Rejected {
            words: Some("Not this one.".into())
        }
    );
    let gone = approvals
        .ask(Uuid::new_v4(), None, Approval::Skill(skill()))
        .unwrap();
    assert_eq!(feed.approvals().len(), 1);
    drop(gone);
    assert!(feed.approvals().is_empty());
}

/// `/approve` and `/reject` at the line prompt: numbers pick the requests
/// to approve (`+` turns the option on), and the words after go with the
/// answer.
#[test]
fn the_commands_read_numbers_then_words() {
    let request = |id: &str, approval: Approval| ApprovalRequest {
        id: id.into(),
        batch: 1,
        approval,
    };
    let billing = Approval::WorkerBilling(BillingApproval {
        model: "Opus 5.5".into(),
        ..BillingApproval::default()
    });
    let requests = [
        request("a1", Approval::Skill(skill())),
        request("a2", billing),
        request("a3", Approval::McpServer(server())),
    ];
    let picked = |answers: &ApprovalAnswers| {
        answers
            .choices
            .iter()
            .map(|choice| (choice.approve, choice.option))
            .collect::<Vec<_>>()
    };
    let all = command_answers(&requests, true, "").unwrap();
    assert_eq!(picked(&all), [(true, false); 3]);
    assert_eq!(all.words, None);
    let some = command_answers(&requests, true, "1 2+ 3rd one later").unwrap();
    assert_eq!(picked(&some), [(true, false), (true, true), (false, false)]);
    assert_eq!(some.words.as_deref(), Some("3rd one later"));
    // A + on a request without an option does nothing.
    let plus = command_answers(&requests, true, "1+").unwrap();
    assert_eq!(picked(&plus)[0], (true, false));
    let none = command_answers(&requests, false, "2 not now").unwrap();
    assert_eq!(picked(&none), [(false, false); 3]);
    assert_eq!(none.words.as_deref(), Some("2 not now"));
    assert_eq!(
        command_answers(&requests, true, "4").unwrap_err(),
        "There's no approval 4. The tray holds 3."
    );
    assert_eq!(
        command_answers(&[], false, "").unwrap_err(),
        "No approvals wait."
    );
}
