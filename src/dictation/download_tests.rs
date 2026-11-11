//! The download against a local server that stands in for Hugging Face.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::*;
use crate::dictation::models::Family;

/// What the server does.
#[derive(Default)]
struct Behaviour {
    /// Answers every request with the whole file.
    ignore_ranges: bool,
    /// The next answer ends after this many bytes of its body.
    cut_next: Option<usize>,
    /// Answers 404 for every file.
    missing: bool,
    /// Answers a range with the whole file, marked as a part of it.
    wrong_range: bool,
    /// Each request's path and the start of its range.
    requests: Vec<(String, Option<u64>)>,
}

/// A local server for the files of a model, at
/// `/{repo}/resolve/{revision}/{name}`, served from disk.
pub(crate) struct FileServer {
    pub base: String,
    behaviour: Arc<Mutex<Behaviour>>,
}

impl FileServer {
    pub(crate) fn start(model: &SpeechModel, dir: &Path) -> FileServer {
        FileServer::serving(
            model
                .files
                .iter()
                .map(|file| {
                    let path = format!("/{}/resolve/{}/{}", model.repo, model.revision, file.name);
                    (path, dir.join(file.name))
                })
                .collect(),
        )
    }

    /// A server for `files`, by the path each is served at.
    fn serving(files: HashMap<String, PathBuf>) -> FileServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let behaviour: Arc<Mutex<Behaviour>> = Arc::default();
        let state = behaviour.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &files, &state);
            }
        });
        FileServer { base, behaviour }
    }

    fn behave(&self, change: impl FnOnce(&mut Behaviour)) {
        change(&mut self.behaviour.lock().unwrap());
    }

    fn requests(&self) -> Vec<(String, Option<u64>)> {
        self.behaviour.lock().unwrap().requests.clone()
    }
}

fn serve(stream: TcpStream, files: &HashMap<String, PathBuf>, state: &Mutex<Behaviour>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let mut range = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("range: bytes=") {
            range = value.trim().trim_end_matches('-').parse::<u64>().ok();
        }
    }
    let mut stream = stream;
    let (ignore_ranges, cut, missing, wrong_range) = {
        let mut state = state.lock().unwrap();
        state.requests.push((path.clone(), range));
        (
            state.ignore_ranges,
            state.cut_next.take(),
            state.missing,
            state.wrong_range,
        )
    };
    let Some(file) = files.get(&path).filter(|_| !missing) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let mut body = std::fs::File::open(file).unwrap();
    let length = body.metadata().unwrap().len();
    let start = range.filter(|_| !ignore_ranges).unwrap_or(0);
    let head = if wrong_range && start > 0 {
        let start = 0;
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{length}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
            length - 1
        )
    } else if start > 0 {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{length}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            length - 1,
            length - start
        )
    } else {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
    };
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let start = if wrong_range { 0 } else { start };
    std::io::Seek::seek(&mut body, std::io::SeekFrom::Start(start)).unwrap();
    let mut left = cut.unwrap_or(usize::MAX);
    let mut buffer = vec![0; 64 * 1024];
    while left > 0 {
        let read = body.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        let take = read.min(left);
        if stream.write_all(&buffer[..take]).is_err() {
            return;
        }
        left -= take;
    }
    let _ = stream.flush();
}

