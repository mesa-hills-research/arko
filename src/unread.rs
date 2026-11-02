//! The notes the user sent during a turn that the agent never got to read.
//! Each goes with the user's next message in its conversation as the agent
//! would have read it, its images too. The images are saved as files in
//! the session's folder, so a conversation's record keeps their paths, not
//! their bytes, and they outlast a restart. The images folder is opened as
//! a [`Folder`], so nothing there is written, read or deleted through a
//! link.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::arko_dir::Folder;
use crate::arko_dir::folder::{Entry, Kind};

/// The folder in the session's folder that holds the unread notes' images.
const IMAGES_DIR: &str = "unread-images";

/// The extensions of the images' files, by their types. Any other type's
/// is the last.
const EXTENSIONS: [(&str, &str); 5] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("", "bin"),
];

/// The images folder of the session's folder `session`, opened without
/// following a link there. With `make`, one that isn't there is made, else
/// it's `None`.
fn images(session: &Path, make: bool) -> Result<Option<Folder>, String> {
    let session = Folder::base(session).map_err(|error| error.to_string())?;
    Ok(session.open(IMAGES_DIR, make)?.map(|(folder, _)| folder))
}

/// Whether `name` is one [`save`] gives an image's file, its hash and its
/// type's extension.
fn is_image_name(name: &OsStr) -> bool {
    let Some((hash, extension)) = name.to_str().and_then(|name| name.split_once('.')) else {
        return false;
    };
    hash.len() == 32
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && EXTENSIONS.iter().any(|(_, known)| *known == extension)
}

/// A note the agent never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unread {
    /// The note as the agent reads it.
    pub text: String,
    /// Its images, in order.
    pub images: Vec<ImageFile>,
}

/// An image of an unread note, saved as a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFile {
    pub path: PathBuf,
    /// Its MIME type, such as `image/png`.
    pub mime: String,
}

impl From<String> for Unread {
    fn from(text: String) -> Self {
        Unread {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for Unread {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

/// A note without images is saved as its text alone, as an older Arko saved
/// every unread note, so either reads the other's.
impl Serialize for Unread {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct WithImages<'a> {
            text: &'a str,
            images: &'a [ImageFile],
        }
        match self.images.is_empty() {
            true => serializer.serialize_str(&self.text),
            false => WithImages {
                text: &self.text,
                images: &self.images,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Unread {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Text(String),
            WithImages {
                text: String,
                #[serde(default)]
                images: Vec<ImageFile>,
            },
        }
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Text(text) => text.into(),
            Saved::WithImages { text, images } => Unread { text, images },
        })
    }
}

/// The note `text` with `images`, kept for the next message, its images saved
/// in the session's folder `session`. An image that can't be saved won't go
/// along, so the note's attachments block no longer says its images follow.
/// Reads and writes files.
pub(crate) fn keep(session: &Path, text: String, images: &[arko_ai::Image]) -> Unread {
    let saved: Vec<ImageFile> = images
        .iter()
        .filter_map(|image| match save(session, image) {
            Ok(file) => Some(file),
            Err(error) => {
                log::warn!("could not keep an unread note's image ({error})");
                None
            }
        })
        .collect();
    if saved.len() < images.len() {
        return crate::attachments::without_images(&text).into();
    }
    Unread {
        text,
        images: saved,
    }
}

/// Saves `image` in the session's folder `session`, once for each image's
/// bytes.
fn save(session: &Path, image: &arko_ai::Image) -> Result<ImageFile, String> {
    use base64::Engine as _;
    use sha2::Digest as _;
    let arko_ai::Image::Base64 { data, mime_type } = image else {
        return Err("it has an address, not its bytes".into());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|error| error.to_string())?;
    let extension = EXTENSIONS
        .iter()
        .find(|(mime, _)| *mime == mime_type.as_str() || mime.is_empty())
        .map_or("bin", |(_, extension)| *extension);
    let hash: String = sha2::Sha256::digest(&bytes)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let name = format!("{hash}.{extension}");
    let folder = images(session, true)?.ok_or("its folder couldn't be made")?;
    let path = folder.path().join(&name);
    match folder.entry(&name).map_err(|error| error.to_string())? {
        Some(Entry {
            kind: Kind::File, ..
        }) => {}
        Some(_) => return Err(format!("{} isn't a file", path.display())),
        None => match folder.create_whole(&name, &bytes) {
            // (Saved a moment ago, with these bytes, since its name is theirs.)
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error.to_string());
            }
            _ => {}
        },
    }
    Ok(ImageFile {
        path,
        mime: mime_type.to_string(),
    })
}

