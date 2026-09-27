use serde::{Deserialize, Serialize};

/// IDM-style categories: each gets its own folder and sidebar filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Category {
    Video,
    Music,
    Archive,
    Program,
    Document,
    Other,
}

/// Single source of truth: extension → category. Every listed extension is captured by default.
const KNOWN: &[(Category, &[&str])] = &[
    (
        Category::Video,
        &[
            "3g2", "3gp", "asf", "avi", "divx", "f4v", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpe", "mpeg",
            "mpg", "mts", "mxf", "ogm", "ogv", "qt", "rm", "rmvb", "ts", "vob", "webm", "wmv", "wtv",
        ],
    ),
    (
        Category::Music,
        &[
            "aac", "ac3", "aif", "aiff", "alac", "amr", "ape", "dts", "flac", "m4a", "m4b", "mid", "midi", "mka",
            "mp3", "mpa", "oga", "ogg", "opus", "ra", "wav", "weba", "wma", "wv",
        ],
    ),
    (
        Category::Archive,
        &[
            "7z", "ace", "arj", "bz2", "cab", "cpio", "gz", "gzip", "lha", "lz", "lzh", "lzma", "rar", "sit",
            "sitx", "tar", "tbz", "tbz2", "tgz", "txz", "tzst", "xz", "z", "zip", "zipx", "zst",
        ],
    ),
    (
        Category::Program,
        &[
            "aab", "apk", "apks", "appimage", "appx", "appxbundle", "bin", "deb", "dmg", "exe", "img", "iso",
            "jar", "msi", "msix", "msixbundle", "msu", "ova", "pkg", "plj", "qcow2", "rpm", "sea", "vhd", "vhdx",
            "vmdk", "xapk",
        ],
    ),
    (
        Category::Document,
        &[
            "azw", "azw3", "cbr", "cbz", "djvu", "doc", "docx", "epub", "mobi", "odp", "ods", "odt", "pdf", "pps",
            "ppsx", "ppt", "pptx", "rtf", "tif", "tiff", "xls", "xlsx",
        ],
    ),
];

impl Category {
    pub const ALL: [Self; 6] = [Self::Video, Self::Music, Self::Archive, Self::Program, Self::Document, Self::Other];

    pub fn of(name: &str) -> Self {
        let ext = extension(name);
        if is_split_archive(&ext) {
            return Self::Archive;
        }
        KNOWN
            .iter()
            .find(|(_, exts)| exts.contains(&ext.as_str()))
            .map_or(Self::Other, |(c, _)| *c)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Video => "Vidéos",
            Self::Music => "Musique",
            Self::Archive => "Compressés",
            Self::Program => "Programmes",
            Self::Document => "Documents",
            Self::Other => "Autres",
        }
    }
}

/// Default capture list (space-separated, user-editable in the settings).
pub fn default_captured() -> String {
    let mut all: Vec<&str> = KNOWN.iter().flat_map(|(_, exts)| exts.iter().copied()).collect();
    all.sort_unstable();
    all.join(" ")
}

/// `true` if the file name / URL ends with an extension from `list` (space/comma separated),
/// or is a split archive part (`.r00`, `.r01`…, `.001`, `.002`…).
pub fn is_capturable(list: &str, name: &str) -> bool {
    let ext = extension(name);
    !ext.is_empty() && (is_split_archive(&ext) || list.split([' ', ',', ';']).any(|e| e.trim().trim_start_matches('.').eq_ignore_ascii_case(&ext)))
}

/// Lower-case extension of a file name, or of a URL's path. Only URLs lose their `?query` and
/// `#fragment`: in a file name, `#` and `?` are ordinary characters ("Clip #42.mp4").
fn extension(name: &str) -> String {
    let path = if name.contains("://") { name.split(['?', '#']).next().unwrap_or_default() } else { name };
    let file = path.rsplit(['/', '\\']).next().unwrap_or_default();
    file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

fn is_split_archive(ext: &str) -> bool {
    match ext.as_bytes() {
        [b'r', digits @ ..] => !digits.is_empty() && digits.len() <= 3 && digits.iter().all(u8::is_ascii_digit),
        digits => digits.len() == 3 && digits.iter().all(u8::is_ascii_digit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_and_split_extensions() {
        let list = default_captured();
        assert!(is_capturable(&list, "https://x.io/a/Video.MP4?token=1"));
        assert!(is_capturable(&list, "archive.r01"));
        assert!(is_capturable(&list, "archive.r1"));
        assert!(is_capturable(&list, "backup.7z.001"));
        assert!(is_capturable(&list, "film.webm"));
        assert!(!is_capturable(&list, "page.html"));
        assert!(!is_capturable(&list, "noext"));
        assert!(!is_capturable(&list, "https://x.io/dir.zip/page"));
        assert!(!is_capturable(&list, "archive.rar5x"));
    }

    #[test]
    fn user_list_is_respected() {
        assert!(is_capturable(".iso, .ZIP", "a.zip"));
        assert!(!is_capturable("iso", "a.zip"));
    }

    #[test]
    fn categories() {
        assert_eq!(Category::of("Film (1080p).MP4"), Category::Video);
        assert_eq!(Category::of("song.m4a"), Category::Music);
        assert_eq!(Category::of("part.r07"), Category::Archive);
        assert_eq!(Category::of("data.7z.002"), Category::Archive);
        assert_eq!(Category::of("setup.exe"), Category::Program);
        assert_eq!(Category::of("book.epub"), Category::Document);
        assert_eq!(Category::of("README"), Category::Other);
    }

    /// Regression: a `#` in a title was taken for a URL fragment → "Autres" instead of "Vidéos".
    #[test]
    fn hash_and_question_mark_in_file_names() {
        assert_eq!(Category::of("Chine–USA _ terres rares #octogone93 (1080p).mp4"), Category::Video);
        assert_eq!(Category::of(r"C:\dl\Vidéos\Why? #1.mkv"), Category::Video);
        assert!(is_capturable(&default_captured(), "Clip #42.zip"));
        assert!(is_capturable(&default_captured(), "https://x.io/a.zip#section"));
        assert!(!is_capturable(&default_captured(), "https://x.io/page#a.zip"));
    }

    #[test]
    fn every_extension_belongs_to_one_category() {
        let list = default_captured();
        let all: Vec<&str> = list.split(' ').collect();
        let mut dedup = all.clone();
        dedup.dedup();
        assert_eq!(all.len(), dedup.len(), "duplicate extension in KNOWN");
        assert!(all.iter().all(|e| Category::of(&format!("x.{e}")) != Category::Other));
    }
}
