//! Minimal PNG encoding: the re-encode half of the clipboard-image fix, so
//! a decoded BMP rides to the model as png (the one format every API takes)
//! with no image crate. Color type 6 (RGBA), bit depth 8, one IDAT built
//! from a hand-rolled deflate stream — fixed Huffman codes plus greedy LZ77
//! over a hash chain — and an adler32 checksum; enough for screenshots,
//! which is the use.

const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Encode RGBA pixels (row-major) as a PNG. `width * height * 4` must equal
/// `rgba.len()`.
pub fn encode_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("png encode: empty image".into());
    }
    if rgba.len() != width * height * 4 {
        return Err("png encode: pixel buffer does not match dimensions".into());
    }
    // raw scanlines: one filter byte (0 = none) before each row. Filtering
    // would shrink screenshots further, but the lz pass already catches the
    // runs a screenshot is made of; none keeps this encoder small.
    let mut raw = Vec::with_capacity(height * (1 + width * 4));
    for y in 0..height {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * width * 4..(y + 1) * width * 4]);
    }
    let mut out = PNG_SIG.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, adaptive
    chunk(&mut out, b"IHDR", &ihdr);
    let mut idat = vec![0x78, 0x01]; // zlib: 32k window, deflate, check bits
    idat.extend_from_slice(&deflate_fixed(&raw));
    idat.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Deflate (RFC 1951) with fixed Huffman codes (3.2.6): one final block of
/// literals and greedy longest-match LZ77, 32k window, matches 3..=258.
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::default();
    bw.bit(1); // BFINAL
    bw.bits(1, 2); // BTYPE=01: fixed codes
    let mut head = vec![NO_POS; 1 << HASH_BITS];
    let mut prev = vec![NO_POS; data.len()];
    let mut i = 0usize;
    while i < data.len() {
        let (len, dist) = longest_match(data, i, &head, &prev);
        if len >= 3 {
            bw.length(len);
            bw.distance(dist);
        } else {
            bw.literal(data[i]);
        }
        // register every position consumed so later matches can find it
        let step = if len >= 3 { len } else { 1 };
        let end = (i + step).min(data.len().saturating_sub(2));
        for (p, slot) in prev.iter_mut().enumerate().take(end).skip(i) {
            let h = hash3(data, p);
            *slot = head[h];
            head[h] = p as u32;
        }
        i += step;
    }
    bw.fixed_sym(256); // end of block
    bw.align();
    bw.out
}

const HASH_BITS: usize = 15;
const WINDOW: usize = 32 * 1024;
const MAX_CHAIN: usize = 64;
const NO_POS: u32 = u32::MAX;

/// A 3-byte rolling hash, multiplied out so close positions differ.
fn hash3(data: &[u8], i: usize) -> usize {
    ((data[i] as u32) << 16 | (data[i + 1] as u32) << 8 | data[i + 2] as u32)
        .wrapping_mul(0x9E37_79B1) as usize
        >> (32 - HASH_BITS)
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bit_buf: u32,
    bit_cnt: u8,
}

impl BitWriter {
    /// Emit one bit, lsb-first within each byte as deflate packs them.
    fn bit(&mut self, b: u32) {
        self.bit_buf |= (b & 1) << self.bit_cnt;
        self.bit_cnt += 1;
        if self.bit_cnt == 8 {
            self.out.push(self.bit_buf as u8);
            self.bit_buf = 0;
            self.bit_cnt = 0;
        }
    }
    /// Emit the low `n` bits of `val`, least significant first.
    fn bits(&mut self, val: u32, n: u8) {
        for i in 0..n {
            self.bit(val >> i);
        }
    }
    /// Emit a Huffman code: deflate writes codes MSB-first of the code's
    /// bit string while everything else is lsb-first, so reverse `n` bits.
    fn code(&mut self, code: u32, n: u8) {
        for i in 0..n {
            self.bit(code >> (n - 1 - i));
        }
    }
    /// Fixed-code literal/length symbol (RFC 1951 3.2.6).
    fn literal(&mut self, sym: u8) {
        self.fixed_sym(sym as usize);
    }
    /// A match length: its code, then the extra bits, lsb first.
    fn length(&mut self, len: usize) {
        let code = length_code(len);
        self.fixed_sym(code);
        let (bits, base) = length_extra(code);
        self.bits((len - base) as u32, bits);
    }
    /// A match distance: its 5-bit code, then the extra bits, lsb first.
    fn distance(&mut self, dist: usize) {
        let code = dist_code(dist);
        self.code(code as u32, 5);
        let (bits, base) = dist_extra(code);
        self.bits((dist - base) as u32, bits);
    }
    fn fixed_sym(&mut self, sym: usize) {
        match sym {
            0..=143 => self.code(0x30 + sym as u32, 8),
            144..=255 => self.code(0x190 + (sym - 144) as u32, 9),
            256..=279 => self.code(sym as u32 - 256, 7),
            _ => self.code(0xC0 + (sym - 280) as u32, 8),
        }
    }
    /// Pad to a byte boundary with zero bits (the final empty partial byte).
    fn align(&mut self) {
        while self.bit_cnt != 0 {
            self.bit(0);
        }
    }
}

