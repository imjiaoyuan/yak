//! Image generation and TTS output kinds (used via `llm -m <model> "..." --out file`).

use serde_json::{Value, json};

use super::ResolvedModel;
use crate::b64;
use crate::core::http::{HttpRequest, post_json};
use std::io::Read;
use std::path::PathBuf;

/// OpenAI images/generations. Returns the raw bytes of EVERY image in the
/// response (`-o n=3` etc. ride through the option merge), b64 or url form.
pub fn generate_image(
    m: &ResolvedModel,
    prompt: &str,
    size: Option<&str>,
) -> Result<Vec<Vec<u8>>, String> {
    let url = format!("{}/images/generations", m.base_url.trim_end_matches('/'));
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(key) = &m.api_key {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    let mut body = json!({"model": m.model_id, "prompt": prompt, "n": 1});
    if let Some(s) = size {
        body["size"] = json!(s);
    }
    for (k, v) in &m.options {
        let parsed: Value = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone()));
        body[k] = parsed;
    }
    let req = HttpRequest {
        url,
        headers,
        body: body.to_string(),
    };
    let text = post_json(&req).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("invalid response: {e}"))?;
    if let Some(err) = value["error"]["message"].as_str() {
        return Err(err.to_string());
    }
    let Some(entries) = value["data"].as_array() else {
        return Err("no image in response".to_string());
    };
    if entries.is_empty() {
        return Err("no image in response".to_string());
    }
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(b64data) = entry["b64_json"].as_str() {
            out.push(b64::decode(b64data).ok_or_else(|| "invalid base64 image".to_string())?);
        } else if let Some(url) = entry["url"].as_str() {
            out.push(download_binary(url)?);
        } else {
            return Err("response entry has neither b64_json nor url".to_string());
        }
    }
    Ok(out)
}

/// OpenAI audio/speech. Returns raw audio bytes.
pub fn generate_speech(
    m: &ResolvedModel,
    input: &str,
    voice: Option<&str>,
) -> Result<Vec<u8>, String> {
    let url = format!("{}/audio/speech", m.base_url.trim_end_matches('/'));
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(key) = &m.api_key {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    let mut body = json!({"model": m.model_id, "input": input});
    if let Some(v) = voice {
        body["voice"] = json!(v);
    }
    for (k, v) in &m.options {
        let parsed: Value = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone()));
        body[k] = parsed;
    }
    let req = HttpRequest {
        url,
        headers,
        body: body.to_string(),
    };
    post_binary(&req)
}

fn post_binary(req: &HttpRequest) -> Result<Vec<u8>, String> {
    let agent = crate::core::http::agent();
    let mut request = agent.post(&req.url);
    for (k, v) in &req.headers {
        request = request.header(k, v);
    }
    let response = request.send(&req.body).map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let mut buf = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&buf)));
    }
    Ok(buf)
}

fn download_binary(url: &str) -> Result<Vec<u8>, String> {
    let agent = crate::core::http::agent();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let mut buf = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(format!("HTTP {status}"));
    }
    Ok(buf)
}

// --out target planning ------------------------------------------------------

/// The file extension for one generated image: sniffed from the bytes when
/// they are a known image format, png otherwise.
pub fn image_ext(bytes: &[u8]) -> &'static str {
    match crate::core::attachments::sniff_mime(bytes) {
        Some("image/png") => "png",
        Some("image/jpeg") => "jpg",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        _ => "png",
    }
}

/// The file extension for a TTS response, from `-o response_format`
/// (the audio/speech parameter), mp3 by default.
pub fn speech_ext(options: &[(String, String)]) -> &'static str {
    let fmt = options
        .iter()
        .find(|(k, _)| k == "response_format")
        .map(|(_, v)| v.as_str());
    match fmt {
        Some("wav") | Some("pcm") => "wav",
        Some("opus") => "opus",
        Some("aac") => "aac",
        Some("flac") => "flac",
        _ => "mp3",
    }
}

