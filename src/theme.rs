//! The color theme: the reference's dark palette, one cached instance per output
//! stream. Every renderer and every chrome line prints through a palette,
//! so `NO_COLOR`, `TERM=dumb` and non-terminal streams stay escape-free
//! (redirected output must never carry raw SGR bytes), and every hue in
//! the tool comes from this one place instead of raw escape strings
//! scattered through the tree. Truecolor terminals get the reference's exact hex
//! values; everything else gets the nearest 256-color index.

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
    /// toolDiffAdded: #b5bd68
    pub diff_add: String,
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
    pub reset: String,
}

/// Build the palette: `tc` picks truecolor (`38;2;R;G;B`, the reference's exact hex)
/// over the nearest 256-color index (`38;5;N`).
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
    let red = c(204, 102, 102, 167); // #cc6666
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
        diff_add: green.clone(),
        diff_del: red.clone(),
        diff_ctx: gray,
        bold: "\x1b[1m".into(),
        dim: "\x1b[2m".into(),
        italic: "\x1b[3m".into(),
        underline: "\x1b[4m".into(),
        strike: "\x1b[9m".into(),
        green,
        red,
        cyan: c(0, 215, 255, 45),
        reset: "\x1b[0m".into(),
    }
}

static TRUECOLOR: LazyLock<Palette> = LazyLock::new(|| build(true));
static ANSI256: LazyLock<Palette> = LazyLock::new(|| build(false));
static PLAIN: LazyLock<Palette> = LazyLock::new(Palette::default);

/// the reference's exact hex palette (truecolor terminals).
pub fn truecolor() -> &'static Palette {
    &TRUECOLOR
}

/// The nearest-256-color palette (every color terminal).
pub fn ansi256() -> &'static Palette {
    &ANSI256
}

/// No escapes at all (pipes, `NO_COLOR`, dumb terminals).
pub fn plain() -> &'static Palette {
    &PLAIN
}

fn truecolor_terminal() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| {
        let v = v.to_ascii_lowercase();
        v.contains("truecolor") || v.contains("24bit")
    })
}

fn detect(is_tty: bool) -> &'static Palette {
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
    if truecolor_terminal() {
        truecolor()
    } else {
        ansi256()
    }
}

static OUT: LazyLock<&'static Palette> = LazyLock::new(|| detect(std::io::stdout().is_terminal()));
static ERR: LazyLock<&'static Palette> = LazyLock::new(|| detect(std::io::stderr().is_terminal()));

/// The stdout palette (answer streams, `llm -r` transcript).
pub fn out() -> &'static Palette {
    *OUT
}

/// The stderr palette (chrome: action lines, tool output, spinner, REPL).
pub fn err() -> &'static Palette {
    *ERR
}

fn wrap(p: &Palette, code: &str, s: &str) -> String {
    format!("{code}{s}{}", p.reset)
}

// Single-style whole-string helpers, the shape most chrome lines use;
// `e*` styles stderr chrome, `o*` stdout output.

pub fn edim(s: &str) -> String {
    wrap(err(), &err().dim, s)
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
        assert_eq!(c256.diff_del, "\x1b[38;5;167m");
        assert_eq!(c256.cyan, "\x1b[38;5;45m");
    }

    #[test]
    fn plain_palette_is_empty_everywhere() {
        let p = plain();
        assert!(p.heading.is_empty());
        assert!(p.bold.is_empty());
        assert!(p.reset.is_empty());
    }

    #[test]
    fn helpers_wrap_with_the_palette() {
        // deterministic under the (plain, piped) test harness
        let p = plain();
        assert_eq!(wrap(p, &p.dim, "x"), "x");
    }
}
