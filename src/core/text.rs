//! Small string utilities shared across modules.

/// Largest byte index <= `idx` that is a char boundary.
pub fn floor_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Smallest byte index >= `idx` that is a char boundary.
/// [A-Za-z0-9_-]{1,max}: the name class shared by the plugin surfaces
/// (script tools, commands-dir lookups); callers layer their extra rules.
pub fn valid_plugin_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn ceil_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Truncate to at most `max` chars, appending "..." when anything was cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}...")
    }
}

/// Parse KEY=VALUE items, rejecting entries without `=`.
pub fn parse_kv(items: &[String]) -> Result<Vec<(String, String)>, String> {
    items
        .iter()
        .map(|item| {
            item.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("expected KEY=VALUE, got '{item}'"))
        })
        .collect()
}

/// Damerau-Levenshtein distance (optimal string alignment) over unicode
/// scalars: substitution, insertion, deletion and transposition each cost
/// one edit, matching how command typos actually happen.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    let mut prev2 = (0..=m).collect::<Vec<usize>>();
    let mut prev = prev2.clone();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(prev2[j - 2] + 1);
            }
            cur[j] = best;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Human file size with a space and one decimal above bytes ("12 B",
/// "340 KB", "2.1 GB").
pub fn human_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_with_space() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(340 * 1024), "340.0 KB");
        assert_eq!(human_bytes(0), "0 B");
    }

    #[test]
    fn boundaries_clamp_to_char_edges() {
        let s = "中a中"; // byte boundaries at 0, 3, 4, 7
        assert_eq!(floor_boundary(s, 2), 0);
        assert_eq!(floor_boundary(s, 4), 4);
        assert_eq!(ceil_boundary(s, 1), 3);
        assert_eq!(ceil_boundary(s, 5), 7);
        assert_eq!(floor_boundary(s, 0), 0);
        assert_eq!(floor_boundary(s, s.len()), s.len());
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_chars("abcd", 3), "abc...");
        assert_eq!(truncate_chars("ab", 3), "ab");
    }

    #[test]
    fn edit_distance_counts_substitutions_and_gaps() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("lgos", "logs"), 1);
    }
}
