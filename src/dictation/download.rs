//! Downloading a speech model and packing it: each file from the model's
//! pinned revision, resumed where an earlier try stopped, checked against
//! its SHA-256, then converted into the engine's pack in the model's 8-bit
//! precision. The downloaded files go once the pack is written.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use speech::pack::{self, FileSink};

use super::Preparing;
use super::models::{PACK_FILE, RemoteFile, SpeechModel};

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

fn failed(why: impl std::fmt::Display) -> Error {
    Error::Failed(format!(
        "Couldn't download the speech model ({why}). {RESUME}"
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
    let unsaved = |error: std::io::Error| {
        Error::Failed(format!(
            "Couldn't save the speech model in {} ({error}).",
            dir.display()
        ))
    };
    fs::create_dir_all(&downloads).map_err(unsaved)?;
    download_files(model, &downloads, base, progress, cancel)?;
    // The notices first: the pack is what makes the model installed.
    fs::write(dir.join("NOTICE.txt"), model.notice()).map_err(unsaved)?;
    fs::write(dir.join("source.json"), source_json(model)).map_err(unsaved)?;
    let source = model
        .source_file
        .map_or(downloads.clone(), |file| downloads.join(file));
    pack_model(model, &source, &dir.join(PACK_FILE))?;
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(failed)?;
    // Hugging Face redirects to its file servers. Nothing secret is sent.
    let client = arko_ai::http_client_builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(concat!("arko/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(failed)?;
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
            &mut |done| {
                progress(Preparing::Downloading {
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
        let digest = sha256(&path).map_err(|error| {
            Error::Failed(format!(
                "Couldn't read the speech model's download in {} ({error}).",
                downloads.display()
            ))
        })?;
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

/// `future`, unless it takes longer than [`STALL`] (`stalled` says what
/// didn't come) or the dictation is cancelled first.
async fn waited<T>(
    future: impl std::future::Future<Output = T>,
    cancel: &AtomicBool,
    stalled: &str,
) -> Result<T, Error> {
    let cancelled = async {
        while !cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        result = tokio::time::timeout(STALL, future) => result.map_err(|_| failed(stalled)),
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
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> Result<(), Error> {
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
    let mut response = waited(request.send(), cancel, "Hugging Face didn't answer")
        .await?
        .map_err(|error| failed(reason(error)))?;
    let resumed = match response.status() {
        reqwest::StatusCode::PARTIAL_CONTENT => true,
        // The whole file: the server ignored the range.
        reqwest::StatusCode::OK => false,
        status => return Err(failed(format!("Hugging Face answered {status}"))),
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
        .map_err(failed)?;
    progress(have);
    loop {
        let chunk = match waited(response.chunk(), cancel, "no data came for a minute").await? {
            Err(error) => return Err(failed(reason(error))),
            Ok(None) => break,
            Ok(Some(chunk)) => chunk,
        };
        out.write_all(&chunk).map_err(failed)?;
        have += chunk.len() as u64;
        if have > file.bytes {
            drop(out);
            let _ = fs::remove_file(path);
            return Err(failed(format!("{} is larger than expected", file.name)));
        }
        progress(have);
    }
    out.flush().map_err(failed)?;
    if have < file.bytes {
        return Err(failed("the connection ended early"));
    }
    Ok(())
}

/// A request's error in a few words, without its URL.
fn reason(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "it timed out".into()
    } else if error.is_connect() {
        "couldn't reach Hugging Face".into()
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

/// A file's SHA-256, in hex.
fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
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

/// Converts the model at `source` into the pack `out`, in its 8-bit
/// precision. The engine writes `out.partial` and renames it once whole.
fn pack_model(model: &SpeechModel, source: &Path, out: &Path) -> Result<(), Error> {
    let engine = super::service::engine_type(model.family);
    let precision = engine.recommended_precision();
    let written = (|| {
        let metadata = pack::metadata_for(source, precision);
        let mut sink = FileSink::create(out, engine.pack_model(), metadata)?;
        engine.convert_path(source, precision, &mut sink)?;
        sink.finish()
    })();
    written.map_err(|error| {
        let mut partial = out.as_os_str().to_owned();
        partial.push(".partial");
        let _ = fs::remove_file(partial);
        Error::Failed(format!("Couldn't prepare the speech model ({error})."))
    })
}

/// `source.json`: what the pack was made from.
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
