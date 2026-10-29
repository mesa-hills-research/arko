use std::sync::Mutex;

use super::*;
use crate::approvals::Look;
use crate::mcp::tests::{python, stub_server};

/// What happens while the user decides.
type During = Box<dyn FnOnce() + Send>;

/// Answers each card with `answer` and keeps the cards.
struct Fake {
    can: bool,
    answer: Mutex<Answer>,
    cards: Mutex<Vec<ServerApproval>>,
    during: Mutex<Option<During>>,
}

impl Fake {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            can: true,
            answer: Mutex::new(answer),
            cards: Mutex::default(),
            during: Mutex::default(),
        })
    }

    fn cards(&self) -> Vec<ServerApproval> {
        self.cards.lock().unwrap().clone()
    }

    /// Does `change` while the user decides about the next card.
    fn meanwhile(&self, change: impl FnOnce() + Send + 'static) {
        *self.during.lock().unwrap() = Some(Box::new(change));
    }
}

#[async_trait]
impl Ask for Fake {
    fn can_answer(&self) -> bool {
        self.can
    }

    async fn ask(&self, _: &TaskContext<'_>, card: ServerApproval) -> Answer {
        self.cards.lock().unwrap().push(card);
        let during = self.during.lock().unwrap().take();
        if let Some(change) = during {
            change();
        }
        self.answer.lock().unwrap().clone()
    }
}

fn approve() -> Answer {
    Answer::Approved {
        words: None,
        option: false,
    }
}

/// A session's servers and the main agent's changes to them, in `temp`.
struct Setup {
    root: PathBuf,
    workspace: PathBuf,
    changes: Changes,
    feed: Feed,
    script: PathBuf,
}

fn setup(temp: &Path, ask: Arc<dyn Ask>, servers: Vec<Server>) -> Setup {
    let root = temp.join("arko");
    let workspace = temp.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = stub_server(temp, "stub").args[0].clone().into();
    let feed = Feed::new(crate::agents::MAIN);
    let changes = Changes {
        root: root.clone(),
        workspace: workspace.clone(),
        given: Vec::new(),
        servers: Servers::of(servers, Some(&root)),
        ask,
        notices: Some(Notices::new(
            Arc::new(Output::new(None, true)),
            Some(feed.clone()),
        )),
        session: "2026-10-29-abcd".into(),
        title: Arc::new(|| Some("Release prep".into())),
    };
    Setup {
        root,
        workspace,
        changes,
        feed,
        script,
    }
}

fn context(temp: &Path) -> TaskContext<'_> {
    TaskContext::new(uuid::Uuid::new_v4(), "main", "test", temp)
}

/// A request for the stub as `name`, with `env`.
fn stub_request(setup: &Setup, name: &str, env: &[&str]) -> AddServer {
    let mut request = AddServer::new(name, python());
    request.args = vec![setup.script.display().to_string()];
    request.env = env.iter().map(|name| name.to_string()).collect();
    request.why = "Echoes text for the tests.".into();
    request
}

fn notices(feed: &Feed) -> Vec<String> {
    feed.subscribe()
        .0
        .items
        .iter()
        .map(|item| item.text.clone())
        .collect()
}

