use super::*;

fn skill(path: &Path, why: &str) -> SkillRecord {
    SkillRecord {
        path: path.to_path_buf(),
        id: None,
        scope: Scope::Project,
        session: "2026-10-28-a1b2".into(),
        session_title: Some("Release prep".into()),
        added: "2026-10-28T18:40:00Z".parse().unwrap(),
        why: why.into(),
        source: Some("https://example.com/release-notes.zip".into()),
    }
}

fn server(file: &Path, name: &str) -> ServerRecord {
    ServerRecord {
        file: file.to_path_buf(),
        folder: None,
        name: name.into(),
        session: "2026-10-28-a1b2".into(),
        session_title: None,
        added: "2026-10-28T18:52:00Z".parse().unwrap(),
        why: "Read and file this repository's issues.".into(),
    }
}

/// Skills and servers are recorded, found, replaced and forgotten, and the
/// file reads as the design shows it.
#[test]
fn skills_and_servers_are_recorded_and_forgotten() {
    let temp = tempfile::tempdir().unwrap();
    // (Skills are recorded by their real paths.)
    let base = temp.path().canonicalize().unwrap();
    let root = base.join("arko");
    let folder = base.join("app/.arko/skills/release-notes");
    std::fs::create_dir_all(&folder).unwrap();
    let file = base.join("app/.arko/mcp.json");
    std::fs::write(&file, r#"{"mcpServers": {"github": {"command": "npx"}}}"#).unwrap();
    assert!(skills(&root).is_empty() && servers(&root).is_empty());
    record_skill(&root, skill(&folder, "First.")).unwrap();
    record_skill(&root, skill(&folder, "The checklist changed.")).unwrap();
    record_server(&root, server(&file, "github")).unwrap();
    assert_eq!(skills(&root), [skill(&folder, "The checklist changed.")]);
    assert_eq!(
        skill_at(&root, &folder).unwrap().why,
        "The checklist changed."
    );
    assert_eq!(
        server_in(&root, &file, "github"),
        Some(server(&file, "github"))
    );
    assert_eq!(server_in(&root, &file, "sentry"), None);
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(FILE)).unwrap()).unwrap();
    assert_eq!(written["skills"][0]["scope"], "project");
    assert_eq!(written["skills"][0]["added"], "2026-10-28T18:40:00Z");
    assert_eq!(written["servers"][0]["name"], "github");
    assert!(written["servers"][0].get("session_title").is_none());
    assert_eq!(forget_skill(&root, &folder), Ok(true));
    assert_eq!(forget_skill(&root, &folder), Ok(false));
    assert_eq!(forget_server(&root, &file, "github"), Ok(true));
    assert!(skills(&root).is_empty() && servers(&root).is_empty());
}

/// An entry whose skill folder is gone, or whose server its file no longer
/// sets up, is left out when the record is read, and dropped at the next
/// write.
#[test]
fn entries_whose_skill_or_server_is_gone_are_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let root = base.join("arko");
    let kept = base.join("kept");
    let gone = base.join("gone");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::create_dir_all(&gone).unwrap();
    let file = temp.path().join("mcp.json");
    std::fs::write(
        &file,
        r#"{"mcpServers": [{"name": "listed", "command": "x"}, {"name": "dropped", "command": "y"}]}"#,
    )
    .unwrap();
    for folder in [&kept, &gone] {
        record_skill(&root, skill(folder, "Why.")).unwrap();
    }
    for name in ["listed", "dropped"] {
        record_server(&root, server(&file, name)).unwrap();
    }
    std::fs::remove_dir_all(&gone).unwrap();
    std::fs::write(
        &file,
        r#"{"mcpServers": [{"name": "listed", "command": "x"}]}"#,
    )
    .unwrap();
    let paths: Vec<PathBuf> = skills(&root).into_iter().map(|skill| skill.path).collect();
    assert_eq!(paths, std::slice::from_ref(&kept));
    let names: Vec<String> = servers(&root)
        .into_iter()
        .map(|server| server.name)
        .collect();
    assert_eq!(names, ["listed"]);
    // Still in the file until a write.
    let text = std::fs::read_to_string(root.join(FILE)).unwrap();
    assert!(text.contains("dropped") && text.contains("gone"));
    assert_eq!(forget_skill(&root, &temp.path().join("never")), Ok(false));
    let text = std::fs::read_to_string(root.join(FILE)).unwrap();
    assert!(
        !text.contains("dropped") && !text.contains("gone"),
        "{text}"
    );
    // A file that's gone takes its servers with it.
    std::fs::remove_file(&file).unwrap();
    assert!(servers(&root).is_empty());
}

/// Sessions writing at the same time keep each other's entries.
#[test]
fn writes_at_the_same_time_keep_each_others_entries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("arko");
    let folders: Vec<PathBuf> = (0..12)
        .map(|index| temp.path().join(format!("skill-{index}")))
        .collect();
    for folder in &folders {
        std::fs::create_dir_all(folder).unwrap();
    }
    let barrier = std::sync::Barrier::new(folders.len());
    std::thread::scope(|scope| {
        for folder in &folders {
            let (root, barrier) = (&root, &barrier);
            scope.spawn(move || {
                barrier.wait();
                record_skill(root, skill(folder, "Why.")).unwrap();
            });
        }
    });
    assert_eq!(skills(&root).len(), folders.len());
}

/// A record that can't be read counts as empty when read, and isn't
/// written over.
#[test]
fn a_record_that_cant_be_read_is_left_alone() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join(FILE), "not json").unwrap();
    assert!(skills(root).is_empty());
    let folder = root.join("skill");
    std::fs::create_dir_all(&folder).unwrap();
    assert!(record_skill(root, skill(&folder, "Why.")).is_err());
    assert_eq!(
        std::fs::read_to_string(root.join(FILE)).unwrap(),
        "not json"
    );
}
