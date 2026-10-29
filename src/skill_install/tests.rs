use std::io::Write;
use std::sync::Mutex;

use serde_json::json;

use super::*;

/// "Save the skill notes for every project?"
fn title(card: &SkillApproval) -> String {
    Approval::Skill(card.clone()).title()
}

/// Answers as it is told, and keeps the cards it was shown.
struct Approve {
    can: bool,
    answer: Answer,
    cards: Mutex<Vec<SkillApproval>>,
}

#[async_trait]
impl Approver for Approve {
    fn can_answer(&self) -> bool {
        self.can
    }

    async fn request(&self, _: &TaskContext<'_>, card: SkillApproval) -> Answer {
        self.cards.lock().unwrap().push(card);
        self.answer.clone()
    }
}

/// Downloads from a table of URLs, and counts the downloads.
struct Table(HashMap<String, Vec<u8>>, Mutex<usize>);

#[async_trait]
impl Fetch for Table {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        *self.1.lock().unwrap() += 1;
        self.0
            .get(url)
            .cloned()
            .ok_or_else(|| "the server answered 404".to_string())
    }
}

/// A project, Arko's folder and a home directory in a temporary folder,
/// with the tools on them.
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    approver: Arc<Approve>,
    fetch: Arc<Table>,
    notices: Arc<Mutex<Vec<String>>>,
    context: Arc<Context>,
}

fn fixture(can: bool, answer: Answer, downloads: Vec<(&str, Vec<u8>)>) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let base = normal(temp.path());
    let root = base.join("arko");
    let workspace = base.join("work");
    let home = base.join("home");
    for folder in [&root, &workspace, &home] {
        std::fs::create_dir_all(folder).unwrap();
    }
    let approver = Arc::new(Approve {
        can,
        answer,
        cards: Mutex::default(),
    });
    let fetch = Arc::new(Table(
        downloads
            .into_iter()
            .map(|(url, bytes)| (url.to_string(), bytes))
            .collect(),
        Mutex::default(),
    ));
    let notices: Arc<Mutex<Vec<String>>> = Arc::default();
    let context = Arc::new(Context {
        root: root.clone(),
        workspace: workspace.clone(),
        home: Some(home.clone()),
        session: "s1".into(),
        title: Arc::new(|| Some("Release prep".into())),
        model_names: vec!["claude-opus-5-5".into(), "opus".into()],
        approver: approver.clone(),
        fetch: fetch.clone(),
        notice: Arc::new({
            let notices = notices.clone();
            move |_, text: &str| notices.lock().unwrap().push(text.to_string())
        }),
        writing: Default::default(),
    });
    Fixture {
        _temp: temp,
        root,
        workspace,
        home,
        approver,
        fetch,
        notices,
        context,
    }
}

/// Does something while the user decides, then answers.
struct Meanwhile {
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    answer: Answer,
    cards: Mutex<Vec<SkillApproval>>,
}

#[async_trait]
impl Approver for Meanwhile {
    fn can_answer(&self) -> bool {
        true
    }

    async fn request(&self, _: &TaskContext<'_>, card: SkillApproval) -> Answer {
        self.cards.lock().unwrap().push(card);
        if let Some(during) = self.during.lock().unwrap().take() {
            during();
        }
        self.answer.clone()
    }
}

/// Approves, after doing `during`.
fn meanwhile(during: impl FnOnce() + Send + 'static) -> Arc<Meanwhile> {
    Arc::new(Meanwhile {
        during: Mutex::new(Some(Box::new(during))),
        answer: Answer::Approved {
            words: None,
            option: false,
        },
        cards: Mutex::default(),
    })
}

/// Never answers.
struct Never;

#[async_trait]
impl Approver for Never {
    fn can_answer(&self) -> bool {
        true
    }

    async fn request(&self, _: &TaskContext<'_>, _: SkillApproval) -> Answer {
        std::future::pending().await
    }
}

/// No one asks, and nothing is downloaded.
fn quiet() -> Fixture {
    fixture(
        true,
        Answer::Approved {
            words: None,
            option: false,
        },
        Vec::new(),
    )
}

fn skill_md(name: &str, extra: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: How this project writes {name}.\n{extra}---\n\nWrite {name} well.\n"
    )
}

/// A skill folder `name` in `folder`, with `files` beside its SKILL.md.
fn write_skill(folder: &Path, name: &str, extra: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = folder.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md(name, extra)).unwrap();
    for (path, text) in files {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    dir
}

