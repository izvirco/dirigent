use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::io::Cursor;

const MAX_SIDE: u32 = 2000;
/// 4.5 MB of base64: headroom below Anthropic's 5 MB.
const MAX_BASE64: usize = 4_718_592;
const QUALITIES: [u8; 4] = [80, 70, 55, 40];

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: String,
    pub data: Vec<u8>,
    pub notes: Vec<String>,
}

pub fn sniff(head: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        // 0xf7: JPEG-LS, which nothing decodes.
        return (head.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if head.starts_with(PNG) {
        let ihdr = head.len() >= 16 && head[8..12] == [0, 0, 0, 13] && &head[12..16] == b"IHDR";
        return (ihdr && !animated_png(head)).then_some("image/png");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if head.starts_with(b"BM") && bmp(head) {
        return Some("image/bmp");
    }
    None
}

/// An `acTL` chunk before the first `IDAT`: an animation.
fn animated_png(head: &[u8]) -> bool {
    let mut at = 8;
    while at + 8 <= head.len() {
        let len = u32::from_be_bytes(head[at..at + 4].try_into().unwrap()) as usize;
        match &head[at + 4..at + 8] {
            b"acTL" => return true,
            b"IDAT" => return false,
            _ => at += 8 + len + 4,
        }
    }
    false
}

/// A `BM` file with a header that makes sense: plenty of other files start with those letters.
fn bmp(head: &[u8]) -> bool {
    if head.len() < 30 {
        return false;
    }
    let u32_at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    let u16_at = |i: usize| u16::from_le_bytes(head[i..i + 2].try_into().unwrap());
    let (file_size, pixels_at, header) = (u32_at(2), u32_at(10), u32_at(14));
    let (planes, bits) = match header {
        12 => (u16_at(22), u16_at(24)),
        40..=124 => (u16_at(26), u16_at(28)),
        _ => return false,
    };
    (file_size == 0 || (file_size >= 26 && pixels_at < file_size))
        && pixels_at >= 14 + header
        && planes == 1
        && [1, 4, 8, 16, 24, 32].contains(&bits)
}

pub fn process(bytes: &[u8], mime: &str) -> Result<Image, String> {
    let mime = match mime.split(';').next().unwrap_or(mime).trim().to_ascii_lowercase().as_str() {
        "image/jpg" => "image/jpeg".to_owned(),
        m => m.to_owned(),
    };
    let supported = matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp");
    let base64_len = bytes.len().div_ceil(3) * 4;
    let omitted = |why: &str| format!("[Image omitted: {why}.]");
    let decoded = decode(bytes).ok_or_else(|| omitted("could not be converted to a supported inline image format"));
    let image = match decoded {
        Ok(image) => image,
        Err(_) if supported && base64_len < MAX_BASE64 => return Ok(Image { mime, data: bytes.to_vec(), notes: vec![] }),
        Err(e) => return Err(e),
    };
    let (width, height) = (image.width(), image.height());
    if supported && width <= MAX_SIDE && height <= MAX_SIDE && base64_len < MAX_BASE64 {
        return Ok(Image { mime, data: bytes.to_vec(), notes: vec![] });
    }
    let scale = (MAX_SIDE as f64 / width as f64).min(MAX_SIDE as f64 / height as f64).min(1.);
    let (mut w, mut h) = (((width as f64 * scale).round() as u32).max(1), ((height as f64 * scale).round() as u32).max(1));
    loop {
        let scaled =
            if (w, h) == (width, height) { image.clone() } else { image.resize_exact(w, h, image::imageops::FilterType::Lanczos3) };
        if let Some((mime_out, data)) = encode(&scaled) {
            let mut notes = vec![];
            if !supported {
                notes.push(format!("[Image converted from {mime} to {mime_out}.]"));
            }
            if (w, h) != (width, height) {
                let factor = width as f64 / w as f64;
                notes.push(format!(
                    "[Image: original {width}x{height}, displayed at {w}x{h}. Multiply coordinates by {factor:.2} to map to original image.]"
                ));
            }
            return Ok(Image { mime: mime_out.into(), data, notes });
        }
        if (w, h) == (1, 1) {
            return Err(omitted("could not be resized below the inline image size limit"));
        }
        (w, h) = ((w * 3 / 4).max(1), (h * 3 / 4).max(1));
    }
}

fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

fn encode(image: &DynamicImage) -> Option<(&'static str, Vec<u8>)> {
    let fits = |data: &Vec<u8>| data.len().div_ceil(3) * 4 < MAX_BASE64;
    let png = {
        let mut out = Cursor::new(vec![]);
        image.write_to(&mut out, ImageFormat::Png).ok().map(|()| out.into_inner())
    };
    let jpeg = |quality: u8| {
        let mut out = vec![];
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        // JPEG has no alpha.
        image.to_rgb8().write_with_encoder(encoder).ok().map(|()| out)
    };
    let first = jpeg(QUALITIES[0]);
    let mut candidates = vec![("image/png", png), ("image/jpeg", first)];
    if let (Some(p), Some(j)) = (&candidates[0].1, &candidates[1].1)
        && j.len() < p.len()
    {
        candidates.swap(0, 1);
    }
    for (mime, data) in candidates {
        if let Some(data) = data.filter(fits) {
            return Some((mime, data));
        }
    }
    QUALITIES[1..].iter().find_map(|q| jpeg(*q).filter(fits)).map(|data| ("image/jpeg", data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Cursor::new(vec![]);
        DynamicImage::new_rgb8(width, height).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn small_images_go_as_they_are_and_big_ones_are_scaled() {
        let small = png(10, 10);
        assert_eq!(sniff(&small), Some("image/png"));
        assert_eq!(process(&small, "image/png").unwrap(), Image { mime: "image/png".into(), data: small, notes: vec![] });
        let big = process(&png(4000, 1000), "image/png").unwrap();
        assert_eq!(
            big.notes,
            ["[Image: original 4000x1000, displayed at 2000x500. Multiply coordinates by 2.00 to map to original image.]"]
        );
        let scaled = image::load_from_memory(&big.data).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (2000, 500));
        let mut bmp = Cursor::new(vec![]);
        DynamicImage::new_rgb8(4, 4).write_to(&mut bmp, ImageFormat::Bmp).unwrap();
        let bmp = bmp.into_inner();
        assert_eq!(sniff(&bmp), Some("image/bmp"));
        let converted = process(&bmp, "image/bmp").unwrap();
        assert_eq!((converted.mime.as_str(), converted.notes.len()), ("image/png", 1));
        assert!(process(b"not an image", "image/bmp").is_err());
        assert_eq!(sniff(b"BM, said the cow"), None);
    }
}
