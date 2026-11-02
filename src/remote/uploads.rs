//! The files the web page uploads before it sends them with a message. Each
//! is written to `.<id>.part` in the project's `.arko/attachments` folder as
//! its bytes come, and renamed to its own name, never over another file,
//! once whole. A message then names the uploads it sends, and they become
//! its files ([`crate::attachments`]). Uploads that are never sent are
//! removed.
//!
//! What an upload holds is the length of its part file, read while the
//! request that writes it holds its writer, so no count is kept beside it.
//! A request that resumes says where it starts (the `Upload-Offset` header)
//! and is refused unless that is the length. The newest request for an
//! upload wins, because a phone's dropped connection can stay half open
//! here after the phone has tried again. A request ends after
//! [`STALL`] without bytes.
//!
//! Every file operation runs off the runtime's workers, on its blocking
//! pool (`tokio::fs` and `spawn_blocking`). The table's lock is taken in a
//! statement of its own and never held across an `.await`.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Body;
use futures_util::StreamExt as _;
use tokio::sync::watch;

use super::protocol::{UploadState, size};
use crate::arko_dir::Folder;
use crate::attachments::{self, Attached, FOLDER, FileRef, IGNORE_TEXT, Kind};
use crate::lock;

/// The space that must stay free on the disk once a file is saved.
pub(super) const FREE_FLOOR: u64 = 2 << 30;

/// The most uploads that wait to be sent.
const MAX_UNSENT: usize = 32;

/// A request that sends no bytes for this long ends.
pub(super) const STALL: Duration = Duration::from_secs(20);

/// An unsent upload no request touched for this long is removed, and so is
/// a part file left this long by an Arko that stopped.
const ABANDONED: Duration = Duration::from_secs(24 * 60 * 60);

/// How much of a file its kind is read from.
const HEAD_BYTES: usize = 64 * 1024;

/// The bytes a request gathers before it writes them.
const WRITE_BYTES: usize = 256 * 1024;

/// Why an upload's request didn't do what it asked.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refused {
    /// The file is over the size limit, or more came than its size.
    TooLarge(String),
    /// The disk lacks the room.
    NoSpace(String),
    /// The project's folder can't be made or written.
    Unwritable(String),
    /// Too many uploads wait to be sent.
    TooMany(String),
    /// No such upload, or it was removed.
    Gone(String),
    /// The request starts elsewhere than at what Arko holds, or a newer
    /// request took over. It holds this many bytes.
    Elsewhere(u64),
    /// The request sent no bytes for [`STALL`], or broke off. It holds
    /// this many bytes.
    Stalled(u64),
    Failed(String),
}

/// The uploads of one server, into the project `workspace`, whose folder
/// follows the settings in Arko's folder `root`.
pub(super) struct Uploads {
    workspace: PathBuf,
    root: PathBuf,
    table: Mutex<HashMap<String, Arc<Upload>>>,
    /// Reads the free space on a disk, as [`free_bytes`] does (a test
    /// gives its own).
    free: fn(&Path) -> Option<u64>,
    #[cfg(test)]
    disk: tests::Disk,
}

struct Upload {
    /// The name it is saved under, when no file has it.
    name: String,
    size: u64,
    folder: Folder,
    part: String,
    /// The part file, while bytes are to come. The request that writes
    /// holds it.
    writer: tokio::sync::Mutex<Option<File>>,
    /// The number of the newest request. An older one stops when it grows.
    newest: watch::Sender<u64>,
    state: Mutex<State>,
}

#[derive(Debug, Clone)]
struct State {
    touched: Instant,
    /// The file, once whole and saved.
    done: Option<(FileRef, Kind)>,
}

impl Uploads {
    pub(super) fn new(workspace: PathBuf, root: PathBuf) -> Self {
        Uploads {
            workspace,
            root,
            table: Mutex::new(HashMap::new()),
            free: free_bytes,
            #[cfg(test)]
            disk: tests::Disk::default(),
        }
    }

    /// The project the files go in.
    pub(super) fn workspace(&self) -> &Path {
        &self.workspace
    }

    #[cfg(test)]
    fn with_free(mut self, free: fn(&Path) -> Option<u64>) -> Self {
        self.free = free;
        self
    }

    /// The upload `id`.
    fn get(&self, id: &str) -> Result<Arc<Upload>, Refused> {
        let upload = lock(&self.table).get(id).cloned();
        upload.ok_or_else(|| Refused::Gone("Arko no longer has this file. Attach it again.".into()))
    }

