//! Attachment loading: `-a`/`--at` references (paths, URLs, stdin) resolved
//! into bytes with a mime type, feeding the wire `Attachment` and the log
//! store. Shared by prompt, chat and agent.

use std::io::Read;

use crate::core::logstore::StoredAttachment;
use crate::providers::Attachment;

/// An attachment with provenance: feeds both the request and the log store.
pub struct Loaded {
    pub path: Option<String>,
    pub url: Option<String>,
    pub mime_type: Option<String>,
    pub content: Vec<u8>,
}

impl Loaded {
    pub fn request(&self) -> Attachment {
        Attachment {
            mime_type: self
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into()),
            base64_data: crate::b64::encode(&self.content),
            filename: self.file_name(),
        }
    }

    pub fn stored(&self) -> StoredAttachment {
        StoredAttachment {
            path: self.path.clone(),
            url: self.url.clone(),
            mime_type: self.mime_type.clone(),
            content: self.content.clone(),
        }
    }

    /// Last path segment of the reference, query string stripped: the
    /// display name for file-typed wire blocks.
    fn file_name(&self) -> Option<String> {
        let r = self.path.as_deref().or(self.url.as_deref())?;
        let name = r.rsplit('/').next().unwrap_or(r);
        let name = name.split('?').next().unwrap_or(name);
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// True when an `-a -` / `--at - MIMETYPE` reference claims stdin, so the
/// command must not consume it as prompt text first.
pub fn wants_stdin(args: &crate::core::args::ParsedArgs) -> bool {
    args.multi(&["attachment"]).iter().any(|v| v == "-")
        || args
            .multi(&["at"])
            .chunks(2)
            .any(|p| p.len() == 2 && p[0] == "-")
}

/// Load every `-a/--attachment` and `--at PATH MIMETYPE` pair from parsed
/// args into wire attachments — the shared entry-flag loop for prompt,
/// chat and agent.
pub fn load_args(args: &crate::core::args::ParsedArgs) -> Result<Vec<Loaded>, String> {
    let mut out = Vec::new();
    for r in &args.multi(&["attachment"]) {
        out.push(load(r.as_str(), None)?);
    }
    for pair in args.multi(&["at"]).chunks(2) {
        if pair.len() == 2 {
            out.push(load(&pair[0], Some(pair[1].as_str()))?);
        }
    }
    Ok(out)
}

/// Resolve one `-a`/`--at` reference: `-` reads stdin, http(s) URLs are
/// fetched (content-type wins the mime), anything else is a local file.
pub fn load(reference: &str, mime: Option<&str>) -> Result<Loaded, String> {
    if reference == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        return Ok(Loaded {
            path: None,
            url: None,
            mime_type: mime
                .map(String::from)
                .or_else(|| sniff_mime(&buf).map(String::from)),
            content: buf,
        });
    }
    if reference.starts_with("http://") || reference.starts_with("https://") {
        let agent = crate::core::http::agent();
        let resp = agent
            .get(reference)
            .call()
            .map_err(|e| format!("attachment fetch failed: {e}"))?;
        if resp.status().as_u16() >= 400 {
            return Err(format!("attachment fetch failed: HTTP {}", resp.status()));
        }
        let mime_type = mime
            .map(String::from)
            .or_else(|| {
                resp.headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|c| c.split(';').next().unwrap_or(c).to_string())
            })
            .unwrap_or_else(|| guess_mime(reference).to_string());
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        Ok(Loaded {
            path: None,
            url: Some(reference.to_string()),
            mime_type: Some(mime_type),
            content: buf,
        })
    } else {
        let path = std::path::Path::new(reference);
        if !path.exists() {
            return Err(format!("attachment does not exist: {reference}"));
        }
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        let mime_type = mime
            .map(String::from)
            .unwrap_or_else(|| guess_mime(reference).to_string());
        Ok(Loaded {
            path: Some(reference.to_string()),
            url: None,
            mime_type: Some(mime_type),
            content: data,
        })
    }
}

/// Extensions whose files are plain text: they attach as text documents
/// (anthropic) or extra text parts (openai-compat) instead of being refused
/// as opaque bytes.
const TEXT_EXTS: &[&str] = &[
    "txt", "text", "md", "markdown", "json", "log", "rs", "py", "js", "ts", "tsx", "jsx", "go",
    "c", "h", "cpp", "hpp", "java", "sh", "bash", "yml", "yaml", "toml", "xml", "html", "css",
    "sql", "rb", "php", "kt", "swift", "lua", "vim", "conf", "ini", "cfg",
];

/// Mime by file extension; unknown extensions stay attachable and the
/// provider decides whether it can send them.
pub fn guess_mime(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    // a real extension needs a non-empty stem, so dotfiles and bare names
    // ("Makefile", ".gitignore") stay opaque
    let ext = lower
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext)
        .unwrap_or("");
    match ext {
        "png" => return "image/png",
        "jpg" | "jpeg" => return "image/jpeg",
        "gif" => return "image/gif",
        "webp" => return "image/webp",
        "bmp" => return "image/bmp",
        "tif" | "tiff" => return "image/tiff",
        "pdf" => return "application/pdf",
        "mp3" => return "audio/mpeg",
        "wav" => return "audio/wav",
        "ogg" | "oga" => return "audio/ogg",
        "flac" => return "audio/flac",
        "m4a" | "mp4" | "m4b" => return "audio/mp4",
        "aac" => return "audio/aac",
        "webm" => return "audio/webm",
        "csv" => return "text/csv",
        _ => {}
    }
    if TEXT_EXTS.contains(&ext) {
        return "text/plain";
    }
    "application/octet-stream"
}