/// A zip archive of `files`.
fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (path, bytes) in files {
        writer.start_file(*path, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

impl Fixture {
    fn call() -> TaskContext<'static> {
        TaskContext::new(uuid::Uuid::new_v4(), "main", "", Path::new("."))
    }

    async fn add(&self, params: Value) -> Result<String, String> {
        Self::add_in(self.context.clone(), params).await
    }

    /// The tools in the project `workspace`, asking `approver`.
    fn context_with(&self, workspace: &Path, approver: Arc<dyn Approver>) -> Arc<Context> {
        Arc::new(Context {
            root: self.root.clone(),
            workspace: workspace.to_path_buf(),
            home: Some(self.home.clone()),
            session: "s1".into(),
            title: Arc::new(|| None),
            model_names: self.context.model_names.clone(),
            approver,
            fetch: self.fetch.clone(),
            notice: self.context.notice.clone(),
            writing: Default::default(),
        })
    }

    /// `add_skill`, asking `approver`.
    async fn add_asking(
        &self,
        approver: Arc<dyn Approver>,
        params: Value,
    ) -> Result<String, String> {
        Self::add_in(self.context_with(&self.workspace, approver), params).await
    }

    async fn add_in(context: Arc<Context>, params: Value) -> Result<String, String> {
        let params: HashMap<String, Value> = serde_json::from_value(params).unwrap();
        AddSkill(context)
            .execute(&Self::call(), params)
            .await
            .map_err(|error| match error {
                ToolError::ExecutionFailed(text) => text,
                other => other.to_string(),
            })
    }

    async fn remove(&self, name: &str) -> Result<String, String> {
        let params: HashMap<String, Value> =
            serde_json::from_value(json!({ "name": name, "why": "It's out of date." })).unwrap();
        RemoveSkill(self.context.clone())
            .execute(&Self::call(), params)
            .await
            .map_err(|error| match error {
                ToolError::ExecutionFailed(text) => text,
                other => other.to_string(),
            })
    }

    fn project(&self) -> PathBuf {
        self.workspace.join(".arko/skills")
    }

    fn user(&self) -> PathBuf {
        self.root.join("skills")
    }

    fn drafts(&self) -> PathBuf {
        self.workspace.join("drafts")
    }

    fn cards(&self) -> Vec<SkillApproval> {
        self.approver.cards.lock().unwrap().clone()
    }

    fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }

    /// The skill folders agents save in hold no skill, and no record says
    /// one was saved.
    fn nothing_saved(&self) {
        for folder in [self.project(), self.user()] {
            let entries: Vec<_> = std::fs::read_dir(&folder)
                .map(|entries| entries.filter_map(Result::ok).collect())
                .unwrap_or_default();
            assert!(entries.is_empty(), "{}: {entries:?}", folder.display());
        }
        assert!(agent_added::skills(&self.root).is_empty());
    }

    fn review_project_skills(&self) {
        crate::extension_prefs::update(&self.root, |prefs| prefs.review_project_skills = true)
            .unwrap();
    }
}

fn params(name: &str, source: &str) -> Value {
    json!({ "name": name, "source": source, "why": "Later conversations need the steps." })
}

fn with(mut params: Value, key: &str, value: Value) -> Value {
    params[key] = value;
    params
}

