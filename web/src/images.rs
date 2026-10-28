//! The images items show, as tiles under the message or call, and the
//! viewer that shows one large.

use leptos::prelude::*;
use lucide_leptos::X;

use crate::app::page;
use crate::protocol::{IMAGE_TYPES, ImageData, ItemData};
use crate::words;

/// The images of the item `data`, as tiles of a fixed size, so a row keeps
/// its height as they load. Each loads once it nears the view.
#[component]
pub fn Thumbnails(data: RwSignal<ItemData>) -> impl IntoView {
    // (Only a change of its images draws the tiles again.)
    let images = Memo::new(move |_| data.with(|item| item.images.clone()));
    move || {
        let images = images.get();
        (!images.is_empty()).then(|| {
            view! {
                <div class="thumbs">
                    {images.into_iter().map(|image| view! { <Thumbnail image=image /> }).collect_view()}
                </div>
            }
        })
    }
}

/// One image, as a tile that opens the viewer.
#[component]
fn Thumbnail(image: ImageData) -> impl IntoView {
    let page = page();
    let caption = words::image_caption(&image.label, image.bytes);
    if !IMAGE_TYPES.contains(&image.mime.as_str()) {
        return view! { <div class="thumb">{caption}</div> }.into_any();
    }
    // Arko no longer keeps it.
    let gone = RwSignal::new(false);
    let url = image.url.clone();
    view! {
        <button
            type="button"
            class="thumb"
            title=caption.clone()
            aria-label=format!("Open image {caption}")
            disabled=move || gone.get()
            on:click=move |_| page.image.set(Some(image.clone()))
        >
            {move || {
                if gone.get() {
                    "Image not kept".into_any()
                } else {
                    view! {
                        <img src=url.clone() alt="" loading="lazy" decoding="async"
                            on:error=move |_| gone.set(true) />
                    }
                    .into_any()
                }
            }}
        </button>
    }
    .into_any()
}

/// The image the page opened, large. Esc, Close or a tap beside it closes
/// it.
#[component]
pub fn ImageViewer() -> impl IntoView {
    let page = page();
    let dialog: NodeRef<leptos::html::Dialog> = NodeRef::new();
    Effect::new(move |_| {
        let open = page.image.with(Option::is_some);
        let Some(dialog) = dialog.get() else { return };
        if open && !dialog.open() {
            let _ = dialog.show_modal();
        } else if !open && dialog.open() {
            dialog.close();
        }
    });
    view! {
        <dialog
            class="viewer"
            node_ref=dialog
            aria-label="Image"
            on:close=move |_| page.image.set(None)
            on:click=move |event| {
                // A tap on the backdrop (the dialog itself, beside the image) closes it.
                if event.target() == event.current_target() {
                    page.image.set(None);
                }
            }
        >
            {move || page.image.get().map(|image| {
                let caption = words::image_caption(&image.label, image.bytes);
                view! {
                    <div class="viewer-head">
                        <span class="viewer-caption">{caption.clone()}</span>
                        <button type="button" class="btn ghost icon" aria-label="Close" title="Close"
                            on:click=move |_| page.image.set(None)>
                            <X size=16 />
                        </button>
                    </div>
                    <img src=image.url alt=caption />
                }
            })}
        </dialog>
    }
}
