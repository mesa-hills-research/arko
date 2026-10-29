use super::*;

fn setting(root: &Path, on: bool) {
    Preferences {
        keep_out_of_git: on,
        ..Preferences::default()
    }
    .save(root)
    .unwrap();
}

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

fn ignore_of(workspace: &Path) -> PathBuf {
    workspace.join(".arko").join(".gitignore")
}

/// The folder a write makes gets the file, with the setting on (the
/// default), once. A user who deletes it to share the files isn't given it
/// again, and git sees nothing of the folder while it is there.
#[test]
fn a_folder_ensure_makes_gets_a_gitignore_once() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    git_init(&work);
    let dir = ensure(&work, &root).unwrap();
    assert_eq!(dir, work.join(".arko"));
    assert_eq!(
        std::fs::read_to_string(ignore_of(&work)).unwrap(),
        "# Arko keeps its project files out of git. Delete this file, or turn off\n# Keep agents' project files out of git in Arko's settings, to share them.\n*\n"
    );
    std::fs::create_dir(dir.join("skills")).unwrap();
    std::fs::write(dir.join("skills").join("note.md"), "x").unwrap();
    std::fs::write(dir.join("mcp.json"), "{}").unwrap();
    let status = crate::sessions::git(&work, &["status", "--porcelain", "--untracked-files=all"]);
    assert_eq!(status.as_deref(), Some(""));

    // Another call leaves it, and so does one after the user deleted it.
    ensure(&work, &root).unwrap();
    assert!(ignore_of(&work).is_file());
    std::fs::remove_file(ignore_of(&work)).unwrap();
    ensure(&work, &root).unwrap();
    assert!(!ignore_of(&work).exists());
}

/// A workspace that isn't there yet is made with the folder.
#[test]
fn ensure_makes_the_workspace_it_needs() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("not").join("yet");
    let dir = ensure(&work, &temp.path().join("arko")).unwrap();
    assert!(dir.is_dir() && ignore_of(&work).is_file());
}

/// With the setting off a folder that is made gets no file.
#[test]
fn nothing_is_written_with_the_setting_off() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    setting(&root, false);
    let dir = ensure(&work, &root).unwrap();
    assert!(dir.is_dir());
    assert!(!ignore_of(&work).exists());
}

/// Turning the setting off takes out Arko's own file and leaves one the user
/// edited. Turning it on writes the file into a folder that has none, and
/// leaves an edited one.
#[test]
fn the_switch_removes_arkos_own_file_only_and_writes_one_into_an_existing_folder() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    ensure(&work, &root).unwrap();
    apply_setting(&work, false).unwrap();
    assert!(!ignore_of(&work).exists(), "Arko's own file is removed");
    assert!(work.join(".arko").is_dir(), "the folder stays");
    // Off again changes nothing.
    apply_setting(&work, false).unwrap();

    apply_setting(&work, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(ignore_of(&work)).unwrap(),
        IGNORE_TEXT
    );
    // On again leaves it, whoever wrote it.
    std::fs::write(ignore_of(&work), "# Mine\nskills/\n").unwrap();
    apply_setting(&work, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(ignore_of(&work)).unwrap(),
        "# Mine\nskills/\n"
    );
    // An edited file stays when the switch goes off.
    apply_setting(&work, false).unwrap();
    assert_eq!(
        std::fs::read_to_string(ignore_of(&work)).unwrap(),
        "# Mine\nskills/\n"
    );
    // So does one that isn't text.
    std::fs::write(ignore_of(&work), [0xff, 0xfe, b'*']).unwrap();
    apply_setting(&work, false).unwrap();
    assert!(ignore_of(&work).is_file());
}

/// A project with no `.arko` is left alone by the switch, since the setting is
/// read when a write makes the folder.
#[test]
fn the_switch_makes_no_folder() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    apply_setting(&work, true).unwrap();
    apply_setting(&work, false).unwrap();
    assert!(!work.join(".arko").exists());
    apply_setting(&temp.path().join("nowhere"), true).unwrap();
}