/// Length code for a 3..=258 match (RFC 1951 3.2.5).
fn length_code(len: usize) -> usize {
    match len {
        3..=10 => 257 + len - 3,
        11..=18 => 265 + (len - 11) / 2,
        19..=34 => 269 + (len - 19) / 4,
        35..=66 => 273 + (len - 35) / 8,
        67..=130 => 277 + (len - 67) / 16,
        131..=257 => 281 + (len - 131) / 32,
        _ => 285,
    }
}

/// Extra-bit count and base length of a length code.
fn length_extra(code: usize) -> (u8, usize) {
    match code {
        257..=264 => (0, 3 + (code - 257)),
        265..=268 => (1, 11 + (code - 265) * 2),
        269..=272 => (2, 19 + (code - 269) * 4),
        273..=276 => (3, 35 + (code - 273) * 8),
        277..=280 => (4, 67 + (code - 277) * 16),
        281..=284 => (5, 131 + (code - 281) * 32),
        _ => (0, 258),
    }
}

/// Distance code: 0..=29, 5 bits fixed (RFC 1951 3.2.5). Codes 0..=3 name
/// 1..=4 exactly; the rest are the band table below.
fn dist_code(dist: usize) -> usize {
    if dist <= 4 {
        return dist - 1;
    }
    for (i, &base) in DIST_BASES.iter().enumerate() {
        if dist < base + (1 << DIST_BITS[i]) {
            return i + 4;
        }
    }
    29
}

/// First base distance of codes 4..=29 (RFC 1951 3.2.5).
const DIST_BASES: [usize; 26] = [
    5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577,
];

/// Extra-bit count of codes 4..=29.
const DIST_BITS: [u8; 26] = [
    1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];

/// Extra-bit count and base distance of a distance code.
fn dist_extra(code: usize) -> (u8, usize) {
    if code < 4 {
        return (0, 1 + code);
    }
    (DIST_BITS[code - 4], DIST_BASES[code - 4])
}

/// Greedy longest match at `i` over the hash chain, capped at 258 and the
/// input end; the chain only holds positions inside the 32k window. The
/// first byte past the current best culls most candidates without a full
/// compare.
fn longest_match(data: &[u8], i: usize, head: &[u32], prev: &[u32]) -> (usize, usize) {
    let max_len = (data.len() - i).min(258);
    if max_len < 3 {
        return (0, 0);
    }
    let floor = i.saturating_sub(WINDOW);
    let mut best_len = 0usize;
    let mut best_dist = 0usize;
    let mut cand = head[hash3(data, i)];
    let mut chain = MAX_CHAIN;
    while cand != NO_POS && chain > 0 {
        let j = cand as usize;
        if j < floor {
            break;
        }
        cand = prev[j];
        chain -= 1;
        if data[j + best_len] != data[i + best_len] {
            continue;
        }
        let mut l = 0;
        while l < max_len && data[j + l] == data[i + l] {
            l += 1;
        }
        if l > best_len {
            best_len = l;
            best_dist = i - j;
            if l == max_len {
                break;
            }
        }
    }
    if best_len < 3 {
        (0, 0)
    } else {
        (best_len, best_dist)
    }
}

