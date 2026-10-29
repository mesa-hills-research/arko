use super::*;
use crate::approvals::{Approval, Look};

fn roots(temp: &Path) -> Roots {
    Roots::new(temp.join("arko"))
        .workspace(temp.join("work"))
        .session(temp.join("session"))
}

fn write(roots: &Roots, place: Place, name: &str, text: &str) -> PathBuf {
    let folder = roots.folder(place).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join(format!("{name}.md"));
    std::fs::write(&file, text).unwrap();
    file
}

/// Writes a profile in the older `.agents/profiles` of `place`.
fn write_older(roots: &Roots, place: Place, name: &str, text: &str) -> PathBuf {
    let folder = roots.older_folder(place).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join(format!("{name}.md"));
    std::fs::write(&file, text).unwrap();
    file
}

fn full() -> Profile {
    let mut profile = Profile::new("3d-judge", "Judges 3D results against a reference image");
    profile.model = Some("sol".into());
    profile.effort = Some(Effort::High);
    profile.fast = true;
    profile.cache = Some(Cache::OneHour);
    profile.nested = true;
    profile.made_by = Some(MADE_BY_AGENT.into());
    profile.instructions =
        "You judge 3D models.\n\nCompare the render with the reference image.".into();
    profile.extra = vec![("tools_from".into(), "a newer Arko".into())];
    profile
}

#[test]
fn every_key_survives_a_round_trip() {
    let profile = full();
    let text = profile.render();
    assert!(
        text.starts_with("---\ndescription: Judges 3D results"),
        "{text}"
    );
    assert_eq!(Profile::parse("3d-judge", &text).unwrap(), profile);
    // The smallest profile writes only a description.
    let small = Profile::new("quick", "Reads files");
    assert_eq!(small.render(), "---\ndescription: Reads files\n---\n");
    assert_eq!(Profile::parse("quick", &small.render()).unwrap(), small);
}

/// `skills` and `mcp` are lists in the front matter: names separated by
/// commas, or `none`. A line with nothing in it leaves the set to the
/// starter, and a bad one is said.
#[test]
fn skills_and_servers_are_lists_in_the_front_matter() {
    let mut profile = Profile::new("narrow", "Reads");
    profile.skills = Some(vec!["ponytail".into(), "release-notes".into()]);
    profile.mcp = Some(Vec::new());
    let text = profile.render();
    assert_eq!(
        text,
        "---\ndescription: Reads\nskills: ponytail, release-notes\nmcp: none\n---\n"
    );
    assert_eq!(Profile::parse("narrow", &text).unwrap(), profile);
    assert_eq!(profile.check(), Ok(()));
    // The other forms read the same, and a repeated name counts once.
    let json = "---\ndescription: Reads\nskills: [\"ponytail\", \"release-notes\", \"ponytail\"]\nmcp: None\n---\n";
    assert_eq!(Profile::parse("narrow", json).unwrap(), profile);
    // Left out or blank is the starter's.
    let blank = Profile::parse("narrow", "---\ndescription: Reads\nskills:\n---\n").unwrap();
    assert_eq!((blank.skills, blank.mcp), (None, None));
    assert_eq!(
        Profile::parse("narrow", "---\ndescription: Reads\nskills: ,\n---\n").unwrap_err(),
        "`skills` isn't names separated by commas, a JSON array or none"
    );
    assert_eq!(
        Profile::parse("narrow", "---\ndescription: Reads\nmcp: [1]\n---\n").unwrap_err(),
        "`mcp` isn't names separated by commas, a JSON array or none"
    );
    assert_eq!(
        Profile::parse("narrow", "---\ndescription: Reads\nmcp: a\nmcp: b\n---\n").unwrap_err(),
        "`mcp` is given twice"
    );
    // A name a list can't hold isn't saved.
    profile.skills = Some(vec!["a,b".into()]);
    assert_eq!(
        profile.check().unwrap_err(),
        "The skills list has a name with a comma or a control character."
    );
}

/// The approval carries what workers with the profile are limited to, in
/// the request's data and in its details in the tray.
#[test]
fn the_approval_says_what_workers_with_the_profile_get() {
    let details = |profile: &Profile| -> Vec<(Look, String)> {
        Approval::Profile(approval(profile, Place::User))
            .details()
            .into_iter()
            .map(|detail| (detail.look, detail.text))
            .collect()
    };
    let mut profile = Profile::new("narrow", "Reads");
    profile.instructions = "Read only.".into();
    assert!(profile.limits().is_empty());
    assert!(approval(&profile, Place::User).limits.is_empty());
    let plain = details(&profile);
    profile.skills = Some(vec!["ponytail".into(), "release-notes".into()]);
    profile.mcp = Some(Vec::new());
    assert_eq!(
        profile.limits(),
        [
            "Workers with it get only the skills ponytail and release-notes.",
            "Workers with it get no MCP servers."
        ]
    );
    assert_eq!(approval(&profile, Place::User).limits, profile.limits());
    // The tray shows them after the description, before the folder and the
    // instructions.
    let mut narrowed = plain.clone();
    narrowed.splice(
        2..2,
        profile
            .limits()
            .into_iter()
            .map(|limit| (Look::Text, limit)),
    );
    assert_eq!(plain[1], (Look::Text, "Reads".to_string()));
    assert_eq!(
        plain[2],
        (Look::Text, "Saved in Arko's profiles folder.".to_string())
    );
    assert_eq!(plain[3], (Look::Heading, "Instructions".to_string()));
    assert_eq!(details(&profile), narrowed);
    profile.skills = Some(vec!["ponytail".into()]);
    profile.mcp = None;
    assert_eq!(
        profile.limits(),
        ["Workers with it get only the skill ponytail."]
    );
}

#[test]
fn awkward_values_read_back_as_written() {
    let mut profile = Profile::new("odd", "\"Quoted\" start: a colon, a # sign and \\ slash");
    profile.model = Some(" spaced ".into());
    profile.extra = vec![("note".into(), "line one\nline two".into())];
    profile.instructions = "Keep the --- line\n---\nafter the front matter.".into();
    let back = Profile::parse("odd", &profile.render()).unwrap();
    assert_eq!(back, profile);
    // A description over several lines is one line once saved.
    let mut spread = Profile::new("spread", "First\nsecond   line");
    assert_eq!(
        Profile::parse("spread", &spread.render())
            .unwrap()
            .description,
        "First second line"
    );
    spread.description = "x".into();
    assert!(spread.check().is_ok());
}

#[test]
fn windows_line_endings_and_a_byte_order_mark_read_the_same() {
    let plain = "---\ndescription: Reads files\nmodel: haiku\n---\nLook, then answer.\nShort.\n";
    let crlf = plain.replace('\n', "\r\n");
    let bom = format!("\u{feff}{crlf}");
    let expected = Profile::parse("quick", plain).unwrap();
    assert_eq!(expected.instructions, "Look, then answer.\nShort.");
    assert_eq!(Profile::parse("quick", &crlf).unwrap(), expected);
    assert_eq!(Profile::parse("quick", &bom).unwrap(), expected);
    // A file the editor saved with a mark and no newline at the end.
    let bare = "\u{feff}---\r\ndescription: Reads files\r\n---";
    assert_eq!(Profile::parse("quick", bare).unwrap().instructions, "");
}