    /// Starts an upload of `size` bytes, from a file the browser names
    /// `name`, and gives its ID.
    pub(super) async fn create(&self, name: &str, size_bytes: u64) -> Result<String, Refused> {
        let name = attachments::sanitize_name(name);
        if size_bytes > attachments::MAX_FILE_BYTES {
            return Err(Refused::TooLarge(format!(
                "{name} is {}, and a file can be {} at most.",
                size(size_bytes),
                size(attachments::MAX_FILE_BYTES)
            )));
        }
        self.remove_abandoned().await;
        let waiting = lock(&self.table).len();
        if waiting >= MAX_UNSENT {
            return Err(Refused::TooMany(format!(
                "{waiting} files wait to be sent. Send or remove some first."
            )));
        }
        let id = super::random_token(16);
        let part = format!(".{id}.part");
        let (workspace, root, free, made) = (
            self.workspace.clone(),
            self.root.clone(),
            self.free,
            part.clone(),
        );
        let named = name.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let folder =
                folder(&workspace, &root).map_err(|why| Refused::Unwritable(sentence(why)))?;
            if let Some(left) = free(folder.real())
                && left < size_bytes.saturating_add(FREE_FLOOR)
            {
                return Err(Refused::NoSpace(format!(
                    "There isn’t room for {named}. The computer has {} free.",
                    size(left)
                )));
            }
            let file = folder.create_file(&made).map_err(|error| {
                Refused::Unwritable(format!(
                    "Couldn’t write in {} ({}).",
                    folder.path().display(),
                    crate::profiles::reason_of(&error)
                ))
            })?;
            Ok((folder, file))
        })
        .await;
        let (folder, file) = match opened {
            Ok(opened) => opened?,
            Err(error) => return Err(Refused::Failed(format!("Couldn’t save {name} ({error})."))),
        };
        let upload = Arc::new(Upload {
            name,
            size: size_bytes,
            folder,
            part,
            writer: tokio::sync::Mutex::new(Some(file)),
            newest: watch::Sender::new(0),
            state: Mutex::new(State {
                touched: Instant::now(),
                done: None,
            }),
        });
        lock(&self.table).insert(id.clone(), upload);
        Ok(id)
    }

    /// Writes `body` into the upload `id` from `offset`, which must be what
    /// it holds, and saves the file once whole. It stops when a newer
    /// request for the upload comes, when [`STALL`] passes without bytes,
    /// or when more comes than the file's size.
    pub(super) async fn put(
        &self,
        id: &str,
        offset: u64,
        body: Body,
    ) -> Result<UploadState, Refused> {
        let upload = self.get(id)?;
        upload.touch();
        let (mut newer, mine) = upload.take_over();
        let mut writer = upload.writer.lock().await;
        let Some(file) = writer.take() else {
            return upload.state_without_writer();
        };
        let Some((file, held)) = on_disk(file, |file| Ok(file.metadata()?.len())).await else {
            return Err(lost(&upload.name));
        };
        let mut received = match held {
            Ok(held) => held,
            Err(error) => {
                *writer = Some(file);
                return Err(write_failed(&upload.name, &error));
            }
        };
        if *upload.newest.borrow() != mine || offset != received {
            *writer = Some(file);
            return Err(Refused::Elsewhere(received));
        }
        let mut file = Some(file);
        let mut waiting = Vec::new();
        let mut stream = body.into_data_stream();
        let ended = loop {
            let chunk = tokio::select! {
                chunk = tokio::time::timeout(STALL, stream.next()) => chunk,
                _ = newer.changed() => break End::TakenOver,
            };
            let bytes = match chunk {
                Err(_) | Ok(Some(Err(_))) => break End::Stalled,
                Ok(None) => break End::Body,
                Ok(Some(Ok(bytes))) => bytes,
            };
            if received + (waiting.len() + bytes.len()) as u64 > upload.size {
                break End::TooLarge;
            }
            waiting.extend_from_slice(&bytes);
            if waiting.len() < WRITE_BYTES {
                continue;
            }
            match self.append(file.take(), std::mem::take(&mut waiting)).await {
                (Some(back), Ok(written)) => (file, received) = (Some(back), received + written),
                (back, Err(error)) => {
                    file = back;
                    break End::Failed(error);
                }
                (None, Ok(_)) => break End::Lost,
            }
        };
        // What came counts, whatever ended the request.
        let (back, rest) = self.append(file.take(), waiting).await;
        let ended = match (ended, rest) {
            (ended @ (End::Failed(_) | End::Lost), _) => ended,
            (_, Err(error)) => End::Failed(error),
            (ended, Ok(written)) => {
                received += written;
                ended
            }
        };
        upload.touch();
        let Some(back) = back else {
            return Err(lost(&upload.name));
        };
        let failed = match ended {
            End::Body if received == upload.size => {
                let Some((back, synced)) = on_disk(back, |file| file.sync_all()).await else {
                    return Err(lost(&upload.name));
                };
                match synced {
                    Ok(()) => {
                        let saved = upload.clone();
                        let done = tokio::task::spawn_blocking(move || {
                            drop(back);
                            saved.save()
                        })
                        .await;
                        let done = done.map_err(|_| lost(&upload.name))??;
                        lock(&upload.state).done = Some(done);
                        return Ok(upload.state_now(upload.size));
                    }
                    Err(error) => {
                        *writer = Some(back);
                        return Err(write_failed(&upload.name, &error));
                    }
                }
            }
            End::Body => Ok(upload.state_now(received)),
            End::TakenOver => Err(Refused::Elsewhere(received)),
            End::Stalled => Err(Refused::Stalled(received)),
            End::TooLarge => Err(Refused::TooLarge(format!(
                "More of {} came than its size. Attach it again.",
                upload.name
            ))),
            End::Failed(error) => Err(write_failed(&upload.name, &error)),
            End::Lost => Err(lost(&upload.name)),
        };
        *writer = Some(back);
        failed
    }

    /// Writes `bytes` at the end of `file` on the runtime's blocking pool,
    /// and gives the file back with how many bytes it wrote. Without a file
    /// (one a write lost), nothing is written.
    async fn append(
        &self,
        file: Option<File>,
        bytes: Vec<u8>,
    ) -> (Option<File>, std::io::Result<u64>) {
        let Some(file) = file else {
            return (None, Ok(0));
        };
        if bytes.is_empty() {
            return (Some(file), Ok(0));
        }
        let written = bytes.len() as u64;
        #[cfg(test)]
        let disk = self.disk;
        let write = move |file: &mut File| {
            use std::io::Write as _;
            #[cfg(test)]
            std::thread::sleep(disk.delay);
            file.write_all(&bytes).map(|()| written)
        };
        #[cfg(test)]
        if disk.on_worker {
            let mut file = file;
            let done = write(&mut file);
            return (Some(file), done);
        }
        match on_disk(file, write).await {
            Some((file, done)) => (Some(file), done),
            None => (None, Ok(0)),
        }
    }

    /// What the upload `id` holds. A request still writing it stops first,
    /// so the page can go on from there.
    pub(super) async fn state(&self, id: &str) -> Result<UploadState, Refused> {
        let upload = self.get(id)?;
        upload.touch();
        let _ = upload.take_over();
        let mut writer = upload.writer.lock().await;
        let Some(file) = writer.take() else {
            return upload.state_without_writer();
        };
        let Some((file, held)) = on_disk(file, |file| Ok(file.metadata()?.len())).await else {
            return Err(lost(&upload.name));
        };
        *writer = Some(file);
        match held {
            Ok(held) => Ok(upload.state_now(held)),
            Err(error) => Err(write_failed(&upload.name, &error)),
        }
    }

    /// Removes the upload `id` and its file, unless it was sent.
    pub(super) async fn remove(&self, id: &str) -> Result<(), Refused> {
        let upload = lock(&self.table).remove(id);
        let upload = upload.ok_or_else(|| Refused::Gone("Arko no longer has this file.".into()))?;
        delete(upload).await;
        Ok(())
    }

    /// The uploads `ids`, whole, as files to send with a message, each with
    /// its bytes when they may go to the model. They wait still, until
    /// [`sent`](Self::sent).
    pub(super) async fn take(&self, ids: &[String]) -> Result<Vec<Attached>, String> {
        if ids.len() > attachments::MAX_FILES {
            return Err(format!(
                "A message takes {} files at most. Remove some, and send them with another.",
                attachments::MAX_FILES
            ));
        }
        let mut uploads = Vec::new();
        for id in ids {
            let Ok(upload) = self.get(id) else {
                return Err("Arko no longer has a file you attached. Attach it again.".into());
            };
            upload.touch();
            let done = lock(&upload.state).done.clone();
            let Some(done) = done else {
                return Err(format!("{} isn’t uploaded yet.", upload.name));
            };
            uploads.push((upload, done));
        }
        tokio::task::spawn_blocking(move || {
            uploads
                .into_iter()
                .map(|(upload, (file, kind))| {
                    let most = match kind {
                        Kind::Image => attachments::MAX_IMAGE_BYTES,
                        Kind::Text => attachments::INLINE_MAX_BYTES as u64,
                        Kind::File => 0,
                    };
                    let content = if file.bytes <= most {
                        let bytes = upload.folder.read(&file.name).map_err(|error| {
                            format!(
                                "Couldn’t read {} ({}).",
                                file.path.display(),
                                crate::profiles::reason_of(&error)
                            )
                        })?;
                        Some(Arc::from(bytes))
                    } else {
                        None
                    };
                    Ok(Attached {
                        file,
                        kind,
                        content,
                    })
                })
                .collect()
        })
        .await
        .map_err(|error| format!("Couldn’t read the files ({error})."))?
    }

    /// The uploads `ids` went with a message, so they are files of the
    /// project now.
    pub(super) fn sent(&self, ids: &[String]) {
        let mut table = lock(&self.table);
        for id in ids {
            table.remove(id);
        }
    }

    /// Removes every upload not sent, with its file, as the server stops.
    pub(super) async fn remove_unsent(&self) {
        let uploads: Vec<Arc<Upload>> = lock(&self.table)
            .drain()
            .map(|(_, upload)| upload)
            .collect();
        for upload in uploads {
            delete(upload).await;
        }
    }

    /// Removes the uploads no request touched for [`ABANDONED`].
    async fn remove_abandoned(&self) {
        let now = Instant::now();
        let uploads: Vec<(String, Arc<Upload>)> = lock(&self.table)
            .iter()
            .map(|(id, upload)| (id.clone(), upload.clone()))
            .collect();
        let old: Vec<String> = uploads
            .into_iter()
            .filter(|(_, upload)| now.duration_since(upload.touched()) > ABANDONED)
            .map(|(id, _)| id)
            .collect();
        let abandoned: Vec<Arc<Upload>> = {
            let mut table = lock(&self.table);
            old.iter().filter_map(|id| table.remove(id)).collect()
        };
        for upload in abandoned {
            delete(upload).await;
        }
    }
}

