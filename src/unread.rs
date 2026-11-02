//! The notes the user sent during a turn that the agent never got to read.
//! Each goes with the user's next message in its conversation as the agent
//! would have read it, its images too. The images are saved as files in
//! the session's folder, so a conversation's record keeps their paths, not
//! their bytes, and they outlast a restart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The folder in the session's folder that holds the unread notes' images.
const IMAGES_DIR: &str = "unread-images";

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
    let extension = match mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    let hash: String = sha2::Sha256::digest(&bytes)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = session.join(IMAGES_DIR);
    let path = dir.join(format!("{hash}.{extension}"));
    if !path.is_file() {
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let partial = dir.join(format!(".{hash}.{}.new", uuid::Uuid::new_v4().simple()));
        std::fs::write(&partial, &bytes)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|error| {
                let _ = std::fs::remove_file(&partial);
                error.to_string()
            })?;
    }
    Ok(ImageFile {
        path,
        mime: mime_type.to_string(),
    })
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
                std::fs::read(&file.path)
                    .map(|bytes| arko_ai::Image::from_bytes(&bytes, &file.mime))
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
/// `notes` has, those of notes that went with a message. Reads and writes
/// files.
pub(crate) fn sweep<'a>(session: &Path, notes: impl IntoIterator<Item = &'a Unread>) {
    let kept: std::collections::HashSet<&Path> = notes
        .into_iter()
        .flat_map(|note| note.images.iter().map(|file| file.path.as_path()))
        .collect();
    let Ok(files) = std::fs::read_dir(session.join(IMAGES_DIR)) else {
        return;
    };
    for file in files.flatten() {
        let path = file.path();
        let name = file.file_name();
        // (A file still being written has a name of its own.)
        if !kept.contains(path.as_path()) && !name.to_string_lossy().starts_with('.') {
            let _ = std::fs::remove_file(&path);
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
