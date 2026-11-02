//! The files the web page uploads before it sends them with a message. Each
//! is written to `.<id>.part` in the project's `.arko/attachments` folder as
//! its bytes come, and renamed to its own name, never over another file,
//! once whole. A message then claims the uploads it names, and they become
//! its files once it goes ([`crate::attachments`]). Uploads that are never
//! sent are removed.
//!
//! The table is the one owner of every upload. An upload is Reserved (its
//! slot and its bytes held against the limits), then Receiving, then
//! Complete, then Claimed by a message, and leaves the table once sent or
//! released. Every change of state happens under the table's lock, in a
//! statement of its own, and no disk work happens under it. The admission
//! of a new upload checks the uploads waiting and the bytes they still need
//! against the disk's free space together with its insert.
//!
//! What an upload holds is the length of its part file. A request that
//! resumes says where it starts (the `Upload-Offset` header) and is refused
//! unless that is the length. The newest request for an upload wins, because
//! a phone's dropped connection can stay half open here after the phone has
//! tried again, and a request ends after [`STALL`] without bytes.
//!
//! The part file stays in its upload's writer slot for good. A request takes
//! the slot's gate, and each disk step runs on the runtime's blocking pool
//! holding the gate, so a request cancelled mid-step leaves the step to
//! finish, the file in its slot and the gate shut until the disk is done.
//! The step that completes an upload also records it as Complete. A step
//! that holds the gate may take the table's lock, and the table's lock is
//! never held while a gate is awaited (a sweep only tries one), so the two
//! never wait on each other.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Body;
use futures_util::StreamExt as _;
use tokio::sync::{OwnedMutexGuard, watch};

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
    /// A message is sending it.
    Claimed(String),
    /// The request starts elsewhere than at what Arko holds, or a newer
    /// request took over. It holds this many bytes.
    Elsewhere(u64),
    /// The request sent no bytes for [`STALL`], or broke off. It holds
    /// this many bytes.
    Stalled(u64),
    Failed(String),
}

/// An upload's place in its life.
#[derive(Debug, Clone)]
enum State {
    /// Its slot and bytes are held while its part file is made.
    Reserved,
    /// Its bytes come into its part file.
    Receiving,
    /// Whole and saved under its name.
    Complete(FileRef, Kind),
    /// A message with the claim this number names is sending it.
    Claimed(FileRef, Kind, u64),
}

/// An upload as the table holds it.
struct Entry {
    upload: Arc<Upload>,
    state: State,
    /// The bytes still to come, held against the disk's free space.
    reserved: u64,
    /// When a request last used it.
    touched: Instant,
}

/// The uploads, by ID. Behind an `Arc`, so a disk step can record what it
/// did.
type Table = Arc<Mutex<HashMap<String, Entry>>>;

/// The uploads of one server, into the project `workspace`, whose folder
/// follows the settings in Arko's folder `root`.
pub(super) struct Uploads {
    workspace: PathBuf,
    root: PathBuf,
    table: Table,
    /// Numbers the messages' claims.
    claims: AtomicU64,
    /// Reads the free space on a disk, as [`free_bytes`] does (a test
    /// gives its own).
    free: fn(&Path) -> Option<u64>,
    #[cfg(test)]
    disk: tests::Disk,
}

/// What doesn't change of an upload.
struct Upload {
    /// The name it is saved under, when no file has it.
    name: String,
    size: u64,
    folder: Folder,
    part: String,
    /// The part file, from when it's made until the upload is whole. A
    /// request takes the gate for its disk steps.
    writer: Arc<tokio::sync::Mutex<Option<File>>>,
    /// The number of the newest request. An older one stops when it grows.
    newest: watch::Sender<u64>,
}

/// The files a message claimed, with their bytes when they may go to the
/// model. The claim is the message's until it is committed or released.
#[derive(Debug)]
pub(super) struct Claim {
    pub(super) number: u64,
    pub(super) files: Vec<Attached>,
}