/// How a request's bytes stopped coming.
enum End {
    Body,
    TakenOver,
    Stalled,
    TooLarge,
    Failed(std::io::Error),
    /// A write that panicked took the file with it.
    Lost,
}

impl Upload {
    fn touch(&self) {
        lock(&self.state).touched = Instant::now();
    }

    fn touched(&self) -> Instant {
        lock(&self.state).touched
    }

    /// Makes this request the newest, which stops an older one, and gives
    /// what tells of a newer one and this one's number.
    fn take_over(&self) -> (watch::Receiver<u64>, u64) {
        self.newest.send_modify(|newest| *newest += 1);
        let mut newer = self.newest.subscribe();
        let mine = *newer.borrow_and_update();
        (newer, mine)
    }

    /// The state of an upload whose part file no request holds: saved,
    /// or lost to a write that panicked.
    fn state_without_writer(&self) -> Result<UploadState, Refused> {
        let saved = lock(&self.state).done.is_some();
        match saved {
            true => Ok(self.state_now(self.size)),
            false => Err(lost(&self.name)),
        }
    }

    fn state_now(&self, received: u64) -> UploadState {
        let kind = lock(&self.state).done.as_ref().map(|(_, kind)| *kind);
        UploadState {
            received,
            size: self.size,
            done: kind.is_some(),
            kind,
        }
    }

