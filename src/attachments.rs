//! Files the user sends with a message from the web page. Each one is saved
//! in the project's `.arko/attachments` folder. An image also goes to the
//! model when the model takes it, and a short text goes in the message too.
//! The message tells the agent every file's path, type and size in an
//! attachments block ([`block`]), which goes before the user's own text and
//! is the one record of what the message carried ([`split_block`] reads it
//! back).
//!
//! A file's kind comes from its first bytes ([`sniff`]), never from its
//! name or the type the browser gives.

use std::path::PathBuf;
use std::sync::Arc;

pub use crate::remote::protocol::FileKind as Kind;

/// The folder in the project's `.arko` folder that holds the files.
pub(crate) const FOLDER: &str = "attachments";

/// The `.gitignore` the folder always has, whatever the setting that keeps
/// agents' project files out of git says, so a file sent from a phone is
/// never committed by accident.
pub(crate) const IGNORE_TEXT: &str =
    "# Arko keeps the files sent from its web page out of git.\n*\n";

/// The largest file the page may send.
pub(crate) const MAX_FILE_BYTES: u64 = 1 << 30;

/// The most files one message takes.
pub(crate) const MAX_FILES: usize = 20;

/// The room Arko's own labels take beside a message's text ("(This message
/// answers q3.)", "(Still open: q5.)").
const LABEL_ROOM: usize = 1024;

/// The most bytes of text and attachments block a message may hold for its
/// text files to go inline. A message sent while a turn runs is a note,
/// which holds 16 KiB at most, and the same limit holds at every time.
pub(crate) const INLINE_MAX_BYTES: usize = arko_workflows::notes::MAX_NOTE_BYTES - LABEL_ROOM;

/// The largest image read to go to the model. A larger one is saved only.
pub(crate) const MAX_IMAGE_BYTES: u64 = 32 << 20;

/// What a block's entry says of an image that went to the model.
const SHOWN: &str = "as an image after this message";

/// A file saved in the attachments folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// Its name in the folder.
    pub name: String,
    /// Its MIME type, from its first bytes.
    pub mime: String,
    pub bytes: u64,
    pub path: PathBuf,
    /// A text file's lines.
    pub lines: Option<u64>,
}

/// A file sent with a message, with its bytes when it may go to the model
/// (an image up to [`MAX_IMAGE_BYTES`], a text up to [`INLINE_MAX_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub file: FileRef,
    pub kind: Kind,
    pub content: Option<Arc<[u8]>>,
}

/// How a message's files go.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Routed {
    /// The images that go to the model, in order.
    pub images: Vec<Attached>,
    /// The attachments block, which goes before the user's text.
    pub block: String,
    /// What to tell the user of the files that went otherwise than they
    /// might think, such as an image saved because the model takes none.
    pub words: Option<String>,
}

/// How `files`, sent with `typed`, go to a session on `model`, which is a
/// chat when `chat` is set. A chat runs no tools, so it takes only images
/// that go to the model and text that goes inline, and `Err` says why
/// another file can't go.
pub(crate) fn route(
    model: &arko_ai::Model,
    chat: bool,
    typed: &str,
    files: Vec<Attached>,
) -> Result<Routed, String> {
    let mut routed = Routed::default();
    let mut entries = Vec::new();
    let mut texts = Vec::new();
    let mut why = Vec::new();
    for attached in files {
        let mut entry = Entry::of(&attached.file);
        match (attached.kind, &attached.content) {
            (Kind::Image, Some(content)) => {
                let image = arko_ai::Image::from_bytes(content, &attached.file.mime);
                match model.check_image(&image) {
                    Ok(()) => entry.shown = true,
                    Err(reason) => why.push(format!(
                        "{} went as a file, because {reason}.",
                        attached.file.name
                    )),
                }
            }
            (Kind::Image, None) => why.push(format!(
                "{} went as a file, because it’s too large to show the model.",
                attached.file.name
            )),
            (Kind::Text, Some(content)) => {
                if let Ok(text) = std::str::from_utf8(content) {
                    texts.push((entries.len(), text.to_string()));
                }
            }
            _ if is_heic(&attached.file.mime) => why.push(format!(
                "{} went as a file, because the model can’t read HEIC.",
                attached.file.name
            )),
            _ => {}
        }
        if entry.shown {
            routed.images.push(attached);
        }
        entries.push(entry);
    }
    // Every entry and the block's frame first, then text inline in the
    // order attached, in the room they leave.
    let mut used = typed.trim().len()
        + "<attachments>\n</attachments>\n\n".len()
        + entries
            .iter()
            .map(|entry| entry.written().len())
            .sum::<usize>();
    for (at, text) in texts {
        let entry = &mut entries[at];
        let listed = entry.written().len();
        entry.inline = Some(text);
        let inlined = entry.written().len();
        if used + inlined - listed > INLINE_MAX_BYTES {
            entry.inline = None;
        } else {
            used += inlined - listed;
        }
    }
    if chat
        && let Some(entry) = entries
            .iter()
            .find(|entry| !entry.shown && entry.inline.is_none())
    {
        return Err(format!(
            "{} can’t go to a chat. A chat takes images and text files up to 16 KB. Other files go to an agent session.",
            entry.name
        ));
    }
    routed.block = block(&entries);
    routed.words = (!why.is_empty()).then(|| why.join(" "));
    Ok(routed)
}

