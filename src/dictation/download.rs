//! Downloading a speech model and packing it, meaning each file from the model's
//! pinned revision, resumed where an earlier try stopped, checked against
//! its SHA-256, then converted into the engine's pack in the model's 8-bit
//! precision. The downloaded files go once the pack is written. Full
//! cleanup's pack downloads the same way, ready to use. One process at a
//! time installs a model, holding the operating system's lock on a file in
//! its folder, and cancelling stops the work at the next megabyte or
//! tensor.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use speech::pack::{self, FileSink};

use super::models::{self, PACK_FILE, RemoteFile, SpeechModel};
use super::{ModelKind, Preparing};

/// How the download ended, when it didn't.
#[derive(Debug, PartialEq)]
pub(super) enum Error {
    /// The dictation was cancelled. What was downloaded stays, to resume.
    Cancelled,
    /// The sentence to show.
    Failed(String),
}

/// The download stops when no data came for this long.
const STALL: Duration = Duration::from_secs(60);
const RESUME: &str = "Start dictation again to resume.";
/// The folder, in the model's, the files download into.
const DOWNLOAD_DIR: &str = "download";
/// The file, in the model's folder, whose lock the process installing it
/// holds. It stays there.
const LOCK_FILE: &str = "install.lock";

/// A model as its download's errors name it, and where it comes from.
#[derive(Clone, Copy)]
struct Origin {
    model: &'static str,
    host: &'static str,
}

const SPEECH: Origin = Origin {
    model: "speech model",
    host: "Hugging Face",
};

const CLEANUP: Origin = Origin {
    model: "cleanup model",
    host: "GitHub",
};

fn failed(origin: Origin, why: impl std::fmt::Display) -> Error {
    Error::Failed(format!(
        "Couldn't download the {} ({why}). {RESUME}",
        origin.model
    ))
}

/// Downloads `model` from `base` into its folder in Arko's folder `root`
/// and packs it, telling `progress` each step.
pub(super) fn install(
    model: &SpeechModel,
    root: &Path,
    base: &str,
    progress: &mut dyn FnMut(Preparing),
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let dir = model.dir(root);
    let downloads = dir.join(DOWNLOAD_DIR);
    let unsaved = |error: std::io::Error| unsaved(SPEECH, &dir, error);
    fs::create_dir_all(&dir).map_err(unsaved)?;
    // Held for the whole install. Another process installing it is followed
    // by the size of its files.
    let total = model.download_bytes();
    let _lock = InstallLock::take(&dir.join(LOCK_FILE), SPEECH, cancel, || {
        progress(Preparing::Downloading {
            model: ModelKind::Speech,
            done: downloaded(model, &downloads),
            total,
        })
    })?;
    if model.is_installed(root) {
        return Ok(());
    }
    fs::create_dir_all(&downloads).map_err(unsaved)?;
    download_files(model, &downloads, base, progress, cancel)?;
    // The notices first, because the pack is what makes the model installed.
    fs::write(dir.join("NOTICE.txt"), model.notice()).map_err(unsaved)?;
    fs::write(dir.join("source.json"), source_json(model)).map_err(unsaved)?;
    let source = model
        .source_file
        .map_or(downloads.clone(), |file| downloads.join(file));
    pack_model(model, &source, &dir.join(PACK_FILE), cancel)?;
    if let Err(error) = fs::remove_dir_all(&downloads) {
        log::warn!(
            "couldn't delete the downloaded files in {} ({error})",
            downloads.display()
        );
    }
    Ok(())
}