/// The bytes of the image at `path`, opened without following a link at
/// its end. Anything but a regular file is refused.
fn read_image(path: &Path) -> std::io::Result<Vec<u8>> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    Folder::base(folder)?.read(name)
}

/// The texts of `notes` as the agent reads them, and their images, in
/// order. A note whose image can't be read goes without its images, and its
/// attachments block no longer says they follow. Reads files.
pub(crate) fn load(notes: &[Unread]) -> (Vec<String>, Vec<arko_ai::Image>) {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for note in notes {
        let read: Result<Vec<arko_ai::Image>, std::io::Error> = note
            .images
            .iter()
            .map(|file| {
                read_image(&file.path).map(|bytes| arko_ai::Image::from_bytes(&bytes, &file.mime))
            })
            .collect();
        match read {
            Ok(read) => {
                texts.push(note.text.clone());
                images.extend(read);
            }
            Err(error) => {
                log::warn!("could not read an unread note's image ({error})");
                texts.push(crate::attachments::without_images(&note.text));
            }
        }
    }
    (texts, images)
}

/// Removes the images in the session's folder `session` that none of
/// `notes` has, those of notes that went with a message. Only the files
/// [`save`] names are removed, and an images folder that is a link is left
/// alone. Reads and writes files.
pub(crate) fn sweep<'a>(session: &Path, notes: impl IntoIterator<Item = &'a Unread>) {
    // (By name. A name is its image's hash, so it's the same image wherever
    // a note has it.)
    let kept: std::collections::HashSet<&OsStr> = notes
        .into_iter()
        .flat_map(|note| note.images.iter().filter_map(|file| file.path.file_name()))
        .collect();
    let folder = match images(session, false) {
        Ok(Some(folder)) => folder,
        Ok(None) => return,
        Err(error) => {
            log::warn!("could not clear the unread notes' images ({error})");
            return;
        }
    };
    let Ok(names) = folder.names() else {
        return;
    };
    for name in names {
        if kept.contains(name.as_os_str()) || !is_image_name(&name) {
            continue;
        }
        if let Ok(Some(Entry {
            kind: Kind::File, ..
        })) = folder.entry(&name)
        {
            let _ = folder.remove_file(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel() -> arko_ai::Image {
        arko_ai::Image::new_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
            "image/png",
        )
    }

    /// A note's image is saved once, read back the same, and a note keeps
    /// its paths across a save of its record, written as an older Arko reads
    /// it when it has no images.
    #[test]
    fn a_notes_images_are_kept_as_files_and_come_back() {
        let temp = tempfile::tempdir().unwrap();
        let note = keep(temp.path(), "Look at this too.".into(), &[pixel(), pixel()]);
        assert_eq!(note.images.len(), 2);
        assert_eq!(note.images[0], note.images[1]);
        assert!(
            note.images[0]
                .path
                .starts_with(temp.path().join(IMAGES_DIR))
        );
        let saved = serde_json::to_string(&vec![note.clone(), Unread::from("Plain.")]).unwrap();
        let back: Vec<Unread> = serde_json::from_str(&saved).unwrap();
        assert_eq!(back, vec![note.clone(), "Plain.".into()]);
        assert!(saved.contains("\"Plain.\""), "{saved}");
        // As an older Arko saved them.
        let older: Vec<Unread> = serde_json::from_str(r#"["Also the docs."]"#).unwrap();
        assert_eq!(older, vec![Unread::from("Also the docs.")]);
        let (texts, images) = load(&back);
        assert_eq!(texts, ["Look at this too.", "Plain."]);
        assert_eq!(images, vec![pixel(), pixel()]);
    }

    /// The images of notes that went are removed, and those of notes still
    /// waiting stay, one image of both too.
    #[test]
    fn the_images_of_notes_that_went_are_removed() {
        let temp = tempfile::tempdir().unwrap();
        let other = arko_ai::Image::from_bytes(b"GIF89a....", "image/gif");
        let went = keep(temp.path(), "went".into(), &[pixel(), other]);
        let waits = keep(temp.path(), "waits".into(), &[pixel()]);
        sweep(temp.path(), [&waits]);
        assert!(waits.images[0].path.is_file());
        assert!(!went.images[1].path.exists());
        sweep(temp.path(), []);
        assert!(!waits.images[0].path.exists());
    }

    /// The sweep leaves a linked images folder and what it leads to alone.
    #[cfg(unix)]
    #[test]
    fn the_sweep_never_follows_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let (session, outside) = (temp.path().join("session"), temp.path().join("outside"));
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let named = format!("{}.png", "0".repeat(32));
        for name in ["keep.txt", named.as_str()] {
            std::fs::write(outside.join(name), b"not an unread image").unwrap();
        }
        std::os::unix::fs::symlink(&outside, session.join(IMAGES_DIR)).unwrap();
        sweep(&session, []);
        assert!(outside.join("keep.txt").is_file());
        assert!(outside.join(named).is_file());
    }

    /// The sweep removes only the files Arko names as it saves an image.
    #[test]
    fn the_sweep_removes_only_the_images_arko_saved() {
        let temp = tempfile::tempdir().unwrap();
        let went = keep(temp.path(), "went".into(), &[pixel()]);
        let dir = temp.path().join(IMAGES_DIR);
        for name in ["notes.txt", "Photo.png", "0123.png"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.join(format!("{}.png", "1".repeat(32)))).unwrap();
        sweep(temp.path(), []);
        assert!(!went.images[0].path.exists());
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "0123.png".to_string(),
                format!("{}.png", "1".repeat(32)),
                "Photo.png".into(),
                "notes.txt".into()
            ]
        );
    }

    /// An image isn't saved through a linked images folder, and the note
    /// goes without it.
    #[cfg(unix)]
    #[test]
    fn an_image_isnt_saved_through_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let (session, outside) = (temp.path().join("session"), temp.path().join("outside"));
        std::fs::create_dir(&session).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, session.join(IMAGES_DIR)).unwrap();
        let note = keep(&session, "Look.".into(), &[pixel()]);
        assert!(note.images.is_empty());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// A note's image swapped for a link isn't read through it.
    #[cfg(unix)]
    #[test]
    fn an_image_swapped_for_a_link_isnt_read() {
        let temp = tempfile::tempdir().unwrap();
        let secret = temp.path().join("id_ed25519");
        std::fs::write(&secret, "secret").unwrap();
        let note = keep(temp.path(), "Look.".into(), &[pixel()]);
        std::fs::remove_file(&note.images[0].path).unwrap();
        std::os::unix::fs::symlink(&secret, &note.images[0].path).unwrap();
        let (texts, images) = load(&[note]);
        assert!(images.is_empty());
        assert_eq!(texts, ["Look."]);
    }

    /// An image that's gone by the next message doesn't go along, and the
    /// note's block no longer says it follows.
    #[test]
    fn a_note_whose_image_is_gone_says_so_no_more() {
        let temp = tempfile::tempdir().unwrap();
        let text = "<attachments>\n<file name=\"bug.png\" type=\"image/png\" size=\"70 bytes\" bytes=\"70\" path=\"/w/bug.png\" shown=\"as an image after this message\"/>\n</attachments>\n\nLook.";
        let note = keep(temp.path(), text.into(), &[pixel()]);
        std::fs::remove_file(&note.images[0].path).unwrap();
        let (texts, images) = load(&[note]);
        assert!(images.is_empty());
        assert!(!texts[0].contains("shown="), "{}", texts[0]);
        assert!(
            texts[0].contains("bug.png") && texts[0].ends_with("Look."),
            "{}",
            texts[0]
        );
        // One that can't be saved doesn't claim it either.
        let address = arko_ai::Image::new_url("https://example.com/a.png");
        let note = keep(temp.path(), text.into(), &[address]);
        assert!(note.images.is_empty());
        assert!(!note.text.contains("shown="), "{}", note.text);
    }
}
