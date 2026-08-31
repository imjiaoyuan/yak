//! BLAKE2b (RFC 7693), unkeyed, hand-written — used for message hashes
//! ("b2:" + digest) and schema ids, matching the reference implementation's
//! digest format.

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

pub struct Blake2b {
    h: [u64; 8],
    buf: [u8; 128],
    buflen: usize,
    /// bytes hashed so far (counter t)
    t: u128,
    digest_len: usize,
}

impl Blake2b {
    pub fn new(digest_len: usize) -> Blake2b {
        let mut h = IV;
        // parameter block: digest_len, key_len=0, fanout=1, depth=1
        h[0] ^= 0x0101_0000 ^ digest_len as u64;
        Blake2b {
            h,
            buf: [0; 128],
            buflen: 0,
            t: 0,
            digest_len,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.buflen == 128 {
                // buffer full and more data arrived: compress as a non-final block
                self.t += 128;
                let block = self.buf;
                compress(&mut self.h, &block, self.t, false);
                self.buflen = 0;
            }
            let take = (128 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
        }
    }

    pub fn finalize(mut self) -> Vec<u8> {
        self.t += self.buflen as u128;
        for b in self.buf[self.buflen..].iter_mut() {
            *b = 0;
        }
        let block = self.buf;
        compress(&mut self.h, &block, self.t, true);
        let mut out = Vec::with_capacity(self.digest_len);
        for word in self.h {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.truncate(self.digest_len);
        out
    }
}

fn g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

fn compress(h: &mut [u64; 8], block: &[u8; 128], t: u128, last: bool) {
    let mut m = [0u64; 16];
    for (i, word) in m.iter_mut().enumerate() {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&block[i * 8..i * 8 + 8]);
        *word = u64::from_le_bytes(bytes);
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    for round in 0..12 {
        let s = &SIGMA[round % 10];
        g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

/// BLAKE2b-128-bit digest as lowercase hex (the original's digest_size=16).
pub fn blake2b16_hex(data: &[u8]) -> String {
    let mut hasher = Blake2b::new(16);
    hasher.update(data);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // known blake2b-16 test vectors
        assert_eq!(blake2b16_hex(b""), "cae66941d9efbd404e4d88758ea67670");
        assert_eq!(blake2b16_hex(b"abc"), "cf4ab791c62b8d2b2109c90275287816");
        assert_eq!(
            blake2b16_hex(b"hello world"),
            "e9a804b2e527fd3601d2ffc0bb023cd6"
        );
        assert_eq!(
            blake2b16_hex(
                &[0u8; 64]
                    .iter()
                    .enumerate()
                    .map(|(i, _)| i as u8)
                    .collect::<Vec<u8>>()
            ),
            "59059895958b8a56277edb046df67166"
        );
        assert_eq!(
            blake2b16_hex(
                b"The quick brown fox jumps over the lazy dog"
                    .repeat(3)
                    .as_slice()
            ),
            "885f673b6fd4b5214840ecba5dbb6f83"
        );
    }

    #[test]
    fn incremental_matches_oneshot() {
        let data: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        let mut h = Blake2b::new(16);
        for chunk in data.chunks(37) {
            h.update(chunk);
        }
        let incremental = h.finalize();
        let oneshot = {
            let mut h = Blake2b::new(16);
            h.update(&data);
            h.finalize()
        };
        assert_eq!(incremental, oneshot);
    }
}
