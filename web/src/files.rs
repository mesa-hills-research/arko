//! The files a user's message carried, as chips in its bubble, each one a
//! download of its file, an image's with its thumbnail, which opens the
//! viewer. A chip whose file Arko no longer has says so.

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::app::{feed, page, request};
use crate::attach::short_name;
use crate::protocol::{FileData, IMAGE_TYPES, ImageData, ItemData, size};

thread_local! {
    /// Whether each file address of the feed shown is there, once Arko
    /// answered, asked once whatever draws its chips. Another feed (another
    /// Arko, or one started again) may have other files at the same
    /// addresses.
    static THERE: RefCell<HashMap<(Option<u64>, String), ArcRwSignal<Option<bool>>>> =
        RefCell::new(HashMap::new());
}

/// Forgets which files were there, for another feed. An answer still on its
/// way reaches only the chips of the feed it was asked for.
pub fn forget() {
    THERE.with(|there| there.borrow_mut().clear());
}

/// Whether the file at `url` is there, as Arko answers, asked once for the
/// feed shown.
fn there(url: &str) -> ArcRwSignal<Option<bool>> {
    let key = (feed(), url.to_string());
    if let Some(there) = THERE.with(|there| there.borrow().get(&key).cloned()) {
        return there;
    }
    let there = ArcRwSignal::new(None);
    THERE.with(|cell| cell.borrow_mut().insert(key, there.clone()));
    let (answer, url) = (there.clone(), url.to_string());
    spawn_local(async move {
        if let Some((status, _)) = request("HEAD", &url, None).await {
            answer.set(Some(status != 404));
        }
    });
    there
}

/// The chips of the files the item `data` carried, before its text.
#[component]
pub fn SentFiles(data: RwSignal<ItemData>) -> impl IntoView {
    // (Only a change of its files, or of the feed, draws the chips again.)
    let page = page();
    let files = Memo::new(move |_| {
        let feeds = page.feeds.get();
        data.with(|item| (item.files.clone(), item.images.clone(), feeds))
    });
    let alone = move || data.with(|item| item.text.trim().is_empty());
    move || {
        let (files, images, _) = files.get();
        (!files.is_empty()).then(|| {
            view! {
                <ul class="attached sent" class:alone=alone>
                    {files
                        .into_iter()
                        .map(|file| {
                            let image = shown_image(&file, &images);
                            view! { <SentChip file=file image=image /> }
                        })
                        .collect_view()}
                </ul>
            }
        })
    }
}

/// The image of `images` that shows `file`, when it went to the model.
fn shown_image(file: &FileData, images: &[ImageData]) -> Option<ImageData> {
    file.shown
        .then(|| {
            images
                .iter()
                .find(|image| image.label == file.name)
                .cloned()
        })
        .flatten()
}

/// Whether the item's images show `image` as one of its files' chips, so the
/// tiles leave it out.
pub fn on_a_chip(image: &ImageData, files: &[FileData]) -> bool {
    files
        .iter()
        .any(|file| file.shown && file.name == image.label)
}

/// One file, as a chip.
#[component]
fn SentChip(file: FileData, image: Option<ImageData>) -> impl IntoView {
    let page = page();
    let there = file.url.as_deref().map(there);
    let removed = Signal::derive(move || {
        there
            .as_ref()
            .is_none_or(|there| there.get() == Some(false))
    });
    let name = file.name.clone();
    let meta = format!(
        "{} · {}",
        kind_word(&file.mime, &file.name),
        size(file.bytes)
    );
    // An image shows its thumbnail, from the image the model got when it
    // did, which the page keeps, else from its file.
    let thumb = IMAGE_TYPES.contains(&file.mime.as_str()).then(|| {
        let tile = image
            .as_ref()
            .map(|image| image.url.clone())
            .or_else(|| file.url.clone());
        let large = ImageData {
            id: image.as_ref().map_or(0, |image| image.id),
            mime: file.mime.clone(),
            label: file.name.clone(),
            bytes: file.bytes,
            url: file.url.clone().or(tile.clone()).unwrap_or_default(),
        };
        let broken = RwSignal::new(false);
        let label = format!("Open image {}", file.name);
        move || {
            (!broken.get() && !removed.get())
                .then(|| tile.clone())
                .flatten()
                .map(|tile| {
                    let large = large.clone();
                    view! {
                        <button type="button" class="chip-open" aria-label=label.clone()
                            on:click=move |_| page.image.set(Some(large.clone()))>
                            <img class="chip-thumb" src=tile alt="" loading="lazy" decoding="async"
                                on:error=move |_| broken.set(true) />
                        </button>
                    }
                })
        }
    });
    let shown_name = short_name(&name);
    let url = file.url.clone().unwrap_or_default();
    let download = format!("Download {name}");
    view! {
        <li class="chip-file" class:removed=move || removed.get()>
            {thumb}
            {move || {
                let words = view! {
                    <span class="chip-name" title=name.clone()>{shown_name.clone()}</span>
                    <span class="chip-meta">
                        {if removed.get() { "Removed".to_string() } else { meta.clone() }}
                    </span>
                };
                if removed.get() {
                    view! { <span class="chip-link">{words}</span> }.into_any()
                } else {
                    view! {
                        <a class="chip-link" href=url.clone() download=name.clone()
                            aria-label=download.clone()>{words}</a>
                    }
                    .into_any()
                }
            }}
        </li>
    }
}

/// What kind of file `mime` and `name` say it is, in a word or two.
pub fn kind_word(mime: &str, name: &str) -> String {
    let (family, subtype) = mime.split_once('/').unwrap_or((mime, ""));
    match (family, subtype) {
        ("image", _) => return "Image".into(),
        ("video", _) => return "Video".into(),
        ("audio", _) => return "Audio".into(),
        ("application", "pdf") => return "PDF".into(),
        ("text", _) => return "Text".into(),
        _ => {}
    }
    match name.rsplit_once('.') {
        Some((_, extension))
            if (1..=5).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            extension.to_ascii_uppercase()
        }
        _ => "File".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_kind_is_a_word_from_its_type_or_its_extension() {
        assert_eq!(kind_word("video/mp4", "RPReplay.MP4"), "Video");
        assert_eq!(kind_word("image/heic", "IMG_1.HEIC"), "Image");
        assert_eq!(kind_word("application/pdf", "spec.pdf"), "PDF");
        assert_eq!(kind_word("text/markdown", "notes.md"), "Text");
        assert_eq!(kind_word("application/zip", "logs.zip"), "ZIP");
        assert_eq!(kind_word("application/octet-stream", "data"), "File");
        assert_eq!(kind_word("application/octet-stream", "a.longname"), "File");
    }

    #[test]
    fn an_image_the_model_got_shows_on_its_chip_alone() {
        let file = |name: &str, shown| FileData {
            name: name.into(),
            mime: "image/png".into(),
            bytes: 70,
            lines: None,
            shown,
            url: Some(format!("/api/attachments/{name}")),
        };
        let image = |label: &str| ImageData {
            id: 1,
            mime: "image/png".into(),
            label: label.into(),
            bytes: 70,
            url: "/api/images/1?v=x".into(),
        };
        let files = [file("bug.png", true), file("saved.png", false)];
        assert!(on_a_chip(&image("bug.png"), &files));
        assert!(!on_a_chip(&image("saved.png"), &files));
        assert!(!on_a_chip(&image("screenshot"), &files));
        assert_eq!(
            shown_image(&files[0], &[image("bug.png")]).map(|image| image.url),
            Some("/api/images/1?v=x".into())
        );
        assert_eq!(shown_image(&files[1], &[image("saved.png")]), None);
    }
}
