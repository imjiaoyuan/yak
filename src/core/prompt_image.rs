//! Prompt-image preparation: what an image attachment looks like on the wire.
//!
//! Providers bill images by pixel dimensions, not by bytes: Anthropic scales
//! anything past 1568px on the long edge, OpenAI past 2048 — so an oversized
//! screenshot costs the same tokens as a right-sized one while making the
//! request body needlessly large (and, past the ceiling, unsendable). Policy
//! follows the reference (bounded long edge, aspect preserved, re-encode) and the mount
//! point follows the reference (normalize at load, so every request and the thread
//! store see one shape); the codecs are crates because PNG's deflate and
//! JPEG's DCT are not things to hand-roll, the box filter is ours.
//!
//! An image already inside the limit is handed back byte-for-byte: a
//! re-encode of a small screenshot would only lose quality and burn CPU.

/// Long-edge ceiling, in pixels. Anthropic's own downscale line: staying at
/// or under it means the bytes we send are the pixels the model sees, and no
/// request ever pays for pixels the provider throws away.
pub const MAX_EDGE: u32 = 1568;

/// Ceiling on total pixels any one image may decode to, as a guard on the
/// resize path alone: the attachment byte cap does not bound pixels (a few KB
/// of flat color can claim a gigabyte of decoder buffer), and an image over
/// the edge is exactly the one that would be decoded. 8192² holds any real
/// camera or screenshot with room to spare.
const MAX_PIXELS: u64 = 8192 * 8192;

/// A normalized image and what changed.
pub struct Prepared {
    /// Encoded bytes: PNG, or the untouched original when nothing was needed.
    pub bytes: Vec<u8>,
    pub mime: String,
    /// The original dimensions, for the notice the model gets.
    pub source: (u32, u32),
    /// The dimensions actually sent.
    pub size: (u32, u32),
}

impl Prepared {
    pub fn resized(&self) -> bool {
        self.source != self.size
    }

    /// What the model is told when the picture it receives is not the one the
    /// user attached: it must not measure anything off a downscaled image.
    pub fn notice(&self) -> String {
        format!(
            "[image resized from {}x{} to {}x{} pixels before sending]",
            self.source.0, self.source.1, self.size.0, self.size.1
        )
    }
}

/// Normalize an attachment. `Ok(None)` means "not an image we resize" — the
/// bytes ride untouched. An image we claim to handle but cannot decode is an
/// error: sending it would promise facts about it we do not have.
pub fn prepare(bytes: &[u8], mime: &str) -> Result<Option<Prepared>, String> {
    let mime = mime.to_ascii_lowercase();
    let png = mime == "image/png";
    let probe = match mime.as_str() {
        "image/png" => png_dimensions(bytes),
        "image/jpeg" | "image/jpg" => jpeg_dimensions(bytes),
        _ => return Ok(None),
    };
    // the cheap path, and the common one: two numbers straight off the header
    // decide it, so an image inside the ceiling is never decoded — its pixels
    // would only be re-encoded back into the same picture
    if let Some((w, h)) = probe
        && w.max(h) <= MAX_EDGE
    {
        return Ok(Some(unchanged(bytes, mime, (w, h))));
    }
    // guard the resize path (an oversized image is exactly the one that
    // decodes): the file cap does not bound pixels, and a few KB of flat
    // color can claim a gigabyte of decoder buffer
    if let Some((w, h)) = probe
        && u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err(format!("image too large to resize: {w}x{h} pixels"));
    }
    let img = if png {
        decode_png(bytes)?
    } else {
        decode_jpeg(bytes)?
    };
    if img.width.max(img.height) <= MAX_EDGE {
        return Ok(Some(unchanged(bytes, mime, (img.width, img.height))));
    }
    let (dw, dh) = fit(img.width, img.height, MAX_EDGE);
    let scaled = box_filter(&img, dw, dh);
    Ok(Some(Prepared {
        bytes: encode_png(&scaled, img.alpha)?,
        mime: "image/png".to_string(),
        source: (img.width, img.height),
        size: (dw, dh),
    }))
}

/// An image that fits: the bytes ride as they are.
fn unchanged(bytes: &[u8], mime: String, size: (u32, u32)) -> Prepared {
    Prepared {
        bytes: bytes.to_vec(),
        mime,
        source: size,
        size,
    }
}