    /// Saves the whole part file under the upload's name, or the first
    /// numbered one no file has, with its type, its kind and a text's
    /// lines. Blocks.
    fn save(&self) -> Result<(FileRef, Kind), Refused> {
        let failed = |error: std::io::Error| write_failed(&self.name, &error);
        let (kind, mime, lines) = self.kind().map_err(failed)?;
        let mut name = self.name.clone();
        for n in 2.. {
            match self.folder.rename_new(&self.part, &name) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => {
                    name = attachments::numbered_name(&self.name, n);
                }
                Err(error) => return Err(failed(error)),
            }
        }
        let file = FileRef {
            path: self.folder.path().join(&name),
            name,
            mime: mime.to_string(),
            bytes: self.size,
            lines: (kind == Kind::Text).then_some(lines),
        };
        Ok((file, kind))
    }

    /// The part file's kind and type, from its first [`HEAD_BYTES`], and a
    /// text's lines. Blocks.
    fn kind(&self) -> std::io::Result<(Kind, &'static str, u64)> {
        use std::io::Read as _;
        let mut file = self.folder.open_file(&self.part)?;
        let mut head = Vec::with_capacity(HEAD_BYTES);
        (&mut file).take(HEAD_BYTES as u64).read_to_end(&mut head)?;
        let (kind, mime) = attachments::sniff(&head, &self.name);
        if kind != Kind::Text {
            return Ok((kind, mime, 0));
        }
        let breaks = |bytes: &[u8]| bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        let (mut lines, mut last) = (breaks(&head), head.last().copied());
        let mut buffer = vec![0; HEAD_BYTES];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            lines += breaks(&buffer[..read]);
            last = Some(buffer[read - 1]);
        }
        // A last line without a line break counts too.
        Ok((
            kind,
            mime,
            lines + u64::from(last.is_some_and(|last| last != b'\n')),
        ))
    }
}

/// Runs `work` on `file` on the runtime's blocking pool, and gives the file
/// back with what it did, or `None` when it panicked.
async fn on_disk<T: Send + 'static>(
    mut file: File,
    work: impl FnOnce(&mut File) -> std::io::Result<T> + Send + 'static,
) -> Option<(File, std::io::Result<T>)> {
    tokio::task::spawn_blocking(move || {
        let done = work(&mut file);
        (file, done)
    })
    .await
    .ok()
}

fn lost(name: &str) -> Refused {
    Refused::Failed(format!("Couldn’t save {name}. Attach it again."))
}

fn write_failed(name: &str, error: &std::io::Error) -> Refused {
    if error.kind() == std::io::ErrorKind::StorageFull {
        return Refused::NoSpace("The computer’s disk is full.".into());
    }
    Refused::Failed(format!(
        "Couldn’t save {name} ({}).",
        crate::profiles::reason_of(error)
    ))
}

/// `words` ending with a full stop.
fn sentence(words: String) -> String {
    if words.ends_with('.') {
        words
    } else {
        format!("{words}.")
    }
}

/// Stops the upload's request and removes its file, part or whole.
async fn delete(upload: Arc<Upload>) {
    let _ = upload.take_over();
    let file = upload.writer.lock().await.take();
    let _ = tokio::task::spawn_blocking(move || {
        let whole = file.is_none();
        drop(file);
        let done = lock(&upload.state).done.clone();
        let name = match (whole, done) {
            (true, Some((file, _))) => file.name,
            _ => upload.part.clone(),
        };
        if let Err(error) = upload.folder.remove_file(&name)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not remove the upload {name} ({error})");
        }
    })
    .await;
}

