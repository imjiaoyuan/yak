//! Attachment loading: `-a`/`--at` references (paths, URLs, stdin) resolved
//! into bytes with a mime type, feeding the wire `Attachment` and the thread
//! store. Shared by prompt and agent.

use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Read};

/// The wire-agnostic class of an attachment: both provider adapters branch on
/// this instead of each keeping a private copy of the accepted mime set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Image,
    Pdf,
    Text,
    /// An OpenAI `input_audio` block, carrying that wire's `format` field.
    Audio(&'static str),
}

/// Classify an attachment mime for the wire adapters; `None` is unsupported,
/// and the caller's error names the set it accepts. Which classes reach the
/// wire stays per adapter — Anthropic has no audio form and takes plain text
/// only — so the branch lives there and just the table lives here.
pub fn kind_of(mime: &str) -> Option<Kind> {
    if mime.starts_with("image/") {
        Some(Kind::Image)
    } else if mime == "application/pdf" {
        Some(Kind::Pdf)
    } else if mime.starts_with("text/") {
        Some(Kind::Text)
    } else {
        audio_format(mime).map(Kind::Audio)
    }
}

/// The `format` field of an `input_audio` block; only wav and mp3 exist.
fn audio_format(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/wav" | "audio/wave" | "audio/x-wav" => Some("wav"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        _ => None,
    }
}

/// The wire form of an attachment, carried on user messages and tool
/// results: mime, base64 bytes and a display name. `path`/`url` ride along
/// as storage provenance (where the bytes came from) — the provider
/// adapters never read them, but the thread store needs them so a stored
/// attachment can point back at its source. The serde shape is the stored
/// one: the same struct the request carries is what a thread file holds, so
/// there is no second type and no converter between them. A legacy thread
/// line wrote the payload as `base64` and a nullable `mime_type`; both are
/// accepted on read so an old thread still resumes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(default, deserialize_with = "mime_or_default")]
    pub mime_type: String,
    /// base64 payload; empty for a metadata-only record (a resumed thread
    /// whose bytes were never reloaded)
    #[serde(default, alias = "base64", skip_serializing_if = "String::is_empty")]
    pub base64_data: String,
    /// display name for file-typed wire blocks; None for stdin/clipboard bytes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// the local path the bytes were loaded from, when they were
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// the URL the bytes were fetched from, when they were
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Attachment {
    /// The same attachment without its payload: what a stored turn keeps. The
    /// provenance (`path`/`url`, name, mime) survives, so a resume can put the
    /// pixels back.
    pub fn without_payload(&self) -> Attachment {
        Attachment {
            base64_data: String::new(),
            ..self.clone()
        }
    }
}

/// Put a stored attachment's payload back from its local source. Only files are
/// read — refetching a URL would make a resume depend on the network — and a
/// source that is gone is not an error here: the adapters send a note naming
/// what went missing, so the model and the transcript both see it.
pub fn reload(a: &Attachment) -> Option<Attachment> {
    if !a.base64_data.is_empty() {
        return Some(a.clone());
    }
    let path = a.path.as_deref()?;
    let bytes = std::fs::read(path).ok()?;
    Some(Attachment {
        base64_data: crate::b64::encode(&bytes),
        ..a.clone()
    })
}

/// A missing or `null` mime reads as the empty string: the old stored shape
/// had no mime on a bytes-only record, and every consumer treats empty as
/// "unknown" rather than failing the whole resume.
fn mime_or_default<'de, D>(de: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(de)?.unwrap_or_default())
}

/// An attachment with provenance: feeds both the request and the log store.
#[derive(Debug)]
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
            path: self.path.clone(),
            url: self.url.clone(),
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

/// Piped stdin joined onto the prompt text (space-prepended) — the shared
/// head of the prompt and agent commands. No-op when stdin is a terminal or
/// an `-a -` attachment claims it.
pub fn read_piped_prompt(
    args: &crate::core::args::ParsedArgs,
    prompt: String,
) -> Result<String, String> {
    if wants_stdin(args) || std::io::stdin().is_terminal() {
        return Ok(prompt);
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Ok(prompt);
    }
    Ok(if prompt.is_empty() {
        buf
    } else {
        format!("{buf} {prompt}")
    })
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
        let (buf, header_type) = crate::core::http::get_bytes(reference)
            .map_err(|e| format!("attachment fetch failed: {e}"))?;
        let mime_type = mime
            .map(String::from)
            .or(header_type)
            .unwrap_or_else(|| guess_mime(reference).to_string());
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
        // refuse before reading: a huge file would otherwise be slurped and
        // base64-inflated into the request body (provider document limits
        // sit around 32MB, so nothing past this can be sent anyway)
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() > crate::core::http::MAX_ATTACHMENT_BYTES as u64
        {
            return Err(format!(
                "attachment {reference} is {} (over the {} limit)",
                crate::core::text::human_bytes(meta.len()),
                crate::core::text::human_bytes(crate::core::http::MAX_ATTACHMENT_BYTES as u64)
            ));
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
        path: None,
        url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_attachments_refuse_an_oversized_file() {
        let dir = crate::core::testutil::scratch_dir("att");
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![0u8; 1000]).unwrap();
        assert!(load(path.to_str().unwrap(), None).is_ok());
        // simulate oversize without writing 50MB: a metadata-only bound is
        // what the check reads, so a sparse/heavy file both refuse
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let big = dir.join("huge.bin");
            let f = std::fs::File::create(&big).unwrap();
            f.write_all_at(&[0u8], 51 * 1024 * 1024).unwrap();
            let e = load(big.to_str().unwrap(), None).unwrap_err();
            assert!(e.contains("over the"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