/// The files under `folder`, by their paths in it.
fn files_in(folder: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for entry in walk(folder) {
        files.push(
            entry
                .strip_prefix(folder)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    files.sort();
    files
}

fn walk(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(folder).unwrap().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// A folder the agent wrote is copied into the project's .arko/skills,
/// which Arko makes with its .gitignore, and nothing goes under .agents.
/// The save is recorded with its reason and source, the transcript says
/// so, and no one is asked. Files lose the executable bit.
#[tokio::test]
async fn a_folder_is_copied_into_the_projects_arko_folder_and_recorded() {
    let fixture = quiet();
    let draft = write_skill(
        &fixture.drafts(),
        "notes",
        "",
        &[("checklist.md", "- Tag it"), ("scripts/run.sh", "echo run")],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = draft.join("scripts/run.sh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = fixture
        .add(params("notes", &draft.display().to_string()))
        .await
        .unwrap();
    assert_eq!(
        result,
        "Saved the skill notes for this project. load_skill loads it now, and your skill list shows it from the next conversation. Workers you start from now on get it."
    );
    let saved = fixture.project().join("notes");
    assert_eq!(
        files_in(&saved),
        ["SKILL.md", "checklist.md", "scripts/run.sh"]
    );
    assert_eq!(
        std::fs::read_to_string(saved.join("SKILL.md")).unwrap(),
        skill_md("notes", "")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(saved.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join(".arko/.gitignore")).unwrap(),
        crate::arko_dir::IGNORE_TEXT
    );
    assert!(!fixture.workspace.join(".agents").exists());
    // The draft stays where it was.
    assert!(draft.join("SKILL.md").is_file());
    let record = agent_added::skill_at(&fixture.root, &saved).unwrap();
    assert_eq!(record.scope, Scope::Project);
    assert_eq!(record.session, "s1");
    assert_eq!(record.session_title.as_deref(), Some("Release prep"));
    assert_eq!(record.why, "Later conversations need the steps.");
    assert_eq!(record.source, Some(draft.display().to_string()));
    assert_eq!(
        fixture.notices(),
        ["The main agent saved the skill notes for this project."]
    );
    assert!(fixture.cards().is_empty());
}

/// A source in any folder skills are read from is refused before anyone is
/// asked, since agents would load it before the user approved it. The
/// agent is told to write it elsewhere, and nothing there is touched or
/// recorded, also for a skill the user has in the project's .arko/skills,
/// which a save can't claim, and for a folder reached through a link.
#[tokio::test]
async fn a_source_in_a_skill_folder_is_refused() {
    let fixture = quiet();
    let folders = [
        fixture.workspace.join(".agents/skills"),
        fixture.workspace.join(".claude/skills"),
        fixture.workspace.join(".skills"),
        fixture.project(),
        fixture.user(),
        fixture.home.join(".agents/skills"),
        fixture.home.join(".claude/skills"),
    ];
    for review in [false, true] {
        if review {
            fixture.review_project_skills();
        }
        for folder in &folders {
            let draft = write_skill(folder, "notes", "", &[("a.md", "A")]);
            std::fs::write(folder.join("notes.md"), skill_md("notes", "")).unwrap();
            for source in [draft.clone(), folder.join("notes.md")] {
                for scope in ["project", "user"] {
                    let error = fixture
                        .add(with(
                            params("notes", &source.display().to_string()),
                            "scope",
                            json!(scope),
                        ))
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error,
                        format!(
                            "{} is in the skill folder {}. Write the skill somewhere else, such as a scratch folder in the project, and add it from there.",
                            source.display(),
                            folder.display()
                        )
                    );
                }
            }
            assert_eq!(files_in(&draft), ["SKILL.md", "a.md"]);
            assert_eq!(
                std::fs::read_to_string(draft.join("SKILL.md")).unwrap(),
                skill_md("notes", "")
            );
            std::fs::remove_dir_all(&draft).unwrap();
            std::fs::remove_file(folder.join("notes.md")).unwrap();
        }
    }
    // The user's own skill in .arko/skills, given by a relative path.
    let theirs = write_skill(&fixture.project(), "checklist", "", &[]);
    let error = fixture
        .add(params("checklist", ".arko/skills/checklist"))
        .await
        .unwrap_err();
    assert!(error.contains(" is in the skill folder "), "{error}");
    assert!(theirs.join("SKILL.md").is_file());
    std::fs::remove_dir_all(&theirs).unwrap();
    // A folder that a link leads into a skill folder.
    #[cfg(unix)]
    {
        write_skill(&fixture.project(), "linked", "", &[]);
        std::os::unix::fs::symlink(fixture.project(), fixture.workspace.join("scratch")).unwrap();
        let error = fixture
            .add(params("linked", "scratch/linked"))
            .await
            .unwrap_err();
        assert!(error.contains(" is in the skill folder "), "{error}");
        assert!(fixture.project().join("linked/SKILL.md").is_file());
        std::fs::remove_dir_all(fixture.project().join("linked")).unwrap();
    }
    fixture.nothing_saved();
    assert!(fixture.cards().is_empty());
    assert!(fixture.notices().is_empty());
}

/// A Markdown file becomes the skill's SKILL.md.
#[tokio::test]
async fn a_markdown_file_becomes_its_skill_md() {
    let fixture = quiet();
    std::fs::create_dir_all(fixture.drafts()).unwrap();
    let file = fixture.drafts().join("notes.md");
    std::fs::write(&file, skill_md("notes", "models: [haiku]\n")).unwrap();
    let result = fixture
        .add(params("notes", "drafts/notes.md"))
        .await
        .unwrap();
    // Its models leave out the main agent's.
    assert!(
        result.contains(" Your skill list won't show it, since its models don't include yours, but load_skill loads it by name."),
        "{result}"
    );
    let saved = fixture.project().join("notes");
    assert_eq!(files_in(&saved), ["SKILL.md"]);
    assert_eq!(
        std::fs::read_to_string(saved.join("SKILL.md")).unwrap(),
        skill_md("notes", "models: [haiku]\n")
    );
}

/// A .zip or .skill bundle is unpacked, its SKILL.md at the root or in one
/// top folder, with its other files.
#[tokio::test]
async fn a_bundle_is_unpacked_with_its_files() {
    for (file, top) in [("notes.zip", "notes/"), ("notes.skill", "")] {
        let fixture = quiet();
        std::fs::create_dir_all(fixture.drafts()).unwrap();
        let skill = skill_md("notes", "");
        let names = [
            format!("{top}SKILL.md"),
            format!("{top}scripts/run.sh"),
            format!("{top}more/forms.md"),
        ];
        let bundle = zip_of(&[
            (names[0].as_str(), skill.as_bytes()),
            (names[1].as_str(), &b"echo run"[..]),
            (names[2].as_str(), &b"Forms."[..]),
        ]);
        std::fs::write(fixture.drafts().join(file), bundle).unwrap();
        fixture
            .add(params("notes", &format!("drafts/{file}")))
            .await
            .unwrap();
        let saved = fixture.project().join("notes");
        assert_eq!(
            files_in(&saved),
            ["SKILL.md", "more/forms.md", "scripts/run.sh"],
            "{file}"
        );
        assert_eq!(
            files_in(&fixture.project()).len(),
            3,
            "nothing but the skill: {file}"
        );
    }
}

/// A URL is read by its first bytes: a zip archive is a bundle whatever its
/// name, and anything else is Markdown. A download asks the user, in either
/// scope, with the URL and its size on the card.
#[tokio::test]
async fn a_url_is_read_by_its_first_bytes_and_asks() {
    let skill = skill_md("notes", "");
    let bundle = zip_of(&[
        ("SKILL.md", skill.as_bytes()),
        ("scripts/go.py", &b"print()"[..]),
    ]);
    let fixture = fixture(
        true,
        Answer::Approved {
            words: None,
            option: false,
        },
        vec![
            ("https://example.com/notes.md", bundle.clone()),
            ("https://example.com/notes.zip", skill.clone().into_bytes()),
        ],
    );
    fixture
        .add(params("notes", "https://example.com/notes.md"))
        .await
        .unwrap();
    let saved = fixture.project().join("notes");
    assert_eq!(files_in(&saved), ["SKILL.md", "scripts/go.py"]);
    let card = &fixture.cards()[0];
    assert_eq!(title(card), "Save the skill notes for this project?");
    assert_eq!(
        card.source,
        SkillSource::Downloaded {
            url: "https://example.com/notes.md".into(),
            bytes: bundle.len() as u64
        }
    );
    assert_eq!(card.scripts, ["scripts/go.py"]);
    let record = agent_added::skill_at(&fixture.root, &saved).unwrap();
    assert_eq!(
        record.source.as_deref(),
        Some("https://example.com/notes.md")
    );

    remove_saved(&fixture.root, &saved).unwrap();
    fixture
        .add(with(
            params("notes", "https://example.com/notes.zip"),
            "scope",
            json!("user"),
        ))
        .await
        .unwrap();
    assert_eq!(files_in(&fixture.user().join("notes")), ["SKILL.md"]);
    assert_eq!(fixture.cards().len(), 2);
    assert_eq!(
        title(&fixture.cards()[1]),
        "Save the skill notes for every project?"
    );
}

/// Each check refuses before anyone is asked, with a reason the agent can
/// act on, and nothing is written.
#[tokio::test]
async fn a_skill_that_fails_a_check_is_refused_and_nothing_is_written() {
    let fixture = fixture(
        true,
        Answer::Approved {
            words: None,
            option: false,
        },
        vec![
            (
                "https://example.com/page",
                b"<!DOCTYPE html><html></html>".to_vec(),
            ),
            (
                "https://example.com/evil.zip",
                zip_of(&[
                    ("SKILL.md", skill_md("evil", "").as_bytes()),
                    ("../escape.md", &b"Out."[..]),
                ]),
            ),
        ],
    );
    let drafts = fixture.drafts();
    let draft = |name: &str, extra: &str, files: &[(&str, &str)]| {
        write_skill(&drafts, name, extra, files)
            .display()
            .to_string()
    };
    std::fs::create_dir_all(&drafts).unwrap();
    std::fs::write(drafts.join("plain.md"), "No front matter.\n").unwrap();
    std::fs::write(drafts.join("notes.txt"), skill_md("notes", "")).unwrap();
    write_skill(&fixture.workspace.join(".claude/skills"), "shared", "", &[]);
    write_skill(&fixture.home.join(".claude/skills"), "mine", "", &[]);
    let many: Vec<(String, String)> = (0..=MAX_FILES)
        .map(|n| (format!("f{n}.md"), "x".to_string()))
        .collect();
    let many: Vec<(&str, &str)> = many
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let big = "x".repeat(MAX_BYTES as usize);
    let latin1 = draft("latin", "", &[]);
    std::fs::write(Path::new(&latin1).join("notes.md"), [0xe9u8, 0xff]).unwrap();

    let cases: Vec<(Value, &str)> = vec![
        (
            params("plain", &format!("{}/plain.md", drafts.display())),
            "The skill doesn't load",
        ),
        (
            params("notes", &draft("other", "", &[])),
            "SKILL.md's front matter names the skill other, and the name you gave is notes. Make them the same.",
        ),
        (
            params("Notes", &draft("Notes", "", &[])),
            "Notes isn't a skill name. Use lowercase letters, digits and hyphens, such as release-notes.",
        ),
        (
            params(&"a".repeat(65), &draft("long", "", &[])),
            "The name is 65 characters, and a skill's name is at most 64.",
        ),
        (
            params("always", &draft("always", "auto_load: true\n", &[])),
            "An agent's skill can't have auto_load: true, which would put it in every later system prompt. Take it out. The user can set it by hand.",
        ),
        (
            params("ponytail", &draft("ponytail", "", &[])),
            "ponytail is the name of a skill built into Arko. Choose another name.",
        ),
        (
            params("shared", &draft("shared", "", &[])),
            "The user has a skill named shared in",
        ),
        (
            params("mine", &draft("mine", "", &[])),
            "The user has a skill named mine in",
        ),
        (
            params("shared", ".claude/skills/shared"),
            "/.claude/skills/shared is in the skill folder ",
        ),
        (
            params("many", &draft("many", "", &many)),
            "The skill has more than 200 files, the most a saved skill may have.",
        ),
        (
            params("big", &draft("big", "", &[("big.txt", &big)])),
            "The skill's files hold more than 1 MB, the most a saved skill may.",
        ),
        (params("latin", &latin1), "notes.md isn't UTF-8 text."),
        (
            params("quirks", &draft("quirks", "models: [\"haiku;rm\"]\n", &[])),
            "models has haiku;rm, and an entry may have only letters, digits and . - _ / : * ?",
        ),
        (
            params("evil", "https://example.com/evil.zip"),
            "The bundle doesn't load",
        ),
        (
            params("notes", "http://example.com/notes.md"),
            "Give an https URL.",
        ),
        (
            params("notes", "https://me:secret@example.com/notes.md"),
            "Give the URL without a user name or password.",
        ),
        (
            params("notes", "https://example.com/page"),
            "https://example.com/page is a web page, not a skill.",
        ),
        (
            params("notes", "https://example.com/gone.md"),
            "Couldn't download https://example.com/gone.md, because the server answered 404.",
        ),
        (params("notes", "drafts/none"), "There's nothing at"),
        (
            params("notes", "drafts/notes.txt"),
            "notes.txt isn't a skill. Give a folder with a SKILL.md, a .md file, a .zip or .skill bundle, or an https URL of one.",
        ),
        (
            with(
                params("notes", &draft("notes", "", &[])),
                "scope",
                json!("team"),
            ),
            "scope is project or user, not team",
        ),
    ];
    for (params, expected) in cases {
        let error = fixture.add(params.clone()).await.unwrap_err();
        assert!(error.contains(expected), "{params}: {error}");
        fixture.nothing_saved();
    }
    // A description over 1,024 characters.
    let long = format!(
        "---\nname: wordy\ndescription: {}\n---\nBody.\n",
        "x".repeat(MAX_DESCRIPTION + 1)
    );
    std::fs::create_dir_all(drafts.join("wordy")).unwrap();
    std::fs::write(drafts.join("wordy/SKILL.md"), long).unwrap();
    let error = fixture
        .add(params("wordy", "drafts/wordy"))
        .await
        .unwrap_err();
    assert!(
        error.contains("The description is 1,025 characters, and a skill's is at most 1,024."),
        "{error}"
    );
    // A link in the skill's folder.
    #[cfg(unix)]
    {
        let linked = draft("linked", "", &[]);
        std::os::unix::fs::symlink("/etc/hostname", Path::new(&linked).join("host.md")).unwrap();
        let error = fixture.add(params("linked", &linked)).await.unwrap_err();
        assert!(
            error.contains("host.md is a link. Copy what it points to into the skill instead."),
            "{error}"
        );
    }
    fixture.nothing_saved();
    assert!(fixture.cards().is_empty());
    assert!(fixture.notices().is_empty());
    assert!(!fixture.workspace.join(".agents").exists());
}

/// A name an agent's skill has in the other scope is refused, with the way
/// out.
#[tokio::test]
async fn a_name_an_agent_saved_in_the_other_scope_is_refused() {
    let fixture = quiet();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let source = draft.display().to_string();
    fixture.add(params("notes", &source)).await.unwrap();
    let error = fixture
        .add(with(params("notes", &source), "scope", json!("user")))
        .await
        .unwrap_err();
    assert!(
        error.contains("An agent saved a skill named notes for this project ("),
        "{error}"
    );
    assert!(
        error.ends_with("). Remove it with remove_skill first, or choose another name."),
        "{error}"
    );
}

/// A skill for every project always asks, with the card and its title. A
/// project skill asks only with Review project skills on.
#[tokio::test]
async fn user_scope_asks_and_project_scope_asks_only_with_review_on() {
    let fixture = quiet();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[("b.md", "B")]);
    let source = draft.display().to_string();
    fixture
        .add(with(params("notes", &source), "scope", json!("user")))
        .await
        .unwrap();
    assert_eq!(fixture.cards().len(), 1);
    let card = &fixture.cards()[0];
    assert_eq!(title(card), "Save the skill notes for every project?");
    assert_eq!(card.scope, crate::approvals::Scope::User);
    assert_eq!(card.files[0].path, "SKILL.md");
    assert_eq!(card.files[1].path, "b.md");
    let saved = fixture.user().join("notes");
    assert!(saved.join("b.md").is_file());
    assert_eq!(
        fixture.notices(),
        [format!(
            "The main agent saved the skill notes for every project. It is in {}.",
            saved.display()
        )]
    );
    assert_eq!(
        agent_added::skill_at(&fixture.root, &saved).unwrap().scope,
        Scope::User
    );

    let other = write_skill(&fixture.drafts(), "tips", "", &[]);
    let source = other.display().to_string();
    fixture.add(params("tips", &source)).await.unwrap();
    assert_eq!(fixture.cards().len(), 1, "a project skill doesn't ask");
    fixture.review_project_skills();
    let more = write_skill(&fixture.drafts(), "more", "", &[]);
    fixture
        .add(params("more", &more.display().to_string()))
        .await
        .unwrap();
    assert_eq!(fixture.cards().len(), 2);
    assert_eq!(
        title(&fixture.cards()[1]),
        "Save the skill more for this project?"
    );
}

/// With no one to ask, a skill for every project and a downloaded skill are
/// refused before anything is read, a project skill is refused while the
/// user reviews them, and saved otherwise.
#[tokio::test]
async fn with_no_one_to_ask_only_a_project_skill_without_review_is_saved() {
    let fixture = fixture(
        false,
        Answer::Unanswered,
        vec![(
            "https://example.com/notes.md",
            skill_md("notes", "").into_bytes(),
        )],
    );
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let source = draft.display().to_string();
    let error = fixture
        .add(with(params("notes", &source), "scope", json!("user")))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "No one can approve a skill for every project in this session. Save it for the project, or tell the user what it would hold."
    );
    for scope in ["project", "user"] {
        let error = fixture
            .add(with(
                params("notes", "https://example.com/notes.md"),
                "scope",
                json!(scope),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "No one can approve a downloaded skill in this session. Tell the user where it is."
        );
    }
    assert_eq!(*fixture.fetch.1.lock().unwrap(), 0, "nothing downloaded");
    fixture.review_project_skills();
    let error = fixture.add(params("notes", &source)).await.unwrap_err();
    assert!(
        error.starts_with("No one can approve a skill in this session, and the user reviews the skills agents save."),
        "{error}"
    );
    fixture.nothing_saved();
    crate::extension_prefs::update(&fixture.root, |prefs| prefs.review_project_skills = false)
        .unwrap();
    fixture.add(params("notes", &source)).await.unwrap();
    assert!(fixture.project().join("notes/SKILL.md").is_file());
    assert!(fixture.cards().is_empty());
}

/// A skill the user turns down, from a bundle, a URL or a folder, leaves
/// nothing in either skill folder, and its source as it was.
#[tokio::test]
async fn a_rejected_skill_leaves_nothing_in_the_skill_folders() {
    let skill = skill_md("notes", "");
    let bundle = zip_of(&[("SKILL.md", skill.as_bytes())]);
    let fixture = fixture(
        true,
        Answer::Rejected { words: None },
        vec![("https://example.com/notes", bundle.clone())],
    );
    fixture.review_project_skills();
    std::fs::create_dir_all(fixture.drafts()).unwrap();
    std::fs::write(fixture.drafts().join("notes.zip"), &bundle).unwrap();
    let folder = write_skill(&fixture.drafts(), "notes", "", &[]);
    for scope in ["project", "user"] {
        for source in [
            "drafts/notes.zip".to_string(),
            "https://example.com/notes".to_string(),
            folder.display().to_string(),
        ] {
            let result = fixture
                .add(with(params("notes", &source), "scope", json!(scope)))
                .await
                .unwrap();
            assert!(
                result.starts_with("The user chose not to save the skill notes for "),
                "{result}"
            );
            fixture.nothing_saved();
        }
    }
    assert_eq!(fixture.cards().len(), 6);
    assert!(folder.join("SKILL.md").is_file());
    assert!(fixture.drafts().join("notes.zip").is_file());
    assert!(fixture.notices().is_empty());
}

/// Words the user writes with a rejection go to the agent as possible
/// changes, and the transcript says Arko asks again. Words with an approval
/// go with the result.
#[tokio::test]
async fn the_users_words_reach_the_agent() {
    let fixture = fixture(
        true,
        Answer::Rejected {
            words: Some("Call it release-steps.".into()),
        },
        Vec::new(),
    );
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let source = draft.display().to_string();
    let result = fixture
        .add(with(params("notes", &source), "scope", json!("user")))
        .await
        .unwrap();
    assert!(
        result.starts_with("The user didn't save the skill notes for every project and wrote \"Call it release-steps.\". If those words ask for changes to the skill, make them and call add_skill again"),
        "{result}"
    );
    assert_eq!(fixture.notices(), ["Not saved. Arko asks again."]);
    fixture.nothing_saved();

    let fixture = fixture_approving_with("Keep it short.");
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let result = fixture
        .add(with(
            params("notes", &draft.display().to_string()),
            "scope",
            json!("user"),
        ))
        .await
        .unwrap();
    assert!(
        result.ends_with(
            " The user approved it and wrote \"Keep it short.\". Act on what they wrote."
        ),
        "{result}"
    );
}

fn fixture_approving_with(words: &str) -> Fixture {
    fixture(
        true,
        Answer::Approved {
            words: Some(words.into()),
            option: false,
        },
        Vec::new(),
    )
}

/// The card says where the skill came from, who gets it, the models it is
/// listed for, its size and scripts, and what it replaces, with the files
/// SKILL.md first.
#[tokio::test]
async fn the_card_shows_what_is_saved() {
    let fixture = quiet();
    let draft = write_skill(
        &fixture.drafts(),
        "notes",
        "models: [haiku, cc:haiku]\n",
        &[("checklist.md", "- Tag it"), ("scripts/release.sh", "echo")],
    );
    let source = draft.display().to_string();
    let user = |workers: bool| {
        with(
            with(params("notes", &source), "scope", json!("user")),
            "workers",
            json!(workers),
        )
    };
    fixture.add(user(true)).await.unwrap();
    fixture.add(user(false)).await.unwrap();
    let card = &fixture.cards()[1];
    let skill = skill_md("notes", "models: [haiku, cc:haiku]\nworkers: false\n");
    assert_eq!(card.files[0].text.as_deref(), Some(skill.as_str()));
    assert_eq!(
        card.files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["SKILL.md", "checklist.md", "scripts/release.sh"]
    );
    let today = day(&chrono::Utc::now());
    let details: Vec<String> = Approval::Skill(card.clone())
        .details()
        .into_iter()
        .map(|detail| detail.text)
        .collect();
    assert_eq!(
        details[..7],
        [
            "How this project writes notes.".to_string(),
            format!("Copied from {}.", draft.display()),
            "Only the main agent gets it. Listed for agents on haiku and cc:haiku.".to_string(),
            format!(
                "SKILL.md is {} characters (8 lines), with 2 more files. It has scripts an agent may run (scripts/release.sh).",
                skill.chars().count()
            ),
            format!("It replaces the skill notes an agent saved on {today}."),
            "Why".to_string(),
            "Later conversations need the steps.".to_string(),
        ]
    );
    assert!(fixture.cards()[0].workers);
    // The saved copy is what the card showed.
    assert_eq!(
        std::fs::read_to_string(fixture.user().join("notes/SKILL.md")).unwrap(),
        skill
    );
}

/// A second save of a skill an agent saved replaces it whole, and the
/// record with it.
#[tokio::test]
async fn an_agents_skill_is_replaced_whole() {
    let fixture = quiet();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[("old.md", "Old.")]);
    let source = draft.display().to_string();
    fixture.add(params("notes", &source)).await.unwrap();
    std::fs::remove_file(draft.join("old.md")).unwrap();
    std::fs::write(draft.join("new.md"), "New.").unwrap();
    let result = fixture
        .add(with(params("notes", &source), "why", json!("Newer steps.")))
        .await
        .unwrap();
    assert!(
        result.starts_with("Saved the skill notes for this project in place of the one saved before. load_skill loads the new one now."),
        "{result}"
    );
    let saved = fixture.project().join("notes");
    assert_eq!(files_in(&saved), ["SKILL.md", "new.md"]);
    assert_eq!(files_in(&fixture.project()).len(), 2);
    assert_eq!(
        agent_added::skill_at(&fixture.root, &saved).unwrap().why,
        "Newer steps."
    );
    assert_eq!(agent_added::skills(&fixture.root).len(), 1);
}

/// `remove_skill` removes only a skill an agent saved, in either scope, and
/// the transcript says so.
#[tokio::test]
async fn remove_skill_removes_only_what_an_agent_saved() {
    let fixture = quiet();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let source = draft.display().to_string();
    fixture.add(params("notes", &source)).await.unwrap();
    let tips = write_skill(&fixture.drafts(), "tips", "", &[]);
    fixture
        .add(with(
            params("tips", &tips.display().to_string()),
            "scope",
            json!("user"),
        ))
        .await
        .unwrap();
    for name in ["notes", "tips"] {
        let result = fixture.remove(name).await.unwrap();
        assert_eq!(
            result,
            format!(
                "Removed the skill {name}. load_skill no longer finds it, and the next conversation's skill list leaves it out."
            )
        );
    }
    fixture.nothing_saved();
    assert_eq!(
        fixture.notices()[2..],
        [
            "The main agent removed the skill notes.",
            "The main agent removed the skill tips."
        ]
    );
    let mine = write_skill(&fixture.workspace.join(".claude/skills"), "mine", "", &[]);
    assert_eq!(
        fixture.remove("mine").await.unwrap_err(),
        format!(
            "mine wasn't saved by an agent, so remove_skill can't remove it. The user removes it by deleting {}.",
            mine.display()
        )
    );
    assert!(mine.is_dir());
    // A user's skill in .arko/skills that no record has stays too.
    let theirs = write_skill(&fixture.project(), "theirs", "", &[]);
    assert!(fixture.remove("theirs").await.is_err());
    assert!(theirs.is_dir());
    assert_eq!(
        fixture.remove("pdff").await.unwrap_err(),
        "There's no skill named pdff."
    );
    assert_eq!(
        fixture.remove("ponytail").await.unwrap_err(),
        "ponytail is built into Arko. The user leaves it out with /builtin-skills ponytail off."
    );
}

/// What a save writes is the copy read before the card was shown, so an
/// edit to the source while the user decides isn't saved, from a folder, a
/// Markdown file or a bundle.
#[tokio::test]
async fn an_edit_to_the_source_while_the_user_decides_isnt_saved() {
    let fixture = quiet();
    let drafts = fixture.drafts();
    let skill = skill_md("notes", "");
    let evil = skill_md("notes", "auto_load: true\n");
    for (source, files) in [
        ("drafts/notes", vec!["SKILL.md", "a.md"]),
        ("drafts/notes.md", vec!["SKILL.md"]),
        ("drafts/notes.zip", vec!["SKILL.md", "a.md"]),
    ] {
        let _ = std::fs::remove_dir_all(&drafts);
        write_skill(&drafts, "notes", "", &[("a.md", "A")]);
        std::fs::write(drafts.join("notes.md"), &skill).unwrap();
        std::fs::write(
            drafts.join("notes.zip"),
            zip_of(&[("SKILL.md", skill.as_bytes()), ("a.md", &b"A"[..])]),
        )
        .unwrap();
        let approver = meanwhile({
            let (drafts, evil) = (drafts.clone(), evil.clone());
            move || {
                std::fs::write(drafts.join("notes/SKILL.md"), &evil).unwrap();
                std::fs::write(drafts.join("notes/a.md"), "Changed.").unwrap();
                std::fs::write(drafts.join("notes/run.sh"), "echo run").unwrap();
                std::fs::write(drafts.join("notes.md"), &evil).unwrap();
                std::fs::write(
                    drafts.join("notes.zip"),
                    zip_of(&[("SKILL.md", evil.as_bytes()), ("run.sh", &b"echo"[..])]),
                )
                .unwrap();
            }
        });
        fixture
            .add_asking(
                approver.clone(),
                with(params("notes", source), "scope", json!("user")),
            )
            .await
            .unwrap();
        let saved = fixture.user().join("notes");
        assert_eq!(files_in(&saved), files, "{source}");
        assert_eq!(
            std::fs::read_to_string(saved.join("SKILL.md")).unwrap(),
            skill
        );
        if files.len() > 1 {
            assert_eq!(std::fs::read_to_string(saved.join("a.md")).unwrap(), "A");
        }
        let card = approver.cards.lock().unwrap()[0].clone();
        assert_eq!(card.files[0].text.as_deref(), Some(skill.as_str()));
        assert_eq!(card.files.len(), files.len());
        remove_saved(&fixture.root, &saved).unwrap();
    }
}

/// A save given up, because the user didn't answer or the call was dropped
/// while they decided, leaves nothing behind (no skill, no record and no
/// hidden folder), and the source as it was.
#[tokio::test]
async fn a_save_given_up_leaves_nothing() {
    let fixture = fixture(true, Answer::Unanswered, Vec::new());
    fixture.review_project_skills();
    let folder = write_skill(&fixture.drafts(), "notes", "", &[("a.md", "A")]);
    let skill = skill_md("notes", "");
    std::fs::write(
        fixture.drafts().join("notes.zip"),
        zip_of(&[("SKILL.md", skill.as_bytes())]),
    )
    .unwrap();
    for source in ["drafts/notes", "drafts/notes.zip"] {
        for scope in ["project", "user"] {
            let call = with(params("notes", source), "scope", json!(scope));
            let result = fixture.add(call.clone()).await.unwrap();
            assert_eq!(
                result,
                "The user didn't answer, so the skill notes wasn't saved."
            );
            fixture.nothing_saved();
            let dropped = tokio::time::timeout(
                Duration::from_millis(100),
                fixture.add_asking(Arc::new(Never), call),
            )
            .await;
            assert!(dropped.is_err(), "{source}");
            fixture.nothing_saved();
        }
    }
    assert_eq!(files_in(&folder), ["SKILL.md", "a.md"]);
    assert!(fixture.drafts().join("notes.zip").is_file());
    assert!(fixture.notices().is_empty());
}

/// Neither the project's .arko, its skills folder, Arko's skills folder nor
/// a skill's own folder may be a link, so a save can't be led elsewhere,
/// such as from the project into Arko's skills for every project. Each is
/// refused with the reason before anyone is asked, and nothing is written
/// where the link leads.
#[cfg(unix)]
#[tokio::test]
async fn a_link_on_the_way_to_a_skills_place_is_refused() {
    use std::os::unix::fs::symlink;
    let fixture = quiet();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let source = draft.display().to_string();
    let elsewhere = fixture.workspace.with_file_name("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(fixture.user()).unwrap();
    let arko = fixture.workspace.join(".arko");
    let refused = |path: &Path| {
        format!(
            "{} is a link, and Arko doesn't write through links.",
            path.display()
        )
    };

    symlink(&elsewhere, &arko).unwrap();
    let error = fixture.add(params("notes", &source)).await.unwrap_err();
    assert_eq!(error, refused(&arko));
    std::fs::remove_file(&arko).unwrap();

    std::fs::create_dir(&arko).unwrap();
    symlink(fixture.user(), fixture.project()).unwrap();
    let error = fixture.add(params("notes", &source)).await.unwrap_err();
    assert_eq!(error, refused(&fixture.project()));
    assert!(files_in(&fixture.user()).is_empty());
    std::fs::remove_file(fixture.project()).unwrap();

    for (scope, place) in [("project", fixture.project()), ("user", fixture.user())] {
        std::fs::create_dir_all(&place).unwrap();
        symlink(&elsewhere, place.join("notes")).unwrap();
        let error = fixture
            .add(with(params("notes", &source), "scope", json!(scope)))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            format!("{} Choose another name.", refused(&place.join("notes")))
        );
        std::fs::remove_file(place.join("notes")).unwrap();
    }

    std::fs::remove_dir(fixture.user()).unwrap();
    symlink(&elsewhere, fixture.user()).unwrap();
    let error = fixture
        .add(with(params("notes", &source), "scope", json!("user")))
        .await
        .unwrap_err();
    assert_eq!(error, refused(&fixture.user()));

    assert!(files_in(&elsewhere).is_empty());
    assert!(agent_added::skills(&fixture.root).is_empty());
    assert!(fixture.cards().is_empty());
    assert!(fixture.notices().is_empty());
}

/// A link put in place of .arko while the user decides stops the save, and
/// nothing is written where it leads.
#[cfg(unix)]
#[tokio::test]
async fn a_link_made_while_the_user_decides_is_refused() {
    let fixture = quiet();
    fixture.review_project_skills();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let elsewhere = fixture.workspace.with_file_name("elsewhere");
    std::fs::create_dir_all(elsewhere.join("skills")).unwrap();
    std::fs::create_dir_all(fixture.project()).unwrap();
    let arko = fixture.workspace.join(".arko");
    let approver = meanwhile({
        let (arko, elsewhere) = (arko.clone(), elsewhere.clone());
        move || {
            std::fs::remove_dir_all(&arko).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &arko).unwrap();
        }
    });
    let error = fixture
        .add_asking(approver, params("notes", &draft.display().to_string()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        format!(
            "{} is a link, and Arko doesn't write through links.",
            arko.display()
        )
    );
    assert!(files_in(&elsewhere).is_empty());
    assert!(agent_added::skills(&fixture.root).is_empty());
}

/// A skill is recorded by its folder's real path, so a project reached
/// through a link finds it. A link put in place of .arko/skills later
/// doesn't have a removal delete what it leads to, such as the user's own
/// skill of that name for every project.
#[cfg(unix)]
#[tokio::test]
async fn the_record_holds_the_real_path_and_a_removal_follows_no_link() {
    use std::os::unix::fs::symlink;
    let fixture = quiet();
    let linked = fixture.workspace.with_file_name("linked");
    symlink(&fixture.workspace, &linked).unwrap();
    let draft = write_skill(&fixture.drafts(), "notes", "", &[]);
    let context = fixture.context_with(&linked, fixture.approver.clone());
    Fixture::add_in(context, params("notes", &draft.display().to_string()))
        .await
        .unwrap();
    let real = fixture
        .workspace
        .canonicalize()
        .unwrap()
        .join(".arko/skills/notes");
    let records = agent_added::skills(&fixture.root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].path, real);
    assert!(saved_skill(&fixture.root, &linked, "notes").is_some());
    assert!(saved_skill(&fixture.root, &fixture.workspace, "notes").is_some());

    let theirs = write_skill(&fixture.user(), "notes", "", &[]);
    std::fs::rename(fixture.project(), fixture.workspace.join("moved")).unwrap();
    symlink(fixture.user(), fixture.project()).unwrap();
    for path in [fixture.project().join("notes"), real.clone()] {
        assert!(remove_saved(&fixture.root, &path).is_err());
    }
    assert!(fixture.remove("notes").await.is_err());
    assert_eq!(files_in(&theirs), ["SKILL.md"]);
}

/// `workers` goes into the front matter as `workers: false`, or comes out
/// for true, whatever the line endings.
#[test]
fn workers_is_written_into_the_front_matter() {
    let text = "---\nname: a\ndescription: A.\n---\nBody.\nworkers: true\n";
    assert_eq!(
        set_workers(text, false).unwrap(),
        "---\nname: a\ndescription: A.\nworkers: false\n---\nBody.\nworkers: true\n"
    );
    assert_eq!(set_workers(text, true).unwrap(), text);
    let off = "---\nname: a\nworkers: false\nmeta:\n  workers: x\n---\nBody.\n";
    assert_eq!(
        set_workers(off, true).unwrap(),
        "---\nname: a\nmeta:\n  workers: x\n---\nBody.\n"
    );
    assert_eq!(
        set_workers("---\r\nname: a\r\n---\r\nBody.\r\n", false).unwrap(),
        "---\r\nname: a\r\nworkers: false\r\n---\r\nBody.\r\n"
    );
    assert_eq!(set_workers("No front matter.\n", false), None);
    assert_eq!(set_workers("---\nname: a\n", false), None);
}

#[test]
fn sizes_read_as_people_read_them() {
    assert_eq!(size_words(300), "300 bytes");
    assert_eq!(size_words(14 * 1024), "14 KB");
    assert_eq!(size_words(MAX_BYTES), "1 MB");
    assert_eq!(size_words(MAX_BYTES * 3 / 2), "1.5 MB");
    assert_eq!(thousands(3210), "3,210");
    assert_eq!(thousands(84), "84");
}

/// A local server with a few paths: a body, one over the limit, chains of
/// redirects, a redirect away from https, and a missing file.
async fn server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, location, body): (&str, Option<String>, Vec<u8>) =
                    if path == "/skill.md" {
                        ("200 OK", None, skill_md("notes", "").into_bytes())
                    } else if path == "/big" {
                        ("200 OK", None, vec![b'x'; MAX_BYTES as usize + 1])
                    } else if let Some(hops) = path.strip_prefix("/hop/") {
                        let hops: usize = hops.parse().unwrap();
                        if hops == 0 {
                            ("200 OK", None, b"arrived".to_vec())
                        } else {
                            ("302 Found", Some(format!("/hop/{}", hops - 1)), Vec::new())
                        }
                    } else if path == "/away" {
                        (
                            "302 Found",
                            Some("http://example.com/skill.md".into()),
                            Vec::new(),
                        )
                    } else {
                        ("404 Not Found", None, Vec::new())
                    };
                let mut head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if let Some(location) = location {
                    head.push_str(&format!("Location: {location}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    base
}

/// A download follows at most five redirects and only to https (a local
/// server in tests), stops past 1 MB, and says what the server answered.
#[tokio::test]
async fn a_download_keeps_to_its_limits() {
    let base = server().await;
    let fetch = |path: &str| {
        let url = format!("{base}{path}");
        async move { Https.fetch(&url).await }
    };
    assert_eq!(
        fetch("/skill.md").await.unwrap(),
        skill_md("notes", "").into_bytes()
    );
    assert_eq!(fetch("/hop/5").await.unwrap(), b"arrived");
    let error = fetch("/hop/6").await.unwrap_err();
    assert_eq!(error, "it redirects more than 5 times");
    let error = fetch("/away").await.unwrap_err();
    assert_eq!(
        error,
        "it redirects to http://example.com/skill.md, which isn't https"
    );
    assert_eq!(fetch("/big").await.unwrap_err(), "it is over 1 MB");
    assert_eq!(
        fetch("/missing").await.unwrap_err(),
        "the server answered 404"
    );
}