fn is_heic(mime: &str) -> bool {
    matches!(mime, "image/heic" | "image/heif")
}

/// One file of an attachments block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    pub path: String,
    pub lines: Option<u64>,
    /// It went to the model as an image.
    pub shown: bool,
    /// Its text, when it went in the block.
    pub inline: Option<String>,
}

impl Entry {
    fn of(file: &FileRef) -> Self {
        Entry {
            name: file.name.clone(),
            mime: file.mime.clone(),
            bytes: file.bytes,
            path: file.path.display().to_string(),
            lines: file.lines,
            shown: false,
            inline: None,
        }
    }

    /// The entry as the block writes it, a line, or its text between two.
    fn written(&self) -> String {
        // (An inline text's own length, so the block reads back whatever
        // length its file was recorded with.)
        let bytes = self
            .inline
            .as_ref()
            .map_or(self.bytes, |text| text.len() as u64);
        let mut out = format!(
            "<file name=\"{}\" type=\"{}\" size=\"{}\" bytes=\"{}\" path=\"{}\"",
            escape(&self.name),
            escape(&self.mime),
            crate::remote::protocol::size(bytes),
            bytes,
            escape(&self.path),
        );
        if self.shown {
            out.push_str(&format!(" shown=\"{SHOWN}\""));
        }
        if let (Some(lines), None) = (self.lines, &self.inline) {
            out.push_str(&format!(" lines=\"{lines}\""));
        }
        match &self.inline {
            // `bytes` is the text's length, so the text can hold anything.
            Some(text) => out.push_str(&format!(">\n{text}\n</file>\n")),
            None => out.push_str("/>\n"),
        }
        out
    }
}

/// The attachments block of `entries`, or nothing without any.
pub(crate) fn block(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("<attachments>\n");
    for entry in entries {
        out.push_str(&entry.written());
    }
    out.push_str("</attachments>\n");
    out
}

/// `block` before `typed`, as the agent reads them.
pub(crate) fn with_block(block: &str, typed: &str) -> String {
    match (block.is_empty(), typed.trim().is_empty()) {
        (true, _) => typed.to_string(),
        (false, true) => block.trim_end().to_string(),
        (false, false) => format!("{block}\n{typed}"),
    }
}

