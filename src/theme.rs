//! The color theme: two palettes behind one gate. The answer text
//! (markdown streaming + replay) uses the reference's dark palette — truecolor
//! where `COLORTERM` says so, nearest-256 otherwise. The chrome keeps
//! the tool's original near-monochrome set (gray 90/2, bold, green `$`
//! preview, cyan `Allow?`, red errors) byte-for-byte. Both are gated the
//! same way — `NO_COLOR`, `TERM=dumb` and non-terminal streams get the
//! all-empty palette, so redirected output never carries raw SGR bytes
//! — and every escape in the tree flows through here.

use std::io::IsTerminal;
use std::sync::LazyLock;

/// the reference's the theme file colors, one field per role. Every field is a complete
/// SGR sequence (the empty string when colors are off) so call sites can
/// interpolate directly; [`Palette::reset`] closes whatever is open.
#[derive(Default)]
pub struct Palette {
    // markdown (theme tokens)
    /// mdHeading: #f0c674
    pub heading: String,
    /// mdLink: #81a2be
    pub link: String,
    /// mdLinkUrl: dim gray
    pub link_url: String,
    /// mdCode (inline code, and the accent): #8abeb7
    pub code: String,
    /// mdCodeBlock content: #b5bd68
    pub code_block: String,
    /// mdCodeBlockBorder (the ``` fences): gray
    pub code_border: String,
    /// mdQuote text: gray (+italic at the call site)
    pub quote: String,
    /// mdQuoteBorder (the │ bar): gray
    pub quote_border: String,
    /// mdHr: gray
    pub hr: String,
    /// mdListBullet: #8abeb7
    pub bullet: String,
    // tool diffs
    /// toolDiffRemoved: #cc6666
    pub diff_del: String,
    /// toolDiffContext: gray
    pub diff_ctx: String,
    // chrome attributes and hues
    pub bold: String,
    pub dim: String,
    pub italic: String,
    pub underline: String,
    pub strike: String,
    pub green: String,
    pub red: String,
    pub cyan: String,
    /// bright black — chrome metadata gray (the 90 in the legacy set)
    pub gray: String,
    pub reset: String,
}

/// The marker a run notice carries (`· retrying in 6s …`), so a line the tool
/// says about itself never reads like model output or like your own input.
/// Routine maintenance wears this; a warning that needs attention wears `!`.
pub const NOTICE: &str = "· ";

/// The answer-text palette: the reference's the theme file markdown tokens. `tc` picks
/// truecolor (`38;2;R;G;B`, the reference's exact hex) over the nearest 256-color
/// index (`38;5;N`). Only `render_md` reads the md tokens; the chrome
/// fields here mirror the legacy chrome palette so a stray use stays
/// consistent.
fn build(tc: bool) -> Palette {
    let c = |r: u8, g: u8, b: u8, i: u8| {
        if tc {
            format!("\x1b[38;2;{r};{g};{b}m")
        } else {
            format!("\x1b[38;5;{i}m")
        }
    };
    let gray = c(128, 128, 128, 244); // #808080
    let accent = c(138, 190, 183, 109); // #8abeb7
    let green = c(181, 189, 104, 143); // #b5bd68
    Palette {
        heading: c(240, 198, 116, 222),  // #f0c674
        link: c(129, 162, 190, 110),     // #81a2be
        link_url: c(102, 102, 102, 242), // #666666
        code: accent.clone(),
        code_block: green.clone(),
        code_border: gray.clone(),
        quote: gray.clone(),
        quote_border: gray.clone(),
        hr: gray.clone(),
        bullet: accent,
        diff_del: "\x1b[90m".into(),
        diff_ctx: "\x1b[2m".into(),
        bold: "\x1b[1m".into(),
        dim: "\x1b[2m".into(),
        italic: "\x1b[3m".into(),
        underline: "\x1b[4m".into(),
        strike: "\x1b[9m".into(),
        green: "\x1b[32m".into(),
        red: "\x1b[31m".into(),
        cyan: "\x1b[36m".into(),
        gray: "\x1b[90m".into(),
        reset: "\x1b[0m".into(),
    }
}

/// The chrome palette: the tool's original near-monochrome set, restored
/// byte-for-byte — default terminal color for content, bold for emphasis,
/// gray (90) for chrome and metadata, dim (2) for quiet notices, red for
/// errors, the only hues the green `$` preview and the bold-cyan `Allow?`.
/// Diff: additions default, deletions gray, headers/context dim.
fn chrome() -> Palette {
    Palette {
        green: "\x1b[32m".into(),
        red: "\x1b[31m".into(),
        cyan: "\x1b[36m".into(),
        gray: "\x1b[90m".into(),
        diff_del: "\x1b[90m".into(),
        diff_ctx: "\x1b[2m".into(),
        ..build(false)
    }
}