/// A model of `files`, each written to `dir`, with their real sizes and
/// SHA-256s.
fn fake_model(dir: &Path, files: &[(&'static str, Vec<u8>)]) -> &'static SpeechModel {
    let remote: Vec<RemoteFile> = files
        .iter()
        .map(|(name, data)| {
            std::fs::write(dir.join(name), data).unwrap();
            let digest: String = Sha256::digest(data)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            RemoteFile {
                name,
                bytes: data.len() as u64,
                sha256: Box::leak(digest.into_boxed_str()),
            }
        })
        .collect();
    Box::leak(Box::new(SpeechModel {
        name: "fake",
        title: "Fake",
        maker: "Arko",
        family: Family::Granite,
        folder: "fake",
        repo: "arko/fake",
        revision: "0123abcd",
        files: Box::leak(remote.into_boxed_slice()),
        source_file: None,
        pack_bytes: 0,
        precision: "int8",
        licence: "MIT",
        licence_url: "https://opensource.org/licenses/MIT",
    }))
}

/// A megabyte and a half that isn't all one byte.
fn weights() -> Vec<u8> {
    (0..1_500_000u32).map(|n| (n % 251) as u8).collect()
}

struct Setup {
    model: &'static SpeechModel,
    server: FileServer,
    downloads: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn setup() -> Setup {
    let served = tempfile::tempdir().unwrap();
    let model = fake_model(
        served.path(),
        &[
            ("model.safetensors", weights()),
            ("config.json", b"{}".to_vec()),
        ],
    );
    let server = FileServer::start(model, served.path());
    let saved = tempfile::tempdir().unwrap();
    let downloads = saved.path().join("download");
    std::fs::create_dir_all(&downloads).unwrap();
    Setup {
        model,
        server,
        downloads,
        _dirs: (served, saved),
    }
}

fn download(setup: &Setup, steps: &mut Vec<Preparing>) -> Result<(), Error> {
    download_files(
        setup.model,
        &setup.downloads,
        &setup.server.base,
        &mut |step| steps.push(step),
        &AtomicBool::new(false),
    )
}

fn saved(setup: &Setup) -> Vec<u8> {
    std::fs::read(setup.downloads.join("model.safetensors")).unwrap()
}

#[test]
fn the_files_download_and_are_checked() {
    let setup = setup();
    let mut steps = Vec::new();
    download(&setup, &mut steps).unwrap();
    assert_eq!(saved(&setup), weights());
    assert_eq!(
        std::fs::read(setup.downloads.join("config.json")).unwrap(),
        b"{}"
    );
    let total = setup.model.download_bytes();
    assert_eq!(
        steps.first(),
        Some(&Preparing::Downloading {
            model: ModelKind::Speech,
            done: 0,
            total
        })
    );
    assert_eq!(
        &steps[steps.len() - 2..],
        &[
            Preparing::Downloading {
                model: ModelKind::Speech,
                done: total,
                total
            },
            Preparing::Packing
        ]
    );
    assert!(
        setup
            .server
            .requests()
            .iter()
            .all(|(_, range)| range.is_none())
    );
}

#[test]
fn a_dropped_connection_resumes_where_it_stopped() {
    let setup = setup();
    setup.server.behave(|b| b.cut_next = Some(600_000));
    let mut steps = Vec::new();
    let Err(Error::Failed(message)) = download(&setup, &mut steps) else {
        panic!("the cut connection went unnoticed");
    };
    assert!(
        message.starts_with("Couldn't download the speech model ("),
        "{message}"
    );
    assert!(
        message.ends_with("). Start dictation again to resume."),
        "{message}"
    );
    let kept = saved(&setup).len();
    assert!(kept > 0 && kept <= 600_000, "{kept}");
    // The next try asks for the rest.
    download(&setup, &mut steps).unwrap();
    assert_eq!(saved(&setup), weights());
    let requests = setup.server.requests();
    assert_eq!(requests[1].1, Some(kept as u64), "{requests:?}");
}

#[test]
fn a_server_that_ignores_the_range_sends_the_whole_file_again() {
    let setup = setup();
    std::fs::write(
        setup.downloads.join("model.safetensors"),
        &weights()[..1000],
    )
    .unwrap();
    setup.server.behave(|b| b.ignore_ranges = true);
    download(&setup, &mut Vec::new()).unwrap();
    assert_eq!(saved(&setup), weights());
    assert_eq!(setup.server.requests()[0].1, Some(1000));
}

/// A part that doesn't start where the file stopped is refused, and the
/// next try starts the file over.
#[test]
fn a_part_from_the_wrong_place_starts_the_file_over() {
    let setup = setup();
    std::fs::write(
        setup.downloads.join("model.safetensors"),
        &weights()[..1000],
    )
    .unwrap();
    setup.server.behave(|b| b.wrong_range = true);
    let error = download(&setup, &mut Vec::new()).unwrap_err();
    assert_eq!(
        error,
        Error::Failed("Couldn't download the speech model (Hugging Face sent the wrong part of the file). Start dictation again to resume.".into())
    );
    assert!(!setup.downloads.join("model.safetensors").exists());
    download(&setup, &mut Vec::new()).unwrap();
    assert_eq!(saved(&setup), weights());
}

#[test]
fn content_ranges_are_read_to_the_files_end() {
    assert_eq!(content_range("bytes 100-999/1000"), Some((100, 1000)));
    assert_eq!(content_range("bytes 0-0/1"), Some((0, 1)));
    assert_eq!(content_range("bytes 100-499/1000"), None);
    assert_eq!(content_range("bytes 100-999/*"), None);
    assert_eq!(content_range("items 1-2/3"), None);
    assert_eq!(content_range("bytes 5-4/5"), None);
}

#[test]
fn a_file_already_whole_isnt_downloaded_again() {
    let setup = setup();
    std::fs::write(setup.downloads.join("model.safetensors"), weights()).unwrap();
    download(&setup, &mut Vec::new()).unwrap();
    let requests = setup.server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].0.ends_with("/config.json"));
}

#[test]
fn a_damaged_download_is_deleted() {
    let setup = setup();
    // As long as it should be, but not what was published.
    let mut wrong = weights();
    wrong[7] ^= 1;
    std::fs::write(setup.downloads.join("model.safetensors"), wrong).unwrap();
    let error = download(&setup, &mut Vec::new()).unwrap_err();
    assert_eq!(
        error,
        Error::Failed("The downloaded speech model was damaged, so Arko deleted it. Start dictation again to download it again.".into())
    );
    assert!(!setup.downloads.join("model.safetensors").exists());
}

#[test]
fn a_missing_file_says_what_the_server_answered() {
    let setup = setup();
    setup.server.behave(|b| b.missing = true);
    let error = download(&setup, &mut Vec::new()).unwrap_err();
    assert_eq!(
        error,
        Error::Failed("Couldn't download the speech model (Hugging Face answered 404 Not Found). Start dictation again to resume.".into())
    );
}

#[test]
fn a_server_that_cant_be_reached_says_so() {
    let setup = setup();
    let error = download_files(
        setup.model,
        &setup.downloads,
        "http://127.0.0.1:1",
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(
        error,
        Error::Failed("Couldn't download the speech model (couldn't reach Hugging Face). Start dictation again to resume.".into())
    );
}

#[test]
fn cancelling_stops_the_download_and_keeps_what_came() {
    let setup = setup();
    let cancel = AtomicBool::new(false);
    let error = download_files(
        setup.model,
        &setup.downloads,
        &setup.server.base,
        &mut |step| {
            if matches!(step, Preparing::Downloading { done, .. } if done > 0) {
                cancel.store(true, Ordering::SeqCst);
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error, Error::Cancelled);
    let kept = saved(&setup).len();
    assert!(kept > 0 && kept < weights().len(), "{kept}");
}

/// Cancelling stops the checksum between megabytes, and the pack at the
/// next tensor.
#[test]
fn cancelling_stops_checking_and_packing() {
    let setup = setup();
    let path = setup.downloads.join("model.safetensors");
    std::fs::write(&path, weights()).unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(sha256(&path, SPEECH, &cancel), Err(Error::Cancelled));
    let out = setup.downloads.join("model.pack");
    let mut writer = PackWriter {
        file: FileSink::create(&out, "test", Vec::new()).unwrap(),
        cancel: &cancel,
    };
    assert!(pack::Sink::bytes(&mut writer, "a", b"bytes").is_err());
    cancel.store(false, Ordering::SeqCst);
    pack::Sink::bytes(&mut writer, "a", b"bytes").unwrap();
    assert_eq!(sha256(&path, SPEECH, &cancel).unwrap().len(), 64);
}

/// One process installs a model at a time. Another waits, following the
/// first one's files, and finds the model installed once it's done.
#[test]
fn a_second_install_waits_for_the_first() {
    let setup = setup();
    let root = tempfile::tempdir().unwrap();
    let dir = setup.model.dir(root.path());
    std::fs::create_dir_all(dir.join(DOWNLOAD_DIR)).unwrap();
    let held =
        InstallLock::take(&dir.join(LOCK_FILE), SPEECH, &AtomicBool::new(false), || {}).unwrap();
    std::fs::write(dir.join(DOWNLOAD_DIR).join("config.json"), b"{}").unwrap();
    let (model, base, root_path) = (
        setup.model,
        setup.server.base.clone(),
        root.path().to_path_buf(),
    );
    let other = std::thread::spawn(move || {
        let mut steps = Vec::new();
        let done = install(
            model,
            &root_path,
            &base,
            &mut |step| steps.push(step),
            &AtomicBool::new(false),
        );
        (done, steps)
    });
    std::thread::sleep(std::time::Duration::from_millis(1200));
    // The first one finishes.
    std::fs::write(setup.model.pack(root.path()), b"pack").unwrap();
    drop(held);
    let (done, steps) = other.join().unwrap();
    assert_eq!(done, Ok(()));
    assert_eq!(
        steps.first(),
        Some(&Preparing::Downloading {
            model: ModelKind::Speech,
            done: 2,
            total: setup.model.download_bytes()
        })
    );
    assert!(setup.server.requests().is_empty(), "downloaded again");
    // The lock's file stays, for the next install to lock.
    assert!(dir.join(LOCK_FILE).exists());
}

/// Two contenders for the lock take turns. The second waits while the
/// first holds it, and a cancelled wait ends.
#[test]
fn contenders_for_the_lock_take_turns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LOCK_FILE);
    let first =
        InstallLock::take(&path, SPEECH, &AtomicBool::new(false), || panic!("waited")).unwrap();
    assert_eq!(
        InstallLock::take(&path, SPEECH, &AtomicBool::new(true), || {}).err(),
        Some(Error::Cancelled)
    );
    let waits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (second_path, counted) = (path.clone(), waits.clone());
    let second = std::thread::spawn(move || {
        InstallLock::take(&second_path, SPEECH, &AtomicBool::new(false), || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .map(|_| ())
    });
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(waits.load(Ordering::SeqCst) >= 1);
    assert!(!second.is_finished(), "both held it");
    drop(first);
    assert_eq!(second.join().unwrap(), Ok(()));
}

/// The variable that makes [`a_lock_whose_holder_dies_is_free`] the
/// process that holds the lock and dies.
const HOLDER: &str = "ARKO_TEST_LOCK_HOLDER";
/// What the holder prints once it holds the lock.
const HELD: &str = "install lock held";

/// A child process, killed when dropped, so a test that fails leaves it
/// running no longer.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A process that dies holding the lock lets go of it, however it ends.
#[test]
fn a_lock_whose_holder_dies_is_free() {
    if let Some(path) = std::env::var_os(HOLDER) {
        let _lock =
            InstallLock::take(Path::new(&path), SPEECH, &AtomicBool::new(false), || {}).unwrap();
        // (The test harness prints it after the test's name.)
        println!("{HELD}");
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LOCK_FILE);
    let mut holder = Killed(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "dictation::download::tests::a_lock_whose_holder_dies_is_free",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(HOLDER, &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut lines = BufReader::new(holder.0.stdout.take().unwrap()).lines();
    assert!(
        lines.any(|line| line.is_ok_and(|line| line.ends_with(HELD))),
        "the holder didn't take the lock"
    );
    assert_eq!(
        InstallLock::take(&path, SPEECH, &AtomicBool::new(true), || {}).err(),
        Some(Error::Cancelled),
        "the lock was free while held"
    );
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    InstallLock::take(&path, SPEECH, &AtomicBool::new(false), || panic!("waited")).unwrap();
}

/// A server that takes the connection and never answers, and cancelling ends
/// the wait at once.
#[test]
fn cancelling_ends_a_wait_for_the_server() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let held: Vec<TcpStream> = listener.incoming().flatten().collect();
        drop(held);
    });
    let setup = setup();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        flag.store(true, Ordering::SeqCst);
    });
    let start = std::time::Instant::now();
    let error =
        download_files(setup.model, &setup.downloads, &base, &mut |_| {}, &cancel).unwrap_err();
    assert_eq!(error, Error::Cancelled);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

/// The whole install of Granite Speech, from a local copy of its files
/// served as Hugging Face would, meaning downloaded, checked, packed, with its
/// notices, and the download gone. It reads and writes about 2 GB, so it
/// runs only with `ARKO_TEST_GRANITE_DIR` naming the folder of the files.
#[test]
fn granite_installs_from_its_files() {
    let Some(files) = std::env::var_os("ARKO_TEST_GRANITE_DIR") else {
        eprintln!("skipped: ARKO_TEST_GRANITE_DIR isn't set");
        return;
    };
    let model = &crate::dictation::models::GRANITE;
    let server = FileServer::start(model, Path::new(&files));
    let root = tempfile::tempdir().unwrap();
    let mut steps = Vec::new();
    install(
        model,
        root.path(),
        &server.base,
        &mut |step| steps.push(step),
        &AtomicBool::new(false),
    )
    .unwrap();
    let dir = model.dir(root.path());
    assert!(model.is_installed(root.path()));
    assert!(!dir.join(DOWNLOAD_DIR).exists());
    assert!(
        std::fs::read_to_string(dir.join("NOTICE.txt"))
            .unwrap()
            .starts_with("Granite Speech 5.0 by IBM\n")
    );
    let source: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("source.json")).unwrap()).unwrap();
    assert_eq!(source["revision"], model.revision);
    assert_eq!(steps.last(), Some(&Preparing::Packing));
    // The pack is the one the engine's own command makes, but for the
    // source's name in its metadata.
    let bytes = std::fs::metadata(model.pack(root.path())).unwrap().len();
    assert!(bytes.abs_diff(model.pack_bytes) < 4096, "{bytes}");
    speech::Model::load(&model.pack(root.path()), &speech::Options::default()).unwrap();
}