/// The project `workspace`'s attachments folder, made as the `.arko`
/// folders of other project files are ([`crate::arko_dir::ensure_in`],
/// which follows the setting in Arko's folder `root`), with a `.gitignore`
/// of its own whenever it has none. Refuses a `.arko` or an `attachments`
/// that is a link. It reads and writes files, so call it off the runtime.
pub(super) fn folder(workspace: &Path, root: &Path) -> Result<Folder, String> {
    let folder = crate::arko_dir::ensure_in(workspace, root, FOLDER)?;
    if folder
        .entry(crate::arko_dir::IGNORE_FILE)
        .is_ok_and(|entry| entry.is_some())
    {
        return Ok(folder);
    }
    match folder.create_whole(crate::arko_dir::IGNORE_FILE, IGNORE_TEXT.as_bytes()) {
        Ok(()) => Ok(folder),
        // (Whatever is there, a file the user edited too, stays.)
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(folder),
        Err(error) => Err(format!(
            "Couldn't write {} ({})",
            folder.path().join(crate::arko_dir::IGNORE_FILE).display(),
            crate::profiles::reason_of(&error)
        )),
    }
}

/// The project `workspace`'s attachments folder, when there is one. Refuses
/// a link as [`folder`] does.
pub(super) fn existing_folder(workspace: &Path) -> Result<Option<Folder>, String> {
    crate::arko_dir::open_in(workspace, &[crate::arko_dir::DIR, FOLDER], false)
}