/// A PNG's dimensions out of its IHDR: signature, chunk length, chunk type,
/// then width and height. No inflate, no pixels.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if bytes.len() < 24 || bytes[..8] != SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// A JPEG's dimensions by walking its segment markers to the first SOF. Every
/// segment carries its own length, so this reads a few dozen bytes of a file
/// however large.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[..2] != [0xff, 0xd8] {
        return None;
    }
    let mut i = 2;
    while i + 3 < bytes.len() {
        if bytes[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // fill bytes and standalone markers carry no length
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0xd8 || (0xd0..=0xd9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15, minus the two markers in that range that are not frames
        let sof = (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xcc;
        if sof {
            let head = bytes.get(i + 2..i + 2 + len)?;
            if head.len() < 7 {
                return None;
            }
            let h = u16::from_be_bytes([head[3], head[4]]) as u32;
            let w = u16::from_be_bytes([head[5], head[6]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// The largest `w`x`h` inside `max` on the long edge, keeping the aspect
/// ratio, never below one pixel on either side.
fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w >= h {
        let nh = ((h as u64 * max as u64) / w as u64).max(1) as u32;
        (max, nh)
    } else {
        let nw = ((w as u64 * max as u64) / h as u64).max(1) as u32;
        (nw, max)
    }
}

/// One RGBA8 buffer plus whether the alpha channel carries information.
struct Pixels {
    width: u32,
    height: u32,
    alpha: bool,
    data: Vec<u8>,
}

/// Average each destination pixel's source box — the cheap, correct
/// downscaler: no ringing, and the cost is linear in the source pixels.
fn box_filter(img: &Pixels, dw: u32, dh: u32) -> Pixels {
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    for dy in 0..dh {
        let y0 = dy as u64 * img.height as u64 / dh as u64;
        let y1 = (((dy as u64 + 1) * img.height as u64) / dh as u64).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx as u64 * img.width as u64 / dw as u64;
            let x1 = (((dx as u64 + 1) * img.width as u64) / dw as u64).max(x0 + 1);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * img.width as u64 + x) * 4) as usize;
                    for (acc, v) in acc.iter_mut().zip(&img.data[i..i + 4]) {
                        *acc += *v as u32;
                    }
                    n += 1;
                }
            }
            let o = (dy as usize * dw as usize + dx as usize) * 4;
            for (out_px, sum) in out[o..o + 4].iter_mut().zip(acc) {
                *out_px = (sum / n) as u8;
            }
        }
    }
    Pixels {
        width: dw,
        height: dh,
        alpha: img.alpha,
        data: out,
    }
}

fn decode_png(bytes: &[u8]) -> Result<Pixels, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("cannot read png: {e}"))?;
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or_else(|| "png output buffer size is unknown".to_string())?
    ];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("cannot decode png: {e}"))?;
    buf.truncate(info.buffer_size());
    let (alpha, rgba) = match info.color_type {
        png::ColorType::Rgba => (true, buf),
        png::ColorType::Rgb => (false, to_rgba(&buf, 3)),
        png::ColorType::GrayscaleAlpha => (true, to_rgba(&buf, 2)),
        png::ColorType::Grayscale => (false, to_rgba(&buf, 1)),
        other => return Err(format!("png color type did not expand: {other:?}")),
    };
    Ok(Pixels {
        width: info.width,
        height: info.height,
        alpha,
        data: rgba,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Pixels, String> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
    let data = decoder
        .decode()
        .map_err(|e| format!("cannot decode jpeg: {e}"))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| "jpeg has no dimensions".to_string())?;
    let (width, height) = (width as u32, height as u32);
    // zune-jpeg hands back RGB8 for the three-component case; a grayscale
    // source comes back as one byte per pixel. CMYK is converted for us.
    let px = data.len() / (width as usize * height as usize).max(1);
    let rgba = match px {
        3 => to_rgba(&data, 3),
        1 => to_rgba(&data, 1),
        other => return Err(format!("unexpected jpeg pixel size: {other}")),
    };
    Ok(Pixels {
        width,
        height,
        alpha: false,
        data: rgba,
    })
}

/// Widen a packed grayscale/RGB pixel buffer to RGBA8.
fn to_rgba(data: &[u8], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / channels * 4);
    for px in data.chunks_exact(channels) {
        match channels {
            1 => out.extend_from_slice(&[px[0], px[0], px[0], 255]),
            2 => out.extend_from_slice(&[px[0], px[0], px[0], px[1]]),
            _ => out.extend_from_slice(&[px[0], px[1], px[2], 255]),
        }
    }
    out
}