/// The card's details as the tray shows them, a line each.
fn details(card: &ServerApproval) -> Vec<String> {
    Approval::McpServer(card.clone())
        .details()
        .into_iter()
        .filter(|detail| detail.look != Look::Box)
        .map(|detail| detail.text)
        .collect()
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// An approved add writes the entry in the project's `.arko/mcp.json`,
/// which `arko_dir` makes with its `.gitignore`, with each variable as
/// `${NAME}`, and nothing under `.agents`. The card names the variables and
/// says whether each is set, never a value. The file is remembered as
/// approved, the record has the server, and the hub starts it. A second add
/// keeps the first entry, and adding the same name again replaces it.
#[tokio::test]
async fn an_add_writes_through_the_arko_folder_and_keeps_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(Answer::Approved {
        words: None,
        option: false,
    });
    let setup = setup(temp.path(), ask.clone(), Vec::new());
    let request = stub_request(&setup, "stubby", &["PATH"]);
    let said = setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap();
    assert_eq!(
        said,
        "Added stubby for this project. It started with 2 tools. List them with action tools."
    );
    let local = local_file(&setup.workspace);
    assert_eq!(
        std::fs::read_to_string(crate::arko_dir::dir(&setup.workspace).join(".gitignore")).unwrap(),
        crate::arko_dir::IGNORE_TEXT
    );
    assert!(!setup.workspace.join(".agents").exists());
    let written = json(&local);
    assert_eq!(
        written["mcpServers"]["stubby"],
        serde_json::json!({
            "command": python(),
            "args": [setup.script.display().to_string()],
            "env": {"PATH": "${PATH}"},
        })
    );
    // The card: its words, and no value of the environment.
    let cards = ask.cards();
    assert_eq!(cards.len(), 1);
    let card = &cards[0];
    assert_eq!(
        Approval::McpServer(card.clone()).title(),
        "Add the MCP server stubby for this project?"
    );
    assert_eq!(
        card.env,
        [EnvName {
            name: "PATH".into(),
            set: true
        }]
    );
    assert!(card.ignored && card.workers && card.shared.is_none());
    let path = std::env::var("PATH").unwrap();
    assert!(!format!("{card:?}").contains(&path));
    assert_eq!(
        details(card),
        [
            format!("Runs `{} {}`.", python(), setup.script.display()),
            "Gets PATH from your environment.".to_string(),
            "Workers can use it too.".to_string(),
            "Kept in this project's .arko/mcp.json, which git ignores.".to_string(),
            "It runs with your permissions, outside the command guard.".to_string(),
            "Why".to_string(),
            "Echoes text for the tests.".to_string(),
        ]
    );
    // Remembered as approved, so a session starts it without asking.
    let plan = crate::mcp::plan(&setup.root, &setup.workspace, &[], true).unwrap();
    assert!(plan.approvals.is_empty());
    assert_eq!(plan.decide(&[]).unwrap().0[0].name, "stubby");
    // Recorded as an agent's, and running.
    let record = agent_added::server_in(&setup.root, &normal(&local), "stubby").unwrap();
    assert_eq!(record.why, "Echoes text for the tests.");
    assert_eq!(record.session, "2026-10-29-abcd");
    assert_eq!(record.session_title.as_deref(), Some("Release prep"));
    assert_eq!(
        setup.changes.servers.state("stubby"),
        Some(crate::mcp::McpState::Running)
    );
    assert!(
        notices(&setup.feed)
            .contains(&"The main agent added the MCP server stubby for this project.".to_string())
    );
    // Another server keeps the first entry. An unset variable is said on
    // the card, and the server doesn't start without it.
    let mut other = stub_request(&setup, "other", &["ARKO_SURELY_UNSET_VARIABLE"]);
    other.workers = false;
    let said = setup
        .changes
        .add(&context(temp.path()), other)
        .await
        .unwrap();
    assert!(
        said.starts_with("Added other for this project. It didn't start, because it uses ${ARKO_SURELY_UNSET_VARIABLE}, which isn't set. The next session starts it once that's fixed."),
        "{said}"
    );
    let card = &ask.cards()[1];
    assert_eq!(
        details(card)[1],
        "Gets ARKO_SURELY_UNSET_VARIABLE from your environment. ARKO_SURELY_UNSET_VARIABLE isn't set."
    );
    assert_eq!(details(card)[2], "Only the main agent can use it.");
    let written = json(&local);
    assert!(written["mcpServers"]["stubby"].is_object());
    assert_eq!(written["mcpServers"]["other"]["workers"], false);
    // The same name again replaces the agent's entry.
    let mut again = stub_request(&setup, "stubby", &[]);
    again.shared = true;
    again.why = "It keeps no state between calls.".into();
    setup
        .changes
        .add(&context(temp.path()), again)
        .await
        .unwrap();
    let card = &ask.cards()[2];
    assert!(card.replaces.is_some());
    assert_eq!(card.why, "It keeps no state between calls.");
    let shown = details(card);
    assert!(shown.contains(&"Sessions open at the same time share one copy.".to_string()));
    // The agent's check is its reason, shown once.
    assert_eq!(
        shown
            .iter()
            .filter(|line| *line == "It keeps no state between calls.")
            .count(),
        1
    );
    assert!(!shown.contains(&"Why shared".to_string()));
    assert_eq!(json(&local)["mcpServers"]["stubby"]["shared"], true);
    setup.changes.servers.shutdown().await;
}