/// The attachments block `text` starts with, and the text after it, or
/// `None` when it starts with none.
pub(crate) fn split_block(text: &str) -> Option<(Vec<Entry>, &str)> {
    let mut rest = text.strip_prefix("<attachments>\n")?;
    let mut entries = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix("</attachments>") {
            let after = after.strip_prefix('\n').unwrap_or(after);
            return Some((entries, after.strip_prefix('\n').unwrap_or(after)));
        }
        let line_end = rest.find('\n')?;
        let line = &rest[..line_end];
        let (attributes, inline) = match line.strip_suffix("/>") {
            Some(attributes) => (attributes, false),
            None => (line.strip_suffix('>')?, true),
        };
        let attributes = attributes.strip_prefix("<file ")?;
        let mut entry = Entry::default();
        for (name, value) in parse_attributes(attributes)? {
            match name {
                "name" => entry.name = value,
                "type" => entry.mime = value,
                "bytes" => entry.bytes = value.parse().ok()?,
                "path" => entry.path = value,
                "lines" => entry.lines = Some(value.parse().ok()?),
                "shown" => entry.shown = true,
                _ => {}
            }
        }
        rest = &rest[line_end + 1..];
        if inline {
            let length = usize::try_from(entry.bytes).ok()?;
            entry.inline = Some(rest.get(..length)?.to_string());
            rest = rest.get(length..)?.strip_prefix("\n</file>\n")?;
        }
        entries.push(entry);
    }
}

/// `text`, a message the agent never read, as it goes with a later message,
/// without its images, so its block no longer says they follow. The agent
/// reads them from their paths instead.
pub(crate) fn without_images(text: &str) -> String {
    match split_block(text) {
        Some((mut entries, typed)) if entries.iter().any(|entry| entry.shown) => {
            for entry in &mut entries {
                entry.shown = false;
            }
            with_block(&block(&entries), typed)
        }
        _ => text.to_string(),
    }
}

/// What the user typed of `text`, a message the agent read, without the
/// attachments block it may start with.
pub(crate) fn typed(text: &str) -> &str {
    split_block(text).map_or(text, |(_, typed)| typed)
}

/// The `name="value"` pairs of `text`, unescaped.
fn parse_attributes(mut text: &str) -> Option<Vec<(&str, String)>> {
    let mut pairs = Vec::new();
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return Some(pairs);
        }
        let (name, after) = text.split_once("=\"")?;
        let (value, after) = after.split_once('"')?;
        pairs.push((name, unescape(value)));
        text = after;
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "&#10;")
}

fn unescape(value: &str) -> String {
    value
        .replace("&#10;", "\n")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// The longest name a file is saved under, in bytes.
const MAX_NAME_BYTES: usize = 100;

/// `name`, as the browser gave it, made a safe name in one folder. Only its
/// last component is kept, so it leads nowhere else. Control characters
/// and those Windows refuses go, spaces become `-`, it can't start with a
/// dot (so it's never `..` or hidden) or end with a dot or a space, a name
/// Windows reserves gets `-file`, and it is cut to [`MAX_NAME_BYTES`]
/// keeping its extension. An empty one is `file`.
pub(crate) fn sanitize_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut safe = String::new();
    for c in last.chars() {
        let c = match c {
            c if c.is_control() => continue,
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '-',
            c if c.is_whitespace() => '-',
            c => c,
        };
        if c == '-' && safe.ends_with('-') {
            continue;
        }
        safe.push(c);
    }
    let safe = safe.replace("-.", ".");
    let safe = safe
        .trim_start_matches(['.', '-'])
        .trim_end_matches(['.', ' ', '-']);
    let mut safe = if safe.is_empty() {
        "file".to_string()
    } else {
        safe.to_string()
    };
    let stem = safe.split('.').next().unwrap_or_default();
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    // (By bytes, since a stem's characters may be longer than one.)
    let numbered = |prefix: &str| {
        let stem = stem.as_bytes();
        stem.len() == 4
            && stem[..3].eq_ignore_ascii_case(prefix.as_bytes())
            && stem[3].is_ascii_digit()
    };
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        || numbered("COM")
        || numbered("LPT")
    {
        safe.insert_str(stem.len(), "-file");
    }
    cut_name(&safe, MAX_NAME_BYTES)
}

/// `name` cut to `most` bytes, its extension kept when it is short.
fn cut_name(name: &str, most: usize) -> String {
    if name.len() <= most {
        return name.to_string();
    }
    let (stem, extension) = match name.rfind('.') {
        Some(at) if at > 0 && name.len() - at <= 16 => (&name[..at], &name[at..]),
        _ => (name, ""),
    };
    let mut end = most - extension.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}{extension}",
        stem[..end].trim_end_matches(['.', ' ', '-'])
    )
}