fn encode_png(img: &Pixels, alpha: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, img.width, img.height);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_color(if alpha {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("cannot encode png header: {e}"))?;
        let data = if alpha {
            img.data.clone()
        } else {
            let mut rgb = Vec::with_capacity(img.data.len() / 4 * 3);
            for px in img.data.chunks(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            rgb
        };
        writer
            .write_image_data(&data)
            .map_err(|e| format!("cannot encode png: {e}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-color PNG, encoded by the same crate the module decodes with:
    /// no fixture files, and the bytes are exactly what a screenshot would be.
    fn png_of(w: u32, h: u32, color: png::ColorType) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_color(color);
            let channels = match color {
                png::ColorType::Rgb => 3,
                png::ColorType::Rgba => 4,
                _ => 1,
            };
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&vec![120u8; (w * h * channels) as usize])
                .unwrap();
        }
        out
    }

    fn shape(bytes: &[u8]) -> (u32, u32, png::ColorType) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        (info.width, info.height, info.color_type)
    }

    #[test]
    fn a_wide_image_is_scaled_to_the_long_edge() {
        let src = png_of(3000, 1000, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        let (w, h, _) = shape(&out.bytes);
        assert_eq!((w, h), (MAX_EDGE, 522), "3000x1000 keeps its aspect ratio");
        assert_eq!(out.mime, "image/png");
    }

    #[test]
    fn a_tall_image_is_scaled_on_the_other_edge() {
        let src = png_of(400, 3000, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        let (w, h, _) = shape(&out.bytes);
        assert_eq!((w, h), (209, MAX_EDGE));
    }

    #[test]
    fn an_image_inside_the_limit_rides_byte_for_byte() {
        let src = png_of(40, 30, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        assert_eq!(out.bytes, src, "a re-encode would only lose quality");
    }

    #[test]
    fn an_image_exactly_at_the_limit_is_left_alone() {
        let src = png_of(MAX_EDGE, MAX_EDGE, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        assert_eq!(out.bytes, src);
    }

    #[test]
    fn alpha_is_kept_only_when_the_source_had_it() {
        let opaque = prepare(&png_of(3000, 1000, png::ColorType::Rgb), "image/png")
            .unwrap()
            .unwrap();
        assert_eq!(shape(&opaque.bytes).2, png::ColorType::Rgb);
        let transparent = prepare(&png_of(3000, 1000, png::ColorType::Rgba), "image/png")
            .unwrap()
            .unwrap();
        assert_eq!(shape(&transparent.bytes).2, png::ColorType::Rgba);
    }

    #[test]
    fn a_downscaled_image_carries_its_dimensions() {
        let src = png_of(3000, 1000, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        assert!(out.resized());
        assert_eq!(out.source, (3000, 1000));
        assert_eq!(out.size, (MAX_EDGE, 522));
        assert_eq!(
            out.notice(),
            "[image resized from 3000x1000 to 1568x522 pixels before sending]"
        );
    }

    #[test]
    fn an_image_inside_the_limit_reports_no_change() {
        let src = png_of(40, 30, png::ColorType::Rgb);
        let out = prepare(&src, "image/png").unwrap().unwrap();
        assert!(!out.resized());
    }

    #[test]
    fn an_image_inside_the_ceiling_is_never_decoded() {
        // a real IHDR and nothing else: the header alone decided, since a
        // decode would have failed on this
        let mut header = png_of(4, 4, png::ColorType::Rgb);
        header.truncate(33);
        let out = prepare(&header, "image/png").unwrap().unwrap();
        assert_eq!(out.bytes, header);

        // the same for a JPEG: SOF0 carries 20x10, and there is no scan
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x0a, 0x00, 0x14, 0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
        ];
        let out = prepare(&jpeg, "image/jpeg").unwrap().unwrap();
        assert_eq!(out.bytes, jpeg);
        assert_eq!(out.size, (20, 10));
    }

    #[test]
    fn anything_but_an_image_mime_is_not_ours_to_touch() {
        assert!(prepare(b"%PDF-1.7", "application/pdf").unwrap().is_none());
        assert!(prepare(b"hello", "").unwrap().is_none());
    }

    #[test]
    fn an_absurd_pixel_count_is_refused_before_decoding() {
        // a PNG that is nothing but its signature and IHDR claiming
        // 20000x20000: the refusal is decided off those 24 bytes, so the
        // test needs no pixels and the decode none either
        let mut bomb = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        bomb.extend_from_slice(&13u32.to_be_bytes());
        bomb.extend_from_slice(b"IHDR");
        bomb.extend_from_slice(&20000u32.to_be_bytes());
        bomb.extend_from_slice(&20000u32.to_be_bytes());
        bomb.extend_from_slice(&[8, 2, 0, 0, 0]); // depth, rgb, compression, filter, interlace
        bomb.extend_from_slice(&[0, 0, 0, 0]); // crc: never read
        let err = match prepare(&bomb, "image/png") {
            Err(e) => e,
            Ok(_) => panic!("an absurd canvas must not be decoded"),
        };
        assert!(err.contains("20000x20000"), "{err}");
    }

    #[test]
    fn a_claimed_image_that_does_not_decode_is_an_error() {
        // a mime we claim but cannot decode means we cannot say anything
        // true about the payload's dimensions, so it never rides silently
        let err = match prepare(b"not a png at all", "image/png") {
            Err(e) => e,
            Ok(_) => panic!("a payload we cannot decode must not ride silently"),
        };
        assert!(err.contains("png"), "{err}");
        assert!(prepare(b"not a jpeg at all", "image/jpeg").is_err());
    }
}