impl Uploads {
    pub(super) fn new(workspace: PathBuf, root: PathBuf) -> Self {
        Uploads {
            workspace,
            root,
            table: Arc::new(Mutex::new(HashMap::new())),
            claims: AtomicU64::new(1),
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

    /// The upload `id` and its state now, touched.
    fn touch(&self, id: &str) -> Result<(Arc<Upload>, State), Refused> {
        let mut table = lock(&self.table);
        let entry = table.get_mut(id).ok_or_else(gone)?;
        entry.touched = Instant::now();
        Ok((entry.upload.clone(), entry.state.clone()))
    }

    /// The state of the upload `id` now, if it is still the upload
    /// `upload`.
    fn state_of(&self, id: &str, upload: &Arc<Upload>) -> Option<State> {
        let table = lock(&self.table);
        table
            .get(id)
            .filter(|entry| Arc::ptr_eq(&entry.upload, upload))
            .map(|entry| entry.state.clone())
    }

    /// Starts an upload of `size` bytes, from a file the browser names
    /// `name`, and gives its ID. Its slot among those waiting and its bytes
    /// are held from now on.
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
        let (workspace, root, free) = (self.workspace.clone(), self.root.clone(), self.free);
        let opened = tokio::task::spawn_blocking(move || {
            let folder =
                folder(&workspace, &root).map_err(|why| Refused::Unwritable(sentence(why)))?;
            let left = free(folder.real());
            Ok::<_, Refused>((folder, left))
        })
        .await;
        let (folder, left) = match opened {
            Ok(opened) => opened?,
            Err(error) => return Err(Refused::Failed(format!("Couldn’t save {name} ({error})."))),
        };
        let id = super::random_token(16);
        let upload = Arc::new(Upload {
            part: format!(".{id}.part"),
            name,
            size: size_bytes,
            folder,
            writer: Arc::new(tokio::sync::Mutex::new(None)),
            newest: watch::Sender::new(0),
        });
        self.reserve(&id, &upload, left)?;
        // (The step goes on if the request is cancelled, so the upload
        // never stays Reserved.)
        let (table, made) = (self.table.clone(), id.clone());
        let ready = upload.clone();
        let done = tokio::task::spawn_blocking(move || {
            let file = ready.folder.create_file(&ready.part);
            let mut slot = ready.writer.blocking_lock();
            let mut table = lock(&table);
            match file {
                Ok(file) => {
                    *slot = Some(file);
                    if let Some(entry) = table.get_mut(&made) {
                        entry.state = State::Receiving;
                    }
                    Ok(())
                }
                Err(error) => {
                    table.remove(&made);
                    Err(Refused::Unwritable(format!(
                        "Couldn’t write in {} ({}).",
                        ready.folder.path().display(),
                        crate::profiles::reason_of(&error)
                    )))
                }
            }
        })
        .await;
        match done {
            Ok(Ok(())) => Ok(id),
            Ok(Err(refused)) => Err(refused),
            Err(error) => {
                lock(&self.table).remove(&id);
                Err(Refused::Failed(format!(
                    "Couldn’t save {} ({error}).",
                    upload.name
                )))
            }
        }
    }

    /// Holds a slot among the uploads waiting and `upload`'s bytes, as
    /// `id`, when the disk has `left` bytes free, or says why not. Both
    /// checks and the insert are one step, so uploads started at once can't
    /// pass the limits together.
    fn reserve(&self, id: &str, upload: &Arc<Upload>, left: Option<u64>) -> Result<(), Refused> {
        let mut table = lock(&self.table);
        if table.len() >= MAX_UNSENT {
            return Err(Refused::TooMany(format!(
                "{} files wait to be sent. Send or remove some first.",
                table.len()
            )));
        }
        let held: u64 = table.values().map(|entry| entry.reserved).sum();
        if let Some(left) = left
            && left < held.saturating_add(upload.size).saturating_add(FREE_FLOOR)
        {
            return Err(Refused::NoSpace(format!(
                "There isn’t room for {}. Arko keeps {} of the computer’s disk free.",
                upload.name,
                size(FREE_FLOOR)
            )));
        }
        table.insert(
            id.to_string(),
            Entry {
                upload: upload.clone(),
                state: State::Reserved,
                reserved: upload.size,
                touched: Instant::now(),
            },
        );
        Ok(())
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
        let (upload, _) = self.touch(id)?;
        let (mut newer, mine) = upload.take_over();
        let gate = upload.writer.clone().lock_owned().await;
        match self.state_of(id, &upload) {
            Some(State::Receiving) => {}
            Some(State::Complete(_, kind) | State::Claimed(_, kind, _)) => {
                return Ok(upload.state_now(upload.size, Some(kind)));
            }
            Some(State::Reserved) | None => return Err(gone()),
        }
        let (mut gate, held) = self.disk(gate, slot_length).await?;
        let mut received = held.map_err(|error| write_failed(&upload.name, &error))?;
        if *newer.borrow() > mine || offset != received {
            return Err(Refused::Elsewhere(received));
        }
        let mut waiting = Vec::new();
        let mut stream = body.into_data_stream();
        let ended = loop {
            let chunk = tokio::select! {
                chunk = tokio::time::timeout(STALL, stream.next()) => chunk,
                changed = newer.changed() => {
                    if changed.is_err() || *newer.borrow_and_update() > mine {
                        break End::TakenOver;
                    }
                    continue;
                }
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
            let (back, written) = self
                .append(gate, id, &upload, std::mem::take(&mut waiting))
                .await?;
            gate = back;
            match written {
                Ok(written) => received += written,
                Err(error) => break End::Failed(error),
            }
        };
        // What came counts, whatever ended the request.
        let ended = match ended {
            End::Failed(error) => End::Failed(error),
            ended => {
                let (back, written) = self.append(gate, id, &upload, waiting).await?;
                gate = back;
                match written {
                    Ok(written) => {
                        received += written;
                        ended
                    }
                    Err(error) => End::Failed(error),
                }
            }
        };
        match ended {
            End::Body if received == upload.size => self.complete(gate, id, &upload).await,
            End::Body => Ok(upload.state_now(received, None)),
            End::TakenOver => Err(Refused::Elsewhere(received)),
            End::Stalled => Err(Refused::Stalled(received)),
            End::TooLarge => Err(Refused::TooLarge(format!(
                "More of {} came than its size. Attach it again.",
                upload.name
            ))),
            End::Failed(error) => Err(write_failed(&upload.name, &error)),
        }
    }

    /// Runs `work` on the writer's slot on the runtime's blocking pool,
    /// holding the gate, and gives the gate back with what it did. Cancelled
    /// while it runs, the step finishes and opens the gate itself.
    async fn disk<T: Send + 'static>(
        &self,
        mut gate: OwnedMutexGuard<Option<File>>,
        work: impl FnOnce(&mut Option<File>) -> std::io::Result<T> + Send + 'static,
    ) -> Result<(OwnedMutexGuard<Option<File>>, std::io::Result<T>), Refused> {
        tokio::task::spawn_blocking(move || {
            let done = work(&mut gate);
            (gate, done)
        })
        .await
        .map_err(|error| Refused::Failed(format!("Couldn’t save the file ({error}).")))
    }

    /// Writes `bytes` at the end of the upload `id`'s part file, when the
    /// disk keeps its floor after them, and counts them off its
    /// reservation. Gives the gate back with how many bytes it wrote.
    async fn append(
        &self,
        gate: OwnedMutexGuard<Option<File>>,
        id: &str,
        upload: &Upload,
        bytes: Vec<u8>,
    ) -> Result<(OwnedMutexGuard<Option<File>>, std::io::Result<u64>), Refused> {
        if bytes.is_empty() {
            return Ok((gate, Ok(0)));
        }
        let (table, id, free) = (self.table.clone(), id.to_string(), self.free);
        let folder = upload.folder.real().to_path_buf();
        #[cfg(test)]
        let disk = self.disk;
        let write = move |slot: &mut Option<File>| {
            use std::io::Write as _;
            #[cfg(test)]
            std::thread::sleep(disk.delay);
            let file = slot
                .as_mut()
                .ok_or_else(|| std::io::Error::other("the file is gone"))?;
            let written = bytes.len() as u64;
            if let Some(left) = free(&folder)
                && left < FREE_FLOOR.saturating_add(written)
            {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            file.write_all(&bytes)?;
            if let Some(entry) = lock(&table).get_mut(&id) {
                entry.reserved = entry.reserved.saturating_sub(written);
                entry.touched = Instant::now();
            }
            Ok(written)
        };
        #[cfg(test)]
        if disk.on_worker {
            let mut gate = gate;
            let done = write(&mut gate);
            return Ok((gate, done));
        }
        self.disk(gate, write).await
    }

    /// Saves the whole part file of the upload `id` under its name, or the
    /// first numbered one no file has, and records it as Complete, in one
    /// disk step. If the upload was removed meanwhile, the step removes the
    /// file it saved.
    async fn complete(
        &self,
        gate: OwnedMutexGuard<Option<File>>,
        id: &str,
        upload: &Arc<Upload>,
    ) -> Result<UploadState, Refused> {
        let (table, id, saving) = (self.table.clone(), id.to_string(), upload.clone());
        let (_gate, done) = self
            .disk(gate, move |slot| {
                if let Some(file) = slot.as_mut() {
                    file.sync_all()?;
                }
                // (Closed before it's renamed.)
                *slot = None;
                let (file, kind) = saving.save()?;
                let mut table = lock(&table);
                match table.get_mut(&id) {
                    Some(entry) if Arc::ptr_eq(&entry.upload, &saving) => {
                        entry.state = State::Complete(file, kind);
                        entry.reserved = 0;
                        entry.touched = Instant::now();
                        Ok(Some(kind))
                    }
                    _ => {
                        drop(table);
                        let _ = saving.folder.remove_file(&file.name);
                        Ok(None)
                    }
                }
            })
            .await?;
        match done {
            Ok(Some(kind)) => Ok(upload.state_now(upload.size, Some(kind))),
            Ok(None) => Err(gone()),
            Err(error) => Err(write_failed(&upload.name, &error)),
        }
    }

    /// What the upload `id` holds. A request still writing it stops first,
    /// so the page can go on from there.
    pub(super) async fn state(&self, id: &str) -> Result<UploadState, Refused> {
        let (upload, _) = self.touch(id)?;
        let _ = upload.take_over();
        let gate = upload.writer.clone().lock_owned().await;
        match self.state_of(id, &upload) {
            Some(State::Receiving) => {}
            Some(State::Complete(_, kind) | State::Claimed(_, kind, _)) => {
                return Ok(upload.state_now(upload.size, Some(kind)));
            }
            Some(State::Reserved) | None => return Err(gone()),
        }
        let (_gate, held) = self.disk(gate, slot_length).await?;
        match held {
            Ok(held) => Ok(upload.state_now(held, None)),
            Err(error) => Err(write_failed(&upload.name, &error)),
        }
    }

    /// Removes the upload `id` and its file, unless a message is sending
    /// it.
    pub(super) async fn remove(&self, id: &str) -> Result<(), Refused> {
        let removed = {
            let mut table = lock(&self.table);
            let claimed = match table.get(id) {
                None => return Err(Refused::Gone("Arko no longer has this file.".into())),
                Some(entry) => match &entry.state {
                    State::Claimed(file, ..) => Some(file.name.clone()),
                    _ => None,
                },
            };
            if let Some(name) = claimed {
                return Err(Refused::Claimed(format!("{name} is being sent.")));
            }
            table.remove(id)
        };
        if let Some(entry) = removed {
            delete(entry).await;
        }
        Ok(())
    }

    /// Claims the uploads `ids` for a message, all or none. Each must be
    /// whole, not claimed by another message, and named once. Their files
    /// come with their bytes when they may go to the model. The claim holds
    /// until [`commit`](Self::commit) or [`release`](Self::release).
    pub(super) async fn take(&self, ids: &[String]) -> Result<Claim, String> {
        if ids.len() > attachments::MAX_FILES {
            return Err(format!(
                "A message takes {} files at most. Remove some, and send them with another.",
                attachments::MAX_FILES
            ));
        }
        let number = self.claims.fetch_add(1, Ordering::Relaxed);
        let claimed = self.claim(ids, number)?;
        let read = tokio::task::spawn_blocking(move || {
            claimed
                .into_iter()
                .map(|(upload, file, kind)| read_for_send(&upload, file, kind))
                .collect::<Result<Vec<_>, String>>()
        })
        .await
        .unwrap_or_else(|error| Err(format!("Couldn’t read the files ({error}).")));
        match read {
            Ok(files) => Ok(Claim { number, files }),
            Err(why) => {
                self.release(number);
                Err(why)
            }
        }
    }

    /// Marks the uploads `ids` Claimed by the claim `number`, in one step,
    /// or none of them when one can't be.
    fn claim(
        &self,
        ids: &[String],
        number: u64,
    ) -> Result<Vec<(Arc<Upload>, FileRef, Kind)>, String> {
        let mut table = lock(&self.table);
        let mut seen = HashSet::new();
        let mut claimed = Vec::new();
        for id in ids {
            let Some(entry) = table.get(id) else {
                return Err("Arko no longer has a file you attached. Attach it again.".into());
            };
            if !seen.insert(id) {
                return Err(format!("{} is attached twice.", entry.upload.name));
            }
            match &entry.state {
                State::Complete(file, kind) => {
                    claimed.push((entry.upload.clone(), file.clone(), *kind))
                }
                State::Claimed(file, ..) => {
                    return Err(format!("{} is being sent with another message.", file.name));
                }
                State::Reserved | State::Receiving => {
                    return Err(format!("{} isn’t uploaded yet.", entry.upload.name));
                }
            }
        }
        for (id, (_, file, kind)) in ids.iter().zip(&claimed) {
            if let Some(entry) = table.get_mut(id) {
                entry.state = State::Claimed(file.clone(), *kind, number);
                entry.touched = Instant::now();
            }
        }
        Ok(claimed)
    }

    /// The message of the claim `number` went, so its uploads are files of
    /// the project now.
    pub(super) fn commit(&self, number: u64) {
        lock(&self.table).retain(
            |_, entry| !matches!(entry.state, State::Claimed(.., claim) if claim == number),
        );
    }

    /// The message of the claim `number` didn't go, so its uploads wait to
    /// be sent again.
    pub(super) fn release(&self, number: u64) {
        let mut table = lock(&self.table);
        for entry in table.values_mut() {
            if let State::Claimed(file, kind, claim) = &entry.state
                && *claim == number
            {
                entry.state = State::Complete(file.clone(), *kind);
                entry.touched = Instant::now();
            }
        }
    }

    /// Removes every upload not being sent, with its file, as the server
    /// stops.
    pub(super) async fn remove_unsent(&self) {
        let removed: Vec<Entry> = {
            let mut table = lock(&self.table);
            let ids: Vec<String> = table
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, State::Claimed(..)))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| table.remove(id)).collect()
        };
        for entry in removed {
            delete(entry).await;
        }
    }

    /// Removes the uploads no request touched for [`ABANDONED`], deciding
    /// and removing in one step from each as it is then. One a message is
    /// sending, or a request is writing, stays.
    async fn remove_abandoned(&self) {
        let now = Instant::now();
        let removed: Vec<Entry> = {
            let mut table = lock(&self.table);
            let ids: Vec<String> = table
                .iter()
                .filter(|(_, entry)| {
                    now.duration_since(entry.touched) > ABANDONED
                        && !matches!(entry.state, State::Claimed(..))
                        && entry.upload.writer.try_lock().is_ok()
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| table.remove(id)).collect()
        };
        for entry in removed {
            delete(entry).await;
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
}

impl Upload {
    /// Makes this request the newest, which stops an older one, and gives
    /// what tells of a newer one and this one's number. The number is taken
    /// in the same step that raises it, so no two requests share one.
    fn take_over(&self) -> (watch::Receiver<u64>, u64) {
        let newer = self.newest.subscribe();
        let mut mine = 0;
        self.newest.send_modify(|newest| {
            *newest += 1;
            mine = *newest;
        });
        (newer, mine)
    }

    fn state_now(&self, received: u64, kind: Option<Kind>) -> UploadState {
        UploadState {
            received,
            size: self.size,
            done: kind.is_some(),
            kind,
        }
    }

    /// Renames the whole part file to the upload's name, or the first
    /// numbered one no file has, with its type, its kind and a text's
    /// lines. Blocks.
    fn save(&self) -> std::io::Result<(FileRef, Kind)> {
        let (kind, mime, lines) = self.kind()?;
        let mut name = self.name.clone();
        for n in 2.. {
            match self.folder.rename_new(&self.part, &name) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => {
                    name = attachments::numbered_name(&self.name, n);
                }
                Err(error) => return Err(error),
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

/// The length of the part file in `slot`.
fn slot_length(slot: &mut Option<File>) -> std::io::Result<u64> {
    let file = slot
        .as_ref()
        .ok_or_else(|| std::io::Error::other("the file is gone"))?;
    Ok(file.metadata()?.len())
}

/// The file `file` of `upload` as a message sends it, with its bytes when
/// they may go to the model. Blocks.
fn read_for_send(upload: &Upload, file: FileRef, kind: Kind) -> Result<Attached, String> {
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
}

fn gone() -> Refused {
    Refused::Gone("Arko no longer has this file. Attach it again.".into())
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

/// Removes an upload taken out of the table, once a request writing it has
/// stopped: its part file, or its saved file when it was whole.
async fn delete(entry: Entry) {
    let Entry { upload, state, .. } = entry;
    let _ = upload.take_over();
    let gate = upload.writer.clone().lock_owned().await;
    let _ = tokio::task::spawn_blocking(move || {
        let mut gate = gate;
        // (Closed before it's removed.)
        *gate = None;
        let name = match state {
            State::Complete(file, _) | State::Claimed(file, ..) => file.name,
            State::Reserved | State::Receiving => upload.part.clone(),
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
                "There isn’t room for large.mov. Arko keeps 2.0 GB of the computer’s disk free."
                    .into()
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
                    (n, taken.files[0].file.name.clone())
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
        let claim = uploads.take(&ids).await.unwrap();
        let taken = &claim.files;
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
        // Claimed, they can't go with another message until released, and
        // they stay once sent.
        assert!(uploads.take(&ids).await.is_err());
        uploads.release(claim.number);
        let again = uploads.take(&ids).await.unwrap();
        uploads.commit(again.number);
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
        let claim = uploads.take(&whole_ids[..1]).await.unwrap();
        uploads.commit(claim.number);
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
                            assert_eq!(taken.files[0].file.bytes, size as u64);
                            store.commit(taken.number);
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

    /// Uploads started at once can't pass the limit on those waiting
    /// together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn uploads_started_at_once_keep_to_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = uploads(temp.path());
        let store = Arc::new(store.with_free(|_| {
            std::thread::sleep(Duration::from_millis(100));
            Some(100 << 30)
        }));
        let results = futures_util::future::join_all((0..64).map(|n| {
            let store = store.clone();
            async move { store.create(&format!("{n}.txt"), 1).await }
        }))
        .await;
        let accepted = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(accepted, MAX_UNSENT);
    }

    /// The bytes of uploads still to come are held against the disk's
    /// free space, and given back as they're written.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploads_on_their_way_hold_their_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = uploads(temp.path());
        let store = store.with_free(|_| Some(FREE_FLOOR + 100));
        let first = store.create("first.bin", 60).await.unwrap();
        let refused = store.create("second.bin", 60).await.unwrap_err();
        assert_eq!(
            refused,
            Refused::NoSpace(
                "There isn’t room for second.bin. Arko keeps 2.0 GB of the computer’s disk free."
                    .into()
            )
        );
        store.put(&first, 0, whole(&data(60))).await.unwrap();
        assert!(store.create("second.bin", 60).await.is_ok());
    }

    /// A write that would take the disk below its floor, as other programs
    /// fill it, is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_keeps_the_disk_above_its_floor() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = uploads(temp.path());
        static FREE: AtomicU64 = AtomicU64::new(u64::MAX);
        let store = store.with_free(|_| Some(FREE.load(Ordering::Relaxed)));
        let id = store.create("a.bin", 3 * WRITE_BYTES as u64).await.unwrap();
        // Another program fills the disk once the upload started.
        FREE.store(FREE_FLOOR + 1000, Ordering::Relaxed);
        let refused = store
            .put(&id, 0, chunks(vec![data(WRITE_BYTES); 3]))
            .await
            .unwrap_err();
        assert_eq!(
            refused,
            Refused::NoSpace("The computer’s disk is full.".into())
        );
    }

    /// A message claims its uploads, all or none. Another message can't
    /// send them, nor can they be removed, until the claim is released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_message_claims_its_uploads_once() {
        let temp = tempfile::tempdir().unwrap();
        let (store, _) = uploads(temp.path());
        let mut ids = Vec::new();
        for name in ["a.mp4", "b.mp4"] {
            let id = store.create(name, 1).await.unwrap();
            store.put(&id, 0, whole(&[0])).await.unwrap();
            ids.push(id);
        }
        let claim = store.take(&ids[..1]).await.unwrap();
        assert_eq!(
            store.take(&ids).await.err().unwrap(),
            "a.mp4 is being sent with another message."
        );
        assert_eq!(
            store.remove(&ids[0]).await,
            Err(Refused::Claimed("a.mp4 is being sent.".into()))
        );
        // The second wasn't claimed by the message refused.
        let other = store.take(&ids[1..]).await.unwrap();
        store.release(other.number);
        let twice = vec![ids[1].clone(), ids[1].clone()];
        assert_eq!(
            store.take(&twice).await.err().unwrap(),
            "b.mp4 is attached twice."
        );
        store.commit(claim.number);
        assert!(claim.files[0].file.path.is_file(), "a sent file stays");
        assert!(store.take(&ids[..1]).await.is_err());
    }

    /// The sweep decides and removes in one step. An upload a message is
    /// sending or a request is writing stays, however old.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_leaves_uploads_in_use() {
        let temp = tempfile::tempdir().unwrap();
        let (store, work) = uploads(temp.path());
        let mut ids = Vec::new();
        for name in ["claimed.bin", "writing.bin", "idle.bin"] {
            let id = store.create(name, 2).await.unwrap();
            if name != "writing.bin" {
                store.put(&id, 0, whole(&[0, 0])).await.unwrap();
            }
            ids.push(id);
        }
        let claim = store.take(&ids[..1]).await.unwrap();
        let writing = lock(&store.table)[&ids[1]].upload.clone();
        let gate = writing.writer.clone().lock_owned().await;
        for entry in lock(&store.table).values_mut() {
            entry.touched = Instant::now() - ABANDONED - Duration::from_secs(1);
        }
        store.remove_abandoned().await;
        drop(gate);
        let left: Vec<String> = lock(&store.table).keys().cloned().collect();
        assert!(left.contains(&ids[0]) && left.contains(&ids[1]), "{left:?}");
        assert!(!left.contains(&ids[2]));
        assert!(!folder_of(&work).join("idle.bin").exists());
        store.commit(claim.number);
    }

    /// No two requests for an upload share a number, so the newest always
    /// stops the one before.
    #[test]
    fn requests_take_numbers_of_their_own() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (store, _) = uploads(temp.path());
        let id = runtime.block_on(store.create("a.bin", 1)).unwrap();
        let upload = lock(&store.table)[&id].upload.clone();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (upload, barrier) = (upload.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..10_000)
                        .map(|_| upload.take_over().1)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut numbers: Vec<u64> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        let taken = numbers.len();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), taken);
    }

    /// A request cancelled while its bytes are written (a phone's
    /// connection dropping) leaves the part file in its upload, which goes
    /// on from what it holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_request_leaves_the_upload_to_go_on() {
        let temp = tempfile::tempdir().unwrap();
        let (store, work) = uploads(temp.path());
        let slow = Disk {
            delay: Duration::from_millis(500),
            on_worker: false,
        };
        let store = Arc::new(Uploads {
            disk: slow,
            ..store
        });
        let bytes = data(300_000);
        let id = store.create("video.mp4", 300_000).await.unwrap();
        let writing = tokio::spawn({
            let (store, id, bytes) = (store.clone(), id.clone(), bytes.clone());
            async move { store.put(&id, 0, whole(&bytes)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        writing.abort();
        let _ = writing.await;
        let state = store.state(&id).await.unwrap();
        assert_eq!(state.received, 300_000, "the write in progress finished");
        let done = store.put(&id, state.received, whole(&[])).await.unwrap();
        assert!(done.done);
        assert_eq!(
            std::fs::read(folder_of(&work).join("video.mp4")).unwrap(),
            bytes
        );
    }

    #[test]
    fn the_free_space_of_a_disk_is_known() {
        let temp = tempfile::tempdir().unwrap();
        let free = free_bytes(temp.path()).unwrap();
        assert!(free > 0);
        assert_eq!(free_bytes(&temp.path().join("not there")), None);
    }
}