#[test]
fn a_file_that_isnt_a_profile_says_what_is_wrong() {
    let reason = |text: &str| Profile::parse("x", text).unwrap_err();
    assert!(reason("Just text.\n").contains("doesn't start with a --- line"));
    assert!(reason("---\ndescription: A\n").contains("isn't closed"));
    assert!(reason("---\nmodel: sol\n---\n").contains("no description"));
    assert!(reason("---\ndescription:   \n---\n").contains("no description"));
    assert_eq!(
        reason("---\ndescription: A\nmodel sol\n---\n"),
        "line 3 isn't `key: value`"
    );
    assert!(
        reason("---\ndescription: A\nmodel: a\nmodel: b\n---\n").contains("`model` is given twice")
    );
    assert!(reason("---\ndescription: A\neffort: extreme\n---\n").contains("`effort: extreme`"));
    assert!(reason("---\ndescription: A\nfast: maybe\n---\n").contains("`fast: maybe`"));
    assert!(reason("---\ndescription: A\ncache: 2h\n---\n").contains("`cache: 2h`"));
    let long = format!("---\ndescription: {}\n---\n", "d".repeat(121));
    assert!(reason(&long).contains("over 120 characters"));
    let body = format!("---\ndescription: A\n---\n{}\n", "b".repeat(4_001));
    assert!(reason(&body).contains("over 4,000 characters"));
    // The limits themselves are fine.
    let edge = format!(
        "---\ndescription: {}\n---\n{}\n",
        "d".repeat(120),
        "b".repeat(4_000)
    );
    assert!(Profile::parse("x", &edge).is_ok());
}

#[test]
fn names_follow_the_rules() {
    for good in ["a", "3d-judge", "quick-read", "0", &"a".repeat(32)] {
        assert!(check_name(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "-a",
        "A",
        "a b",
        "a_b",
        "a.b",
        "../x",
        "a/b",
        &"a".repeat(33),
        "é",
    ] {
        let why = check_name(bad).unwrap_err();
        assert!(why.contains("isn't a profile name"), "{bad}: {why}");
    }
    for reserved in RESERVED {
        let why = check_name(reserved).unwrap_err();
        assert!(why.contains("reserved"), "{reserved}: {why}");
    }
}

#[test]
fn checking_a_profile_covers_its_model_and_effort() {
    let mut profile = Profile::new("judge", "Judges");
    assert!(profile.check().is_ok());
    profile.model = Some("gpt-5.4-nano-2026-03-17".into());
    profile.effort = Some(Effort::Max);
    let why = profile.check().unwrap_err();
    assert_eq!(
        why,
        "gpt-5.4-nano-2026-03-17 takes low, medium, high or xhigh effort, not max."
    );
    profile.effort = Some(Effort::High);
    assert!(profile.check().is_ok());
    profile.model = Some("gpt9".into());
    assert!(
        profile
            .check()
            .unwrap_err()
            .contains("isn't a model Arko knows")
    );
    // An effort without a model is checked against the model that runs it.
    profile.model = None;
    profile.effort = Some(Effort::Max);
    assert!(profile.check().is_ok());
    profile.description = "  ".into();
    assert!(profile.check().unwrap_err().contains("needs a description"));
    profile.description = "d".repeat(121);
    assert!(profile.check().unwrap_err().contains("limit is 120"));
    profile.description = "ok".into();
    profile.instructions = "i".repeat(4_001);
    assert!(profile.check().unwrap_err().contains("limit is 4,000"));
}

#[test]
fn the_nearest_place_wins_and_the_others_are_listed_as_hidden() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let file = |description: &str| format!("---\ndescription: {description}\n---\nBody.\n");
    write(&roots, Place::User, "reviewer", &file("The user's"));
    write(&roots, Place::User, "quick", &file("Quick, the user's"));
    write(&roots, Place::Project, "reviewer", &file("The project's"));
    write(&roots, Place::Session, "reviewer", &file("The session's"));
    write(
        &roots,
        Place::Project,
        "audit",
        &file("The project's audit"),
    );

    let profiles = list(&roots);
    let names: Vec<(&str, Place)> = profiles
        .active
        .iter()
        .map(|found| (found.profile.name.as_str(), found.place))
        .collect();
    assert_eq!(
        names,
        [
            ("audit", Place::Project),
            ("quick", Place::User),
            ("reviewer", Place::Session)
        ]
    );
    let hidden: Vec<(&str, Place)> = profiles
        .hidden
        .iter()
        .map(|found| (found.profile.name.as_str(), found.place))
        .collect();
    assert_eq!(
        hidden,
        [("reviewer", Place::Project), ("reviewer", Place::User)]
    );
    assert_eq!(
        profiles.get("reviewer").unwrap().profile.description,
        "The session's"
    );
    assert_eq!(read(&roots, "reviewer").unwrap().place, Place::Session);
    assert_eq!(read(&roots, "quick").unwrap().place, Place::User);
    assert_eq!(read(&roots, "nothing").unwrap_err(), ReadError::Missing);

    // Without a session or a project only the user's place is read.
    let user_only = Roots::new(temp.path().join("arko"));
    let profiles = list(&user_only);
    assert_eq!(profiles.active.len(), 2);
    assert!(profiles.hidden.is_empty());
    assert_eq!(
        folders(&user_only),
        [(Place::User, temp.path().join("arko").join("profiles"))]
    );
    // The project has two folders, the one a save writes first.
    assert_eq!(
        folders(&roots)
            .iter()
            .map(|(place, _)| *place)
            .collect::<Vec<_>>(),
        [Place::Session, Place::Project, Place::Project, Place::User]
    );
    let listed = folders(&roots);
    assert!(listed[1].1.ends_with(Path::new(".arko").join("profiles")));
    assert!(listed[2].1.ends_with(Path::new(".agents").join("profiles")));
    assert_eq!(Some(listed[1].1.clone()), roots.folder(Place::Project));
}

#[test]
fn a_file_that_cant_be_read_hides_no_other() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write(
        &roots,
        Place::User,
        "reviewer",
        "---\ndescription: The user's\n---\n",
    );
    // Nearer, broken: the user's is still the profile in force.
    write(
        &roots,
        Place::Project,
        "reviewer",
        "---\ndescription: oops\nmodel sol\n---\n",
    );
    write(&roots, Place::Project, "notes", "no front matter at all\n");
    write(
        &roots,
        Place::Project,
        "Bad-Name",
        "---\ndescription: A\n---\n",
    );
    write(&roots, Place::User, "default", "---\ndescription: A\n---\n");
    write(
        &roots,
        Place::User,
        "huge",
        &format!("---\ndescription: A\n---\n{}", "x".repeat(17_000)),
    );
    write(&roots, Place::User, "README", "About these profiles.\n");
    std::fs::write(
        roots.folder(Place::User).unwrap().join("binary.md"),
        [0xff, 0xfe, 0xfd],
    )
    .unwrap();
    std::fs::write(roots.folder(Place::User).unwrap().join("note.txt"), "x").unwrap();

    let profiles = list(&roots);
    assert_eq!(profiles.active.len(), 1);
    assert_eq!(profiles.active[0].place, Place::User);
    let problems: Vec<(String, String)> = profiles
        .unreadable
        .iter()
        .map(|problem| (problem.name.clone(), problem.reason.clone()))
        .collect();
    let reason = |name: &str| {
        problems
            .iter()
            .find(|(found, _)| found == name)
            .unwrap_or_else(|| panic!("{name} in {problems:?}"))
            .1
            .clone()
    };
    assert_eq!(reason("reviewer"), "line 3 isn't `key: value`");
    assert!(reason("notes").contains("doesn't start with a --- line"));
    assert!(reason("Bad-Name").contains("isn't a profile name"));
    assert!(reason("default").contains("reserved"));
    assert!(reason("huge").contains("over 16 KB"));
    assert!(reason("binary").contains("UTF-8"));
    assert_eq!(
        problems.len(),
        6,
        "README and note.txt are not profiles: {problems:?}"
    );
    // Read by name, a broken nearer file gives way to a readable one.
    assert_eq!(read(&roots, "reviewer").unwrap().place, Place::User);
    // With nothing else to use, the reason comes back.
    match read(&roots, "notes").unwrap_err() {
        ReadError::Unreadable(problem) => assert_eq!(problem.place, Place::Project),
        other => panic!("{other:?}"),
    }
    assert!(
        read(&roots, "notes")
            .unwrap_err()
            .to_string()
            .contains("can't be used")
    );
    // A name that could leave the folder is refused before any file is read.
    assert!(matches!(read(&roots, "../x"), Err(ReadError::BadName(_))));
    assert!(matches!(read(&roots, "a/b"), Err(ReadError::BadName(_))));
}