/// The name of the `n`th file to try saving `name` as (2 and on), when the
/// name is taken, such as `image-2.jpg`.
pub(crate) fn numbered_name(name: &str, n: u32) -> String {
    let (stem, extension) = match name.rfind('.') {
        Some(at) if at > 0 => (&name[..at], &name[at..]),
        _ => (name, ""),
    };
    let suffix = format!("-{n}{extension}");
    let mut end = stem.len().min(MAX_NAME_BYTES.saturating_sub(suffix.len()));
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &stem[..end])
}

/// The kind and MIME type of a file that starts with `head`, named `name`.
/// The bytes decide, and the name only names a text's type or a binary
/// file the bytes don't tell. `head` is text when it is UTF-8 (a character
/// cut at its end counts) without NUL bytes.
pub(crate) fn sniff(head: &[u8], name: &str) -> (Kind, &'static str) {
    if let Some(signed) = signature(head, name) {
        return signed;
    }
    if is_text(head) {
        return (Kind::Text, text_type(name));
    }
    (
        Kind::File,
        match extension(name).as_str() {
            "mov" => "video/quicktime",
            "mp4" | "m4v" => "video/mp4",
            "mp3" => "audio/mpeg",
            "m4a" => "audio/mp4",
            _ => "application/octet-stream",
        },
    )
}

/// The type a file the page sent is downloaded as. Only an image, a video
/// or a PDF its bytes name is served as itself, a text as plain text, and
/// anything else (HTML, SVG and scripts included) as bytes.
pub(crate) fn download_type(head: &[u8], name: &str) -> &'static str {
    const SERVED: [&str; 7] = [
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "video/mp4",
        "video/quicktime",
        "application/pdf",
    ];
    match signature(head, name) {
        Some((_, mime)) if SERVED.contains(&mime) => mime,
        Some(_) => "application/octet-stream",
        None if is_text(head) => "text/plain; charset=utf-8",
        None => "application/octet-stream",
    }
}

/// The kind and type the signature `head` starts with names, if any.
fn signature(head: &[u8], name: &str) -> Option<(Kind, &'static str)> {
    let starts = |magic: &[u8]| head.starts_with(magic);
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    let image = |mime| Some((Kind::Image, mime));
    let file = |mime| Some((Kind::File, mime));
    if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return image("image/png");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return image("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return image("image/gif");
    }
    if starts(b"RIFF") && at(8, b"WEBP") {
        return image("image/webp");
    }
    if starts(b"RIFF") && at(8, b"WAVE") {
        return file("audio/wav");
    }
    if starts(b"RIFF") && at(8, b"AVI ") {
        return file("video/x-msvideo");
    }
    if at(4, b"ftyp") {
        return file(match head.get(8..12).unwrap_or_default() {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" => "image/heic",
            b"mif1" | b"msf1" | b"heif" => "image/heif",
            b"avif" | b"avis" => "image/avif",
            b"qt  " => "video/quicktime",
            b"M4A " | b"M4B " => "audio/mp4",
            _ => "video/mp4",
        });
    }
    if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let webm = head.windows(4).take(64).any(|window| window == b"webm");
        return file(if webm {
            "video/webm"
        } else {
            "video/x-matroska"
        });
    }
    if starts(b"%PDF-") {
        return file("application/pdf");
    }
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        return file(match extension(name).as_str() {
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "epub" => "application/epub+zip",
            "jar" => "application/java-archive",
            "apk" => "application/vnd.android.package-archive",
            _ => "application/zip",
        });
    }
    if starts(&[0x1F, 0x8B]) {
        return file("application/gzip");
    }
    if starts(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        return file("application/x-7z-compressed");
    }
    if at(257, b"ustar") {
        return file("application/x-tar");
    }
    // (Each signature that text could start with is checked further, by
    // the bytes after it that text never holds.)
    let id3 = starts(b"ID3") && matches!(head.get(3), Some(2..=4)) && head.get(4) == Some(&0);
    if id3 || starts(&[0xFF, 0xFB]) || starts(&[0xFF, 0xF3]) || starts(&[0xFF, 0xF2]) {
        return file("audio/mpeg");
    }
    if starts(b"OggS\0") {
        return file("audio/ogg");
    }
    if starts(b"fLaC\0") || starts(b"fLaC\x80") {
        return file("audio/flac");
    }
    if starts(b"II*\0") || starts(b"MM\0*") {
        return file("image/tiff");
    }
    if starts(b"BM") && at(6, &[0, 0, 0, 0]) {
        return file("image/bmp");
    }
    None
}

