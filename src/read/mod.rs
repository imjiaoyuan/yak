//! File reading behind the agent's `read` tool and rag ingest: plain text
//! only, streamed through a line window (`window`) or read whole under a
//! budget (`text`). Everything else is refused as binary with a hint at
//! local tooling (pdftotext, samtools, duckdb, ...) — parsing other formats
//! is the model's job via the bash tool, not the binary's.

mod lines;

use std::io::{BufRead, BufReader};
use std::path::Path;

/// Per-line character cap applied to windowed lines.
pub(crate) const LINE_CHAR_CAP: usize = 2000;

/// Extensions refused before any sniffing: known-binary families and the
/// formats deliberately left to local tooling.
const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tgz", "7z", "rar", "exe",
    "dll", "so", "dylib", "class", "jar", "wasm", "mp3", "mp4", "mov", "avi", "sqlite", "db",
    "bam", "cram", "bcf", "parquet", "feather", "arrow", "ipc", "hdf5", "h5", "xls", "doc", "ppt",
    "npy", "npz",
];

/// A line count that is exact only when the scan reached end-of-input.
pub enum LineCount {
    Exact(usize),
    AtLeast(usize),
}

impl LineCount {
    /// "3" or "≥3": rendered into headers and continuation notes.
    pub fn describe(&self) -> String {
        match self {
            LineCount::Exact(n) => n.to_string(),
            LineCount::AtLeast(n) => format!("≥{n}"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(String),
    /// a format we do not parse; the caller renders a bash hint.
    Binary {
        ext: String,
    },
}

pub struct Window {
    pub lines: Vec<String>,
    pub total: LineCount,
    /// 1-based file line number of `lines[0]`; 0 when offset ran past EOF.
    pub start: usize,
    /// the scan reached end-of-input inside the window.
    pub eof: bool,
    pub size: u64,
}

/// Windowed read for the agent's read tool: one open, one stat; the NUL
/// sniff peeks the buffer without consuming, so the scan rereads it.
pub fn window(path: &Path, offset: usize, limit: usize) -> Result<Window, Error> {
    let ext = ext_of(path);
    if BINARY_EXTS.contains(&ext.as_str()) {
        return Err(Error::Binary { ext });
    }
    let file = std::fs::File::open(path).map_err(io_err)?;
    let size = file.metadata().map_err(io_err)?.len();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    if is_binary(reader.fill_buf().map_err(io_err)?) {
        return Err(Error::Binary { ext });
    }
    let w = lines::window_reader(&mut reader, offset.max(1), limit.max(1)).map_err(io_err)?;
    Ok(Window {
        lines: w.lines,
        total: w.total,
        start: w.start,
        eof: w.eof,
        size,
    })
}

/// Whole-file read for rag ingest: the document's text, cut at `budget`
/// bytes with `truncated` set when anything was left behind.
/// NUL byte in the first 8 KiB means binary.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Suggested local tooling for the formats we refuse to parse.
pub(crate) fn binary_hint(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "pdftotext file.pdf -",
        "bam" | "cram" | "bcf" => "samtools view -h",
        "parquet" | "feather" | "arrow" | "ipc" | "hdf5" | "h5" => "duckdb",
        "xls" | "doc" | "ppt" => "libreoffice --headless --convert-to csv",
        "npy" | "npz" => "python (numpy)",
        "gz" | "tgz" | "zip" => "zcat / unzip -p",
        _ => return None,
    })
}

fn io_err(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

/// True when the path's extension marks it as binary — the grep tool skips
/// these without reading them (the read tool refuses them with a hint).
pub(crate) fn is_binary_path(path: &Path) -> bool {
    BINARY_EXTS.contains(&ext_of(path).as_str())
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_binary_detects_nul() {
        assert!(!is_binary(b"plain text\nlines"));
        assert!(is_binary(b"tex\0t"));
        let mut far = vec![b'a'; 9000];
        far.push(0);
        assert!(!is_binary(&far), "NUL past the first 8 KiB is not a signal");
    }

    #[test]
    fn test_binary_hint_per_ext() {
        assert_eq!(binary_hint("pdf"), Some("pdftotext file.pdf -"));
        assert_eq!(binary_hint("bam"), Some("samtools view -h"));
        assert_eq!(binary_hint("parquet"), Some("duckdb"));
        assert_eq!(binary_hint("txt"), None);
    }
}