#[test]
fn a_profile_with_a_model_arko_does_not_know_is_listed_with_a_note() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write(
        &roots,
        Place::User,
        "future",
        "---\ndescription: A\nmodel: gpt9\n---\n",
    );
    write(
        &roots,
        Place::User,
        "wrong-effort",
        "---\ndescription: A\nmodel: gpt-5.4-nano-2026-03-17\neffort: max\n---\n",
    );
    write(
        &roots,
        Place::User,
        "fine",
        "---\ndescription: A\nmodel: sol\neffort: max\n---\n",
    );
    let profiles = list(&roots);
    let note = |name: &str| profiles.get(name).unwrap().note.clone();
    // A note is one short sentence, with no list of aliases.
    assert_eq!(note("future").unwrap(), "`gpt9` isn't a model Arko knows.");
    assert!(note("wrong-effort").unwrap().contains("not max"));
    assert_eq!(note("fine"), None);
}

#[test]
fn saving_writes_the_file_and_keeps_unknown_keys() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let saved = save(&roots, Place::User, &full(), Author::User).unwrap();
    assert_eq!(
        saved.file,
        temp.path()
            .join("arko")
            .join("profiles")
            .join("3d-judge.md")
    );
    assert!(!saved.replaced);
    assert_eq!(saved.hidden_by, None);
    let found = read(&roots, "3d-judge").unwrap();
    // The user's save takes the agent's mark off, and keeps the key this
    // version does not know.
    assert_eq!(found.profile.made_by, None);
    assert_eq!(found.profile.extra, full().extra);
    assert_eq!(found.profile.instructions, full().instructions);

    let mut edited = found.profile.clone();
    edited.description = "Judges renders".into();
    let again = save(&roots, Place::User, &edited, Author::User).unwrap();
    assert!(again.replaced);
    let text = std::fs::read_to_string(&again.file).unwrap();
    assert!(text.contains("tools_from: a newer Arko"), "{text}");
    assert!(text.contains("description: Judges renders"), "{text}");

    // A profile of the same name nearer hides it, and the answer says so.
    let nearer = save(&roots, Place::Session, &edited, Author::User).unwrap();
    assert_eq!(nearer.hidden_by, None);
    let behind = save(&roots, Place::Project, &edited, Author::User).unwrap();
    assert_eq!(behind.hidden_by, Some(Place::Session));
    // A profile that fails its checks writes nothing.
    let mut bad = edited.clone();
    bad.name = "Bad Name".into();
    assert!(save(&roots, Place::User, &bad, Author::User).is_err());
    // No session, no place for the session's profiles.
    let none = Roots::new(temp.path().join("arko"));
    assert!(
        save(&none, Place::Session, &edited, Author::User)
            .unwrap_err()
            .contains("no session")
    );
    assert!(
        save(&none, Place::Project, &edited, Author::User)
            .unwrap_err()
            .contains("no project")
    );
}

#[test]
fn an_agent_never_replaces_or_hides_a_profile_the_user_made() {
    let temp = tempfile::tempdir().unwrap();
    // A project in a subfolder of a repository, so every place has a folder.
    let app = temp.path().join("work").join("app");
    git_init(&temp.path().join("work"));
    std::fs::create_dir_all(&app).unwrap();
    let roots = Roots::new(temp.path().join("arko"))
        .workspace(&app)
        .session(temp.path().join("session"));
    let mut mine = Profile::new("reviewer", "The user's reviewer");
    mine.model = Some("opus".into());
    save(&roots, Place::User, &mine, Author::User).unwrap();

    let theirs = Profile::new("reviewer", "The agent's reviewer");
    for place in Place::ALL {
        let refused = save(&roots, place, &theirs, Author::Agent).unwrap_err();
        assert_eq!(
            refused, "There is already a profile `reviewer` made by the user. Choose another name.",
            "{place:?}"
        );
    }
    // The same for a profile the user made at the repository's root.
    write(
        &roots,
        Place::Repository,
        "judge",
        "---\ndescription: Judges\n---\n",
    );
    for place in Place::ALL {
        let judge = Profile::new("judge", "The agent's judge");
        let refused = save(&roots, place, &judge, Author::Agent).unwrap_err();
        assert!(refused.contains("made by the user"), "{place:?} {refused}");
    }
    assert_eq!(
        read(&roots, "reviewer").unwrap().profile.description,
        "The user's reviewer"
    );

    // Its own profiles it may write again, and they carry the mark.
    let own = Profile::new("scratch", "For one job");
    let first = save(&roots, Place::Session, &own, Author::Agent).unwrap();
    assert!(!first.replaced);
    assert!(read(&roots, "scratch").unwrap().profile.made_by_agent());
    let mut better = own.clone();
    better.description = "For two jobs".into();
    assert!(
        save(&roots, Place::Session, &better, Author::Agent)
            .unwrap()
            .replaced
    );
    assert_eq!(
        read(&roots, "scratch").unwrap().profile.description,
        "For two jobs"
    );
    // Once the user edits it, it is the user's.
    let edited = read(&roots, "scratch").unwrap().profile;
    save(&roots, Place::Session, &edited, Author::User).unwrap();
    assert!(save(&roots, Place::Session, &better, Author::Agent).is_err());

    // A file it can't read is not replaced either.
    write(&roots, Place::Project, "broken", "nonsense\n");
    let broken = Profile::new("broken", "Fixes it");
    let refused = save(&roots, Place::Session, &broken, Author::Agent).unwrap_err();
    assert!(refused.contains("can't be read"), "{refused}");
}

