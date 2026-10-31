//! The speech models Arko downloads, each from a pinned revision on Hugging
//! Face, and where they're kept: `models/speech/<model>/` in Arko's folder,
//! with the pack the engine loads (`model.pack`), what it was made from
//! (`source.json`) and who made it under which licence (`NOTICE.txt`).

use std::path::{Path, PathBuf};

/// The model families the engine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Granite,
    Parakeet,
}

/// A file of a model on Hugging Face.
#[derive(Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// A model Arko can download and pack.
#[derive(Debug, PartialEq, Eq)]
pub struct SpeechModel {
    /// The name the settings use (`granite`).
    pub name: &'static str,
    /// The name people read.
    pub title: &'static str,
    pub maker: &'static str,
    pub family: Family,
    /// Its folder in `models/speech`, named like its repository.
    pub folder: &'static str,
    /// The Hugging Face repository and the revision downloaded.
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: &'static [RemoteFile],
    /// The downloaded file that gets packed, or `None` for the folder of
    /// files.
    pub source_file: Option<&'static str>,
    /// The pack's size, in the model's 8-bit precision.
    pub pack_bytes: u64,
    /// The precision it's packed in, as the engine names it.
    pub precision: &'static str,
    pub licence: &'static str,
    pub licence_url: &'static str,
}

/// IBM's Granite Speech, the default: English, French, German, Spanish,
/// Portuguese, Italian and Japanese.
pub const GRANITE: SpeechModel = SpeechModel {
    name: "granite",
    title: "Granite Speech 5.0",
    maker: "IBM",
    family: Family::Granite,
    folder: "granite-speech-5.0-470m-turboctc",
    repo: "ibm-granite/granite-speech-5.0-470m-turboctc",
    revision: "1dd23f46f6d4e6eae2531982ba2e33220086dd97",
    files: &[
        RemoteFile {
            name: "model.safetensors",
            bytes: 946_311_576,
            sha256: "5b569c3744c42a9172ac976881539926b66176d03fbaaa3c51b5d2960067f6fc",
        },
        RemoteFile {
            name: "tokenizer.json",
            bytes: 1_699_784,
            sha256: "a1c3ca763ce0690faf0df3456365358547acfdf766122c9e4a69be8e8708042e",
        },
        RemoteFile {
            name: "config.json",
            bytes: 921,
            sha256: "aabd64a2da736a1a1c21237c896b71628f6dd7364f1f5ec8a77f5e04b3ef1011",
        },
    ],
    source_file: None,
    pack_bytes: 476_527_765,
    precision: "int8",
    licence: "Apache 2.0",
    licence_url: "https://www.apache.org/licenses/LICENSE-2.0",
};

/// NVIDIA's Parakeet, for the European languages Granite doesn't know, from
/// NVIDIA's own 8-bit file.
pub const PARAKEET: SpeechModel = SpeechModel {
    name: "parakeet",
    title: "Parakeet TDT 0.6B v3",
    maker: "NVIDIA",
    family: Family::Parakeet,
    folder: "parakeet-tdt-0.6b-v3",
    repo: "nvidia/parakeet-tdt-0.6b-v3",
    revision: "541d1f99c6b0c3cd0b11a95167540bb8edefd82b",
    files: &[
        RemoteFile {
            name: "parakeet-tdt-0.6b-v3.q8_0.gguf",
            bytes: 713_975_456,
            sha256: "e3880d0aaaaf2c308ea2c35016b2b895c423eb3fda924c1b463d1c19b7f4d32e",
        },
        RemoteFile {
            name: "tokenizer.json",
            bytes: 1_159_960,
            sha256: "bd321b096832a3f270bd3b2a88823957920f1a5c5ada71114a26ea729d0cbe91",
        },
    ],
    source_file: Some("parakeet-tdt-0.6b-v3.q8_0.gguf"),
    pack_bytes: 723_284_742,
    precision: "q8_0",
    licence: "CC BY 4.0",
    licence_url: "https://creativecommons.org/licenses/by/4.0/",
};