/// With Ask before adding MCP servers off, an add asks nothing, also with
/// no one to ask, and is still remembered as approved.
#[tokio::test]
async fn the_setting_off_skips_the_card_and_records_the_approval() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Arc::new(Fake {
        can: false,
        answer: Mutex::new(Answer::Unanswered),
        cards: Mutex::default(),
        during: Mutex::default(),
    });
    let setup = setup(temp.path(), ask.clone(), Vec::new());
    crate::extension_prefs::update(&setup.root, |prefs| prefs.ask_before_adding_mcp = false)
        .unwrap();
    let mut request = AddServer::new("gone", "arko-no-such-mcp-server");
    request.why = "A server that isn't there.".into();
    let said = setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap();
    assert!(
        said.starts_with("Added gone for this project. It didn't start"),
        "{said}"
    );
    assert!(ask.cards().is_empty());
    assert!(
        crate::mcp::plan(&setup.root, &setup.workspace, &[], true)
            .unwrap()
            .approvals
            .is_empty()
    );
    assert!(notices(&setup.feed).iter().any(|line| {
        line.starts_with("The main agent added the MCP server gone, but it didn't start (")
    }));
}

/// What an add can't do is said before anyone is asked: a name with other
/// characters or too long, a URL, a variable's value, no reason, and a name
/// another file sets up (or the same file, by the user). With no one to
/// ask, an add is refused.
#[tokio::test]
async fn an_add_is_checked_before_anyone_is_asked() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(Answer::Approved {
        words: None,
        option: false,
    });
    let setup = setup(temp.path(), ask.clone(), Vec::new());
    std::fs::write(
        setup.workspace.join(crate::mcp::PROJECT_FILE),
        r#"{"mcpServers": {"github": {"command": "npx"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(setup.root.clone()).unwrap();
    std::fs::write(
        setup.root.join(crate::mcp::USER_FILE),
        r#"{"mcpServers": {"mine": {"command": "mine"}}}"#,
    )
    .unwrap();
    let refused = async |change: &dyn Fn(&mut AddServer)| {
        let mut request = AddServer::new("fine", "npx");
        request.why = "A reason.".into();
        change(&mut request);
        setup
            .changes
            .add(&context(temp.path()), request)
            .await
            .unwrap_err()
    };
    assert!(
        refused(&|request| request.name = "my server".into())
            .await
            .contains("letters, digits, - and _")
    );
    assert!(
        refused(&|request| request.name = "x".repeat(41))
            .await
            .contains("at most 40")
    );
    assert!(
        refused(&|request| request.command = "https://example.com/mcp".into())
            .await
            .contains("standard input and output only")
    );
    let value = refused(&|request| request.env = vec!["TOKEN=secret".into()]).await;
    assert!(
        value.contains("names only") && !value.contains("secret"),
        "{value}"
    );
    assert!(
        refused(&|request| request.env = vec!["token".into()])
            .await
            .contains("isn't an environment variable's name")
    );
    assert!(
        refused(&|request| request.why = " ".into())
            .await
            .contains("Say in why")
    );
    assert!(
        refused(&|request| request.name = "github".into())
            .await
            .contains(".mcp.json sets up a server named github already")
    );
    let mine = refused(&|request| {
        request.name = "mine".into();
        request.scope = McpScope::User;
    })
    .await;
    assert!(
        mine.contains("sets up a server named mine already"),
        "{mine}"
    );
    assert!(ask.cards().is_empty());
    assert!(!crate::arko_dir::dir(&setup.workspace).exists());
    // No one to ask.
    let nobody = setup_nobody(temp.path());
    let mut request = AddServer::new("fine", "npx");
    request.why = "A reason.".into();
    assert_eq!(
        nobody
            .changes
            .add(&context(temp.path()), request)
            .await
            .unwrap_err(),
        "No one can approve an MCP server in this session."
    );
}

fn setup_nobody(temp: &Path) -> Setup {
    let ask = Arc::new(Fake {
        can: false,
        answer: Mutex::new(Answer::Unanswered),
        cards: Mutex::default(),
        during: Mutex::default(),
    });
    setup(&temp.join("nobody"), ask, Vec::new())
}