#[test]
fn deleting_removes_the_file_of_that_place_only() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let profile = Profile::new("judge", "Judges");
    save(&roots, Place::User, &profile, Author::User).unwrap();
    save(&roots, Place::Project, &profile, Author::User).unwrap();
    let removed = delete(&roots, Place::Project, "judge").unwrap();
    assert!(!removed.exists());
    assert_eq!(read(&roots, "judge").unwrap().place, Place::User);
    assert_eq!(
        delete(&roots, Place::Project, "judge").unwrap_err(),
        "There is no profile `judge` for this project."
    );
    assert!(delete(&roots, Place::User, "../judge").is_err());
    delete(&roots, Place::User, "judge").unwrap();
    assert!(list(&roots).active.is_empty());
}

#[test]
fn two_writers_leave_a_whole_file() {
    let temp = tempfile::tempdir().unwrap();
    let roots = std::sync::Arc::new(roots(temp.path()));
    let long = "Instructions line.\n".repeat(150);
    let writers: Vec<_> = (0..2)
        .map(|writer| {
            let roots = roots.clone();
            let long = long.clone();
            std::thread::spawn(move || {
                for round in 0..40 {
                    let mut profile =
                        Profile::new("shared", format!("Writer {writer} round {round}"));
                    profile.instructions = long.clone();
                    save(&roots, Place::User, &profile, Author::User).unwrap();
                }
            })
        })
        .collect();
    let reader = {
        let roots = roots.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                match read(&roots, "shared") {
                    Ok(found) => assert!(found.profile.instructions.len() > 2_000),
                    Err(ReadError::Missing) => {}
                    Err(other) => panic!("a reader found half a file: {other}"),
                }
            }
        })
    };
    for writer in writers {
        writer.join().unwrap();
    }
    reader.join().unwrap();
    let folder = roots.folder(Place::User).unwrap();
    let left: Vec<_> = std::fs::read_dir(folder).unwrap().collect();
    assert_eq!(left.len(), 1, "no temporary file stays behind");
}

#[test]
fn the_approval_shows_the_whole_profile() {
    let card = Approval::Profile(approval(&full(), Place::User));
    assert_eq!(
        card.details()
            .into_iter()
            .map(|detail| (detail.look, detail.text))
            .collect::<Vec<_>>(),
        [
            (Look::Text, "Runs on sol at high effort.".to_string()),
            (
                Look::Text,
                "Judges 3D results against a reference image".to_string()
            ),
            (Look::Text, "Saved in Arko's profiles folder.".to_string()),
            (Look::Heading, "Instructions".to_string()),
            (
                Look::Box,
                "You judge 3D models.\n\nCompare the render with the reference image.".to_string()
            ),
        ]
    );
    let plain = Approval::Profile(approval(&Profile::new("quick", "Reads"), Place::User));
    assert!(
        plain
            .details()
            .iter()
            .any(|detail| detail.text == "It has no instructions.")
    );
}

#[test]
fn an_approval_card_has_the_profiles_parts() {
    let profile = full();
    let card = approval(&profile, Place::Project);
    assert_eq!(card.name, "3d-judge");
    assert_eq!(card.model, "sol");
    assert_eq!(card.effort.as_deref(), Some("high"));
    assert_eq!(card.scope, "this project");
    assert_eq!(
        card.description,
        "Judges 3D results against a reference image"
    );
    assert!(card.instructions.starts_with("You judge 3D models."));
    let plain = approval(&Profile::new("quick", "Reads"), Place::User);
    assert_eq!(plain.model, "the session's worker model");
    assert_eq!(plain.effort, None);
    assert_eq!(plain.scope, "every project");
}

#[test]
fn places_are_named_for_the_commands() {
    for place in Place::ALL
        .into_iter()
        .filter(|place| *place != Place::Repository)
    {
        assert_eq!(Place::parse(place.name()), Some(place));
        assert_eq!(Place::parse(&place.name().to_uppercase()), Some(place));
    }
    assert_eq!(Place::parse("everywhere"), None);
    // The repository's folder is read and not saved to.
    assert_eq!(Place::parse("repository"), None);
    assert!(
        Place::Session < Place::Project
            && Place::Project < Place::Repository
            && Place::Repository < Place::User
    );
    assert_eq!(Place::User.reach(), "every project");
}

/// A folder reached by two places (a link) is read once, in the nearest.
#[cfg(unix)]
#[test]
fn the_same_folder_reached_twice_is_read_once() {
    let temp = tempfile::tempdir().unwrap();
    // The project's .agents/profiles is also Arko's profiles folder.
    let arko = temp.path().join("arko");
    let roots = Roots::new(&arko).workspace(&arko);
    std::fs::create_dir_all(arko.join(".agents")).unwrap();
    std::fs::create_dir_all(arko.join("profiles")).unwrap();
    std::os::unix::fs::symlink(arko.join("profiles"), arko.join(".agents").join("profiles"))
        .unwrap();
    std::fs::write(
        arko.join("profiles").join("one.md"),
        "---\ndescription: One\n---\n",
    )
    .unwrap();
    let profiles = list(&roots);
    assert_eq!(profiles.active.len(), 1);
    assert!(profiles.hidden.is_empty(), "{:?}", profiles.hidden);
}

/// A profile can't hold what would make the question that asks the user to
/// approve it show something else: control and escape characters, a
/// direction override, a zero-width character. A file with one is listed as
/// unreadable.
#[test]
fn text_that_could_disguise_the_question_is_refused() {
    let with = |text: &str| {
        let mut profile = Profile::new("judge", "Judges");
        profile.instructions = text.into();
        profile
    };
    for bad in [
        "line\u{1b}[2K",
        "up\u{1b}[3A\u{1b}[J",
        "over\rwritten",
        "right\u{202e}to left",
        "zero\u{200b}width",
        "bom\u{feff}inside",
        "bell\u{7}",
    ] {
        assert_eq!(
            with(bad).check().unwrap_err(),
            "The instructions can't have a control or invisible character."
        );
        assert_eq!(
            Profile::parse("judge", &format!("---\ndescription: A\n---\n{bad}\n")).unwrap_err(),
            "the instructions can't have a control or invisible character"
        );
    }
    // Line breaks and tabs in the instructions are fine.
    assert!(with("one\n\ttwo\n").check().is_ok());
    let mut profile = Profile::new("judge", "Judges\u{1b}[2K");
    assert!(
        profile
            .check()
            .unwrap_err()
            .starts_with("The description can't have")
    );
    profile.description = "Judges".into();
    profile.model = Some("sol\u{200b}".into());
    assert!(
        profile
            .check()
            .unwrap_err()
            .starts_with("The model can't have")
    );
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write(
        &roots,
        Place::User,
        "bad",
        "---\ndescription: A\u{1b}[2K\n---\n",
    );
    let listed = list(&roots);
    assert!(listed.active.is_empty());
    assert!(
        listed.unreadable[0]
            .reason
            .contains("description can't have a control")
    );
}