/// The bytes this user may still write on the disk that holds `path`, or
/// `None` when the system doesn't say.
pub(super) fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` is plain data, which zeroes make valid.
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a C string, and `stats` is ours to write.
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        #[allow(clippy::useless_conversion)]
        Some(u64::from(stats.f_bavail).saturating_mul(u64::from(stats.f_frsize)))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: `wide` ends with a NUL, `free` is ours to write, and the
        // totals not asked for are null.
        let done = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (done != 0).then_some(free)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

/// Removes the part files an Arko left in the project `workspace`'s
/// attachments folder more than [`ABANDONED`] ago. A newer one may be
/// another session's upload. Blocks.
pub(super) fn remove_left_parts(workspace: &Path) {
    let Ok(Some(folder)) = existing_folder(workspace) else {
        return;
    };
    let Ok(names) = folder.names() else {
        return;
    };
    let now = SystemTime::now();
    for name in names {
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with('.') && name.ends_with(".part")) {
            continue;
        }
        let old = folder
            .open_file(name)
            .and_then(|file| file.metadata())
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > ABANDONED);
        if old {
            let _ = folder.remove_file(name);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// How a test's disk writes: each write first waits `delay`, and with
    /// `on_worker` it runs on the runtime's worker that asked, as a
    /// blocking write would.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) struct Disk {
        pub delay: Duration,
        pub on_worker: bool,
    }

    fn uploads(temp: &Path) -> (Uploads, PathBuf) {
        let work = temp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        (Uploads::new(work.clone(), temp.join("arko")), work)
    }

    fn folder_of(work: &Path) -> PathBuf {
        work.join(".arko").join(FOLDER)
    }

    /// Bytes that aren't text.
    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn whole(bytes: &[u8]) -> Body {
        Body::from(bytes.to_vec())
    }

    /// A body of `chunks`, each as it comes.
    fn chunks(chunks: Vec<Vec<u8>>) -> Body {
        let chunks = chunks
            .into_iter()
            .map(|chunk| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)));
        Body::from_stream(futures_util::stream::iter(chunks.collect::<Vec<_>>()))
    }

    /// A body that sends `bytes`, then breaks off.
    fn cut(bytes: &[u8]) -> Body {
        let chunks = vec![
            Ok(axum::body::Bytes::from(bytes.to_vec())),
            Err(std::io::Error::other("the connection broke")),
        ];
        Body::from_stream(futures_util::stream::iter(chunks))
    }

    /// A body that sends `bytes`, then nothing more and no end.
    fn stalled(bytes: &[u8]) -> Body {
        let first = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from(bytes.to_vec()),
        )]);
        Body::from_stream(first.chain(futures_util::stream::pending()))
    }

    fn sha(bytes: &[u8]) -> Vec<u8> {
        use sha2::Digest as _;
        sha2::Sha256::digest(bytes).to_vec()
    }

    fn names(work: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder_of(work))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A whole file goes under its own name, for its owner alone, and no
    /// part file is left.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_whole_file_is_saved_under_its_name() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let bytes = data(700_000);
        let id = uploads
            .create("../../x/RPReplay.MP4", bytes.len() as u64)
            .await
            .unwrap();
        let state = uploads.put(&id, 0, whole(&bytes)).await.unwrap();
        assert_eq!(
            state,
            UploadState {
                received: 700_000,
                size: 700_000,
                done: true,
                kind: Some(Kind::File),
            }
        );
        let saved = folder_of(&work).join("RPReplay.MP4");
        assert_eq!(std::fs::read(&saved).unwrap(), bytes);
        assert_eq!(names(&work), [".gitignore", "RPReplay.MP4"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&saved).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Asked again, it says the same.
        assert_eq!(uploads.state(&id).await.unwrap(), state);
        assert_eq!(uploads.put(&id, 0, whole(b"")).await.unwrap(), state);
    }

    /// An upload whose connection broke goes on from what Arko holds,
    /// and only from there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_broken_upload_goes_on_from_what_arko_holds() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let bytes = data(1_000_000);
        let id = uploads
            .create("screen.mov", bytes.len() as u64)
            .await
            .unwrap();
        let first = 600_000;
        let broke = uploads.put(&id, 0, cut(&bytes[..first])).await;
        assert_eq!(broke, Err(Refused::Stalled(first as u64)));
        assert_eq!(uploads.state(&id).await.unwrap().received, first as u64);
        assert_eq!(
            uploads.put(&id, 0, whole(&bytes)).await,
            Err(Refused::Elsewhere(first as u64))
        );
        let state = uploads
            .put(&id, first as u64, whole(&bytes[first..]))
            .await
            .unwrap();
        assert!(state.done);
        let saved = std::fs::read(folder_of(&work).join("screen.mov")).unwrap();
        assert_eq!(sha(&saved), sha(&bytes));
    }

    /// A request still waiting on a dropped connection ends as soon as a
    /// newer one asks, and what it got counts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_newest_request_takes_over_from_a_stalled_one() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let uploads = Arc::new(uploads);
        let bytes = data(300_000);
        let id = uploads.create("a.bin", bytes.len() as u64).await.unwrap();
        let first = 100_000;
        let old = tokio::spawn({
            let (uploads, id, bytes) = (uploads.clone(), id.clone(), bytes.clone());
            async move { uploads.put(&id, 0, stalled(&bytes[..first])).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        let state = uploads.state(&id).await.unwrap();
        assert_eq!(state.received, first as u64);
        assert_eq!(old.await.unwrap(), Err(Refused::Elsewhere(first as u64)));
        let done = uploads
            .put(&id, first as u64, whole(&bytes[first..]))
            .await
            .unwrap();
        assert!(done.done);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            std::fs::read(folder_of(&work).join("a.bin")).unwrap(),
            bytes
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_over_its_size_or_the_limit_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let refused = uploads.create("huge.mov", (1 << 30) + 1).await.unwrap_err();
        assert_eq!(
            refused,
            Refused::TooLarge("huge.mov is 1.0 GB, and a file can be 1.0 GB at most.".into())
        );
        assert!(!folder_of(&work).exists(), "nothing is made for it");
        let id = uploads.create("a.bin", 10).await.unwrap();
        let refused = uploads.put(&id, 0, whole(&data(11))).await.unwrap_err();
        assert!(matches!(refused, Refused::TooLarge(_)), "{refused:?}");
        assert_eq!(uploads.state(&id).await.unwrap().received, 0);
    }

    /// A file that would leave less than the floor free is refused before
    /// any of it comes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_with_no_room_on_the_disk_is_refused_first() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let uploads = uploads.with_free(|_| Some(FREE_FLOOR + 50));
        assert!(uploads.create("small.txt", 50).await.is_ok());
        let refused = uploads.create("large.mov", 51).await.unwrap_err();
        assert_eq!(
            refused,
            Refused::NoSpace(
                "There isn’t room for large.mov. The computer has 2.0 GB free.".into()
            )
        );
        assert_eq!(names(&work).len(), 2, "{:?}", names(&work));
    }

    /// Files of one name get the next free number, never one over another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn files_of_one_name_never_replace_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let uploads = Arc::new(uploads);
        std::fs::create_dir_all(folder_of(&work)).unwrap();
        std::fs::write(folder_of(&work).join("image.jpg"), b"kept").unwrap();
        let tasks: Vec<_> = (0..8u8)
            .map(|n| {
                let uploads = uploads.clone();
                tokio::spawn(async move {
                    let bytes = vec![0xFF, 0xD8, 0xFF, n];
                    let id = uploads.create("image.jpg", 4).await.unwrap();
                    uploads.put(&id, 0, whole(&bytes)).await.unwrap();
                    let taken = uploads.take(std::slice::from_ref(&id)).await.unwrap();
                    (n, taken[0].file.name.clone())
                })
            })
            .collect();
        let mut saved = Vec::new();
        for task in tasks {
            let (n, name) = task.await.unwrap();
            let bytes = std::fs::read(folder_of(&work).join(&name)).unwrap();
            assert_eq!(bytes, [0xFF, 0xD8, 0xFF, n], "{name}");
            saved.push(name);
        }
        saved.sort();
        let mut wanted: Vec<String> = (2..=9).map(|n| format!("image-{n}.jpg")).collect();
        wanted.sort();
        assert_eq!(saved, wanted);
        assert_eq!(
            std::fs::read(folder_of(&work).join("image.jpg")).unwrap(),
            b"kept"
        );
    }

    /// A PNG of one pixel.
    fn pixel() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
            .unwrap()
    }

    /// The bytes of an image and of a short text go with a message, and a
    /// text's lines are counted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_takes_the_bytes_that_may_reach_the_model() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, _work) = uploads(temp.path());
        let mut ids = Vec::new();
        for (name, bytes) in [
            ("bug.png", pixel()),
            ("notes.md", b"# Notes\nOne more line".to_vec()),
            ("RPReplay.MP4", data(5_000)),
        ] {
            let id = uploads.create(name, bytes.len() as u64).await.unwrap();
            uploads.put(&id, 0, whole(&bytes)).await.unwrap();
            ids.push(id);
        }
        let taken = uploads.take(&ids).await.unwrap();
        assert_eq!(taken[0].kind, Kind::Image);
        assert_eq!(taken[0].file.mime, "image/png");
        assert_eq!(taken[0].content.as_deref(), Some(&pixel()[..]));
        assert_eq!(taken[1].kind, Kind::Text);
        assert_eq!(taken[1].file.lines, Some(2));
        assert_eq!(
            taken[1].content.as_deref(),
            Some(&b"# Notes\nOne more line"[..])
        );
        assert_eq!(taken[2].kind, Kind::File);
        assert_eq!(taken[2].file.lines, None);
        assert_eq!(taken[2].content, None);
        // They wait until sent, and stay once sent.
        assert!(uploads.take(&ids).await.is_ok());
        uploads.sent(&ids);
        uploads.remove_unsent().await;
        assert!(uploads.take(&ids).await.is_err());
        assert!(taken.iter().all(|attached| attached.file.path.is_file()));
    }

    /// A file swapped for a link after it was saved isn't read through it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_swapped_for_a_link_isnt_read() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let secret = temp.path().join("id_ed25519");
        std::fs::write(&secret, "secret").unwrap();
        let id = uploads.create("notes.md", 5).await.unwrap();
        uploads.put(&id, 0, whole(b"notes")).await.unwrap();
        let saved = folder_of(&work).join("notes.md");
        std::fs::remove_file(&saved).unwrap();
        std::os::unix::fs::symlink(&secret, &saved).unwrap();
        let refused = uploads.take(std::slice::from_ref(&id)).await.unwrap_err();
        assert!(refused.starts_with("Couldn’t read"), "{refused}");
    }

    /// Removing an upload removes its file, part or whole, and stops its
    /// request. Removing those not sent, as the server stops, leaves the
    /// sent ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_upload_removed_takes_its_file_along() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, work) = uploads(temp.path());
        let uploads = Arc::new(uploads);
        let half = uploads.create("half.bin", 10).await.unwrap();
        let writing = tokio::spawn({
            let (uploads, half) = (uploads.clone(), half.clone());
            async move { uploads.put(&half, 0, stalled(&data(4))).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        uploads.remove(&half).await.unwrap();
        assert!(matches!(writing.await.unwrap(), Err(Refused::Elsewhere(4))));
        assert!(uploads.take(std::slice::from_ref(&half)).await.is_err());
        let mut whole_ids = Vec::new();
        for name in ["sent.bin", "unsent.bin", "removed.bin"] {
            let id = uploads.create(name, 3).await.unwrap();
            uploads.put(&id, 0, whole(&data(3))).await.unwrap();
            whole_ids.push(id);
        }
        uploads.sent(&whole_ids[..1]);
        uploads.remove(&whole_ids[2]).await.unwrap();
        uploads.remove_unsent().await;
        assert_eq!(names(&work), [".gitignore", "sent.bin"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn too_many_unsent_uploads_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let (uploads, _work) = uploads(temp.path());
        for n in 0..MAX_UNSENT {
            uploads.create(&format!("{n}.txt"), 1).await.unwrap();
        }
        let refused = uploads.create("one-more.txt", 1).await.unwrap_err();
        assert_eq!(
            refused,
            Refused::TooMany("32 files wait to be sent. Send or remove some first.".into())
        );
    }

    /// Part files an Arko left go after a day. A newer one may be another
    /// session's upload, and other files stay.
    #[test]
    fn part_files_left_behind_go_after_a_day() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(folder_of(&work)).unwrap();
        let old = folder_of(&work).join(".old.part");
        std::fs::write(&old, "x").unwrap();
        let two_days = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(two_days)
            .unwrap();
        std::fs::write(folder_of(&work).join(".new.part"), "x").unwrap();
        std::fs::write(folder_of(&work).join("old.part"), "x").unwrap();
        remove_left_parts(&work);
        assert_eq!(names(&work), [".new.part", "old.part"]);
        // A project without the folder is left as it is.
        remove_left_parts(&temp.path().join("other"));
        assert!(!temp.path().join("other").exists());
    }

    /// Runs `uploads` uploads of `size` bytes in `rounds` rounds each, all
    /// at once, on a runtime of two workers whose disk is `disk`, with a
    /// timeout, and gives the longest the runtime's timers waited
    /// meanwhile.
    fn longest_wait(uploads: usize, rounds: usize, size: usize, disk: Disk) -> Duration {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let temp = tempfile::tempdir().unwrap();
            let (store, work) = self::uploads(temp.path());
            let store = Arc::new(Uploads { disk, ..store });
            let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let heart = tokio::spawn({
                let beating = beating.clone();
                async move {
                    let mut longest = Duration::ZERO;
                    let mut last = Instant::now();
                    while beating.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        longest = longest.max(last.elapsed());
                        last = Instant::now();
                    }
                    longest
                }
            });
            let tasks: Vec<_> = (0..uploads)
                .map(|n| {
                    let store = store.clone();
                    tokio::spawn(async move {
                        for round in 0..rounds {
                            let bytes = data(size);
                            let name = format!("{n}-{round}.bin");
                            let id = store.create(&name, size as u64).await.unwrap();
                            let parts = bytes.chunks(WRITE_BYTES).map(<[u8]>::to_vec).collect();
                            assert!(store.put(&id, 0, chunks(parts)).await.unwrap().done);
                            let taken = store.take(std::slice::from_ref(&id)).await.unwrap();
                            assert_eq!(taken[0].file.bytes, size as u64);
                            store.sent(std::slice::from_ref(&id));
                        }
                    })
                })
                .collect();
            let all = futures_util::future::join_all(tasks);
            let done = tokio::time::timeout(Duration::from_secs(120), all).await;
            beating.store(false, std::sync::atomic::Ordering::Relaxed);
            for task in done.expect("the uploads finish") {
                task.unwrap();
            }
            assert_eq!(names(&work).len(), uploads * rounds + 1);
            heart.await.unwrap()
        })
    }

    /// Many uploads at once all finish, and the runtime's timers keep
    /// firing while they write.
    #[test]
    fn many_uploads_at_once_never_hold_up_the_runtime() {
        let longest = longest_wait(8, 50, 300_000, Disk::default());
        assert!(longest < Duration::from_millis(250), "{longest:?}");
    }

    /// A slow disk holds up no worker of the runtime, since every write
    /// runs on its blocking pool.
    #[test]
    fn a_slow_disk_holds_up_no_worker() {
        let slow = Disk {
            delay: Duration::from_millis(400),
            on_worker: false,
        };
        let longest = longest_wait(8, 1, 3 * WRITE_BYTES, slow);
        assert!(longest < Duration::from_millis(200), "{longest:?}");
    }

    /// The test above can tell. The same writes made on the workers hold up
    /// the runtime's timers.
    #[test]
    fn writes_on_the_workers_would_hold_up_the_runtime() {
        let blocking = Disk {
            delay: Duration::from_millis(400),
            on_worker: true,
        };
        let longest = longest_wait(8, 1, 3 * WRITE_BYTES, blocking);
        assert!(longest >= Duration::from_millis(300), "{longest:?}");
    }
    /// The folder keeps its files out of git whatever the setting says, and
    /// gets its `.gitignore` back when it's deleted.
    #[test]
    fn the_folder_keeps_its_files_out_of_git_whatever_the_setting() {
        let temp = tempfile::tempdir().unwrap();
        let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |args: &[&str]| {
            let done = std::process::Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(args)
                .output()
                .unwrap();
            assert!(done.status.success(), "{done:?}");
            String::from_utf8(done.stdout).unwrap()
        };
        git(&["init", "-q"]);
        crate::profiles::Preferences {
            keep_out_of_git: false,
            ..crate::profiles::Preferences::default()
        }
        .save(&root)
        .unwrap();
        let ignore = work.join(".arko").join(FOLDER).join(".gitignore");
        let folder = folder(&work, &root).unwrap();
        assert_eq!(folder.path(), work.join(".arko").join(FOLDER));
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), IGNORE_TEXT);
        // `.arko` itself has no `.gitignore` with the setting off.
        assert!(!work.join(".arko").join(".gitignore").exists());
        folder.create("RPReplay.MP4", b"video").unwrap();
        assert_eq!(git(&["status", "--porcelain", "--untracked-files=all"]), "");
        std::fs::remove_file(&ignore).unwrap();
        super::folder(&work, &root).unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), IGNORE_TEXT);
        // A file the user edited stays as it is.
        std::fs::write(&ignore, "*.mov\n").unwrap();
        super::folder(&work, &root).unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*.mov\n");
        assert!(existing_folder(&work).unwrap().is_some());
        assert!(
            existing_folder(&temp.path().join("other"))
                .unwrap()
                .is_none()
        );
    }

    /// A `.arko` or an attachments folder that is a link is refused, with
    /// the sentence the page shows.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_is_a_link_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, work.join(".arko")).unwrap();
        let refused = folder(&work, &root).unwrap_err();
        assert!(refused.contains("is a link"), "{refused}");
        assert!(existing_folder(&work).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// A read-only project can't take files, and says why.
    #[cfg(unix)]
    #[test]
    fn a_project_that_cant_be_written_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let (work, root) = (temp.path().join("work"), temp.path().join("arko"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o555)).unwrap();
        let refused = folder(&work, &root);
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o755)).unwrap();
        // (Root may write anywhere.)
        if let Err(refused) = refused {
            assert!(refused.starts_with("Couldn't make"), "{refused}");
            assert!(refused.contains("permission denied"), "{refused}");
        }
    }

    #[test]
    fn the_free_space_of_a_disk_is_known() {
        let temp = tempfile::tempdir().unwrap();
        let free = free_bytes(temp.path()).unwrap();
        assert!(free > 0);
        assert_eq!(free_bytes(&temp.path().join("not there")), None);
    }
}