/// A card the user turns down writes nothing. Words with it go to the
/// agent, and the transcript says it wasn't added.
#[tokio::test]
async fn a_card_turned_down_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(Answer::Rejected { words: None });
    let setup = setup(temp.path(), ask.clone(), Vec::new());
    let mut request = AddServer::new("fine", "npx");
    request.why = "A reason.".into();
    let said = setup
        .changes
        .add(&context(temp.path()), request.clone())
        .await
        .unwrap();
    assert!(
        said.starts_with("The user chose not to add fine for this project"),
        "{said}"
    );
    *ask.answer.lock().unwrap() = Answer::Rejected {
        words: Some("Use uvx instead.".into()),
    };
    let said = setup
        .changes
        .add(&context(temp.path()), request.clone())
        .await
        .unwrap();
    assert!(said.contains("wrote “Use uvx instead.”"), "{said}");
    assert!(notices(&setup.feed).contains(&"Not added.".to_string()));
    *ask.answer.lock().unwrap() = Answer::Unanswered;
    let said = setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap();
    assert_eq!(said, "The user didn't answer, so fine wasn't added.");
    assert!(!local_file(&setup.workspace).exists());
    assert!(agent_added::servers(&setup.root).is_empty());
}

/// An add to a project's `.arko/mcp.json` whose servers the user turned
/// down doesn't approve them: the next session asks about the file.
#[tokio::test]
async fn an_add_to_a_file_the_user_turned_down_leaves_it_to_ask() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(Answer::Approved {
        words: None,
        option: false,
    });
    let setup = setup(temp.path(), ask, Vec::new());
    std::fs::create_dir_all(crate::arko_dir::dir(&setup.workspace)).unwrap();
    let local = local_file(&setup.workspace);
    std::fs::write(
        &local,
        r#"{"mcpServers": {"committed": {"command": "someone-elses-server"}}}"#,
    )
    .unwrap();
    crate::mcp::plan(&setup.root, &setup.workspace, &[], true)
        .unwrap()
        .decide(&[Some(false)])
        .unwrap();
    let mut request = AddServer::new("mine", "arko-no-such-mcp-server");
    request.why = "A reason.".into();
    let said = setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap();
    assert!(said.contains("the next session asks them"), "{said}");
    let plan = crate::mcp::plan(&setup.root, &setup.workspace, &[], true).unwrap();
    assert_eq!(plan.approvals.len(), 1);
    assert_eq!(plan.approvals[0].servers.len(), 2);
}

/// Remove takes out a server an agent added, from its file, the record and
/// the hub, and says so. A server the user set up stays.
#[tokio::test]
async fn remove_takes_out_only_what_an_agent_added() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(Answer::Approved {
        words: None,
        option: false,
    });
    let users = temp.path().join("users.json");
    std::fs::write(
        &users,
        r#"{"mcpServers": {"users": {"command": "arko-no-such-mcp-server"}}}"#,
    )
    .unwrap();
    let parsed = crate::mcp::parse_file(&users, &crate::mcp::environment).unwrap();
    let setup = setup(temp.path(), ask, parsed.servers);
    let mut request = AddServer::new("added", "arko-no-such-mcp-server");
    request.scope = McpScope::User;
    request.why = "A reason.".into();
    setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap();
    let user_file = setup.root.join(crate::mcp::USER_FILE);
    assert!(crate::mcp::sets_up(&user_file, "added"));
    let refused = setup
        .changes
        .remove(&context(temp.path()), "users", "")
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("users wasn't added by an agent, so it stays."),
        "{refused}"
    );
    assert!(setup.changes.servers.server("users").is_some());
    let removed = setup
        .changes
        .remove(&context(temp.path()), "added", "Not needed any more.")
        .await
        .unwrap();
    assert_eq!(removed, "Removed added and stopped it.");
    assert!(!crate::mcp::sets_up(&user_file, "added"));
    assert!(agent_added::servers(&setup.root).is_empty());
    assert!(setup.changes.servers.server("added").is_none());
    assert!(setup.changes.servers.hub.config("added").is_none());
    assert!(
        notices(&setup.feed).contains(&"The main agent removed the MCP server added.".to_string())
    );
}