/// Downloads `model`'s files from `base` into `downloads` and checks them,
/// then says [`Preparing::Packing`].
fn download_files(
    model: &SpeechModel,
    downloads: &Path,
    base: &str,
    progress: &mut dyn FnMut(Preparing),
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let (runtime, client) = connection(SPEECH)?;
    let total = model.download_bytes();
    let mut before = 0;
    for file in model.files {
        let url = model.url(base, file.name);
        let path = downloads.join(file.name);
        runtime.block_on(fetch(
            &client,
            &url,
            &path,
            file,
            SPEECH,
            &mut |done| {
                progress(Preparing::Downloading {
                    model: ModelKind::Speech,
                    done: before + done,
                    total,
                })
            },
            cancel,
        ))?;
        before += file.bytes;
    }
    progress(Preparing::Packing);
    for file in model.files {
        let path = downloads.join(file.name);
        let digest = sha256(&path, SPEECH, cancel)?;
        if digest != file.sha256 {
            let _ = fs::remove_file(&path);
            return Err(Error::Failed(
                "The downloaded speech model was damaged, so Arko deleted it. Start dictation again to download it again.".into(),
            ));
        }
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

/// Full cleanup's pack, as Telmo pins it.
pub(super) const TAGGER: RemoteFile = RemoteFile {
    name: speech::disfluency::PACK_DOWNLOAD.name,
    bytes: speech::disfluency::PACK_DOWNLOAD.bytes,
    sha256: speech::disfluency::PACK_DOWNLOAD.sha256,
};

/// Full cleanup's pack, from `url`, into its folder in Arko's folder `root`,
/// telling `progress` the bytes. It's checked against `file` and put in
/// place with its notice.
pub(super) fn install_tagger(
    root: &Path,
    url: &str,
    file: &RemoteFile,
    progress: &mut dyn FnMut(Preparing),
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let dir = models::tagger_dir(root);
    let unsaved = |error: std::io::Error| unsaved(CLEANUP, &dir, error);
    fs::create_dir_all(&dir).map_err(unsaved)?;
    let downloads = dir.join(DOWNLOAD_DIR);
    let path = downloads.join(file.name);
    let step = |done| Preparing::Downloading {
        model: ModelKind::Cleanup,
        done,
        total: file.bytes,
    };
    // Held for the whole install, as for a speech model.
    let _lock = InstallLock::take(&dir.join(LOCK_FILE), CLEANUP, cancel, || {
        progress(step(
            fs::metadata(&path).map_or(0, |meta| meta.len().min(file.bytes)),
        ))
    })?;
    let pack = models::tagger_pack(root);
    if pack.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&downloads).map_err(unsaved)?;
    let (runtime, client) = connection(CLEANUP)?;
    runtime.block_on(fetch(
        &client,
        url,
        &path,
        file,
        CLEANUP,
        &mut |done| progress(step(done)),
        cancel,
    ))?;
    if sha256(&path, CLEANUP, cancel)? != file.sha256 {
        let _ = fs::remove_file(&path);
        return Err(Error::Failed(
            "The downloaded cleanup model was damaged, so Arko deleted it. Start dictation again to download it again.".into(),
        ));
    }
    // Whole on disk before it counts as installed, and the notice first,
    // because the pack is what makes the model installed. Windows flushes a
    // file only through a handle that may write to it.
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|written| written.sync_all())
        .map_err(unsaved)?;
    fs::write(dir.join("NOTICE.txt"), tagger_notice(url)).map_err(unsaved)?;
    fs::rename(&path, &pack).map_err(unsaved)?;
    if let Err(error) = fs::remove_dir_all(&downloads) {
        log::warn!("couldn't delete {} ({error})", downloads.display());
    }
    Ok(())
}

/// `NOTICE.txt` beside full cleanup's pack, which says what it's made of and
/// where Arko got it.
fn tagger_notice(url: &str) -> String {
    format!(
        "{}\n\nArko downloaded it from {url}, Telmo's {} release.\n",
        speech::disfluency::PACK_NOTICE,
        models::TAGGER_RELEASE
    )
}