/// A pack of a megabyte and a half served at `/{name}`, as a GitHub release
/// file, with how it's pinned (its SHA-256 given, or its own).
fn served_pack(sha256: Option<&'static str>) -> (FileServer, RemoteFile, tempfile::TempDir) {
    let served = tempfile::tempdir().unwrap();
    let name = "tagger-q8_0.pack";
    std::fs::write(served.path().join(name), weights()).unwrap();
    let digest: String = Sha256::digest(weights())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let file = RemoteFile {
        name,
        bytes: weights().len() as u64,
        sha256: sha256.unwrap_or(Box::leak(digest.into_boxed_str())),
    };
    let server = FileServer::serving([(format!("/{name}"), served.path().join(name))].into());
    (server, file, served)
}

/// Full cleanup's pack downloads, is checked and goes in place with its
/// notice, and the download is gone.
#[test]
fn the_cleanup_pack_downloads_and_goes_in_place() {
    let (server, file, _served) = served_pack(None);
    let root = tempfile::tempdir().unwrap();
    let mut steps = Vec::new();
    let url = format!("{}/{}", server.base, file.name);
    let cancel = AtomicBool::new(false);
    install_tagger(
        root.path(),
        &url,
        &file,
        &mut |step| steps.push(step),
        &cancel,
    )
    .unwrap();
    let dir = models::tagger_dir(root.path());
    assert_eq!(
        std::fs::read(models::tagger_pack(root.path())).unwrap(),
        weights()
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("NOTICE.txt")).unwrap(),
        tagger_notice(&url)
    );
    assert!(tagger_notice(&url).starts_with(speech::disfluency::PACK_NOTICE));
    assert!(tagger_notice(&url).ends_with(" release.\n"));
    assert!(!dir.join(DOWNLOAD_DIR).exists());
    let step = |done| Preparing::Downloading {
        model: ModelKind::Cleanup,
        done,
        total: file.bytes,
    };
    assert_eq!(steps.first(), Some(&step(0)));
    assert_eq!(steps.last(), Some(&step(file.bytes)));
    // Installed, it isn't downloaded again.
    install_tagger(root.path(), &url, &file, &mut |_| {}, &cancel).unwrap();
    assert_eq!(server.requests().len(), 1);
}

