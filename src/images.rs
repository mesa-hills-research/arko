//! The images a session's transcript shows: those the user sends with a
//! message and those tools return (a file `read_file` shows, a screenshot).
//! Items name them by ID ([`ImageRef`]), and a front end reads their bytes
//! from the session's store ([`Images`]), which keeps the latest within a
//! budget. Events and items carry no image bytes.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use arko_agents::{ToolImage, ToolImageSink};

use crate::lock;
use crate::remote::Feed;

/// The most image bytes a session keeps for its transcript (the latest).
const BUDGET: usize = 128 * 1024 * 1024;

/// An image an item shows, by its ID in the session's [`Images`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub id: u64,
    /// Its MIME type: `image/png`.
    pub mime: String,
    /// What it is: a file's name, a tool's description of it.
    pub label: String,
    /// Its size in bytes.
    pub bytes: usize,
}

/// An image's bytes, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub bytes: Arc<[u8]>,
    pub mime: String,
    pub label: String,
}

/// A session's images, by ID: the latest, within 128 MB. Clones share them.
#[derive(Clone, Default)]
pub struct Images {
    inner: Arc<Mutex<Store>>,
}

#[derive(Default)]
struct Store {
    next: u64,
    images: HashMap<u64, Arc<Stored>>,
    /// IDs, oldest first.
    order: VecDeque<u64>,
    bytes: usize,
    /// The images by their bytes' hash: the same image (one restored again
    /// with its conversation) is kept once.
    by_hash: HashMap<u64, Vec<u64>>,
}

fn hash_of(bytes: &[u8], mime: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    mime.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

impl Images {
    /// The image `id`, while it is kept.
    pub fn get(&self, id: u64) -> Option<Arc<Stored>> {
        lock(&self.inner).images.get(&id).cloned()
    }

    /// Keeps `bytes`, an image of type `mime`, and names it.
    pub(crate) fn add(&self, bytes: Arc<[u8]>, mime: &str, label: &str) -> ImageRef {
        let hash = hash_of(&bytes, mime);
        let mut store = lock(&self.inner);
        // The same image again: the one kept, as the latest.
        let same = store.by_hash.get(&hash).and_then(|ids| {
            ids.iter().copied().find(|id| {
                store
                    .images
                    .get(id)
                    .is_some_and(|kept| kept.mime == mime && kept.bytes[..] == bytes[..])
            })
        });
        if let Some(id) = same {
            store.order.retain(|kept| *kept != id);
            store.order.push_back(id);
            return ImageRef {
                id,
                mime: mime.to_string(),
                label: label.to_string(),
                bytes: bytes.len(),
            };
        }
        store.next += 1;
        let id = store.next;
        let shown = ImageRef {
            id,
            mime: mime.to_string(),
            label: label.to_string(),
            bytes: bytes.len(),
        };
        store.by_hash.entry(hash).or_default().push(id);
        store.bytes += bytes.len();
        store.images.insert(
            id,
            Arc::new(Stored {
                bytes,
                mime: mime.to_string(),
                label: label.to_string(),
            }),
        );
        store.order.push_back(id);
        // The oldest go first; the newest stays, however large.
        while store.bytes > BUDGET && store.order.len() > 1 {
            if let Some(old) = store.order.pop_front()
                && let Some(image) = store.images.remove(&old)
            {
                store.bytes -= image.bytes.len();
                let hash = hash_of(&image.bytes, &image.mime);
                if let Some(ids) = store.by_hash.get_mut(&hash) {
                    ids.retain(|kept| *kept != old);
                    if ids.is_empty() {
                        store.by_hash.remove(&hash);
                    }
                }
            }
        }
        shown
    }

    /// Keeps `image` (an image sent as data; one sent by URL has no bytes
    /// here), and names it.
    pub(crate) fn add_image(&self, image: &arko_ai::Image, label: &str) -> Option<ImageRef> {
        let arko_ai::Image::Base64 { data, mime_type } = image else {
            return None;
        };
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .ok()?;
        Some(self.add(bytes.into(), mime_type, label))
    }
}

/// Takes the images the agents' tool calls return to the transcript: each
/// goes to the session's [`Images`], and the call's item shows it.
pub(crate) struct ToolImages {
    pub(crate) feed: Feed,
    pub(crate) images: Images,
}

impl ToolImageSink for ToolImages {
    fn tool_images(
        &self,
        _task_id: uuid::Uuid,
        tool_call_id: uuid::Uuid,
        _tool_name: &str,
        images: &[ToolImage],
    ) {
        let shown: Vec<ImageRef> = images
            .iter()
            .filter_map(|image| self.images.add_image(&image.image, &image.description))
            .collect();
        if !shown.is_empty() {
            self.feed.call_images(tool_call_id, shown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The latest images are kept within the budget, the oldest go first,
    /// and the newest stays whatever its size.
    #[test]
    fn the_latest_images_are_kept() {
        let images = Images::default();
        // (Each image its own: the same one is kept once.)
        let quarter = |n: u8| -> Arc<[u8]> { vec![n; BUDGET / 4].into() };
        let first = images.add(quarter(0), "image/png", "a.png");
        assert_eq!(first.bytes, BUDGET / 4);
        assert_eq!(images.get(first.id).unwrap().label, "a.png");
        let later: Vec<ImageRef> = (1..=4)
            .map(|n| images.add(quarter(n), "image/png", "b.png"))
            .collect();
        assert!(images.get(first.id).is_none());
        assert!(later.iter().all(|image| images.get(image.id).is_some()));
        let huge = images.add(vec![0u8; BUDGET * 2].into(), "image/png", "c.png");
        assert!(images.get(huge.id).is_some());
        assert!(later.iter().all(|image| images.get(image.id).is_none()));
        let data = arko_ai::Image::from_bytes(b"png bytes", "image/png");
        let shown = images.add_image(&data, "d.png").unwrap();
        assert_eq!(&*images.get(shown.id).unwrap().bytes, b"png bytes");
        assert!(
            images
                .add_image(&arko_ai::Image::new_url("https://x/y.png"), "y")
                .is_none()
        );
    }

    /// The same image added again (a conversation restored again) is kept
    /// once, under its first ID, as the latest.
    #[test]
    fn the_same_image_is_kept_once() {
        let images = Images::default();
        let first = images.add(Arc::from(&b"pixels"[..]), "image/png", "a.png");
        let other = images.add(Arc::from(&b"other"[..]), "image/png", "b.png");
        let again = images.add(Arc::from(&b"pixels"[..]), "image/png", "a again.png");
        assert_eq!(again.id, first.id);
        assert_eq!(again.label, "a again.png");
        assert_ne!(other.id, first.id);
        let as_jpeg = images.add(Arc::from(&b"pixels"[..]), "image/jpeg", "a.jpg");
        assert_ne!(as_jpeg.id, first.id, "another type is another image");
        assert_eq!(lock(&images.inner).bytes, 6 + 5 + 6);
    }
}