/// A runtime and an HTTP client for a download from `origin`.
fn connection(origin: Origin) -> Result<(tokio::runtime::Runtime, reqwest::Client), Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| failed(origin, error))?;
    // Hugging Face and GitHub redirect to their file servers. Nothing
    // secret is sent.
    let client = arko_ai::http_client_builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(concat!("arko/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| failed(origin, error))?;
    Ok((runtime, client))
}

/// `future`, unless it takes longer than [`STALL`] (`stalled` says what
/// didn't come) or the dictation is cancelled first.
async fn waited<T>(
    future: impl std::future::Future<Output = T>,
    origin: Origin,
    cancel: &AtomicBool,
    stalled: &str,
) -> Result<T, Error> {
    let cancelled = async {
        while !cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        result = tokio::time::timeout(STALL, future) => result.map_err(|_| failed(origin, stalled)),
        () = cancelled => Err(Error::Cancelled),
    }
}

/// Downloads `url` to `path`, resuming what's there, telling `progress` the
/// bytes it has.
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    file: &RemoteFile,
    origin: Origin,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let failed = |why: String| failed(origin, why);
    let host = origin.host;
    let mut have = fs::metadata(path).map_or(0, |meta| meta.len());
    // A file larger than it should be can't be resumed.
    if have > file.bytes {
        let _ = fs::remove_file(path);
        have = 0;
    }
    if have == file.bytes {
        progress(have);
        return Ok(());
    }
    let mut request = client.get(url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = waited(
        request.send(),
        origin,
        cancel,
        &format!("{host} didn't answer"),
    )
    .await?
    .map_err(|error| failed(reason(error, host)))?;
    let resumed = match response.status() {
        reqwest::StatusCode::PARTIAL_CONTENT => {
            let range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(content_range);
            if range != Some((have, file.bytes)) {
                // The next try starts the file over.
                let _ = fs::remove_file(path);
                return Err(failed(format!("{host} sent the wrong part of the file")));
            }
            true
        }
        // The whole file, because the server ignored the range.
        reqwest::StatusCode::OK => false,
        status => return Err(failed(format!("{host} answered {status}"))),
    };
    if !resumed {
        have = 0;
    }
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(true)
        .truncate(!resumed)
        .open(path)
        .map_err(|error| failed(error.to_string()))?;
    progress(have);
    loop {
        let chunk = match waited(
            response.chunk(),
            origin,
            cancel,
            "no data came for a minute",
        )
        .await?
        {
            Err(error) => return Err(failed(reason(error, host))),
            Ok(None) => break,
            Ok(Some(chunk)) => chunk,
        };
        out.write_all(&chunk)
            .map_err(|error| failed(error.to_string()))?;
        have += chunk.len() as u64;
        if have > file.bytes {
            drop(out);
            let _ = fs::remove_file(path);
            return Err(failed(format!("{} is larger than expected", file.name)));
        }
        progress(have);
    }
    out.flush().map_err(|error| failed(error.to_string()))?;
    if have < file.bytes {
        return Err(failed("the connection ended early".into()));
    }
    Ok(())
}

/// A request's error to `host` in a few words, without its URL.
fn reason(error: reqwest::Error, host: &str) -> String {
    if error.is_timeout() {
        "it timed out".into()
    } else if error.is_connect() {
        format!("couldn't reach {host}")
    } else {
        let error = error.without_url();
        let mut text = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            text = format!("{text}, {cause}");
            source = cause.source();
        }
        text
    }
}

/// The start of a 206 answer's `Content-Range` (`bytes 100-999/1000`) and
/// the file's length, when the range runs to the file's end.
fn content_range(value: &str) -> Option<(u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total): (u64, u64, u64) =
        (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (end + 1 == total && start <= end).then_some((start, total))
}

fn unsaved(origin: Origin, dir: &Path, error: std::io::Error) -> Error {
    Error::Failed(format!(
        "Couldn't save the {} in {} ({error}).",
        origin.model,
        dir.display()
    ))
}

