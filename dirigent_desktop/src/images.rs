//! Pasted images are shrunk before they are attached, so the thumbnail, the saved draft and
//! the prompt all carry the same small image. A screenshot of a big screen is tens of MB
//! as a PNG: more than a request can carry, and more than models take (Anthropic scales
//! anything past ~1568 px on the long edge down and rejects images over 5 MB).

use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};
use std::io::Cursor;

/// Longest edge an attached image keeps.
pub const MAX_EDGE: u32 = 1568;
/// Size past which an image is re-encoded, whatever its pixel size; well under the 5 MB the
/// API takes, base64 included.
pub const MAX_BYTES: usize = 2 << 20;
/// Quality of the JPEG a PNG too big for `MAX_BYTES` (a photo) falls back to.
const JPEG_QUALITY: u8 = 85;

/// `image` as it gets attached: as it is if it fits (`MAX_EDGE`, `MAX_BYTES`, a format the
/// models take), else scaled to fit and re-encoded. Kept as is when it doesn't decode.
pub fn fit(image: gpui::Image) -> gpui::Image {
    match shrink(&image.bytes) {
        Some((format, bytes)) => gpui::Image::from_bytes(format, bytes),
        None => image,
    }
}

/// `bytes` re-encoded to fit: a PNG, or a JPEG if the PNG is still over `MAX_BYTES`.
/// `None` if they fit already or don't decode.
fn shrink(bytes: &[u8]) -> Option<(gpui::ImageFormat, Vec<u8>)> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let taken =
        matches!(reader.format()?, image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif | image::ImageFormat::WebP);
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    if taken && w.max(h) <= MAX_EDGE && bytes.len() <= MAX_BYTES {
        return None;
    }
    // Turned upright first: the EXIF orientation doesn't survive the re-encode.
    let orientation = decoder.orientation().ok()?;
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    if img.width().max(img.height()) > MAX_EDGE {
        img = img.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3);
    }
    let mut png = Vec::new();
    img.write_to(Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    if png.len() <= MAX_BYTES {
        return Some((gpui::ImageFormat::Png, png));
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).encode_image(&img.into_rgb8()).ok()?;
    Some((gpui::ImageFormat::Jpeg, jpeg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn png(img: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn size(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes).unwrap().into_rgb8().dimensions()
    }

    #[test]
    fn small_images_stay() {
        assert!(shrink(&png(&RgbImage::new(MAX_EDGE, 20))).is_none());
    }

    #[test]
    fn big_screenshots_scale_to_the_long_edge() {
        let (format, bytes) = shrink(&png(&RgbImage::new(2000, 1000))).unwrap();
        assert_eq!(format, gpui::ImageFormat::Png);
        assert_eq!(size(&bytes), (MAX_EDGE, 784));
    }

    #[test]
    fn noisy_images_fall_back_to_jpeg() {
        // Noise doesn't compress: as a PNG this is ~3 MB, over `MAX_BYTES`.
        let mut seed = 1u32;
        let noise = RgbImage::from_fn(1000, 1000, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [_, r, g, b] = seed.to_le_bytes();
            Rgb([r, g, b])
        });
        let original = png(&noise);
        assert!(original.len() > MAX_BYTES);
        let (format, bytes) = shrink(&original).unwrap();
        assert_eq!(format, gpui::ImageFormat::Jpeg);
        assert!(bytes.len() < original.len());
        assert_eq!(size(&bytes), (1000, 1000));
    }
}