/// A model is up to 100 characters, so a slug can't make a file nobody can
/// read, and the reasons a file gives for being unreadable cut what it wrote.
#[test]
fn a_long_model_or_value_is_cut_short() {
    let mut profile = Profile::new("judge", "Judges");
    profile.model = Some(format!("openrouter:{}", "a".repeat(200)));
    assert!(
        profile
            .check()
            .unwrap_err()
            .starts_with("The model is 211 characters")
    );
    profile.model = Some(format!("openrouter:{}", "a".repeat(88)));
    assert!(profile.check().is_ok(), "100 characters are fine");
    let long = format!(
        "---\ndescription: A\nmodel: openrouter:{}\n---\n",
        "a".repeat(200)
    );
    assert_eq!(
        Profile::parse("judge", &long).unwrap_err(),
        "the model is over 100 characters"
    );
    let effort = format!("---\ndescription: A\neffort: {}\n---\n", "e".repeat(12_000));
    let why = Profile::parse("judge", &effort).unwrap_err();
    assert!(why.len() < 120, "{why}");
    assert!(why.starts_with("`effort: eeee") && why.contains("…` isn't an effort"));
    let cache = format!("---\ndescription: A\ncache: {}\n---\n", "c".repeat(12_000));
    assert!(Profile::parse("judge", &cache).unwrap_err().len() < 120);
}

/// The names Windows keeps for devices can't be profile names, since a
/// file by one can't be made there and profiles travel through git. A file
/// by one that is there is listed as unreadable and can still be deleted.
#[test]
fn windows_device_names_are_not_profile_names() {
    for device in ["con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9"] {
        let why = check_name(device).unwrap_err();
        assert!(
            why.contains("a name Windows keeps for a device"),
            "{device}: {why}"
        );
    }
    assert!(check_name("com10").is_ok() && check_name("console").is_ok());
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write(&roots, Place::User, "nul", "---\ndescription: A\n---\n");
    let listed = list(&roots);
    assert_eq!(
        listed.unreadable[0].reason,
        "its name, `nul`, is one Windows keeps for a device"
    );
    assert!(save(&roots, Place::User, &Profile::new("nul", "A"), Author::User).is_err());
    delete(&roots, Place::User, "nul").unwrap();
}

/// A value in quotes that the quotes don't enclose whole is read as written,
/// and a delete refuses a name that is a path.
#[test]
fn quotes_that_are_not_a_pair_stay_and_a_delete_takes_no_path() {
    let text = "---\ndescription: \"a\" and \"b\"\n---\n";
    assert_eq!(
        Profile::parse("judge", text).unwrap().description,
        "\"a\" and \"b\""
    );
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    for bad in ["../x", "a/b", "..", "A", ""] {
        assert!(delete(&roots, Place::User, bad).is_err(), "{bad}");
    }
}

#[test]
fn a_cache_that_is_not_read_says_what_it_takes() {
    assert_eq!(" 5M ".parse::<Cache>(), Ok(Cache::FiveMinutes));
    assert_eq!("1h".parse::<Cache>(), Ok(Cache::OneHour));
    assert_eq!("long".parse::<Cache>(), Ok(Cache::OneHour));
    assert_eq!("Short".parse::<Cache>(), Ok(Cache::FiveMinutes));
    // A profile file keeps the names earlier versions read.
    assert_eq!(Cache::OneHour.as_str(), "1h");
    let why = "2h".parse::<Cache>().unwrap_err();
    assert!(why.contains("long or short"), "{why}");
}

#[test]
fn a_branch_gets_the_profiles_of_its_session_and_nothing_else() {
    let temp = tempfile::tempdir().unwrap();
    let (from, to) = (temp.path().join("from"), temp.path().join("to"));
    // A session without profiles leaves the branch without a folder.
    std::fs::create_dir_all(&from).unwrap();
    std::fs::create_dir_all(&to).unwrap();
    copy_session_profiles(&from, &to).unwrap();
    assert!(!to.join("profiles").exists());
    let folder = from.join("profiles");
    std::fs::create_dir_all(folder.join("sub.md")).unwrap();
    std::fs::write(folder.join("reviewer.md"), "---\ndescription: R\n---\n").unwrap();
    std::fs::write(folder.join("notes.txt"), "not a profile").unwrap();
    copy_session_profiles(&from, &to).unwrap();
    let mut copied: Vec<_> = std::fs::read_dir(to.join("profiles"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    copied.sort();
    assert_eq!(copied, ["reviewer.md"]);
}

/// An earlier file can say `isolated`, which a profile no longer sets. It
/// reads as a key this version doesn't know and survives a rewrite.
#[test]
fn an_isolated_key_from_an_earlier_file_is_kept_and_means_nothing() {
    let file = "---\ndescription: Works apart\nmodel: sol\nisolated: true\n---\nWork.\n";
    let profile = Profile::parse("tree", file).unwrap();
    assert_eq!(
        profile.extra,
        [("isolated".to_string(), "true".to_string())]
    );
    let card = Approval::Profile(approval(&profile, Place::User));
    assert!(
        card.details()
            .iter()
            .all(|detail| !detail.text.contains("worktree"))
    );
    assert!(profile.render().contains("isolated: true"));
    let again = Profile::parse("tree", &profile.render()).unwrap();
    assert_eq!(again, profile);
}

/// What a file makes a reason or a list say can't move a terminal's cursor
/// or be as long as the file likes: a value or a name with a control
/// character is shown with a replacement mark.
#[test]
fn a_reason_or_a_name_from_a_file_has_no_control_characters() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write(
        &roots,
        Place::Project,
        "bad-effort",
        "---\ndescription: A\neffort: \u{1b}[2Kfake\n---\n",
    );
    let folder = roots.folder(Place::Project).unwrap();
    std::fs::write(folder.join("odd\u{1b}[1Aname.md"), "no front matter").unwrap();
    let listed = list(&roots);
    assert_eq!(listed.unreadable.len(), 2);
    for problem in &listed.unreadable {
        let shown = format!("{} {}", problem.name, problem.reason);
        assert!(
            !shown.chars().any(|c| c.is_control()),
            "{shown:?} has a control character"
        );
    }
    let effort = listed
        .unreadable
        .iter()
        .find(|problem| problem.name == "bad-effort")
        .unwrap();
    assert!(
        effort.reason.contains("\u{fffd}[2Kfake"),
        "{}",
        effort.reason
    );
}

/// A name finds a file by its exact name, so a file system that ignores case
/// can't find `Reviewer.md` for `reviewer`, which the list calls unreadable.
#[test]
fn a_lookup_by_name_matches_the_files_exact_name() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("Reviewer.md"), "x").unwrap();
    std::fs::write(temp.path().join("judge.md"), "x").unwrap();
    assert!(has_entry(temp.path(), "judge.md"));
    assert!(has_entry(temp.path(), "Reviewer.md"));
    assert!(!has_entry(temp.path(), "reviewer.md"));
    assert!(!has_entry(&temp.path().join("nowhere"), "judge.md"));
}