/// Resolve a `--out` target into the files to write, refusing to overwrite
/// anything that already exists (checked for every target before the first
/// byte lands). `-` means stdout and only works for a single output; a
/// target ending in a separator (or naming an existing directory) switches
/// to auto-naming inside it — `stem.ext` for one output, `stem-N.ext` for
/// several — the directory created if needed. An explicit file name takes
/// the single output verbatim, or numbers every output `name-N.ext` when
/// there are several. `dir_stem` is the auto-naming prefix ("image" or
/// "speech").
pub fn plan_outputs(target: &str, exts: &[&str], dir_stem: &str) -> Result<Vec<PathBuf>, String> {
    let count = exts.len();
    if count == 0 {
        return Err("nothing to write".to_string());
    }
    if target == "-" {
        if count > 1 {
            return Err(format!(
                "--out - writes one output to stdout, but {count} were generated \
                 (drop -o n=... or write to a directory)"
            ));
        }
        return Ok(Vec::new()); // caller writes stdout directly
    }
    let dir_paths = |dir: &std::path::Path| -> Vec<PathBuf> {
        (1..=count)
            .map(|i| {
                if count == 1 {
                    dir.join(format!("{dir_stem}.{}", exts[0]))
                } else {
                    dir.join(format!("{dir_stem}-{i}.{}", exts[i - 1]))
                }
            })
            .collect()
    };
    let paths = if target.ends_with('/') || target.ends_with('\\') {
        let dir = std::path::Path::new(target);
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        dir_paths(dir)
    } else {
        let path = std::path::Path::new(target);
        if path.is_dir() {
            dir_paths(path)
        } else if count == 1 {
            vec![path.to_path_buf()]
        } else {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(dir_stem);
            let parent = path.parent().unwrap_or(std::path::Path::new(""));
            (1..=count)
                .map(|i| parent.join(format!("{stem}-{i}.{}", exts[i - 1])))
                .collect()
        }
    };
    let existing: Vec<String> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !existing.is_empty() {
        return Err(format!(
            "refusing to overwrite existing file(s): {} (choose another --out target)",
            existing.join(", ")
        ));
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn single_output_keeps_the_exact_name() {
        let dir = tmp("single");
        let p = plan_outputs(
            &dir.join("shot.png").display().to_string(),
            &["png"],
            "image",
        )
        .unwrap();
        assert_eq!(p, vec![dir.join("shot.png")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_extensions_keep_their_own_suffix() {
        let dir = tmp("mixed");
        let p = plan_outputs(
            &dir.join("shot.png").display().to_string(),
            &["png", "jpg"],
            "image",
        )
        .unwrap();
        assert_eq!(p, vec![dir.join("shot-1.png"), dir.join("shot-2.jpg")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_outputs_number_every_file() {
        let dir = tmp("multi");
        let target = dir.join("shot.png");
        let p = plan_outputs(
            &target.display().to_string(),
            &["png", "png", "png"],
            "image",
        )
        .unwrap();
        assert_eq!(
            p,
            vec![
                dir.join("shot-1.png"),
                dir.join("shot-2.png"),
                dir.join("shot-3.png"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trailing_separator_names_into_the_directory() {
        let dir = tmp("dirout");
        let target = format!("{}/", dir.join("d").display());
        let p = plan_outputs(&target, &["png", "png"], "image").unwrap();
        assert_eq!(
            p,
            vec![dir.join("d/image-1.png"), dir.join("d/image-2.png")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_directory_auto_names() {
        let dir = tmp("autos");
        let p = plan_outputs(&dir.display().to_string(), &["mp3"], "speech").unwrap();
        assert_eq!(p, vec![dir.join("speech.mp3")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdout_needs_exactly_one_output() {
        assert!(plan_outputs("-", &["png"], "image").unwrap().is_empty());
        assert!(plan_outputs("-", &["png", "png"], "image").is_err());
    }

    #[test]
    fn refuses_to_overwrite_any_existing_target() {
        let dir = tmp("over");
        let a = dir.join("a.png");
        std::fs::write(&a, b"old").unwrap();
        // the second target is free, but the first exists: nothing may write
        let err =
            plan_outputs(&dir.join("a.png").display().to_string(), &["png"], "image").unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        // multi-file planning fails when any NUMBERED target exists
        std::fs::write(dir.join("b-1.png"), b"").unwrap();
        let err = plan_outputs(
            &dir.join("b.png").display().to_string(),
            &["png", "png"],
            "image",
        )
        .unwrap_err();
        assert!(err.contains("b-1.png"), "{err}");
        assert!(!dir.join("b-2.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_and_speech_extensions() {
        assert_eq!(image_ext(&[0x89, b'P', b'N', b'G']), "png");
        assert_eq!(image_ext(&[0xFF, 0xD8, 0xFF]), "jpg");
        assert_eq!(image_ext(b"not an image"), "png");
        assert_eq!(speech_ext(&[]), "mp3");
        assert_eq!(
            speech_ext(&[("response_format".into(), "wav".into())]),
            "wav"
        );
        assert_eq!(
            speech_ext(&[("response_format".into(), "opus".into())]),
            "opus"
        );
        assert_eq!(
            speech_ext(&[("response_format".into(), "pcm".into())]),
            "wav"
        );
    }
}