/// What an add found of its server's name before the user was asked is
/// checked again as it writes. A user's entry of the same name in its file,
/// the name set up in a file read before it, and an edit of the agent's own
/// entry, each made while the user decides, are kept as they are, nothing
/// is written or recorded, and the agent is told to call add again.
#[tokio::test]
async fn what_changes_while_the_user_decides_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(approve());
    let setup = setup(temp.path(), ask.clone(), Vec::new());
    let local = local_file(&setup.workspace);
    // The user sets up a server of the same name in .arko/mcp.json.
    let users = r#"{"mcpServers": {"stub": {"command": "users-own"}}}"#;
    {
        let local = local.clone();
        ask.meanwhile(move || {
            std::fs::create_dir_all(local.parent().unwrap()).unwrap();
            std::fs::write(&local, users).unwrap();
        });
    }
    let refused = setup
        .changes
        .add(&context(temp.path()), stub_request(&setup, "stub", &[]))
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        format!(
            "A server named stub was set up in {} while the user decided, so Arko wrote nothing. Choose another name, and call add again.",
            normal(&local).display()
        )
    );
    assert_eq!(std::fs::read_to_string(&local).unwrap(), users);
    // The project's .mcp.json, read before Arko's mcp.json, gains the name of
    // a server for every project.
    {
        let project = setup.workspace.join(crate::mcp::PROJECT_FILE);
        ask.meanwhile(move || {
            std::fs::write(project, r#"{"mcpServers": {"other": {"command": "npx"}}}"#).unwrap();
        });
    }
    let mut request = stub_request(&setup, "other", &[]);
    request.scope = McpScope::User;
    let refused = setup
        .changes
        .add(&context(temp.path()), request)
        .await
        .unwrap_err();
    assert!(
        refused.contains("/.mcp.json while the user decided, so Arko wrote nothing."),
        "{refused}"
    );
    assert!(!setup.root.join(crate::mcp::USER_FILE).exists());
    // The user edits the agent's own server while it asks to replace it.
    setup
        .changes
        .add(&context(temp.path()), stub_request(&setup, "mine", &[]))
        .await
        .unwrap();
    let first = agent_added::server_in(&setup.root, &normal(&local), "mine").unwrap();
    {
        let local = local.clone();
        ask.meanwhile(move || {
            let mut written = json(&local);
            written["mcpServers"]["mine"]["command"] = "edited".into();
            std::fs::write(&local, written.to_string()).unwrap();
        });
    }
    let mut again = stub_request(&setup, "mine", &[]);
    again.why = "Another reason.".into();
    let refused = setup
        .changes
        .add(&context(temp.path()), again)
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("mine changed in ") && refused.contains("Call add again"),
        "{refused}"
    );
    assert_eq!(json(&local)["mcpServers"]["mine"]["command"], "edited");
    assert_eq!(agent_added::servers(&setup.root), [first]);
    assert!(setup.changes.servers.hub.config("stub").is_none());
    assert!(setup.changes.servers.hub.config("other").is_none());
    setup.changes.servers.shutdown().await;
}

/// An add whose record of what agents added can't be written writes no
/// entry and remembers no approval, so nothing starts in the next session.
#[tokio::test]
async fn an_add_that_cant_be_recorded_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let ask = Fake::new(approve());
    let setup = setup(temp.path(), ask, Vec::new());
    // A folder where the record's file goes can't be read or written.
    std::fs::create_dir_all(setup.root.join(agent_added::FILE)).unwrap();
    let refused = setup
        .changes
        .add(&context(temp.path()), stub_request(&setup, "stub", &[]))
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("Couldn't read ") && refused.ends_with(" Arko didn't add stub."),
        "{refused}"
    );
    let local = local_file(&setup.workspace);
    assert!(!crate::mcp::sets_up(&local, "stub"));
    assert_eq!(
        crate::mcp::local_approval(&setup.root, &setup.workspace),
        Ok(None)
    );
    let plan = crate::mcp::plan(&setup.root, &setup.workspace, &[], true).unwrap();
    assert!(plan.decide(&[]).unwrap().0.is_empty());
    assert!(setup.changes.servers.hub.config("stub").is_none());
}