pub const MODELS: [&SpeechModel; 2] = [&GRANITE, &PARAKEET];

/// The pack's name in a model's folder.
pub const PACK_FILE: &str = "model.pack";

/// Megabytes, rounded, as the download question gives sizes.
pub fn megabytes(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

/// Where speech models are kept, in Arko's folder `root`.
pub fn models_dir(root: &Path) -> PathBuf {
    root.join("models").join("speech")
}

impl SpeechModel {
    /// The model the settings name (`granite`, `parakeet`).
    pub fn named(name: &str) -> Option<&'static SpeechModel> {
        MODELS.into_iter().find(|model| model.name == name)
    }

    /// Its folder, in Arko's folder `root`.
    pub fn dir(&self, root: &Path) -> PathBuf {
        models_dir(root).join(self.folder)
    }

    /// Its pack, in Arko's folder `root`.
    pub fn pack(&self, root: &Path) -> PathBuf {
        self.dir(root).join(PACK_FILE)
    }

    pub fn is_installed(&self, root: &Path) -> bool {
        self.pack(root).is_file()
    }

    pub fn download_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    /// Where a file of it downloads from, at its revision, from `base`
    /// (`https://huggingface.co`).
    pub fn url(&self, base: &str, file: &str) -> String {
        format!(
            "{}/{}/resolve/{}/{file}",
            base.trim_end_matches('/'),
            self.repo,
            self.revision
        )
    }

    /// Its page on Hugging Face.
    pub fn page(&self) -> String {
        format!("https://huggingface.co/{}", self.repo)
    }

    /// What the download question says of it.
    pub fn offer(&self) -> String {
        format!(
            "Dictation runs on this computer with {} by {} ({} MB download, {} MB on disk, {}).",
            self.title,
            self.maker,
            megabytes(self.download_bytes()),
            megabytes(self.pack_bytes),
            self.licence
        )
    }

    /// `NOTICE.txt` in its folder: who made it, its licence, and what Arko
    /// did to it.
    pub fn notice(&self) -> String {
        format!(
            "{} by {}\n{} (revision {})\nLicensed under {}, {}\n\nArko downloaded it from Hugging Face and converted its weights to {} on this computer, into {PACK_FILE}.\n",
            self.title,
            self.maker,
            self.page(),
            self.revision,
            self.licence,
            self.licence_url,
            self.precision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_names_the_model_its_sizes_and_its_licence() {
        assert_eq!(
            GRANITE.offer(),
            "Dictation runs on this computer with Granite Speech 5.0 by IBM (948 MB download, 477 MB on disk, Apache 2.0)."
        );
        assert_eq!(
            PARAKEET.offer(),
            "Dictation runs on this computer with Parakeet TDT 0.6B v3 by NVIDIA (715 MB download, 723 MB on disk, CC BY 4.0)."
        );
    }

    #[test]
    fn files_download_from_the_pinned_revision() {
        assert_eq!(
            GRANITE.url("https://huggingface.co/", "config.json"),
            "https://huggingface.co/ibm-granite/granite-speech-5.0-470m-turboctc/resolve/1dd23f46f6d4e6eae2531982ba2e33220086dd97/config.json"
        );
        for model in MODELS {
            assert!(model.files.iter().all(|file| file.sha256.len() == 64));
            assert_eq!(SpeechModel::named(model.name), Some(model));
        }
        let root = Path::new("arko");
        assert_eq!(
            GRANITE.pack(root),
            root.join("models/speech/granite-speech-5.0-470m-turboctc/model.pack")
        );
    }

    #[test]
    fn the_notice_credits_the_maker_and_says_what_arko_changed() {
        let notice = PARAKEET.notice();
        assert!(notice.starts_with("Parakeet TDT 0.6B v3 by NVIDIA\nhttps://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 (revision 541d1f99"));
        assert!(notice.contains("CC BY 4.0, https://creativecommons.org/licenses/by/4.0/"));
        assert!(notice.contains("converted its weights to q8_0"));
    }
}
