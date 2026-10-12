//! BMP decoding: the one image format clipboards hand out that model APIs
//! refuse (they take png/jpeg/webp/gif). Decoded to RGBA rows so the png
//! encoder can re-encode a paste the wire accepts, with no image crate.
//!
//! What is covered and why: BITMAPINFOHEADER-and-later (`os/2` style
//! headers, 12-byte `BITMAPCOREHEADER`, are museum pieces a clipboard
//! never produces), BI_RGB and BI_BITFIELDS uncompressed rows, and 1/4/8
//! (palette), 16/24/32 bpp. RLE and PNG/JPEG-linked BI_* modes are refused:
//! a refusal names the conversion, it does not guess.

/// A decoded BMP: 8-bit RGBA rows, top-down, tightly packed.
pub struct Bmp {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` bytes, row-major from the top row.
    pub rgba: Vec<u8>,
}

/// Why a BMP could not be decoded.
pub fn is_bmp(bytes: &[u8]) -> bool {
    bytes.len() >= 14 && &bytes[0..2] == b"BM"
}

pub fn decode(bytes: &[u8]) -> Result<Bmp, String> {
    if !is_bmp(bytes) {
        return Err("not a BMP".into());
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let i32_at = |o: usize| u32_at(o) as i32;
    let data_offset = u32_at(10) as usize;
    let header_size = u32_at(14) as usize;
    // BITMAPCOREHEADER (12) has its own layout and no clipboards produce it;
    // everything real is 40 (INFO) through 124 (V5)
    if header_size < 40 {
        return Err(format!(
            "unsupported BMP header (size {header_size}; need BITMAPINFOHEADER or later)"
        ));
    }
    if bytes.len() < 14 + 40 {
        return Err("truncated BMP header".into());
    }
    let width = i32_at(18) as usize;
    let raw_height = i32_at(22);
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs() as usize;
    let bpp = u16_at(28) as usize;
    let compression = u32_at(30);
    if width == 0 || height == 0 || width > 30_000 || height > 30_000 {
        return Err(format!("unsupported BMP dimensions {width}x{height}"));
    }
    let mut rgba = vec![0u8; width * height * 4];
    let (mask, shift) = match bpp {
        24 => ([0, 0, 0], 0),
        32 => match compression {
            3 => {
                let base = 14 + header_size;
                if bytes.len() < base + 12 {
                    return Err("BI_BITFIELDS BMP missing its masks".into());
                }
                ([u32_at(base), u32_at(base + 4), u32_at(base + 8)], 0)
            }
            // BI_RGB 32bpp is fixed BGRX with the X byte ignored
            0 => ([0x00FF0000, 0x0000FF00, 0x000000FF], 0),
            c => return Err(format!("unsupported BMP compression {c} at {bpp}bpp")),
        },
        16 => match compression {
            3 => {
                let base = 14 + header_size;
                if bytes.len() < base + 12 {
                    return Err("BI_BITFIELDS BMP missing its masks".into());
                }
                ([u32_at(base), u32_at(base + 4), u32_at(base + 8)], 0)
            }
            0 => ([0x7C00, 0x03E0, 0x001F], 0),
            c => return Err(format!("unsupported BMP compression {c} at {bpp}bpp")),
        },
        8 | 4 | 1 => {
            if compression != 0 {
                return Err(format!(
                    "unsupported BMP compression {compression} at {bpp}bpp (RLE needs a decoder)"
                ));
            }
            ([0, 0, 0], 0)
        }
        bpp => return Err(format!("unsupported BMP depth {bpp}bpp")),
    };
    let _ = shift; // reserved: no current bpp path needs a palette shift
    // palette sits right after the header (V4/V5 may add masks first: the
    // offset math respects data_offset, the palette base does not need to)
    let palette_base = 14 + header_size;
    // 1/4/8bpp: palette entries are BGRX; how many is min(2^bpp, colors_used)
    let colors_used = if bpp <= 8 {
        let n = u32_at(46) as usize;
        if n == 0 { 1 << bpp } else { n.min(1 << bpp) }
    } else {
        0
    };
    if colors_used > 0 && bytes.len() < palette_base + colors_used * 4 {
        return Err("truncated BMP palette".into());
    }
    // source row stride: 4-byte aligned as the format requires
    let src_stride = (width * bpp).div_ceil(32) * 4;
    for y in 0..height {
        let off = data_offset + y * src_stride;
        let dst = if top_down { y } else { height - 1 - y };
        let row = &mut rgba[dst * width * 4..(dst + 1) * width * 4];
        let src = bytes
            .get(off..off + src_stride)
            .ok_or("truncated BMP rows")?;
        match bpp {
            24 => {
                for x in 0..width {
                    row[x * 4] = src[x * 3 + 2];
                    row[x * 4 + 1] = src[x * 3 + 1];
                    row[x * 4 + 2] = src[x * 3];
                    row[x * 4 + 3] = 0xFF;
                }
            }
            32 => {
                let masks = mask;
                for x in 0..width {
                    let px = u32::from_le_bytes([
                        src[x * 4],
                        src[x * 4 + 1],
                        src[x * 4 + 2],
                        src[x * 4 + 3],
                    ]);
                    row[x * 4] = chan(px, masks[0]);
                    row[x * 4 + 1] = chan(px, masks[1]);
                    row[x * 4 + 2] = chan(px, masks[2]);
                    row[x * 4 + 3] = 0xFF;
                }
            }
            16 => {
                let masks = mask;
                for x in 0..width {
                    let px = u16::from_le_bytes([src[x * 2], src[x * 2 + 1]]) as u32;
                    row[x * 4] = chan(px, masks[0]);
                    row[x * 4 + 1] = chan(px, masks[1]);
                    row[x * 4 + 2] = chan(px, masks[2]);
                    row[x * 4 + 3] = 0xFF;
                }
            }
            8 | 4 | 1 => {
                for x in 0..width {
                    let idx = match bpp {
                        8 => src[x] as usize,
                        4 => (src[x / 2] >> (if x % 2 == 0 { 4 } else { 0 })) as usize & 0x0F,
                        _ => ((src[x / 8] >> (7 - x % 8)) & 1) as usize,
                    };
                    let p = palette_base + idx * 4;
                    if idx >= colors_used {
                        return Err("BMP pixel index outside the palette".into());
                    }
                    row[x * 4] = bytes[p + 2];
                    row[x * 4 + 1] = bytes[p + 1];
                    row[x * 4 + 2] = bytes[p];
                    row[x * 4 + 3] = 0xFF;
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(Bmp {
        width,
        height,
        rgba,
    })
}

/// Pull one channel out of a masked pixel: `mask` names the bit run, the
/// run is scaled to 8 bits (a 5-bit run doubles up, 8-bit copies).
fn chan(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = 32 - mask.leading_zeros() - shift;
    let v = (px & mask) >> shift;
    match bits {
        0 => 0,
        8 => v as u8,
        // 5-bit x 255/31 ≈ x*8.23; the *8+ bit replication is the standard
        // 5→8 expansion and keeps white white
        5 => ((v << 3) | (v >> 2)) as u8,
        b if b < 32 => {
            let max = (1u32 << b) - 1;
            ((v as u64 * 255 + max as u64 / 2) / max as u64) as u8
        }
        _ => v as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp(bpp: u32, w: i32, h: i32, compression: u32) -> Vec<u8> {
        let mut b = vec![0u8; 14 + 40];
        b[0..2].copy_from_slice(b"BM");
        let stride = (bpp as usize * w as usize).div_ceil(32) * 4;
        let pal = if bpp <= 8 { 1024 } else { 0 };
        let off = 14 + 40 + pal;
        b[2..6].copy_from_slice(&((off + stride * h.unsigned_abs() as usize) as u32).to_le_bytes());
        b[10..14].copy_from_slice(&(off as u32).to_le_bytes());
        b[14..18].copy_from_slice(&40u32.to_le_bytes());
        b[18..22].copy_from_slice(&w.to_le_bytes());
        b[22..26].copy_from_slice(&h.to_le_bytes());
        b[26..28].copy_from_slice(&1u16.to_le_bytes());
        b[28..30].copy_from_slice(&(bpp as u16).to_le_bytes());
        b[30..34].copy_from_slice(&compression.to_le_bytes());
        b.extend(std::iter::repeat_n(0, pal));
        b.extend(std::iter::repeat_n(
            0x80,
            stride * h.unsigned_abs() as usize,
        ));
        b
    }

    #[test]
    fn decodes_24bpp_bottom_up() {
        let mut b = bmp(24, 2, 2, 0);
        let off = 54;
        // first stored row (the bottom): red, green
        b[off..off + 3].copy_from_slice(&[0, 0, 255]);
        b[off + 3..off + 6].copy_from_slice(&[0, 255, 0]);
        let img = decode(&b).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        // bottom-up storage: the stored first row is the image's last row
        assert_eq!(&img.rgba[8..12], &[255, 0, 0, 255]); // red
        assert_eq!(&img.rgba[12..16], &[0, 255, 0, 255]); // green
    }

    #[test]
    fn decodes_24bpp_top_down() {
        let mut b = bmp(24, 1, -1, 0);
        b[54..57].copy_from_slice(&[0, 255, 0]);
        let img = decode(&b).unwrap();
        assert_eq!(&img.rgba[0..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn decodes_32bpp_bitfields() {
        let mut b = vec![0u8; 14 + 40 + 12];
        b[0..2].copy_from_slice(b"BM");
        b[2..6].copy_from_slice(&(14u32 + 52 + 4).to_le_bytes());
        b[10..14].copy_from_slice(&66u32.to_le_bytes()); // past the masks
        b[14..18].copy_from_slice(&40u32.to_le_bytes());
        b[18..22].copy_from_slice(&1i32.to_le_bytes());
        b[22..26].copy_from_slice(&1i32.to_le_bytes());
        b[26..28].copy_from_slice(&1u16.to_le_bytes());
        b[28..30].copy_from_slice(&32u16.to_le_bytes());
        b[30..34].copy_from_slice(&3u32.to_le_bytes());
        b[34..38].copy_from_slice(&1u32.to_le_bytes()); // bpp plane spec: ignored
        b[54..58].copy_from_slice(&0x0000FF00u32.to_le_bytes()); // R
        b[58..62].copy_from_slice(&0x00FF0000u32.to_le_bytes()); // G
        b[62..66].copy_from_slice(&0x000000FFu32.to_le_bytes()); // B
        b.extend([0x11, 0x22, 0x33, 0x44]);
        let img = decode(&b).unwrap();
        // little-endian 0x44332211: G=0x33, R=0x22, B=0x11 under the masks
        assert_eq!(&img.rgba[0..4], &[0x22, 0x33, 0x11, 0xFF]);
    }

    #[test]
    fn decodes_16bpp_565() {
        let mut b = vec![0u8; 14 + 40];
        b[0..2].copy_from_slice(b"BM");
        b[2..6].copy_from_slice(&(14u32 + 40 + 2).to_le_bytes());
        b[10..14].copy_from_slice(&(14u32 + 40).to_le_bytes());
        b[14..18].copy_from_slice(&40u32.to_le_bytes());
        b[18..22].copy_from_slice(&2i32.to_le_bytes()); // two pixels so a 16bpp row fills 4 bytes
        b[22..26].copy_from_slice(&1i32.to_le_bytes());
        b[26..28].copy_from_slice(&1u16.to_le_bytes());
        b[28..30].copy_from_slice(&16u16.to_le_bytes());
        b.extend([0xFF, 0xFF, 0xFF, 0xFF]); // two white 565 pixels
        let img = decode(&b).unwrap();
        assert_eq!(&img.rgba[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn decodes_8bpp_palette() {
        let mut b = bmp(8, 1, 1, 0);
        // palette[0] = BGR(255,0,0), stored B,G,R,X
        b[54..58].copy_from_slice(&[0, 0, 255, 0]);
        b[54 + 1024] = 0; // pixel index 0
        let img = decode(&b).unwrap();
        assert_eq!(&img.rgba[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn refuses_rle_and_core_header() {
        let rle = bmp(8, 1, 1, 1);
        assert!(decode(&rle).is_err());
        // BITMAPCOREHEADER: header size 12
        let mut core = vec![0u8; 26];
        core[0..2].copy_from_slice(b"BM");
        core[14..18].copy_from_slice(&12u32.to_le_bytes());
        assert!(decode(&core).is_err());
    }
}

#[cfg(test)]
#[test]
fn decodes_synthetic_variants() {
    for name in ["t24", "t24t", "t32", "t32z", "t8"] {
        let p = std::path::Path::new("/home/jiaoyuan/util").join(format!("{name}.bmp"));
        if !p.exists() {
            continue;
        }
        let b = std::fs::read(&p).unwrap();
        match name {
            "t24" | "t24t" => {
                let img = decode(&b).unwrap();
                assert_eq!((img.width, img.height), (37, 23));
            }
            "t32" | "t32z" => {
                let img = decode(&b).unwrap();
                assert_eq!((img.width, img.height), (37, 23));
            }
            "t8" => {
                let img = decode(&b).unwrap();
                assert_eq!((img.width, img.height), (8, 8));
            }
            _ => {}
        }
    }
}

mod verify_tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn decodes_the_real_clipboard_bmp() {
        let path =
            std::path::Path::new("/home/jiaoyuan/.yak/tmp/paste-01m4r4tajcs08ynzh37msvqzq7.png");
        if !path.exists() {
            return; // machine-local check, not a fixture in the repo
        }
        let b = std::fs::read(path).unwrap();
        let img = decode(&b).unwrap();
        assert_eq!((img.width, img.height), (1264, 2780));
    }
}