/// Of an agent's first save and a user's file written in between, the
/// user's stays.
#[test]
fn an_agents_first_write_never_replaces_a_file_that_appeared_meanwhile() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let folder = roots.folder(Place::User).unwrap();
    std::fs::create_dir_all(&folder).unwrap();
    // The file is there when the write comes, as if made after the check.
    let file = folder.join("judge.md");
    std::fs::write(&file, "---\ndescription: The user's own\n---\n").unwrap();
    let error = crate::session::write_new(&file, b"agent").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    // Saved through the library, the agent is refused outright.
    let mut profile = Profile::new("judge", "Judges");
    profile.model = Some("sol".into());
    let why = save(&roots, Place::User, &profile, Author::Agent).unwrap_err();
    assert!(why.contains("made by the user"), "{why}");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("The user's own")
    );
}

/// Makes `dir` a git repository.
fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let done = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(done.success());
}

fn judge() -> Profile {
    Profile::new("judge", "Judges renders")
}

/// A save that makes the project's `.arko` folder gets its `.gitignore`,
/// which ignores the folder and the file itself. A folder that was there, and
/// one whose file the user deleted, don't get one, and the setting turns it
/// off.
#[test]
fn a_project_folder_a_save_makes_gets_a_gitignore_once() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let folder = roots.folder(Place::Project).unwrap();
    git_init(&temp.path().join("work"));
    save(&roots, Place::Project, &judge(), Author::Agent).unwrap();
    let ignore = temp.path().join("work").join(".arko").join(".gitignore");
    assert_eq!(
        std::fs::read_to_string(&ignore).unwrap(),
        crate::arko_dir::IGNORE_TEXT
    );
    assert!(!folder.join(".gitignore").exists(), "none in profiles");
    // Git sees neither the profile nor the file.
    let status = crate::sessions::git(
        &temp.path().join("work"),
        &["status", "--porcelain", "--untracked-files=all"],
    );
    assert_eq!(status.as_deref(), Some(""));
    // The user deletes the file, and the next save doesn't write it again.
    std::fs::remove_file(&ignore).unwrap();
    save(
        &roots,
        Place::Project,
        &Profile::new("other", "Another"),
        Author::User,
    )
    .unwrap();
    assert!(!ignore.exists());
    // The other places get none.
    save(&roots, Place::User, &judge(), Author::User).unwrap();
    assert!(
        !roots
            .folder(Place::User)
            .unwrap()
            .join(".gitignore")
            .exists()
    );
    // With the setting off a new folder doesn't get one either.
    let off = Preferences {
        keep_out_of_git: false,
        ..Preferences::default()
    };
    off.save(&temp.path().join("arko")).unwrap();
    std::fs::remove_dir_all(temp.path().join("work").join(".arko")).unwrap();
    save(&roots, Place::Project, &judge(), Author::User).unwrap();
    assert!(!ignore.exists());
    assert_eq!(Preferences::load(&temp.path().join("arko")), off);
}

/// An agent saves in a project folder that git ignores without asking, and
/// asks for one git tracks. Outside a git repository the user is asked, as
/// for the user's own folder.
#[test]
fn a_project_folder_git_ignores_needs_no_approval() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    assert!(needs_approval(&roots, Place::Project, "judge"), "no repo");
    assert!(needs_approval(&roots, Place::User, "judge"));
    assert!(!needs_approval(&roots, Place::Session, "judge"));

    // In a repository, the folder a save is about to make will ignore itself.
    git_init(&work);
    assert!(
        !needs_approval(&roots, Place::Project, "judge"),
        "to be made"
    );
    save(&roots, Place::Project, &judge(), Author::Agent).unwrap();
    assert!(!needs_approval(&roots, Place::Project, "judge"), "ignored");
    assert!(
        !needs_approval(&roots, Place::Project, "another"),
        "ignored"
    );

    // Without the file the folder is git's to track.
    let arko = work.join(".arko");
    std::fs::remove_file(arko.join(".gitignore")).unwrap();
    assert!(needs_approval(&roots, Place::Project, "judge"), "tracked");

    // With the setting off a folder to be made is tracked too.
    std::fs::remove_dir_all(&arko).unwrap();
    assert!(!needs_approval(&roots, Place::Project, "judge"));
    Preferences {
        keep_out_of_git: false,
        ..Preferences::default()
    }
    .save(&temp.path().join("arko"))
    .unwrap();
    assert!(
        needs_approval(&roots, Place::Project, "judge"),
        "setting off"
    );
}

/// A `.arko` that links to another place is not the project's folder, so the
/// save asks even when that place is a git work tree. A link that stays in
/// the project asks too, since the folder wasn't made by a save and has no
/// `.gitignore` of Arko's.
#[cfg(unix)]
#[test]
fn a_linked_arko_folder_still_asks() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let work = temp.path().join("work");
    let other = temp.path().join("other");
    git_init(&work);
    git_init(&other);
    let arko = work.join(".arko");
    std::fs::create_dir(work.join("inner")).unwrap();
    std::os::unix::fs::symlink(work.join("inner"), &arko).unwrap();
    assert!(needs_approval(&roots, Place::Project, "judge"), "inside");
    std::fs::remove_file(&arko).unwrap();
    std::os::unix::fs::symlink(&other, &arko).unwrap();
    assert!(needs_approval(&roots, Place::Project, "judge"), "outside");
    // A link in the older folder's place changes nothing for a save, which
    // writes `.arko` and is judged by it.
    std::fs::remove_file(&arko).unwrap();
    std::os::unix::fs::symlink(&other, work.join(".agents")).unwrap();
    assert!(!needs_approval(&roots, Place::Project, "judge"), "older");
}

/// In a subfolder of a git repository the repository root's profiles are
/// read too, behind the project's and ahead of the user's, unless the
/// setting is off.
#[test]
fn a_subfolder_reads_the_repository_roots_profiles_unless_turned_off() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().join("repo");
    let sub = top.join("packages").join("app");
    std::fs::create_dir_all(&sub).unwrap();
    git_init(&top);
    let arko = temp.path().join("arko");
    let roots = Roots::new(&arko).workspace(&sub);
    assert_eq!(
        roots.folder(Place::Repository),
        Some(top.join(".arko").join("profiles"))
    );
    let make = |place, description: &str| {
        write(
            &roots,
            place,
            "reviewer",
            &format!("---\ndescription: {description}\n---\n"),
        );
    };
    make(Place::User, "Yours");
    let found = read(&roots, "reviewer").unwrap();
    assert_eq!(
        (found.place, found.profile.description.as_str()),
        (Place::User, "Yours")
    );
    make(Place::Repository, "The repository's");
    let found = read(&roots, "reviewer").unwrap();
    assert_eq!(found.place, Place::Repository);
    assert_eq!(list(&roots).hidden[0].place, Place::User);
    make(Place::Project, "The project's");
    let listed = list(&roots);
    assert_eq!(listed.active[0].place, Place::Project);
    assert_eq!(
        listed
            .hidden
            .iter()
            .map(|found| found.place)
            .collect::<Vec<_>>(),
        [Place::Repository, Place::User]
    );
    // The repository's folder is the project's at the root, and there is none
    // outside a repository.
    assert_eq!(
        Roots::new(&arko).workspace(&top).folder(Place::Repository),
        None
    );
    assert_eq!(
        Roots::new(&arko)
            .workspace(temp.path())
            .folder(Place::Repository),
        None
    );

    // Turned off, the repository root's profiles are not read.
    Preferences {
        repo_root: false,
        ..Preferences::default()
    }
    .save(&arko)
    .unwrap();
    assert_eq!(roots.folder(Place::Repository), None);
    std::fs::remove_file(roots.folder(Place::Project).unwrap().join("reviewer.md")).unwrap();
    assert_eq!(read(&roots, "reviewer").unwrap().place, Place::User);
}

