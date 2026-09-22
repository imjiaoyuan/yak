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
         file, continue with offset until complete."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to read"}
            },
            "required": ["path"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let path = resolve_path(cwd, args["path"].as_str().unwrap_or(""));
        if let Some(mime) = image_mime(&path) {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(format!("cannot read {} ({e})", path.display())),
            };
            // images are sent as base64 to the model: refuse a huge file
            // rather than balloon the request (we have no image processor
            // to downscale without adding a dependency)
            if bytes.len() > IMAGE_MAX_BYTES {
                return ToolOutput::err(format!(
                    "{}: image is {} (over the {} limit); downscale it first with e.g. `python -c \"from PIL import Image; im=Image.open('{}'); im.thumbnail((2000,2000)); im.save('{}')\"`",
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
                mime_type: mime,
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
        // assemble under the byte cap at line boundaries, so the note can
        // point at the first line the model has not actually seen
        let mut out = String::new();
        let mut kept = 0usize;
        for line in &w.lines {
            if out.len() + line.len() + 1 > MAX_BYTES {
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

/// The mime type for a file extension that the read tool can send to a
/// vision-capable model; `None` for anything else.
pub(super) fn image_mime(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => return None,
        }
        .to_string(),
    )
}