/// Whether `head` reads as text, UTF-8 without NUL bytes, its last
/// character allowed to be cut.
fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // A character cut at the end, and nothing wrong before it.
        Err(error) => error.error_len().is_none() && head.len() - error.valid_up_to() < 4,
    }
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

/// A text file's MIME type, by its name.
fn text_type(name: &str) -> &'static str {
    match extension(name).as_str() {
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_to_one_folder_and_stay_readable() {
        let cases = [
            ("IMG_0412.jpeg", "IMG_0412.jpeg"),
            ("../../etc/passwd", "passwd"),
            ("a/b/c.txt", "c.txt"),
            ("C:\\Users\\me\\notes.md", "notes.md"),
            ("..", "file"),
            ("...", "file"),
            (".env", "env"),
            ("", "file"),
            ("  ", "file"),
            ("bad\0name\n.txt", "badname.txt"),
            (
                "Screen Recording 2026-11-01 at 10.15.42.mov",
                "Screen-Recording-2026-11-01-at-10.15.42.mov",
            ),
            ("what?*<>|\":.png", "what.png"),
            ("trailing. ", "trailing"),
            ("CON", "CON-file"),
            ("con.txt", "con-file.txt"),
            ("COM1.log", "COM1-file.log"),
            ("CONSOLE.txt", "CONSOLE.txt"),
            ("Café menü.pdf", "Café-menü.pdf"),
            // Stems whose bytes and characters differ in number.
            ("éé.txt", "éé.txt"),
            ("Aé1.log", "Aé1.log"),
            ("ÇOM1.txt", "ÇOM1.txt"),
        ];
        for (given, saved) in cases {
            assert_eq!(sanitize_name(given), saved, "{given:?}");
            // Saving it again keeps the name.
            assert_eq!(sanitize_name(saved), saved, "{saved:?}");
        }
        let long = format!("{}.mp4", "é".repeat(200));
        let cut = sanitize_name(&long);
        assert!(cut.len() <= MAX_NAME_BYTES, "{}", cut.len());
        assert!(cut.ends_with(".mp4"), "{cut}");
    }

    #[test]
    fn a_taken_name_is_numbered_before_its_extension() {
        assert_eq!(numbered_name("image.jpg", 2), "image-2.jpg");
        assert_eq!(numbered_name("README", 3), "README-3");
        let long = format!("{}.txt", "a".repeat(96));
        let numbered = numbered_name(&long, 12);
        assert!(numbered.len() <= MAX_NAME_BYTES, "{}", numbered.len());
        assert!(numbered.ends_with("-12.txt"), "{numbered}");
    }

    #[test]
    fn the_bytes_say_what_a_file_is_whatever_its_name() {
        let ftyp = |brand: &[u8]| {
            let mut head = vec![0, 0, 0, 0x18];
            head.extend_from_slice(b"ftyp");
            head.extend_from_slice(brand);
            head.extend_from_slice(&[0; 8]);
            head
        };
        let mut tar = vec![0u8; 300];
        tar[257..262].copy_from_slice(b"ustar");
        let cases: Vec<(Vec<u8>, &str, Kind, &str)> = vec![
            (
                b"\x89PNG\r\n\x1a\n....".to_vec(),
                "photo.txt",
                Kind::Image,
                "image/png",
            ),
            (vec![0xFF, 0xD8, 0xFF, 0xE0], "x", Kind::Image, "image/jpeg"),
            (b"GIF89a....".to_vec(), "x", Kind::Image, "image/gif"),
            (
                b"RIFF\0\0\0\0WEBPVP8 ".to_vec(),
                "x",
                Kind::Image,
                "image/webp",
            ),
            (
                b"RIFF\0\0\0\0WAVEfmt ".to_vec(),
                "x",
                Kind::File,
                "audio/wav",
            ),
            (ftyp(b"heic"), "IMG.HEIC", Kind::File, "image/heic"),
            (ftyp(b"mif1"), "x", Kind::File, "image/heif"),
            (ftyp(b"qt  "), "x", Kind::File, "video/quicktime"),
            (ftyp(b"isom"), "RPReplay.MP4", Kind::File, "video/mp4"),
            (ftyp(b"M4A "), "x", Kind::File, "audio/mp4"),
            (
                b"\x1a\x45\xdf\xa3\x01\x00\x00\x00webm".to_vec(),
                "x",
                Kind::File,
                "video/webm",
            ),
            (
                b"%PDF-1.7\n".to_vec(),
                "x.txt",
                Kind::File,
                "application/pdf",
            ),
            (
                b"PK\x03\x04....".to_vec(),
                "report.docx",
                Kind::File,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                b"PK\x03\x04....".to_vec(),
                "x.zip",
                Kind::File,
                "application/zip",
            ),
            (vec![0x1F, 0x8B, 8, 0], "x", Kind::File, "application/gzip"),
            (tar, "x", Kind::File, "application/x-tar"),
            (b"ID3\x04\x00\x00".to_vec(), "x", Kind::File, "audio/mpeg"),
            (
                b"ID3 tags are read here.".to_vec(),
                "x.md",
                Kind::Text,
                "text/markdown",
            ),
            (b"BMW notes".to_vec(), "x", Kind::Text, "text/plain"),
            (
                b"# Notes\n".to_vec(),
                "notes.md",
                Kind::Text,
                "text/markdown",
            ),
            (
                b"{\"a\": 1}".to_vec(),
                "data.json",
                Kind::Text,
                "application/json",
            ),
            (
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
                "x.svg",
                Kind::Text,
                "image/svg+xml",
            ),
            (b"plain".to_vec(), "x", Kind::Text, "text/plain"),
            // A character cut by the end of the head still reads as text.
            (
                "café".as_bytes()[..4].to_vec(),
                "x.log",
                Kind::Text,
                "text/plain",
            ),
            (
                b"text\0with nul".to_vec(),
                "x.txt",
                Kind::File,
                "application/octet-stream",
            ),
            (
                vec![0xC3, 0x28, b'a'],
                "x.mov",
                Kind::File,
                "video/quicktime",
            ),
            (
                vec![0xFE, 0xED, 0xFA, 0xCE, 0],
                "tool",
                Kind::File,
                "application/octet-stream",
            ),
        ];
        for (head, name, kind, mime) in cases {
            assert_eq!(sniff(&head, name), (kind, mime), "{name} {head:?}");
        }
    }

    #[test]
    fn a_download_is_served_as_itself_only_when_its_bytes_say_so() {
        let mp4 = b"\0\0\0\x18ftypisom\0\0\0\0isommp41";
        let not_mp4 = [0u8, 7, 14, 21, 28, 35];
        let cases: [(&[u8], &str, &str); 7] = [
            (mp4, "RPReplay.MP4", "video/mp4"),
            (&not_mp4, "RPReplay.MP4", "application/octet-stream"),
            (b"%PDF-1.7\n", "x.bin", "application/pdf"),
            (
                b"<html><script>alert(1)</script>",
                "page.html",
                "text/plain; charset=utf-8",
            ),
            (
                b"<svg onload=\"alert(1)\"/>",
                "x.svg",
                "text/plain; charset=utf-8",
            ),
            (b"PK\x03\x04....", "a.zip", "application/octet-stream"),
            (b"\x89PNG\r\n\x1a\n....", "a.png", "image/png"),
        ];
        for (head, name, served) in cases {
            assert_eq!(download_type(head, name), served, "{name}");
        }
    }

    fn file(name: &str, mime: &str, bytes: u64) -> FileRef {
        FileRef {
            name: name.into(),
            mime: mime.into(),
            bytes,
            path: PathBuf::from("/work/.arko/attachments").join(name),
            lines: None,
        }
    }

    /// A 1×1 PNG.
    fn pixel() -> Arc<[u8]> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
            .unwrap()
            .into()
    }

    fn image(name: &str) -> Attached {
        let content = pixel();
        Attached {
            file: file(name, "image/png", content.len() as u64),
            kind: Kind::Image,
            content: Some(content),
        }
    }

    fn text(name: &str, text: &str) -> Attached {
        let mut file = file(name, "text/plain", text.len() as u64);
        file.lines = Some(text.lines().count() as u64);
        Attached {
            file,
            kind: Kind::Text,
            content: (text.len() <= INLINE_MAX_BYTES).then(|| text.as_bytes().into()),
        }
    }

    fn saved(name: &str, mime: &str) -> Attached {
        Attached {
            file: file(name, mime, 48_200_000),
            kind: Kind::File,
            content: None,
        }
    }

    fn model(images: bool) -> arko_ai::Model {
        let mut model = arko_ai::Model::anthropic_claude_opus_5_5();
        model.supports_image_input = images;
        model
    }

    #[test]
    fn images_go_to_a_model_that_takes_them_and_every_file_is_listed() {
        let routed = route(
            &model(true),
            false,
            "The spacing is wrong.",
            vec![
                image("bug.png"),
                saved("RPReplay.MP4", "video/mp4"),
                text("notes.md", "# Notes\n"),
            ],
        )
        .unwrap();
        assert_eq!(routed.images.len(), 1);
        assert_eq!(routed.words, None);
        let sent = with_block(&routed.block, "The spacing is wrong.");
        let (entries, rest) = split_block(&sent).unwrap();
        assert_eq!(rest, "The spacing is wrong.");
        assert_eq!(entries.len(), 3);
        assert!(entries[0].shown && entries[0].inline.is_none());
        assert_eq!(entries[1].mime, "video/mp4");
        assert_eq!(entries[1].bytes, 48_200_000);
        assert_eq!(entries[1].path, "/work/.arko/attachments/RPReplay.MP4");
        assert_eq!(entries[2].inline.as_deref(), Some("# Notes\n"));
        assert!(
            routed.block.contains("size=\"46.0 MB\""),
            "{}",
            routed.block
        );
    }

    #[test]
    fn an_image_the_model_cant_take_is_saved_and_the_user_told() {
        let routed = route(&model(false), false, "", vec![image("bug.png")]).unwrap();
        assert!(routed.images.is_empty());
        let words = routed.words.unwrap();
        assert!(
            words.starts_with("bug.png went as a file, because model"),
            "{words}"
        );
        let sent = with_block(&routed.block, "");
        let (entries, rest) = split_block(&sent).unwrap();
        assert!(!entries[0].shown);
        assert_eq!(rest, "");
        let heic = route(
            &model(true),
            false,
            "",
            vec![saved("IMG.heic", "image/heic")],
        )
        .unwrap();
        assert_eq!(
            heic.words.as_deref(),
            Some("IMG.heic went as a file, because the model can’t read HEIC.")
        );
    }

    #[test]
    fn text_goes_inline_while_the_message_stays_within_a_notes_size() {
        let half = "x".repeat(INLINE_MAX_BYTES / 2);
        let routed = route(
            &model(true),
            false,
            "Look.",
            vec![
                text("a.txt", &half),
                text("b.txt", &half),
                text("c.txt", "small\n"),
            ],
        )
        .unwrap();
        let entries = split_block(&routed.block).unwrap().0;
        // The second doesn't fit after the first, and the third still does.
        assert!(entries[0].inline.is_some());
        assert!(entries[1].inline.is_none());
        assert_eq!(entries[1].lines, Some(1));
        assert!(entries[2].inline.is_some());
        let sent = with_block(&routed.block, "Look.");
        assert!(
            sent.len() + LABEL_ROOM <= arko_workflows::notes::MAX_NOTE_BYTES,
            "{}",
            sent.len()
        );
        // A large text is listed with its lines, so the agent reads it in ranges.
        let large = text("server.log", &"line\n".repeat(10_000));
        let routed = route(&model(true), false, "", vec![large]).unwrap();
        assert!(routed.block.contains("lines=\"10000\""), "{}", routed.block);
    }

    /// Text goes inline only in the room every file's entry leaves, so the
    /// whole message stays within a note's size however many files follow.
    #[test]
    fn later_files_leave_room_for_their_entries() {
        let mut files = vec![text("notes.md", &"x".repeat(14_000))];
        for n in 0..19 {
            files.push(saved(&format!("{}-{n}.mp4", "a".repeat(85)), "video/mp4"));
        }
        let routed = route(&model(true), false, "Look", files).unwrap();
        let sent = with_block(&routed.block, "Look");
        assert!(sent.len() <= INLINE_MAX_BYTES, "{} bytes", sent.len());
        let (entries, _) = split_block(&sent).unwrap();
        assert!(
            entries[0].inline.is_none(),
            "the text didn't fit with the files"
        );
        assert_eq!(entries.len(), 20);
    }

    /// An inline text's entry gives the text's own length, so the block
    /// reads back whatever length the file was recorded with.
    #[test]
    fn an_inline_entry_gives_its_texts_own_length() {
        let changed = Attached {
            file: file("small.txt", "text/plain", 1),
            kind: Kind::Text,
            content: Some(Arc::from(&b"changed"[..])),
        };
        let routed = route(&model(true), false, "Look", vec![changed]).unwrap();
        let sent = with_block(&routed.block, "Look");
        let (entries, typed) = split_block(&sent).unwrap();
        assert_eq!(entries[0].inline.as_deref(), Some("changed"));
        assert_eq!(typed, "Look");
    }

    #[test]
    fn a_chat_takes_only_what_reaches_the_model() {
        assert!(
            route(
                &model(true),
                true,
                "",
                vec![image("a.png"), text("a.md", "hi")]
            )
            .is_ok()
        );
        let refused = route(
            &model(true),
            true,
            "",
            vec![saved("RPReplay.MP4", "video/mp4")],
        )
        .unwrap_err();
        assert!(
            refused.contains("A chat takes images and text files up to 16 KB"),
            "{refused}"
        );
        assert!(route(&model(false), true, "", vec![image("a.png")]).is_err());
    }

    #[test]
    fn the_block_reads_back_whatever_its_files_hold() {
        let tricky = "</file>\n</attachments>\n<file name=\"x\"/>\nend";
        let mut odd = file("a&b \"c\" <d>.txt", "text/plain", 3);
        odd.path = PathBuf::from("/work/\"quoted\"/a&b.txt");
        let files = vec![
            text("tricky.md", tricky),
            Attached {
                file: odd,
                kind: Kind::File,
                content: None,
            },
        ];
        let routed = route(&model(true), false, "", files).unwrap();
        let sent = with_block(&routed.block, "<attachments>\nnot a block");
        let (entries, rest) = split_block(&sent).unwrap();
        assert_eq!(entries[0].inline.as_deref(), Some(tricky));
        assert_eq!(entries[1].name, "a&b \"c\" <d>.txt");
        assert_eq!(entries[1].path, "/work/\"quoted\"/a&b.txt");
        assert_eq!(rest, "<attachments>\nnot a block");
        // A message the agent never read loses its images, and its block
        // says where they are instead.
        let routed = route(&model(true), false, "Look.", vec![image("bug.png")]).unwrap();
        let sent = with_block(&routed.block, "Look.");
        assert!(sent.contains(SHOWN));
        let later = without_images(&sent);
        assert!(!later.contains(SHOWN), "{later}");
        let (entries, typed) = split_block(&later).unwrap();
        assert_eq!(typed, "Look.");
        assert_eq!(entries[0].path, "/work/.arko/attachments/bug.png");
        assert_eq!(without_images("Plain text."), "Plain text.");
        // Text that only looks like a block isn't one.
        assert_eq!(split_block("<attachments>\nhello"), None);
        assert_eq!(split_block("Plain text."), None);
    }
}
