use std::{io::Cursor, path::Path, sync::Arc};

use gpui::{Image, ImageFormat};

pub(crate) fn is_harness_image_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp
    )
}

fn decoder_format(format: ImageFormat) -> Option<image::ImageFormat> {
    match format {
        ImageFormat::Png => Some(image::ImageFormat::Png),
        ImageFormat::Jpeg => Some(image::ImageFormat::Jpeg),
        ImageFormat::Webp => Some(image::ImageFormat::WebP),
        ImageFormat::Gif => Some(image::ImageFormat::Gif),
        ImageFormat::Bmp => Some(image::ImageFormat::Bmp),
        ImageFormat::Tiff => Some(image::ImageFormat::Tiff),
        ImageFormat::Ico => Some(image::ImageFormat::Ico),
        ImageFormat::Pnm => Some(image::ImageFormat::Pnm),
        ImageFormat::Svg => None,
    }
}

fn harness_format(format: image::ImageFormat) -> Option<ImageFormat> {
    match format {
        image::ImageFormat::Png => Some(ImageFormat::Png),
        image::ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
        image::ImageFormat::WebP => Some(ImageFormat::Webp),
        image::ImageFormat::Gif => Some(ImageFormat::Gif),
        _ => None,
    }
}

fn encode_png(
    bytes: &[u8],
    format: image::ImageFormat,
    source: &str,
) -> Result<Arc<Image>, String> {
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|error| format!("could not decode {source} as {format:?}: {error}"))?;
    let mut png = Vec::new();
    decoded
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| format!("could not encode {source} as PNG: {error}"))?;
    tracing::info!(
        source,
        from_format = ?format,
        input_bytes = bytes.len(),
        output_bytes = png.len(),
        "converted image attachment to PNG"
    );
    Ok(Arc::new(Image::from_bytes(ImageFormat::Png, png)))
}

/// Ensures that an image has one of the MIME types accepted by the Pi/Codex harness.
pub(crate) fn normalize_for_harness(image: Arc<Image>, source: &str) -> Result<Arc<Image>, String> {
    if image.bytes.is_empty() {
        return Err(format!("{source} contains no image data"));
    }
    if is_harness_image_format(image.format) {
        return Ok(image);
    }
    let format = decoder_format(image.format).ok_or_else(|| {
        format!(
            "{source} uses {}, which cannot be converted to PNG",
            image.format.mime_type()
        )
    })?;
    encode_png(&image.bytes, format, source)
}

/// Loads a path when its contents are an image. Unknown file types return `Ok(None)` so callers
/// can preserve the normal path-text paste behavior.
pub(crate) fn load_external_image(path: &Path) -> Result<Option<Arc<Image>>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let Ok(format) = image::guess_format(&bytes) else {
        return Ok(None);
    };
    let source = path.display().to_string();
    let image = if let Some(harness_format) = harness_format(format) {
        // File-list clipboard entries are untrusted paths, so validate their contents even when
        // no format conversion is required. Keep the original bytes for accepted animated/image
        // formats instead of needlessly re-encoding them.
        image::load_from_memory_with_format(&bytes, format)
            .map_err(|error| format!("could not decode {source} as {format:?}: {error}"))?;
        Arc::new(Image::from_bytes(harness_format, bytes))
    } else {
        encode_png(&bytes, format, &source)?
    };
    tracing::info!(
        path = %path.display(),
        mime_type = image.format.mime_type(),
        bytes = image.bytes.len(),
        "loaded image attachment from clipboard path"
    );
    Ok(Some(image))
}
