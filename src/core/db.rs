//! Ids and timestamps: monotonic ULIDs plus the ISO turn-timestamp format
//! the thread store relies on.

use std::time::{SystemTime, UNIX_EPOCH};

/// Turns store `response.datetime_utc()` — datetime.isoformat():
/// "2026-08-16T08:41:02.123456+00:00", microseconds omitted when zero.
pub fn now_turn_datetime() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!(
        "{}+00:00",
        format_utc(d.as_secs() as i64, d.subsec_micros())
    )
}

/// Compact timestamp for the session list: HH:MM when it is today, MM-DD
/// otherwise (both inputs are ISO-ish strings, so dates compare lexically).
pub fn short_time(now: &str, then: &str) -> String {
    let today = now.get(..10).unwrap_or("");
    if then.get(..10) == Some(today) {
        then.get(11..16).unwrap_or("").to_string()
    } else {
        then.get(5..10).unwrap_or("").replace('-', "/")
    }
}

fn format_utc(secs: i64, micros: u32) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil-from-days algorithm (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    if micros == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{micros:06}")
    }
}

// ULID

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

static LAST_ULID: std::sync::Mutex<Option<(u64, u128)>> = std::sync::Mutex::new(None);

/// 80 random bits — the width of a ULID's randomness half.
const RANDOM_BITS: u128 = 1u128 << 80;

pub fn ulid() -> String {
    let now_ms = now_ms();
    let mut last = LAST_ULID.lock().unwrap();
    let state = advance(*last, now_ms, random_bits);
    *last = Some(state);
    encode_ulid(state.0, state.1)
}

/// The next (millisecond, randomness) pair, given the previous one. Ids are
/// strictly monotonic within a process: at or below the last millisecond the
/// timestamp is reused and the randomness incremented, so a clock that stands
/// still *or steps backwards* can never hand out a smaller id (the wall clock
/// is read outside the lock and can regress). `fresh` supplies the random
/// half for a genuinely new millisecond.
fn advance(last: Option<(u64, u128)>, now_ms: u64, fresh: impl FnOnce() -> u128) -> (u64, u128) {
    match last {
        // the randomness half is exhausted (2**80 ids in one millisecond):
        // borrow a millisecond instead of wrapping the randomness back to
        // zero, which would break the ordering
        Some((t, prev)) if now_ms <= t && prev == RANDOM_BITS - 1 => (t + 1, fresh()),
        Some((t, prev)) if now_ms <= t => (t, prev + 1),
        _ => (now_ms, fresh()),
    }
}

fn random_bits() -> u128 {
    let mut buf = [0u8; 10];
    getrandom_fill(&mut buf);
    let mut v: u128 = 0;
    for b in buf {
        v = (v << 8) | b as u128;
    }
    v
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn encode_ulid(ts: u64, rand: u128) -> String {
    let mut chars = [0u8; 26];
    let mut t = ts as u128;
    for slot in chars.iter_mut().take(10).rev() {
        *slot = CROCKFORD[(t & 31) as usize];
        t >>= 5;
    }
    let mut r = rand;
    for slot in chars.iter_mut().skip(10).rev() {
        *slot = CROCKFORD[(r & 31) as usize];
        r >>= 5;
    }
    String::from_utf8(chars.to_vec()).unwrap()
}

fn getrandom_fill(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom")
            && f.read_exact(buf).is_ok()
        {
            return;
        }
    }
    // non-unix (and a failed /dev/urandom): RandomState is OS-seeded per
    // process and differs per call — no crate needed for ten random bytes
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut filled = 0usize;
    while filled < buf.len() {
        let v = RandomState::new().build_hasher().finish().to_le_bytes();
        let take = (buf.len() - filled).min(v.len());
        buf[filled..filled + take].copy_from_slice(&v[..take]);
        filled += take;
    }
}

#[cfg(test)]
mod ulid_tests {
    use super::*;

    #[test]
    fn ulid_shape_and_order() {
        let a = ulid();
        let b = ulid();
        assert_eq!(a.len(), 26);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert!(b > a, "same-process ulids must be strictly monotonic");
        assert!(!a.contains('i') && !a.contains('l') && !a.contains('o') && !a.contains('u'));
    }

    /// A wall clock that stands still or steps backwards must never produce
    /// an id below the one already handed out (the clock is read outside the
    /// lock, so both happen in practice).
    #[test]
    fn ids_stay_monotonic_when_the_clock_stalls_or_regresses() {
        let zero = || 0u128;
        let first = advance(None, 1_000, || 7);
        assert_eq!(first, (1_000, 7));
        assert_eq!(advance(Some(first), 1_000, zero), (1_000, 8));
        assert_eq!(advance(Some((1_000, 8)), 999, zero), (1_000, 9));
        // a fresh millisecond takes new randomness even if it is lower
        assert_eq!(advance(Some((1_000, 9)), 1_001, || 2), (1_001, 2));
        // randomness exhausted: borrow a millisecond, never wrap to zero
        assert_eq!(
            advance(Some((1_000, RANDOM_BITS - 1)), 1_000, || 3),
            (1_001, 3)
        );
    }

    #[test]
    fn an_id_generated_after_a_clock_regression_still_sorts_later() {
        let ts = now_ms();
        let last = Some((ts + 60_000, 5));
        let next = advance(last, ts, || 0);
        assert!(encode_ulid(next.0, next.1) > encode_ulid(ts + 60_000, 5));
    }

    #[test]
    fn encode_known_layout() {
        assert_eq!(encode_ulid(0, 1), "00000000000000000000000001");
        assert_eq!(&encode_ulid(1, 0)[..10], "0000000001");
    }

    #[test]
    fn datetime_formats() {
        assert!(now_turn_datetime().contains('T'));
        assert!(now_turn_datetime().ends_with("+00:00"));
    }
}
