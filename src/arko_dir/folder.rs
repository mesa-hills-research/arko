//! A folder Arko writes in, opened so that it never writes through a link.
//!
//! [`Folder::base`] opens a folder the user chose, such as the project or
//! Arko's folder, and follows links on the way there. Each folder below it
//! is opened by [`Folder::open`], which refuses a link, and everything
//! inside is made, read, renamed and deleted by name relative to the opened
//! folder. On Unix the folder is held open, so a folder above it that is
//! swapped for a link after the check changes nothing. Elsewhere each step
//! checks the path again, which leaves a short window in which a folder
//! above could be swapped.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// What an entry of a folder is, read without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Folder,
    File,
    Link,
    Other,
}

/// An entry of a folder: what it is, and on Unix its device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub kind: Kind,
    pub id: Option<(u64, u64)>,
}

/// An opened folder.
#[derive(Debug)]
pub(crate) struct Folder {
    /// Its path as Arko names it.
    path: PathBuf,
    /// Its real path.
    real: PathBuf,
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

/// Why `path` can't be written in: it is a link.
pub(crate) fn link_refused(path: &Path) -> String {
    format!(
        "{} is a link, and Arko doesn't write through links.",
        path.display()
    )
}

impl Folder {
    /// The folder at `path`, following links on the way.
    pub(crate) fn base(path: &Path) -> io::Result<Folder> {
        let real = std::fs::canonicalize(path)?;
        if !real.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        Ok(Folder {
            path: path.to_path_buf(),
            #[cfg(unix)]
            fd: sys::open_dir(&real)?,
            real,
        })
    }

    /// Its path as Arko names it.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Its real path, with no link in it, as it was when it was opened.
    pub(crate) fn real(&self) -> &Path {
        &self.real
    }

    /// Its device and inode, where the system has them.
    pub(crate) fn id(&self) -> Option<(u64, u64)> {
        #[cfg(unix)]
        return sys::id_of(&self.fd).ok();
        #[cfg(not(unix))]
        return None;
    }

