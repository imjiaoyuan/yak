use super::*;

/// Images the read tool will base64 into the model request; beyond this we
/// have no downscaler, so we refuse rather than balloon the context.
const IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
         Images are sent as attachments. For text files, output is truncated to 2000 lines or \
         50KB (whichever is hit first). Use offset/limit for large files. When you need the full \
         file, continue with offset until complete. For JSON/JSONL, append `?q=<filter>` to query \
         instead: data.json?q=.items[] (key paths, .[], .[N], |, [collect], select(), keys, \
         length, type, trailing ?)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to read. JSON/JSONL: append ?q=<filter> to query, e.g. file.json?q=.items[0].name"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to read"}
            },
            "required": ["path"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        let path = args["path"].as_str().unwrap_or("?").to_string();
        // a query reads as `file?q=…` on the `$` line: keep it verbatim, the
        // offset/limit window below does not apply to a query
        if split_query(&path).is_some() {
            return path;
        }
        // the requested line window, `:start-end`: two reads of the same
        // file at different offsets stay distinguishable on the `$` action
        // line. Shown only when offset/limit were given, and clamped the same
        // way `execute` reads them (offset >= 1, limit >= 1).
        let offset = args["offset"].as_u64();
        let limit = args["limit"].as_u64();
        if offset.is_none() && limit.is_none() {
            return path;
        }
        let start = offset.unwrap_or(1).max(1);
        match limit {
            Some(l) => format!("{path}:{start}-{}", start + l.max(1) - 1),
            None => format!("{path}:{start}"),
        }
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let raw_path = args["path"].as_str().unwrap_or("");
        // `?q=<filter>`: query a JSON/JSONL file instead of reading it whole
        // Split before resolving: the query is not
        // part of the path.
        if let Some((file, query)) = split_query(raw_path) {
            let path = resolve_path(cwd, &file);
            return super::jq::run_file_query(&path, &file, &query, args);
        }
        let path = resolve_path(cwd, raw_path);
        // the mime type comes from the file's magic bytes, not its extension:
        // a renamed or extension-less image still rides as vision input, and
        // a text file with an image extension still reads as text (the
        // `detectSupportedImageMimeType`). Only a prefix is read for the
        // sniff — the text path streams through the window, never loading
        // the file whole.
        let mut head = [0u8; 32];
        let head_len = {
            let mut file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
            };
            match std::io::Read::read(&mut file, &mut head) {
                Ok(n) => n,
                Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
            }
        };
        if let Some(mime) = image_mime(&head[..head_len]) {
            if mime == "image/bmp" {
                // model APIs take png/jpeg/webp/gif; BMP arrives only through
                // a processor we do not have (refused, with the local
                // way past it)
                return ToolOutput::ok(format!(
                    "Read image file [image/bmp]\n[Image omitted: BMP is not accepted by model \
                     APIs; convert it first, e.g. `python -c \"from PIL import Image; \
                     im=Image.open('{}'); im.save('{}.png')\"`]",
                    path.display(),
                    path.display(),
                ));
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
            };
            // images are sent as base64 to the model: refuse a huge file
            // rather than balloon the request (we have no image processor
            // to downscale without adding a dependency)
            if bytes.len() > IMAGE_MAX_BYTES {
                return ToolOutput::err(format!(
                    "{}: image is {} (over the {} limit); downscale it first with e.g. \
                     `python -c \"from PIL import Image; im=Image.open('{}'); \
                     im.thumbnail((2000,2000)); im.save('{}')\"`",
                    path.display(),
                    crate::core::text::human_bytes(bytes.len() as u64),
                    crate::core::text::human_bytes(IMAGE_MAX_BYTES as u64),
                    path.display(),
                    path.display(),
                ));
            }
            let mut out = ToolOutput::ok(format!(
                "Read image file [{}; {}] — passed to the model as a vision attachment.",
                mime,
                crate::core::text::human_bytes(bytes.len() as u64)
            ));
            out.attachments.push(crate::providers::Attachment {
                mime_type: mime.to_string(),
                base64_data: crate::b64::encode(&bytes),
                filename: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                path: Some(path.display().to_string()),
                url: None,
            });
            return out;
        }
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"]
            .as_u64()
            .map(|l| l as usize)
            .unwrap_or(MAX_LINES)
            .clamp(1, MAX_LINES);
        let w = match crate::read::window(&path, offset, limit) {
            Ok(w) => w,
            Err(crate::read::Error::Io(e)) => {
                return ToolOutput::err(format!("cannot read {} ({e})", path.display()));
            }
            Err(crate::read::Error::Interrupted) => {
                return ToolOutput::err("read interrupted");
            }
            Err(crate::read::Error::Binary { ext }) => {
                let hint = crate::read::binary_hint(&ext)
                    .map(|h| format!(": try `{h}`"))
                    .unwrap_or_default();
                return ToolOutput::err(format!(
                    "{}: binary format{hint}, use bash",
                    path.display()
                ));
            }
            Err(crate::read::Error::NonUtf8) => {
                return ToolOutput::err(format!(
                    "{}: not UTF-8 text — use bash to inspect or convert it (e.g. iconv)",
                    path.display()
                ));
            }
        };
        if w.lines.is_empty() {
            return match w.total {
                crate::read::LineCount::Exact(0) => ToolOutput::ok(""),
                crate::read::LineCount::Exact(n) => ToolOutput::err(format!(
                    "offset {offset} is past the end of the file ({n} lines)"
                )),
                crate::read::LineCount::AtLeast(_) => {
                    ToolOutput::err(format!("offset {offset} is past the end of the file"))
                }
            };
        }
        // the line an offset lands on can dwarf the whole cap (a minified
        // bundle): stepping offsets would keep landing on lines like it, so
        // name the bash way past it — carrying the line's true
        // size (the stored head is char-capped long before this)
        if w.first_line_capped && w.first_line_bytes > MAX_BYTES {
            return ToolOutput::ok(format!(
                "[Line {} is {}, exceeds {} limit. Use bash: sed -n '{}p' {} | head -c {}]",
                w.start,
                format_size(w.first_line_bytes),
                format_size(MAX_BYTES),
                w.start,
                raw_path,
                MAX_BYTES
            ));
        }
        // assemble under the byte cap at line boundaries, so the note can
        // point at the first line the model has not actually seen. The first
        // line rides without its newline , so a line of
        // exactly the cap fits.
        let mut out = String::new();
        let mut kept = 0usize;
        for line in &w.lines {
            let line_bytes = line.len() + if kept > 0 { 1 } else { 0 };
            if out.len() + line_bytes > MAX_BYTES {
                break;
            }
            if kept > 0 {
                out.push('\n');
            }
            out.push_str(line);
            kept += 1;
        }
        let byte_cut = kept < w.lines.len();
        let last = w.start + kept.saturating_sub(1);
        if !w.eof || byte_cut {
            // one note for both stop reasons: lines remain unseen either way,
            // and the model needs the resume offset in both
            if byte_cut {
                out.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {} (50KB limit). Use offset={} to continue.]",
                    w.start,
                    last,
                    w.total.describe(),
                    last + 1
                ));
            } else {
                out.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                    w.start,
                    last,
                    w.total.describe(),
                    last + 1
                ));
            }
        }
        ToolOutput::ok(out)
    }
}