/// A project other than the session's follows the switch the next time Arko
/// writes into its folder.
#[test]
fn another_project_follows_the_switch_at_its_next_write() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    ensure(&work, &root).unwrap();
    assert!(ignore_of(&work).is_file());
    setting(&root, false);
    ensure(&work, &root).unwrap();
    assert!(!ignore_of(&work).exists(), "Arko's own file goes");

    // A file the user edited stays.
    setting(&root, true);
    apply_setting(&work, true).unwrap();
    std::fs::write(ignore_of(&work), "# Mine\n*\n").unwrap();
    setting(&root, false);
    ensure(&work, &root).unwrap();
    assert!(ignore_of(&work).is_file());
}

/// The preferences' own save calls the switch when the choice changed, and
/// not when only the other choice did, so a file the user deleted isn't
/// written again.
#[test]
fn saving_the_preferences_in_a_project_applies_a_changed_switch() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    ensure(&work, &root).unwrap();
    let mut prefs = Preferences::load(&root);
    prefs.keep_out_of_git = false;
    prefs.save_in(&root, &work).unwrap();
    assert!(!ignore_of(&work).exists(), "removed by the change");
    assert!(!Preferences::load(&root).keep_out_of_git);
    prefs.keep_out_of_git = true;
    prefs.save_in(&root, &work).unwrap();
    assert!(ignore_of(&work).is_file(), "written by the change");

    // The user deletes it. Saving the other choice doesn't bring it back.
    std::fs::remove_file(ignore_of(&work)).unwrap();
    prefs.repo_root = false;
    prefs.save_in(&root, &work).unwrap();
    assert!(!ignore_of(&work).exists());
    // The plain save never touches a project.
    prefs.keep_out_of_git = false;
    prefs.save(&root).unwrap();
    prefs.keep_out_of_git = true;
    prefs.save(&root).unwrap();
    assert!(!ignore_of(&work).exists());
}

/// A file where the folder should be is said so.
#[test]
fn a_file_named_arko_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join(".arko"), "x").unwrap();
    let why = ensure(&work, &temp.path().join("arko")).unwrap_err();
    assert!(why.ends_with(".arko isn't a folder."), "{why}");
}

/// A `.arko` that is a link, or a folder in it that is one, is refused with
/// the reason, so no write (a skill, `mcp.json`, a profile or the
/// `.gitignore`) goes where it leads, and the switch leaves it alone.
#[cfg(unix)]
#[test]
fn a_link_at_arko_or_in_it_is_refused() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let arko = work.join(".arko");
    let refused = |path: &Path| {
        format!(
            "{} is a link, and Arko doesn't write through links.",
            path.display()
        )
    };
    symlink(&elsewhere, &arko).unwrap();
    assert_eq!(ensure(&work, &root).unwrap_err(), refused(&arko));
    assert_eq!(
        ensure_in(&work, &root, "profiles").unwrap_err(),
        refused(&arko)
    );
    assert_eq!(apply_setting(&work, true).unwrap_err(), refused(&arko));
    // Arko's own text where the link leads doesn't count as the project's.
    std::fs::write(elsewhere.join(IGNORE_FILE), IGNORE_TEXT).unwrap();
    assert!(!has_own_ignore(&work));
    assert_eq!(apply_setting(&work, false).unwrap_err(), refused(&arko));
    assert_eq!(
        std::fs::read_to_string(elsewhere.join(IGNORE_FILE)).unwrap(),
        IGNORE_TEXT
    );
    std::fs::remove_file(elsewhere.join(IGNORE_FILE)).unwrap();
    std::fs::remove_file(&arko).unwrap();

    std::fs::create_dir(&arko).unwrap();
    symlink(&elsewhere, arko.join("profiles")).unwrap();
    assert_eq!(
        ensure_in(&work, &root, "profiles").unwrap_err(),
        refused(&arko.join("profiles"))
    );
    assert!(ensure_in(&work, &root, "skills").unwrap().path().is_dir());
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// A `.gitignore` that is a link is neither written through nor taken for
/// Arko's own, and isn't deleted.
#[cfg(unix)]
#[test]
fn a_gitignore_that_is_a_link_is_left_alone() {
    let temp = tempfile::tempdir().unwrap();
    let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
    let target = temp.path().join("target");
    std::fs::create_dir_all(work.join(".arko")).unwrap();
    std::os::unix::fs::symlink(&target, ignore_of(&work)).unwrap();
    apply_setting(&work, true).unwrap();
    assert!(!target.exists(), "nothing is written through the link");
    std::fs::write(&target, IGNORE_TEXT).unwrap();
    assert!(!has_own_ignore(&work));
    setting(&root, false);
    ensure(&work, &root).unwrap();
    apply_setting(&work, false).unwrap();
    assert!(ignore_of(&work).symlink_metadata().is_ok());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), IGNORE_TEXT);
}

