//! Tests that reach the engine's private state, to hold a call at a chosen
//! point while another session acts.

use super::*;

/// Creating an index again (to change its settings) keeps what another
/// session saved in its manifest while the creation waited, such as the
/// tokens and dollars an embedding run spent.
#[test]
#[cfg(target_os = "linux")]
fn create_keeps_what_another_session_saved_while_it_waited() {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn example() {}\n").unwrap();
    let data = temp.path().join("data");
    let open =
        || Indexes::open_with(&data, &root, Backend::Hash, false, Timing::default(), None).unwrap();
    let first = open();
    let args = CreateArgs {
        path: root.clone(),
        cwd: root.clone(),
        kind: Some(Kind::Code),
        name: Some("proj".into()),
        include: Vec::new(),
        exclude: Vec::new(),
        monitor: false,
        embed: Some(false),
        max_embed_tokens: None,
        wait: false,
        defer_embed: false,
    };
    let outcome = first.create_blocking(&args).unwrap();
    let second = open();

    // The second creation waits to stop the index's watcher, after it read
    // the manifest, while this test holds the watcher's lock.
    let entry = first.entry(&outcome.layout);
    let watcher = lock(&entry.watcher);
    // SAFETY: a plain system call, whose descriptor `OwnedFd` closes.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: `fd` is a new descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let file = outcome.layout.dir.join(store::MANIFEST_FILE);
    let file = CString::new(file.as_os_str().as_bytes()).unwrap();
    // SAFETY: `file` is a valid C string for the call's length.
    let watched =
        unsafe { libc::inotify_add_watch(fd.as_raw_fd(), file.as_ptr(), libc::IN_CLOSE_NOWRITE) };
    assert!(watched >= 0);
    let creator = {
        let (first, args) = (first.clone(), args.clone());
        std::thread::spawn(move || first.create_blocking(&args).unwrap())
    };
    let until = Instant::now() + Duration::from_secs(5);
    let mut events = [0u8; 1024];
    loop {
        // SAFETY: `events` is writable for its whole length.
        let read = unsafe { libc::read(fd.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
        if read > 0 {
            break;
        }
        assert!(
            Instant::now() < until,
            "the creation didn't read the manifest"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    second
        .update_manifest(&outcome.layout, |manifest| {
            manifest.embed_tokens += 123;
            manifest.embed_cost += 0.25;
        })
        .unwrap();
    drop(watcher);
    creator.join().unwrap();
    let saved = store::load_manifest(&outcome.layout.dir).unwrap();
    runtime.block_on(first.shutdown());
    runtime.block_on(second.shutdown());
    assert_eq!(
        saved.embed_tokens, 123,
        "the creation undid the other session's spending"
    );
    assert_eq!(saved.embed_cost, 0.25);
}