/// Split a `path?q=filter` argument into its file and filter halves. Only
/// `q` is recognized (the parameter set narrows to the one supported);
/// a `?` with anything else is a parse error, not a filename with a `?` in
/// it (a real `?` in a path is exotic enough that the error is the clearer
/// outcome).
fn split_query(raw: &str) -> Option<(String, String)> {
    let (file, query) = raw.split_once('?')?;
    let (key, value) = query.split_once('=')?;
    if key.trim() != "q" || value.trim().is_empty() {
        return None;
    }
    Some((file.to_string(), value.trim().to_string()))
}

/// `format_size`: `51200` → `50.0KB` (no space, one decimal past 1KB).
fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The mime type the file's magic bytes declare, for the formats the read
/// tool can send to a vision-capable model; `None` for anything else (text
/// falls through to the windowed read, other binaries fail there with a
/// hint). the signature table — a renamed or extension-less image is still
/// detected, a text file with an image extension still reads as text.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    let starts = |offset: usize, sig: &[u8]| {
        bytes.len() >= offset + sig.len() && &bytes[offset..offset + sig.len()] == sig
    };
    if starts(0, &[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if starts(0, &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if starts(0, b"GIF") {
        return Some("image/gif");
    }
    if starts(0, b"RIFF") && starts(8, b"WEBP") {
        return Some("image/webp");
    }
    if starts(0, b"BM") {
        return Some("image/bmp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_names_the_requested_line_window() {
        let tool = ReadTool;
        let p = |args: Value| tool.preview(&args);
        // no window: the bare path, unchanged
        assert_eq!(p(json!({"path": "R/x.R"})), "R/x.R");
        // offset only, limit only, both — `:start-end`
        assert_eq!(p(json!({"path": "R/x.R", "offset": 2455})), "R/x.R:2455");
        assert_eq!(p(json!({"path": "R/x.R", "limit": 100})), "R/x.R:1-100");
        assert_eq!(
            p(json!({"path": "R/x.R", "offset": 2455, "limit": 20})),
            "R/x.R:2455-2474"
        );
        // offset 0 clamps to 1, matching what execute reads
        assert_eq!(
            p(json!({"path": "R/x.R", "offset": 0, "limit": 5})),
            "R/x.R:1-5"
        );
    }

    #[test]
    fn magic_bytes_name_the_format_not_the_extension() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime(b"\xff\xd8\xff\xe0data"), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(image_mime(b"RIFF....WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"BM\x36\x00\x00\x00"), Some("image/bmp"));
        // a text file that merely ends in .png is not an image
        assert_eq!(image_mime(b"# notes\n"), None);
        // a text file whose name lies is caught by nothing here — the tool
        // sniffs, so it reads as text regardless of extension
    }
}