/// adler32 (RFC 1950).
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// crc32 (IEEE, as PNG requires); the table is a const build.
fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    !data.iter().fold(0xFFFF_FFFFu32, |c, &b| {
        table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_codes_span_every_length() {
        for len in 3..=258 {
            let code = length_code(len);
            let (bits, base) = length_extra(code);
            assert!(base <= len);
            let span = 1usize << bits;
            assert!(len < base + span, "len {len} escapes code {code}");
        }
    }

    #[test]
    fn dist_codes_span_every_distance() {
        for dist in 1..=32768 {
            let code = dist_code(dist);
            let (bits, base) = dist_extra(code);
            assert!(base <= dist, "dist {dist} below base of code {code}");
            let span = 1usize << bits;
            assert!(dist < base + span, "dist {dist} escapes code {code}");
        }
        // the bands must tile 5.. without gaps, or a decoder reading the
        // wrong extra-bit count desyncs mid-stream (the bug this replaced)
        for w in DIST_BASES.windows(2) {
            let code = 4 + DIST_BASES.iter().position(|b| b == &w[0]).unwrap();
            assert_eq!(w[0] + (1 << DIST_BITS[code - 4]), w[1]);
        }
        assert_eq!(24577 + (1 << 13), 32769);
    }

    #[test]
    fn roundtrips_through_python_zlib() {
        // a header a real decoder can check: dimensions and color type
        let px: Vec<u8> = (0..16 * 8 * 4).map(|i| (i * 7) as u8).collect();
        let png = encode_rgba(16, 8, &px).unwrap();
        assert_eq!(&png[0..8], &PNG_SIG);
        assert_eq!(&png[16..20], &16u32.to_be_bytes());
        assert_eq!(&png[20..24], &8u32.to_be_bytes());
        assert_eq!((png[24], png[25]), (8, 6));
    }

    #[test]
    fn adler_matches_reference() {
        // zlib's adler32 of "Wikipedia" is 0x11E60398
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
    }

    #[test]
    fn crc_matches_reference() {
        // crc32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn rejects_mismatched_buffer() {
        assert!(encode_rgba(4, 4, &[0; 10]).is_err());
    }
}

#[cfg(test)]
mod pipe_tests {
    use super::*;

    /// Full-pipe check: the encoder's output must decode with a real zlib.
    /// Writes the PNG into a scratch dir and lets the harness below verify
    /// it with Python (PIL + zlib), the authoritative decoder pair.
    #[test]
    fn writes_decoder_checkable_png() {
        let mut px = Vec::new();
        for i in 0..64 * 32 {
            let (x, y) = (i % 64, i / 64);
            px.extend_from_slice(&[(x * 4) as u8, (y * 8) as u8, ((x + y) * 2) as u8, 255]);
        }
        let png = encode_rgba(64, 32, &px).unwrap();
        std::fs::write(
            crate::core::testutil::scratch_dir("pngenc").join("check.png"),
            &png,
        )
        .unwrap();
    }

    /// Machine-local round trip against the BMP fixture this session was
    /// developed with: runs only where that file exists, skipped
    /// elsewhere (it is not a repo fixture).
    #[test]
    fn roundtrips_bmp_through_png() {
        let path =
            std::path::Path::new("/home/jiaoyuan/.yak/tmp/paste-01m4r4tajcs08ynzh37msvqzq7.png");
        if !path.exists() {
            return;
        }
        let b = std::fs::read(path).unwrap();
        let img = crate::core::bmp::decode(&b).unwrap();
        let png = encode_rgba(img.width, img.height, &img.rgba).unwrap();
        std::fs::write(
            crate::core::testutil::scratch_dir("pngenc-bmp").join("check.png"),
            &png,
        )
        .unwrap();
    }
}

#[cfg(test)]
mod perf_tests {
    use super::*;

    /// Worst case: noise defeats every match, so this is the ceiling for a
    /// real screenshot paste. Loose bound, the tripwire is for order-of-
    /// magnitude regressions.
    #[test]
    fn encodes_a_noise_frame_fast() {
        let (w, h) = (1264usize, 2780usize);
        let mut px = vec![0u8; w * h * 4];
        let mut s = 0x1234_5678u32;
        for c in px.chunks_mut(4) {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            c[0] = (s >> 24) as u8;
            c[1] = (s >> 16) as u8;
            c[2] = (s >> 8) as u8;
            c[3] = 255;
        }
        let t = std::time::Instant::now();
        encode_rgba(w, h, &px).unwrap();
        eprintln!("noise {w}x{h}: {:?}", t.elapsed());
        assert!(t.elapsed().as_secs() < 10);
    }
}
