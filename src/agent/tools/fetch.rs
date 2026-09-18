use super::*;

/// Fetch a URL and hand the model readable text: HTML is stripped to text
/// with its <title> extracted, JSON/XML/plain text pass through untouched, and
/// the final URL is reported when redirects moved the fetch. Reads are
/// Tier::Read, so the agent can fetch freely without an approval prompt.
pub(super) struct FetchTool;

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn description(&self) -> &str {
        "Fetch a URL as text (HTML stripped); JSON, XML and plain text pass through."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "http(s) URL to fetch"}
            },
            "required": ["url"]
        })
    }
    fn preview(&self, args: &Value) -> String {
        args["url"].as_str().unwrap_or("").to_string()
    }
    fn execute(&self, args: &Value, _cwd: &Path, _log: &mut dyn FnMut(&str)) -> ToolOutput {
        let Some(url) = args["url"].as_str() else {
            return ToolOutput::err("missing string argument 'url'");
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolOutput::err(format!("only http(s) URLs are allowed (refusing {url})"));
        }
        let page = match crate::core::http::fetch_page(url) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let is_html = page.content_type.eq_ignore_ascii_case("text/html")
            || page
                .content_type
                .eq_ignore_ascii_case("application/xhtml+xml");
        let out = if is_html {
            let text = html_to_text(&page.body);
            let mut s = String::new();
            if page.url != url {
                s.push_str(&format!("[final URL: {}]\n", page.url));
            }
            if let Some(title) = extract_title(&page.body)
                && !title.is_empty()
            {
                s.push_str(&title);
                s.push_str("\n\n");
            }
            s.push_str(&text);
            s
        } else {
            // JSON, XML, plain text, CSV, … — pass through untouched
            page.body.trim().to_string()
        };
        // the shared head cut: a fetched page reads top-down, so keep the
        // beginning, and enforce the same line/byte/token caps every other
        // tool result obeys — a 256KB page used to ride into history whole
        // and be re-sent on every later round
        ToolOutput::ok(truncate_head_marked(&out))
    }
}

/// The first <title>…</title> of an HTML document, whitespace-collapsed and
/// entity-decoded (a bare helper — no full HTML parser in the dependency set).
pub(super) fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after_open = &html[start..];
    let close = after_open.find('>')?;
    let content_start = start + close + 1;
    let lower_rest = &lower[content_start..];
    let end = lower_rest.find("</title")?;
    let raw = &html[content_start..content_start + end];
    let title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

/// Strip markup down to readable text without pulling in an HTML parser:
/// drop script/style blocks, remove tags, decode common entities, collapse
/// runs of blank lines. Good enough for docs and articles.
pub(super) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut skip_depth = 0i32; // inside <script>/<style>
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        if skip_depth == 0 {
            out.push_str(&rest[..pos]);
        }
        let tag_start = pos;
        let tag_end = rest[pos..]
            .find('>')
            .map(|i| pos + i + 1)
            .unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        let lower = tag.to_ascii_lowercase();
        let name: String = lower
            .trim_start_matches('<')
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_string();
        match name.as_str() {
            "script" | "style" => {
                if lower.contains("</") {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
            }
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "pre"
                if skip_depth == 0 =>
            {
                out.push('\n');
            }
            _ => {}
        }
        rest = &rest[tag_end..];
        // consume the text until the next tag, skipping script/style bodies
        if let Some(end) = rest.find('<') {
            if skip_depth == 0 {
                out.push_str(&rest[..end]);
            }
            rest = &rest[end..];
        } else {
            if skip_depth == 0 {
                out.push_str(rest);
            }
            rest = "";
        }
    }
    out.push_str(rest);
    // decode the common entities (&amp; last: it must not re-decode the
    // text the other expansions just produced)
    let mut decoded = out.replace("&nbsp;", " ");
    decoded = decoded.replace("&lt;", "<").replace("&gt;", ">");
    decoded = decoded.replace("&quot;", "\"").replace("&#39;", "'");
    decoded = decoded.replace("&amp;", "&");
    // collapse blank-line runs and trim
    let mut clean = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                clean.push('\n');
            }
            continue;
        }
        blank = 0;
        clean.push_str(t);
        clean.push('\n');
    }
    clean.trim().to_string()
}