fn described(description: &str) -> String {
    format!("---\ndescription: {description}\n---\nBody.\n")
}

/// A profile in the older `.agents/profiles` is still read in the project and
/// at the repository root, behind the `.arko/profiles` of the same place,
/// which wins a name both have. Reading moves nothing.
#[test]
fn the_older_folder_is_still_read_and_arko_profiles_win() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let old = write_older(&roots, Place::Project, "kept", &described("In the older"));
    write_older(&roots, Place::Project, "both", &described("Older both"));
    write(&roots, Place::Project, "both", &described("Newer both"));
    // A file in `.arko/profiles` that can't be read hides no other.
    write_older(&roots, Place::Project, "broken", &described("Older broken"));
    write(&roots, Place::Project, "broken", "no front matter\n");

    let listed = list(&roots);
    let active: Vec<_> = listed
        .active
        .iter()
        .map(|found| (found.profile.name.as_str(), found.in_older_folder()))
        .collect();
    assert_eq!(
        active,
        [("both", false), ("broken", true), ("kept", true)],
        "{listed:?}"
    );
    assert_eq!(
        listed.get("both").unwrap().profile.description,
        "Newer both"
    );
    // The one it hides is listed, with its folder.
    assert_eq!(listed.hidden.len(), 1);
    assert_eq!(listed.hidden[0].profile.description, "Older both");
    assert_eq!(listed.hidden[0].reach(), "this project (.agents/profiles)");
    assert_eq!(listed.unreadable.len(), 1);
    assert_eq!(listed.unreadable[0].reach(), "this project");

    let kept = read(&roots, "kept").unwrap();
    assert_eq!(
        (kept.place, kept.file.as_path()),
        (Place::Project, old.as_path())
    );
    assert_eq!(kept.reach(), "this project (.agents/profiles)");
    let both = read(&roots, "both").unwrap();
    assert_eq!(both.reach(), "this project");
    assert!(!both.in_older_folder());
    assert_eq!(
        read(&roots, "broken").unwrap().profile.description,
        "Older broken"
    );

    // A user's profile, in any place, is not in the older folder.
    write(&roots, Place::User, "mine", &described("Mine"));
    assert_eq!(read(&roots, "mine").unwrap().reach(), "every project");
    // Nothing was moved or made.
    assert!(old.is_file());
    assert!(
        !temp
            .path()
            .join("work")
            .join(".arko")
            .join(".gitignore")
            .exists()
    );
}

/// An unreadable file in the older folder says so in its place.
#[test]
fn an_unreadable_file_in_the_older_folder_names_it() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    write_older(&roots, Place::Project, "odd", "no front matter\n");
    let listed = list(&roots);
    assert_eq!(
        listed.unreadable[0].reach(),
        "this project (.agents/profiles)"
    );
}

/// The repository root's two folders are read like the project's.
#[test]
fn both_folders_are_read_at_the_repository_root() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().join("repo");
    let sub = top.join("packages").join("app");
    std::fs::create_dir_all(&sub).unwrap();
    git_init(&top);
    let roots = Roots::new(temp.path().join("arko")).workspace(&sub);
    write_older(
        &roots,
        Place::Repository,
        "shared",
        &described("Shared, older"),
    );
    write_older(
        &roots,
        Place::Repository,
        "twice",
        &described("Twice, older"),
    );
    write(
        &roots,
        Place::Repository,
        "twice",
        &described("Twice, newer"),
    );
    assert_eq!(
        folders(&roots)
            .iter()
            .map(|(place, folder)| (*place, folder.clone()))
            .collect::<Vec<_>>(),
        [
            (Place::Project, sub.join(".arko").join("profiles")),
            (Place::Project, sub.join(".agents").join("profiles")),
            (Place::Repository, top.join(".arko").join("profiles")),
            (Place::Repository, top.join(".agents").join("profiles")),
            (Place::User, temp.path().join("arko").join("profiles")),
        ]
    );
    let shared = read(&roots, "shared").unwrap();
    assert_eq!(shared.place, Place::Repository);
    assert_eq!(shared.reach(), "this repository (.agents/profiles)");
    assert_eq!(
        read(&roots, "twice").unwrap().profile.description,
        "Twice, newer"
    );
    // The project's own `.arko/profiles` is still nearer than the root's.
    write(
        &roots,
        Place::Project,
        "twice",
        &described("Twice, the project's"),
    );
    assert_eq!(read(&roots, "twice").unwrap().place, Place::Project);
}

/// A save writes `.arko/profiles` and nothing under `.agents`, for the
/// project and for the repository root.
#[test]
fn a_save_writes_arko_profiles_only() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().join("repo");
    let sub = top.join("packages").join("app");
    std::fs::create_dir_all(&sub).unwrap();
    git_init(&top);
    let roots = Roots::new(temp.path().join("arko")).workspace(&sub);
    for (place, base) in [(Place::Project, &sub), (Place::Repository, &top)] {
        let saved = save(&roots, place, &judge(), Author::Agent).unwrap();
        assert_eq!(
            saved.file,
            base.join(".arko").join("profiles").join("judge.md")
        );
        assert!(!saved.replaced);
        assert!(saved.file.is_file());
        assert!(!base.join(".agents").exists(), "{place:?}");
        assert!(base.join(".arko").join(".gitignore").is_file(), "{place:?}");
    }
    // A user's save the same.
    let saved = save(
        &roots,
        Place::Project,
        &Profile::new("two", "Two"),
        Author::User,
    )
    .unwrap();
    assert!(saved.file.starts_with(sub.join(".arko")));
    assert!(!sub.join(".agents").exists());
}