/// An opened folder renames without replacing, even an empty folder, makes
/// no file through a link, reads none through one, and deletes a folder
/// with a link in it without following the link.
#[cfg(unix)]
#[test]
fn a_folder_writes_and_deletes_without_following_links() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(outside.join("kept")).unwrap();
    std::fs::write(outside.join("file"), "Mine.").unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir_all(&base).unwrap();
    let folder = Folder::base(&base).unwrap();
    folder.make("staging").unwrap();
    folder.make("empty").unwrap();
    let error = folder.rename_new("staging", "empty").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    folder.rename_new("staging", "fresh").unwrap();
    assert!(base.join("fresh").is_dir() && base.join("empty").is_dir());

    symlink(outside.join("file"), base.join("link")).unwrap();
    assert!(folder.create("link", b"x").is_err());
    assert!(folder.read("link").is_err());
    assert_eq!(
        folder.create_whole("link", b"x").unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("file")).unwrap(),
        "Mine."
    );

    let (fresh, made) = folder.open("fresh", false).unwrap().unwrap();
    assert!(!made);
    fresh.create("a.md", b"A").unwrap();
    symlink(outside.join("kept"), fresh.path().join("kept")).unwrap();
    folder.remove_all("fresh").unwrap();
    assert!(!base.join("fresh").exists());
    assert!(outside.join("kept").is_dir());
    assert!(folder.open("link", false).is_err());
}

/// Reading a name that is a pipe fails at once, rather than waiting for a
/// writer.
#[cfg(unix)]
#[test]
fn a_folder_reads_only_regular_files() {
    let temp = tempfile::tempdir().unwrap();
    let pipe = std::ffi::CString::new(
        temp.path()
            .join("pipe")
            .into_os_string()
            .into_encoded_bytes(),
    )
    .unwrap();
    // SAFETY: a C string for a path, and a mode.
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    let folder = Folder::base(temp.path()).unwrap();
    assert!(folder.read("pipe").is_err());
    assert!(folder.open_file("pipe").is_err());
}

/// Listing a folder within a budget fails as soon as it finds one entry past
/// it, and lists a folder within it whole and sorted.
#[test]
fn a_folder_lists_its_names_within_a_budget() {
    let temp = tempfile::tempdir().unwrap();
    for n in (0..50).rev() {
        std::fs::write(temp.path().join(format!("f{n:02}")), "").unwrap();
    }
    let folder = Folder::base(temp.path()).unwrap();
    let error = folder.names_up_to(10).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::FileTooLarge);
    assert_eq!(
        folder.names_up_to(49).unwrap_err().kind(),
        ErrorKind::FileTooLarge
    );
    let names = folder.names_up_to(50).unwrap();
    assert_eq!(names.len(), 50);
    assert_eq!(names[0], "f00");
    assert_eq!(names[49], "f49");
}
