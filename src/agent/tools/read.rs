use super::*;

/// Max files one `read` call may batch.
pub(super) const READ_MAX_FILES: usize = 5;

/// The read tool serves one window at a time; bash/grep keep MAX_LINES.
/// the reference's value: 2000 lines under the same 50KB cap — one call covers a whole
/// typical source file, so the model spends fewer round trips paging.
pub(super) const READ_MAX_LINES: usize = 2000;

/// Images the read tool will base64 into the model request; beyond this we
/// have no downscaler, so we refuse rather than balloon the context.
pub(super) const IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Read file contents: `path` for one file or `paths` (up to 5) for several. `offset`/`limit` \
         slice lines; images become vision input. Other binary formats are refused with a hint at \
         local tooling (pdftotext, samtools, ...)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Single file path, relative to the working directory"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Several file paths (max 5) read in one call"},
                "offset": {"type": "integer", "description": "1-based line to start from (applies to each file)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return per file"},
            },
            "required": []
        })
    }
    fn preview(&self, args: &Value) -> String {
        if let Some(list) = args["paths"].as_array().filter(|a| !a.is_empty()) {
            let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
            return names.join(", ");
        }
        args["path"].as_str().unwrap_or("?").to_string()
    }
    fn execute(&self, args: &Value, cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let multi: Vec<String> = match args["paths"].as_array() {
            Some(list) => list
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            None => Vec::new(),
        };
        if multi.is_empty() {
            if args["path"].as_str().is_none() {
                return ToolOutput::err("read: pass `path` (one file) or `paths` (several)");
            }
            return self.read_one(args, cwd);
        }
        if multi.len() > READ_MAX_FILES {
            return ToolOutput::err(format!(
                "read: at most {READ_MAX_FILES} files per call, got {} — split the work",
                multi.len()
            ));
        }
        // one slice for every file: offset/limit apply uniformly
        let mut combined = String::new();
        let mut attachments = Vec::new();
        let mut ok = 0usize;
        for p in &multi {
            let mut one = json!({"path": p});
            if let Some(o) = args.get("offset") {
                one["offset"] = o.clone();
            }
            if let Some(l) = args.get("limit") {
                one["limit"] = l.clone();
            }
            let out = self.read_one(&one, cwd);
            combined.push_str(&out.content);
            combined.push('\n');
            attachments.extend(out.attachments);
            if !out.is_error {
                ok += 1;
            }
        }
        if ok == 0 {
            ToolOutput::err(combined.trim_end().to_string())
        } else {
            let mut out = ToolOutput::ok(combined.trim_end().to_string());
            out.attachments = attachments;
            out
        }
    }
}

impl ReadTool {
    /// The single-file body both call shapes share.
    fn read_one(&self, args: &Value, cwd: &Path) -> ToolOutput {
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
            .unwrap_or(READ_MAX_LINES)
            .clamp(1, READ_MAX_LINES);
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
        };
        if w.lines.is_empty() {
            return match w.total {
                crate::read::LineCount::Exact(0) => ToolOutput::ok("(empty file)"),
                crate::read::LineCount::Exact(n) => ToolOutput::err(format!(
                    "offset {offset} is past the end of the file ({n} lines)"
                )),
                crate::read::LineCount::AtLeast(_) => {
                    ToolOutput::err(format!("offset {offset} is past the end of the file"))
                }
            };
        }
        let numbered: Vec<String> = w
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}: {}", w.start + i, l))
            .collect();
        // assemble under the byte cap at line boundaries, so the note can
        // point at the first line the model has not actually seen
        let mut out = String::new();
        let mut kept = 0usize;
        for line in &numbered {
            if out.len() + line.len() + 1 > MAX_BYTES {
                break;
            }
            if kept > 0 {
                out.push('\n');
            }
            out.push_str(line);
            kept += 1;
        }
        let byte_cut = kept < numbered.len();
        let last = w.start + kept - 1;
        if !w.eof || byte_cut {
            // one note for both stop reasons: lines remain unseen either way,
            // and the model needs the resume offset in both
            out.push_str(&format!(
                "\n[Showing lines {}-{} of {}. Use offset={} to continue.]\n",
                w.start,
                last,
                w.total.describe(),
                last + 1
            ));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let header = format!(
            "[{} · text · {} lines · {} · showing {}-{}]\n",
            name,
            w.total.describe(),
            crate::core::text::human_bytes(w.size),
            w.start,
            last
        );
        ToolOutput::ok(format!("{header}{out}"))
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
