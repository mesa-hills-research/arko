use super::*;

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

fn full() -> Profile {
    let mut profile = Profile::new("3d-judge", "Judges 3D results against a reference image");
    profile.model = Some("sol".into());
    profile.effort = Some(Effort::High);
    profile.fast = true;
    profile.cache = Some(Cache::OneHour);
    profile.nested = true;
    profile.isolated = true;
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
    assert_eq!(
        folders(&roots)
            .iter()
            .map(|(place, _)| *place)
            .collect::<Vec<_>>(),
        [Place::Session, Place::Project, Place::User]
    );
    assert!(
        folders(&roots)[1]
            .1
            .ends_with(Path::new(".agents").join("profiles"))
    );
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
    assert!(note("future").unwrap().contains("isn't a model Arko knows"));
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
    let roots = roots(temp.path());
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
fn the_approval_question_shows_the_whole_profile() {
    let mut profile = full();
    profile.nested = false;
    profile.fast = false;
    profile.cache = None;
    profile.isolated = false;
    let question = approval_text(&profile, Place::User);
    assert_eq!(
        question,
        "The main agent wants to save the worker profile 3d-judge in your Arko folder, for every project.\n\nJudges 3D results against a reference image\nRuns on sol at high effort.\nIts instructions are 66 characters (3 lines).\n\nInstructions\nYou judge 3D models.\n\nCompare the render with the reference image.\n\nReply yes to save it, or tell the agent what to change."
    );
    let project = approval_text(&profile, Place::Project);
    assert!(
        project.contains("in this project's .agents/profiles folder, for everyone who works on it")
    );
    let mut plain = Profile::new("quick", "Reads");
    plain.nested = true;
    plain.isolated = true;
    plain.fast = true;
    plain.cache = Some(Cache::FiveMinutes);
    let text = approval_text(&plain, Place::User);
    assert!(
        text.contains("Runs on the session's worker model in fast mode with a 5m prompt cache."),
        "{text}"
    );
    assert!(text.contains("Its workers work in a worktree of their own."));
    assert!(text.contains("Its workers can start workers of their own."));
    assert!(text.contains("It has no standing instructions."));
}

#[test]
fn only_a_plain_yes_approves() {
    for yes in [
        "yes", "Yes", "YES.", "y", "ok", "OK!", " okay ", "allow", "yes\n",
    ] {
        assert!(is_approval(yes), "{yes:?}");
    }
    for no in [
        "",
        "no",
        "yes, but use high effort",
        "yes please change the name",
        "yes, please use high",
        "please",
        "thanks",
        "no thanks",
        "use high effort, not max",
        "maybe",
        "yep",
    ] {
        assert!(!is_approval(no), "{no:?}");
    }
}

#[test]
fn places_are_named_for_the_commands() {
    for place in Place::ALL {
        assert_eq!(Place::parse(place.name()), Some(place));
        assert_eq!(Place::parse(&place.name().to_uppercase()), Some(place));
    }
    assert_eq!(Place::parse("everywhere"), None);
    assert!(Place::Session < Place::Project && Place::Project < Place::User);
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
    let why = "2h".parse::<Cache>().unwrap_err();
    assert!(why.contains("5m or 1h"), "{why}");
}