    /// The folder `name` in this one, and whether this call made it. With
    /// `make`, one that isn't there is made, else it is `None`. Refuses a
    /// link, something that isn't a folder, and a folder that leads out of
    /// this one.
    pub(crate) fn open(
        &self,
        name: impl AsRef<OsStr>,
        make: bool,
    ) -> Result<Option<(Folder, bool)>, String> {
        let name = name.as_ref();
        let path = self.path.join(name);
        let reason = |error: &io::Error| crate::profiles::reason_of(error);
        let mut made = false;
        match self.entry(name) {
            Err(error) => {
                return Err(format!(
                    "Couldn't read {} ({}).",
                    path.display(),
                    reason(&error)
                ));
            }
            Ok(None) if !make => return Ok(None),
            Ok(None) => match self.make(name) {
                Ok(()) => made = true,
                // (Made a moment ago by another writer.)
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "Couldn't make {} ({})",
                        path.display(),
                        reason(&error)
                    ));
                }
            },
            Ok(Some(_)) => {}
        }
        match self.entry(name) {
            Ok(Some(Entry {
                kind: Kind::Folder, ..
            })) => {}
            Ok(Some(Entry {
                kind: Kind::Link, ..
            })) => return Err(link_refused(&path)),
            Ok(_) => return Err(format!("{} isn't a folder.", path.display())),
            Err(error) => {
                return Err(format!(
                    "Couldn't read {} ({}).",
                    path.display(),
                    reason(&error)
                ));
            }
        }
        let outside = || {
            format!(
                "{} leads outside {}, so Arko doesn't write there.",
                path.display(),
                self.path.display()
            )
        };
        #[cfg(unix)]
        let folder = {
            let fd = sys::open_child(&self.fd, name).map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    link_refused(&path)
                } else {
                    format!("Couldn't open {} ({}).", path.display(), reason(&error))
                }
            })?;
            // The real path names the folder opened, so a record of it holds.
            let real = self.real.join(name);
            let named = sys::id_of_path(&real).ok();
            if named.is_none() || named != sys::id_of(&fd).ok() {
                return Err(outside());
            }
            Folder { path, real, fd }
        };
        #[cfg(not(unix))]
        let folder = {
            let real = std::fs::canonicalize(&path).map_err(|error| {
                format!("Couldn't open {} ({}).", path.display(), reason(&error))
            })?;
            if real.parent() != Some(self.real.as_path()) {
                return Err(outside());
            }
            Folder { path, real }
        };
        Ok(Some((folder, made)))
    }

    /// The entry `name`, or `None` when nothing is there.
    pub(crate) fn entry(&self, name: impl AsRef<OsStr>) -> io::Result<Option<Entry>> {
        let name = name.as_ref();
        #[cfg(unix)]
        let found = sys::entry(&self.fd, name);
        #[cfg(not(unix))]
        let found = std::fs::symlink_metadata(self.checked(name)?).map(|meta| {
            let kind = meta.file_type();
            Entry {
                kind: if kind.is_symlink() {
                    Kind::Link
                } else if kind.is_dir() {
                    Kind::Folder
                } else if kind.is_file() {
                    Kind::File
                } else {
                    Kind::Other
                },
                id: None,
            }
        });
        match found {
            Ok(entry) => Ok(Some(entry)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The names of the entries in the folder, sorted.
    pub(crate) fn names(&self) -> io::Result<Vec<OsString>> {
        self.names_up_to(usize::MAX)
    }

    /// The names of the entries in the folder, sorted, when it has at most
    /// `most`. Reading stops at the one past that, which fails with
    /// `FileTooLarge`, so a huge folder isn't read whole.
    pub(crate) fn names_up_to(&self, most: usize) -> io::Result<Vec<OsString>> {
        #[cfg(unix)]
        let mut names = sys::names(&self.fd, most)?;
        #[cfg(not(unix))]
        let mut names = {
            self.checked(OsStr::new("."))?;
            std::fs::read_dir(&self.path)?
                .take(most.saturating_add(1))
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?
        };
        if names.len() > most {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        names.sort();
        Ok(names)
    }

    /// Makes the folder `name`, which mustn't be there.
    pub(crate) fn make(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        #[cfg(unix)]
        return sys::make_dir(&self.fd, name);
        #[cfg(not(unix))]
        return std::fs::create_dir(self.checked(name)?);
    }

    /// Writes the new file `name` with `bytes`. Fails when anything, a link
    /// too, is there.
    pub(crate) fn create(&self, name: impl AsRef<OsStr>, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let name = name.as_ref();
        #[cfg(unix)]
        let mut file = sys::create_file(&self.fd, name)?;
        #[cfg(not(unix))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.checked(name)?)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    /// Writes the file `name` whole, through a new file renamed into place,
    /// unless something is there already (`AlreadyExists`).
    pub(crate) fn create_whole(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temporary = format!(".{name}.{}.new", uuid::Uuid::new_v4().simple());
        let written = self
            .create(&temporary, bytes)
            .and_then(|()| self.rename_new(&temporary, name));
        if written.is_err() {
            let _ = self.remove_file(&temporary);
        }
        written
    }

    /// Writes the file `name` whole, through a new file renamed into place,
    /// in place of what is there. A link there is replaced, never followed.
    pub(crate) fn write_whole(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temporary = format!(".{name}.{}.new", uuid::Uuid::new_v4().simple());
        let written = self
            .create(&temporary, bytes)
            .and_then(|()| self.rename(&temporary, name));
        if written.is_err() {
            let _ = self.remove_file(&temporary);
        }
        written
    }

    /// The regular file `name`, opened to read. Refuses a link and anything
    /// else that isn't a regular file, such as a pipe, which opening doesn't
    /// wait for.
    pub(crate) fn open_file(&self, name: impl AsRef<OsStr>) -> io::Result<std::fs::File> {
        let name = name.as_ref();
        #[cfg(unix)]
        return sys::open_file(&self.fd, name);
        #[cfg(not(unix))]
        {
            let path = self.checked(name)?;
            if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
                return Err(io::Error::other("not a regular file"));
            }
            let file = std::fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("not a regular file"));
            }
            Ok(file)
        }
    }

    /// The bytes of the regular file `name`, refusing a link.
    pub(crate) fn read(&self, name: impl AsRef<OsStr>) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut bytes = Vec::new();
        self.open_file(name)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Renames `from` to `to`, in place of what `to` is.
    pub(crate) fn rename(&self, from: impl AsRef<OsStr>, to: impl AsRef<OsStr>) -> io::Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        #[cfg(unix)]
        return sys::rename(&self.fd, from, to);
        #[cfg(not(unix))]
        return std::fs::rename(self.checked(from)?, self.checked(to)?);
    }

    /// Renames `from` to `to`, which mustn't be there (`AlreadyExists`).
    /// The check and the rename are one step on Linux and macOS. Elsewhere,
    /// and on a file system without it, `to` is checked first.
    pub(crate) fn rename_new(
        &self,
        from: impl AsRef<OsStr>,
        to: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        let (from, to) = (from.as_ref(), to.as_ref());
        #[cfg(unix)]
        match sys::rename_new(&self.fd, from, to) {
            Err(error) if sys::unsupported(&error) => {}
            other => return other,
        }
        if self.entry(to)?.is_some() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        self.rename(from, to)
    }

    /// Deletes the file or link `name`.
    pub(crate) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        #[cfg(unix)]
        return sys::remove(&self.fd, name, false);
        #[cfg(not(unix))]
        return std::fs::remove_file(self.checked(name)?);
    }

    /// Deletes the folder `name` and everything in it. A link in it is
    /// deleted, never followed.
    pub(crate) fn remove_all(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        self.remove_all_of(name, None)
    }

    /// Deletes the folder `name` and everything in it, as [`remove_all`]
    /// does, when it is the folder `id` names (its device and inode, checked
    /// on the folder opened), else fails with `InvalidData`.
    ///
    /// [`remove_all`]: Self::remove_all
    pub(crate) fn remove_all_of(
        &self,
        name: impl AsRef<OsStr>,
        id: Option<(u64, u64)>,
    ) -> io::Result<()> {
        let name = name.as_ref();
        #[cfg(unix)]
        {
            match self.entry(name)? {
                None if id.is_none() => return Ok(()),
                None => return Err(io::Error::from(io::ErrorKind::NotFound)),
                Some(Entry {
                    kind: Kind::Folder, ..
                }) => {}
                Some(_) if id.is_none() => return self.remove_file(name),
                Some(_) => return Err(io::Error::from(io::ErrorKind::InvalidData)),
            }
            let Some((inner, _)) = self.open(name, false).map_err(io::Error::other)? else {
                return Ok(());
            };
            if id.is_some() && inner.id() != id {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            for entry in inner.names()? {
                inner.remove_all(&entry)?;
            }
            sys::remove(&self.fd, name, true)
        }
        #[cfg(not(unix))]
        {
            // (No identity to check where the system has none.)
            let _ = id;
            let path = self.checked(name)?;
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return std::fs::remove_file(path);
            }
            std::fs::remove_dir_all(path)
        }
    }

    /// The path of `name` in the folder, once the folder is checked to be
    /// where it was opened, not swapped for a link.
    #[cfg(not(unix))]
    fn checked(&self, name: &OsStr) -> io::Result<PathBuf> {
        if std::fs::symlink_metadata(&self.path)?
            .file_type()
            .is_symlink()
            || std::fs::canonicalize(&self.path)? != self.real
        {
            return Err(io::Error::other(link_refused(&self.path)));
        }
        Ok(if name == "." {
            self.path.clone()
        } else {
            self.path.join(name)
        })
    }
}