/// Replacing a profile that lives in the older folder of the same place
/// writes the new file in `.arko/profiles` and deletes the old, so the name
/// isn't listed twice. The older folder of another place stays.
#[test]
fn replacing_a_profile_from_the_older_folder_deletes_the_old_file() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().join("repo");
    let sub = top.join("packages").join("app");
    std::fs::create_dir_all(&sub).unwrap();
    git_init(&top);
    let roots = Roots::new(temp.path().join("arko")).workspace(&sub);
    let old = write_older(&roots, Place::Project, "judge", &described("Older judge"));
    let other = write_older(&roots, Place::Project, "other", &described("Another"));
    let at_root = write_older(
        &roots,
        Place::Repository,
        "judge",
        &described("Root's judge"),
    );

    let mut profile = judge();
    profile.model = Some("sol".into());
    let saved = save(&roots, Place::Project, &profile, Author::User).unwrap();
    assert!(saved.replaced);
    assert_eq!(
        saved.file,
        sub.join(".arko").join("profiles").join("judge.md")
    );
    assert!(!old.exists(), "the old file is deleted");
    assert!(other.is_file() && at_root.is_file(), "the others stay");
    let listed = list(&roots);
    let judges: Vec<_> = listed
        .active
        .iter()
        .chain(&listed.hidden)
        .filter(|found| found.profile.name == "judge")
        .map(|found| (found.place, found.in_older_folder()))
        .collect();
    assert_eq!(
        judges,
        [(Place::Project, false), (Place::Repository, true)],
        "{listed:?}"
    );
    // A second save replaces the new file, with nothing to delete.
    let again = save(&roots, Place::Project, &profile, Author::User).unwrap();
    assert!(again.replaced);

    // The same goes for the repository root.
    save(&roots, Place::Repository, &judge(), Author::User).unwrap();
    assert!(!at_root.exists());
    assert!(
        top.join(".arko")
            .join("profiles")
            .join("judge.md")
            .is_file()
    );
}

/// An agent replaces a profile it made in the older folder, and the old file
/// goes. It can't replace the user's, in either folder, and the user's file
/// stays.
#[test]
fn an_agent_cant_replace_a_users_profile_in_either_folder() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let theirs = write_older(&roots, Place::Project, "judge", &described("The user's"));
    let why = save(&roots, Place::Project, &judge(), Author::Agent).unwrap_err();
    assert!(why.contains("made by the user"), "{why}");
    assert!(check_agent_save(&roots, "judge").is_err());
    assert!(theirs.is_file());
    assert!(!roots.folder(Place::Project).unwrap().exists());
    std::fs::remove_file(&theirs).unwrap();

    let theirs = write(
        &roots,
        Place::Project,
        "judge",
        &described("The user's, new"),
    );
    let why = save(&roots, Place::Project, &judge(), Author::Agent).unwrap_err();
    assert!(why.contains("made by the user"), "{why}");
    assert!(
        std::fs::read_to_string(&theirs)
            .unwrap()
            .contains("The user's, new")
    );
    std::fs::remove_file(&theirs).unwrap();

    // A file that can't be read in the older folder isn't replaced either.
    let broken = write_older(&roots, Place::Project, "judge", "no front matter\n");
    let why = save(&roots, Place::Project, &judge(), Author::Agent).unwrap_err();
    assert!(why.contains("can't be read"), "{why}");
    assert!(broken.is_file());
    std::fs::remove_file(&broken).unwrap();

    // The agent's own, in the older folder, is replaced and moves.
    let mine = write_older(
        &roots,
        Place::Project,
        "judge",
        "---\ndescription: The agent's\nmade_by: agent\n---\n",
    );
    let saved = save(&roots, Place::Project, &judge(), Author::Agent).unwrap();
    assert!(saved.replaced);
    assert!(!mine.exists());
    assert!(
        saved
            .file
            .starts_with(roots.folder(Place::Project).unwrap())
    );
    assert!(read(&roots, "judge").unwrap().profile.made_by_agent());
}

/// When a link makes `.agents/profiles` the same folder as `.arko/profiles`,
/// a save keeps its file.
#[cfg(unix)]
#[test]
fn a_link_between_the_two_folders_keeps_the_saved_file() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let work = temp.path().join("work");
    let new = roots.folder(Place::Project).unwrap();
    std::fs::create_dir_all(&new).unwrap();
    std::fs::create_dir_all(work.join(".agents")).unwrap();
    std::os::unix::fs::symlink(&new, work.join(".agents").join("profiles")).unwrap();
    write(&roots, Place::Project, "judge", &described("Judges"));
    let saved = save(&roots, Place::Project, &judge(), Author::User).unwrap();
    assert!(
        saved.file.is_file(),
        "the link didn't take the file with it"
    );
    assert_eq!(list(&roots).active.len(), 1);
}

/// An agent saves for the project without asking while `.arko` is ignored,
/// also when the older folder has tracked profiles, and is asked with the
/// setting off inside a git repository.
#[test]
fn the_approval_follows_arko_and_not_the_older_folder() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let work = temp.path().join("work");
    git_init(&work);
    write_older(&roots, Place::Project, "shared", &described("The team's"));
    assert!(!needs_approval(&roots, Place::Project, "judge"));
    save(&roots, Place::Project, &judge(), Author::Agent).unwrap();
    assert!(!needs_approval(&roots, Place::Project, "judge"));
    assert!(needs_approval(&roots, Place::Repository, "judge"));
    Preferences {
        keep_out_of_git: false,
        ..Preferences::default()
    }
    .save(&temp.path().join("arko"))
    .unwrap();
    // A save takes Arko's own file out first, so it asks before the switch
    // reaches this project, and after.
    assert!(needs_approval(&roots, Place::Project, "judge"));
    crate::arko_dir::apply_setting(&work, false).unwrap();
    assert!(needs_approval(&roots, Place::Project, "judge"));
    // A file the user edited stays, and git's answer about it stands.
    std::fs::write(work.join(".arko").join(".gitignore"), "# Mine\n*\n").unwrap();
    assert!(!needs_approval(&roots, Place::Project, "judge"));
}

/// A delete takes the file `read` finds, the newer one first, and one in the
/// older folder when that is all there is.
#[test]
fn a_delete_reaches_the_older_folder() {
    let temp = tempfile::tempdir().unwrap();
    let roots = roots(temp.path());
    let old = write_older(&roots, Place::Project, "judge", &described("Older"));
    let new = write(&roots, Place::Project, "judge", &described("Newer"));
    assert_eq!(delete(&roots, Place::Project, "judge").unwrap(), new);
    assert_eq!(read(&roots, "judge").unwrap().file, old);
    assert_eq!(delete(&roots, Place::Project, "judge").unwrap(), old);
    assert!(
        delete(&roots, Place::Project, "judge")
            .unwrap_err()
            .contains("There is no profile `judge`")
    );
    assert!(delete(&Roots::new(temp.path()), Place::Project, "judge").is_err());
}

/// The approval names the folder a save writes, so the terminal, the page
/// and an app show it.
#[test]
fn the_approval_names_the_folder_it_saves_in() {
    for (place, line) in [
        (
            Place::Project,
            "Saved in this project's .arko/profiles folder.",
        ),
        (
            Place::Repository,
            "Saved in the repository's .arko/profiles folder.",
        ),
        (Place::User, "Saved in Arko's profiles folder."),
        (Place::Session, "Saved in this session's folder."),
    ] {
        let details = Approval::Profile(approval(&judge(), place)).details();
        assert!(
            details.iter().any(|detail| detail.text == line),
            "{place:?}: {details:?}"
        );
    }
}