static TRUECOLOR: LazyLock<Palette> = LazyLock::new(|| build(true));
static ANSI256: LazyLock<Palette> = LazyLock::new(|| build(false));
static PLAIN: LazyLock<Palette> = LazyLock::new(Palette::default);

/// the reference's exact hex palette (truecolor terminals).
pub(crate) fn truecolor() -> &'static Palette {
    &TRUECOLOR
}

/// The nearest-256-color palette (every color terminal).
pub(crate) fn ansi256() -> &'static Palette {
    &ANSI256
}

/// No escapes at all (pipes, `NO_COLOR`, dumb terminals).
pub(crate) fn plain() -> &'static Palette {
    &PLAIN
}

static CHROME: LazyLock<Palette> = LazyLock::new(chrome);

/// The chrome palette (the legacy near-monochrome set).
pub(crate) fn legacy() -> &'static Palette {
    &CHROME
}

fn truecolor_terminal() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| {
        let v = v.to_ascii_lowercase();
        v.contains("truecolor") || v.contains("24bit")
    })
}

fn detect(is_tty: bool, on: fn() -> &'static Palette) -> &'static Palette {
    // NO_COLOR (non-empty) wins over everything, per no-color.org
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return plain();
    }
    if std::env::var("TERM").as_deref() == Ok("dumb") {
        return plain();
    }
    let forced = std::env::var("CLICOLOR_FORCE").is_ok_and(|v| v != "0");
    if !is_tty && !forced {
        return plain();
    }
    on()
}

static OUT: LazyLock<&'static Palette> = LazyLock::new(|| {
    detect(
        std::io::stdout().is_terminal(),
        if truecolor_terminal() {
            truecolor
        } else {
            ansi256
        },
    )
});
static ERR: LazyLock<&'static Palette> =
    LazyLock::new(|| detect(std::io::stderr().is_terminal(), legacy));

/// Cursor-control chrome for stderr: sequences that move or erase (not
/// color) live here too, so no other module hand-writes an escape. Gated
/// on stderr being a real terminal (and not `TERM=dumb`); `NO_COLOR` does
/// not apply — it turns color off, not cursor movement — and empty strings
/// when off, so printing them is a no-op, which is the point.
pub struct Cursor {
    /// erase the line the cursor is on and return to its start (`\r\x1b[2K`)
    pub clear_line: &'static str,
    /// erase the whole screen and home the cursor (`\x1b[2J\x1b[H`)
    pub clear_screen: &'static str,
}

static CURSOR: LazyLock<Cursor> = LazyLock::new(|| {
    let dumb = std::env::var("TERM").as_deref() == Ok("dumb");
    if std::io::stderr().is_terminal() && !dumb {
        Cursor {
            clear_line: "\r\x1b[2K",
            clear_screen: "\x1b[2J\x1b[H",
        }
    } else {
        Cursor {
            clear_line: "",
            clear_screen: "",
        }
    }
});

/// The cursor-control sequences for stderr chrome (see [`Cursor`]).
pub fn cursor() -> &'static Cursor {
    &CURSOR
}

/// The stdout palette (answer streams, `llm -r` transcript).
pub fn out() -> &'static Palette {
    *OUT
}

/// The stderr palette (chrome: action lines, tool output, spinner, REPL).
pub fn err() -> &'static Palette {
    *ERR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_and_256_palettes_carry_the_hues() {
        let tc = truecolor();
        assert_eq!(tc.heading, "\x1b[38;2;240;198;116m");
        assert_eq!(tc.link, "\x1b[38;2;129;162;190m");
        assert_eq!(tc.bold, "\x1b[1m");
        assert_eq!(tc.reset, "\x1b[0m");
        let c256 = ansi256();
        assert_eq!(c256.heading, "\x1b[38;5;222m");
        assert_eq!(c256.code, "\x1b[38;5;109m");
        assert_eq!(c256.green, "\x1b[32m");
        assert_eq!(c256.diff_del, "\x1b[90m");
    }

    #[test]
    fn chrome_palette_restores_the_legacy_codes() {
        let c = legacy();
        assert_eq!(c.green, "\x1b[32m");
        assert_eq!(c.red, "\x1b[31m");
        assert_eq!(c.cyan, "\x1b[36m");
        assert_eq!(c.gray, "\x1b[90m");
        assert_eq!(c.diff_del, "\x1b[90m");
        assert_eq!(c.diff_ctx, "\x1b[2m");
        assert_eq!(c.bold, "\x1b[1m");
        assert_eq!(c.dim, "\x1b[2m");
        assert_eq!(c.reset, "\x1b[0m");
        // md tokens are unused by chrome but stay well-formed
        assert!(c.heading.starts_with("\x1b[38;5;"));
    }

    #[test]
    fn plain_palette_is_empty_everywhere() {
        let p = plain();
        assert!(p.heading.is_empty());
        assert!(p.bold.is_empty());
        assert!(p.reset.is_empty());
    }
}