/// A whole pack an earlier try downloaded but didn't put in place goes in
/// place without downloading it again. On Windows the install flushes it
/// through a handle that may write to it, which Windows requires.
#[test]
fn a_whole_cleanup_pack_from_an_earlier_try_goes_in_place() {
    let (server, file, served) = served_pack(None);
    let root = tempfile::tempdir().unwrap();
    let downloads = models::tagger_dir(root.path()).join(DOWNLOAD_DIR);
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::copy(served.path().join(file.name), downloads.join(file.name)).unwrap();
    let url = format!("{}/{}", server.base, file.name);
    let mut steps = Vec::new();
    install_tagger(
        root.path(),
        &url,
        &file,
        &mut |step| steps.push(step),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(models::tagger_pack(root.path())).unwrap(),
        weights()
    );
    assert!(server.requests().is_empty());
    assert!(!downloads.exists());
    assert_eq!(
        steps.last(),
        Some(&Preparing::Downloading {
            model: ModelKind::Cleanup,
            done: file.bytes,
            total: file.bytes,
        })
    );
}

/// A cleanup pack that isn't the one pinned is deleted, and nothing goes in
/// place.
#[test]
fn a_damaged_cleanup_pack_is_deleted() {
    let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
    let (server, file, _served) = served_pack(Some(wrong));
    let root = tempfile::tempdir().unwrap();
    let url = format!("{}/{}", server.base, file.name);
    let cancel = AtomicBool::new(false);
    let error = install_tagger(root.path(), &url, &file, &mut |_| {}, &cancel).unwrap_err();
    assert_eq!(
        error,
        Error::Failed("The downloaded cleanup model was damaged, so Arko deleted it. Start dictation again to download it again.".into())
    );
    let dir = models::tagger_dir(root.path());
    assert!(!models::tagger_pack(root.path()).exists());
    assert!(!dir.join(DOWNLOAD_DIR).join(file.name).exists());
}

/// The tagger's folder is named after the release Telmo pins, so a new pin
/// downloads into a folder of its own instead of finding the old pack.
#[test]
fn the_tagger_folder_follows_the_pinned_release() {
    let name = speech::disfluency::PACK_DOWNLOAD.name;
    assert!(
        name.starts_with(&format!("{}-", models::TAGGER_RELEASE)),
        "{name}"
    );
    let url = speech::disfluency::PACK_DOWNLOAD.url;
    assert!(
        url.contains(&format!("/download/{}/", models::TAGGER_RELEASE)),
        "{url}"
    );
}