/// Mime by magic bytes: the only source for stdin and unnamed bytes
/// (clipboard images). None when nothing matches.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") {
        if &bytes[8..12] == b"WEBP" {
            return Some("image/webp");
        }
        if &bytes[8..12] == b"WAVE" {
            return Some("audio/wav");
        }
    }
    if bytes.starts_with(b"%PDF") {
        return Some("application/pdf");
    }
    if bytes.starts_with(b"OggS") {
        return Some("audio/ogg");
    }
    if bytes.starts_with(b"fLaC") {
        return Some("audio/flac");
    }
    // ISO base media: ftyp box at offset 4 with an M4A/M4B audio brand
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"M4A " || brand == b"M4B " || brand == b"M4AP" {
            return Some("audio/mp4");
        }
    }
    // EBML (webm/matroska); attaching one is an audio use, so say that
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some("audio/webm");
    }
    // ADTS AAC frame sync
    if bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] == 0xF1 || bytes[1] == 0xF9) {
        return Some("audio/aac");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some("image/tiff");
    }
    // ID3 header or a bare MPEG audio frame sync
    if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0)
    {
        return Some("audio/mpeg");
    }
    None
}

/// Build an attachment from raw bytes (stdin, clipboard): explicit mime wins,
/// then magic bytes, then nothing is guessable and the provider decides.
#[cfg(test)]
pub fn from_bytes(mime: Option<&str>, content: Vec<u8>) -> Attachment {
    let mime_type = mime
        .map(str::to_string)
        .or_else(|| sniff_mime(&content).map(str::to_string))
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Attachment {
        mime_type,
        base64_data: crate::b64::encode(&content),
        filename: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_matches_common_magics() {
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0, 0]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"%PDF-1.7\n"), Some("application/pdf"));
        assert_eq!(
            sniff_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_mime(b"hello"), None);
    }

    #[test]
    fn frame_sync_does_not_match_jpeg() {
        // jpeg starts FF D8 FF; an mpeg frame sync needs FF Ex
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF]), Some("image/jpeg"));
        assert_eq!(sniff_mime(&[0xFF, 0xFB, 0x90, 0x00]), Some("audio/mpeg"));
    }

    #[test]
    fn guess_covers_audio_extensions() {
        assert_eq!(guess_mime("a/b/song.MP3"), "audio/mpeg");
        assert_eq!(guess_mime("x/clip.wav"), "audio/wav");
        assert_eq!(guess_mime("x/doc.pdf"), "application/pdf");
        assert_eq!(guess_mime("x/unknown.bin"), "application/octet-stream");
    }

    #[test]
    fn guess_covers_text_audio_and_image_families() {
        assert_eq!(guess_mime("notes.md"), "text/plain");
        assert_eq!(guess_mime("src/main.rs"), "text/plain");
        assert_eq!(guess_mime("data/rows.CSV"), "text/csv");
        assert_eq!(guess_mime("a/clip.ogg"), "audio/ogg");
        assert_eq!(guess_mime("a/lossless.flac"), "audio/flac");
        assert_eq!(guess_mime("a/voice-memo.m4a"), "audio/mp4");
        assert_eq!(guess_mime("a/clip.webm"), "audio/webm");
        assert_eq!(guess_mime("img/scan.bmp"), "image/bmp");
        assert_eq!(guess_mime("img/scan.tiff"), "image/tiff");
        // dotfiles and extensionless names stay opaque
        assert_eq!(guess_mime(".gitignore"), "application/octet-stream");
        assert_eq!(guess_mime("Makefile"), "application/octet-stream");
    }

    #[test]
    fn sniff_covers_audio_containers_and_images() {
        assert_eq!(sniff_mime(b"OggS\x00\x02"), Some("audio/ogg"));
        assert_eq!(sniff_mime(b"fLaC\x00\x00"), Some("audio/flac"));
        assert_eq!(
            sniff_mime(b"\x00\x00\x00\x20ftypM4A mp42"),
            Some("audio/mp4")
        );
        assert_eq!(
            sniff_mime(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]),
            Some("audio/webm")
        );
        // ADTS sync beats the generic MPEG frame sync
        assert_eq!(sniff_mime(&[0xFF, 0xF1, 0x50, 0x80]), Some("audio/aac"));
        assert_eq!(sniff_mime(b"BM\x36\x00"), Some("image/bmp"));
        assert_eq!(sniff_mime(b"II*\x00\x08\x00"), Some("image/tiff"));
        assert_eq!(sniff_mime(b"MM\x00*"), Some("image/tiff"));
    }

    #[test]
    fn file_name_strips_query_and_dirs() {
        let l = Loaded {
            path: None,
            url: Some("https://example.com/a/shot.png?token=1".into()),
            mime_type: None,
            content: Vec::new(),
        };
        assert_eq!(l.file_name().as_deref(), Some("shot.png"));
    }

    #[test]
    fn from_bytes_prefers_explicit_mime() {
        let a = from_bytes(Some("image/png"), vec![1, 2, 3]);
        assert_eq!(a.mime_type, "image/png");
        assert_eq!(a.filename, None);
        assert_eq!(a.base64_data, crate::b64::encode(&[1, 2, 3]));
    }
}