/// A file's SHA-256, in hex, unless the dictation is cancelled first.
fn sha256(path: &Path, origin: Origin, cancel: &AtomicBool) -> Result<String, Error> {
    let unread = |error: std::io::Error| {
        Error::Failed(format!(
            "Couldn't read the {}'s download {} ({error}).",
            origin.model,
            path.display()
        ))
    };
    let mut file = fs::File::open(path).map_err(unread)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let read = file.read(&mut buffer).map_err(unread)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The bytes of `model`'s files in `downloads` so far.
fn downloaded(model: &SpeechModel, downloads: &Path) -> u64 {
    model
        .files
        .iter()
        .map(|file| {
            fs::metadata(downloads.join(file.name)).map_or(0, |meta| meta.len().min(file.bytes))
        })
        .sum()
}

/// A model's install lock, while this process holds it, meaning the operating
/// system's lock on the model's `install.lock`, which it lets go when the
/// lock is dropped or the process ends, however it ends.
struct InstallLock {
    _file: fs::File,
}

impl InstallLock {
    /// Takes the lock on the file at `path`, in the folder of a model from
    /// `origin`, waiting while another process holds it and telling
    /// `waiting` every half second meanwhile.
    fn take(
        path: &Path,
        origin: Origin,
        cancel: &AtomicBool,
        mut waiting: impl FnMut(),
    ) -> Result<Self, Error> {
        let unsaved = |error: std::io::Error| unsaved(origin, path.parent().unwrap_or(path), error);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(unsaved)?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(error)) => return Err(unsaved(error)),
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            waiting();
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The engine's pack being written. It stops at the next tensor once the
/// dictation is cancelled.
struct PackWriter<'a> {
    file: FileSink,
    cancel: &'a AtomicBool,
}

impl PackWriter<'_> {
    fn go_on(&self) -> speech::Result<()> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(speech::Error::Model("Cancelled.".into()));
        }
        Ok(())
    }
}

impl pack::Sink for PackWriter<'_> {
    fn matrix(
        &mut self,
        name: &str,
        rows: &speech::quant::Rows,
        n: usize,
        k: usize,
        precision: speech::Precision,
    ) -> speech::Result<()> {
        self.go_on()?;
        pack::Sink::matrix(&mut self.file, name, rows, n, k, precision)
    }

    fn f32(&mut self, name: &str, shape: &[usize], data: &[f32]) -> speech::Result<()> {
        self.go_on()?;
        pack::Sink::f32(&mut self.file, name, shape, data)
    }

    fn bytes(&mut self, name: &str, data: &[u8]) -> speech::Result<()> {
        self.go_on()?;
        pack::Sink::bytes(&mut self.file, name, data)
    }
}

/// Converts the model at `source` into the pack `out`, in its 8-bit
/// precision. The engine writes `out.partial` and renames it once whole.
fn pack_model(
    model: &SpeechModel,
    source: &Path,
    out: &Path,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let engine = super::service::engine_type(model.family);
    let precision = engine.recommended_precision();
    let written = (|| {
        let metadata = pack::metadata_for(source, precision);
        let mut writer = PackWriter {
            file: FileSink::create(out, engine.pack_model(), metadata)?,
            cancel,
        };
        engine.convert_path(source, precision, &mut writer)?;
        writer.file.finish()
    })();
    written.map_err(|error| {
        let mut partial = out.as_os_str().to_owned();
        partial.push(".partial");
        let _ = fs::remove_file(partial);
        if cancel.load(Ordering::SeqCst) {
            Error::Cancelled
        } else {
            Error::Failed(format!("Couldn't prepare the speech model ({error})."))
        }
    })
}

/// `source.json` says what the pack was made from.
fn source_json(model: &SpeechModel) -> String {
    let files: Vec<serde_json::Value> = model
        .files
        .iter()
        .map(|file| {
            serde_json::json!({ "name": file.name, "bytes": file.bytes, "sha256": file.sha256 })
        })
        .collect();
    let source = serde_json::json!({
        "repository": model.repo,
        "revision": model.revision,
        "files": files,
        "licence": model.licence,
        "precision": model.precision,
        "packed": chrono::Utc::now().to_rfc3339(),
    });
    serde_json::to_string_pretty(&source).unwrap_or_default() + "\n"
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