/// A step of an add that fails after the entry is written puts back the
/// file and the record, and says so. When the file can't be put back, the
/// entry keeps its record, so an agent's remove takes it out, and the
/// result says what is left.
#[test]
fn a_failed_step_puts_back_what_the_add_changed() {
    let temp = tempfile::tempdir().unwrap();
    let setup = setup(temp.path(), Fake::new(approve()), Vec::new());
    let file = setup.root.join(crate::mcp::USER_FILE);
    let added = |name: &str| {
        crate::mcp::write_entry(&file, name, serde_json::json!({"command": "npx"})).unwrap();
        agent_added::record_server(
            &setup.root,
            ServerRecord {
                file: normal(&file),
                name: name.into(),
                session: "s".into(),
                session_title: None,
                added: chrono::Utc::now(),
                why: "A reason.".into(),
            },
        )
        .unwrap();
    };
    std::fs::create_dir_all(&setup.root).unwrap();
    let undo = |name: &'static str| Undo {
        changes: &setup.changes,
        name,
        file: &file,
        scope: Scope::User,
        replaced: None,
    };
    // A file that wasn't there goes again, and so does the record.
    added("first");
    assert_eq!(
        undo("first").all(
            "couldn't save the approvals (permission denied)",
            Some(None)
        ),
        "Couldn't save the approvals (permission denied). Arko didn't add first, and changed nothing."
    );
    assert!(!file.exists());
    assert!(agent_added::servers(&setup.root).is_empty());
    // A file that can't be put back keeps the entry and its record.
    std::fs::write(&file, r#"{"mcpServers": {}}"#).unwrap();
    let before = std::fs::read(&file).unwrap();
    added("second");
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let said = undo("second").all("Couldn't save the record.", Some(Some(&before)));
    assert!(
        said.starts_with(&format!(
            "Couldn't save the record. Arko didn't add second, but couldn't put {} back as it was (",
            file.display()
        )) && said.ends_with(
            "so the new entry is there. It's recorded as an agent's, so remove takes it out. The next session starts it unless it's removed."
        ),
        "{said}"
    );
}

/// A server an agent added that didn't start, because a variable it uses
/// isn't set, is in `servers`'s list with why, and `remove` takes it out of
/// its file and the record, both through the `mcp` tool. A name no server
/// has is refused.
#[tokio::test]
async fn a_server_that_didnt_start_is_listed_and_removed() {
    let temp = tempfile::tempdir().unwrap();
    let setup = setup(temp.path(), Fake::new(approve()), Vec::new());
    let request = stub_request(&setup, "issues", &["ARKO_SURELY_UNSET_VARIABLE"]);
    let Setup {
        root,
        workspace,
        changes,
        ..
    } = setup;
    let changes = Arc::new(changes);
    changes.add(&context(temp.path()), request).await.unwrap();
    let session = crate::mcp::Session {
        servers: changes.servers.clone(),
        changes: Some(changes.clone()),
        notices: None,
    };
    let bridge = session.lead_tool(Arc::new(arko_agents::mcp::AllServers));
    // The tool's answer to `pairs`, or its error's words.
    let mcp = |pairs: &[(&str, &str)]| {
        let bridge = bridge.clone();
        let temp = temp.path().to_path_buf();
        let params: std::collections::HashMap<String, Value> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), Value::from(*value)))
            .collect();
        async move {
            match bridge.execute(&context(&temp), params).await {
                Ok(text) => text,
                Err(arko_agents::ToolError::ExecutionFailed(text)) => text,
                Err(error) => error.to_string(),
            }
        }
    };
    let servers = || mcp(&[("action", "servers")]);
    assert_eq!(
        servers().await,
        "issues  not started, because it uses ${ARKO_SURELY_UNSET_VARIABLE}, which isn't set. The next session starts it once that's fixed."
    );
    let remove = |name: &'static str| {
        mcp(&[
            ("action", "remove"),
            ("server", name),
            ("why", "Not needed."),
        ])
    };
    assert_eq!(
        remove("nosuch").await,
        "There's no MCP server named nosuch."
    );
    assert_eq!(remove("issues").await, "Removed issues.");
    assert!(!crate::mcp::sets_up(&local_file(&workspace), "issues"));
    assert!(agent_added::servers(&root).is_empty());
    assert_eq!(
        servers().await,
        "No MCP servers are set up. add sets one up once the user approves it."
    );
}