/// The calls on Unix, relative to an open folder.
#[cfg(unix)]
mod sys {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use super::{Entry, Kind};

    /// `name` as C wants it. A name is one part of a path.
    fn c_name(name: &OsStr) -> io::Result<CString> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name:?} isn't a name in a folder"),
            ));
        }
        CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn result(value: libc::c_int) -> io::Result<libc::c_int> {
        if value < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        Ok(OwnedFd::from(file))
    }

    pub fn open_child(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        let name = c_name(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `name` is a C string, and the descriptor is open.
        let fd = result(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) })?;
        // SAFETY: `openat` returned a new descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn kind(mode: libc::mode_t) -> Kind {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Folder,
            libc::S_IFREG => Kind::File,
            libc::S_IFLNK => Kind::Link,
            _ => Kind::Other,
        }
    }

    // (`dev_t` and `ino_t` are `u64` on some systems and not on others.)
    #[allow(clippy::unnecessary_cast)]
    pub fn entry(dir: &OwnedFd, name: &OsStr) -> io::Result<Entry> {
        let name = c_name(name)?;
        // SAFETY: an all-zero `stat` is valid, and `fstatat` fills it.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is a C string, `stat` is writable, and the
        // descriptor is open.
        result(unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        Ok(Entry {
            kind: kind(stat.st_mode),
            id: Some((stat.st_dev as u64, stat.st_ino as u64)),
        })
    }

    /// The device and inode of the open `fd`.
    #[allow(clippy::unnecessary_cast)]
    pub fn id_of(fd: &OwnedFd) -> io::Result<(u64, u64)> {
        // SAFETY: an all-zero `stat` is valid, and `fstat` fills it.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `stat` is writable, and the descriptor is open.
        result(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) })?;
        Ok((stat.st_dev as u64, stat.st_ino as u64))
    }

    /// The device and inode of `path`, without following a link at its end.
    pub fn id_of_path(path: &Path) -> io::Result<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)?;
        Ok((meta.dev(), meta.ino()))
    }

    pub fn make_dir(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: `name` is a C string, and the descriptor is open.
        result(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) }).map(|_| ())
    }

    fn open_at(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<std::fs::File> {
        let name = c_name(name)?;
        let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mode: libc::c_uint = 0o666;
        // SAFETY: `name` is a C string, and the descriptor is open.
        let fd = result(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) })?;
        // SAFETY: `openat` returned a new descriptor that nothing else owns.
        Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn create_file(dir: &OwnedFd, name: &OsStr) -> io::Result<std::fs::File> {
        open_at(dir, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
    }

    /// Opens the regular file `name` to read, without waiting on a pipe or
    /// a device.
    pub fn open_file(dir: &OwnedFd, name: &OsStr) -> io::Result<std::fs::File> {
        let file = open_at(dir, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(file)
    }

    pub fn rename(dir: &OwnedFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let (from, to) = (c_name(from)?, c_name(to)?);
        let fd = dir.as_raw_fd();
        // SAFETY: both names are C strings, and the descriptor is open.
        result(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) }).map(|_| ())
    }

    /// Renames without replacing, in one step, where the system has it.
    pub fn rename_new(dir: &OwnedFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let (from, to) = (c_name(from)?, c_name(to)?);
        let fd = dir.as_raw_fd();
        #[cfg(target_os = "linux")]
        // SAFETY: both names are C strings, and the descriptor is open.
        let done =
            unsafe { libc::renameat2(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_NOREPLACE) };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        // SAFETY: both names are C strings, and the descriptor is open.
        let done =
            unsafe { libc::renameatx_np(fd, from.as_ptr(), fd, to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
        let done = {
            let _ = (fd, from, to);
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        };
        result(done).map(|_| ())
    }

    /// Whether `error` says the system or file system has no rename without
    /// replacing.
    pub fn unsupported(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::EINVAL | libc::ENOTSUP)
        )
    }

    pub fn remove(dir: &OwnedFd, name: &OsStr, folder: bool) -> io::Result<()> {
        let name = c_name(name)?;
        let flags = if folder { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is a C string, and the descriptor is open.
        result(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(|_| ())
    }

    /// The names in the folder, without `.` and `..`, reading at most one
    /// past `most`.
    pub fn names(dir: &OwnedFd, most: usize) -> io::Result<Vec<OsString>> {
        // A copy of the descriptor, which the listing owns and closes.
        // SAFETY: the descriptor is open.
        let copy = result(unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
        // SAFETY: `copy` is a new descriptor, which `fdopendir` takes.
        let stream = unsafe { libc::fdopendir(copy) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so `copy` is still ours to close.
            unsafe { libc::close(copy) };
            return Err(error);
        }
        // (The copy shares its place in the listing with the original.)
        // SAFETY: `stream` is an open listing.
        unsafe { libc::rewinddir(stream) };
        let mut names = Vec::new();
        loop {
            // SAFETY: `stream` is an open listing.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `readdir` returned an entry whose name is a C string,
            // valid until the next call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes != b"." && bytes != b".." {
                names.push(OsString::from_vec(bytes.to_vec()));
                if names.len() > most {
                    break;
                }
            }
        }
        // SAFETY: `stream` is an open listing, closed once.
        unsafe { libc::closedir(stream) };
        Ok(names)
    }
}
