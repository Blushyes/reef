use std::path::Path;

use super::binary::{BinaryInfo, BinaryReason, decode_error, human_bytes};
use super::{PreviewBody, PreviewDocument};

#[derive(Debug, Clone)]
pub struct ImagePreview {
    pub image: Option<::image::DynamicImage>,
    pub width_px: u32,
    pub height_px: u32,
    pub format: ::image::ImageFormat,
    pub bytes_on_disk: u64,
    pub animated: bool,
    pub meta_line: String,
}

pub const DECODE_CAP_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 50_000_000;

const METADATA_HEADER_BYTES: usize = 64 * 1024;
const MAX_PROTOCOL_DIM: u32 = 2048;

impl ImagePreview {
    fn new(
        image: ::image::DynamicImage,
        width_px: u32,
        height_px: u32,
        format: ::image::ImageFormat,
        bytes_on_disk: u64,
        animated: bool,
    ) -> Self {
        Self {
            image: Some(image),
            width_px,
            height_px,
            format,
            bytes_on_disk,
            animated,
            meta_line: image_meta_line(width_px, height_px, format, bytes_on_disk, animated),
        }
    }

    fn metadata_only(
        width_px: u32,
        height_px: u32,
        format: ::image::ImageFormat,
        bytes_on_disk: u64,
        animated: bool,
    ) -> Self {
        Self {
            image: None,
            width_px,
            height_px,
            format,
            bytes_on_disk,
            animated,
            meta_line: image_meta_line(width_px, height_px, format, bytes_on_disk, animated),
        }
    }
}

pub(crate) fn load_image_preview(
    full: &Path,
    rel_str: &str,
    file_size: u64,
    mime: &'static str,
    wants_decoded_image: bool,
) -> PreviewDocument {
    use ::image::ImageReader;
    use std::io::Cursor;

    let card = |reason| binary_card(rel_str, file_size, mime, reason);

    if file_size > DECODE_CAP_BYTES {
        return card(BinaryReason::TooLarge);
    }

    if !wants_decoded_image {
        let header = match read_up_to(full, METADATA_HEADER_BYTES) {
            Ok(bytes) => bytes,
            Err(e) => return card(decode_error(e.to_string())),
        };
        return metadata_only_from_bytes(&header, rel_str, file_size, card);
    }

    let bytes = match std::fs::read(full) {
        Ok(bytes) => bytes,
        Err(e) => return card(decode_error(e.to_string())),
    };

    let reader = match ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(e) => return card(decode_error(e.to_string())),
    };
    let format = match reader.format() {
        Some(format) => format,
        None => return card(BinaryReason::UnsupportedImage),
    };
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(e) => {
            let reason = if matches!(e, ::image::ImageError::Unsupported(_)) {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(e.to_string())
            };
            return card(reason);
        }
    };

    if (width as u64).saturating_mul(height as u64) > MAX_PIXELS {
        return card(BinaryReason::TooLarge);
    }

    let decoded = match ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .and_then(|reader| reader.decode().map_err(std::io::Error::other))
    {
        Ok(image) => image,
        Err(e) => {
            let msg = e.to_string();
            let reason = if msg.contains("unsupported") || msg.contains("Unsupported") {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(msg)
            };
            return card(reason);
        }
    };

    let animated = if format == ::image::ImageFormat::Gif {
        use ::image::AnimationDecoder;
        use ::image::codecs::gif::GifDecoder;
        GifDecoder::new(Cursor::new(&bytes))
            .map(|decoder| decoder.into_frames().take(2).count() > 1)
            .unwrap_or(false)
    } else {
        false
    };

    let decoded = downscale_if_oversized(decoded);

    PreviewDocument {
        path: rel_str.to_string(),
        body: PreviewBody::Image(ImagePreview::new(
            decoded, width, height, format, file_size, animated,
        )),
    }
}

fn binary_card(
    rel_str: &str,
    file_size: u64,
    mime: &'static str,
    reason: BinaryReason,
) -> PreviewDocument {
    PreviewDocument {
        path: rel_str.to_string(),
        body: PreviewBody::Binary(BinaryInfo::new(file_size, Some(mime), reason)),
    }
}

fn metadata_only_from_bytes<F>(
    bytes: &[u8],
    rel_str: &str,
    file_size: u64,
    card: F,
) -> PreviewDocument
where
    F: Fn(BinaryReason) -> PreviewDocument,
{
    use ::image::ImageReader;
    use std::io::Cursor;

    let reader = match ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(e) => return card(decode_error(e.to_string())),
    };
    let format = match reader.format() {
        Some(format) => format,
        None => return card(BinaryReason::UnsupportedImage),
    };
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(e) => {
            let reason = if matches!(e, ::image::ImageError::Unsupported(_)) {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(e.to_string())
            };
            return card(reason);
        }
    };

    PreviewDocument {
        path: rel_str.to_string(),
        body: PreviewBody::Image(ImagePreview::metadata_only(
            width, height, format, file_size, false,
        )),
    }
}

fn read_up_to(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn downscale_if_oversized(img: ::image::DynamicImage) -> ::image::DynamicImage {
    let (width, height) = (img.width(), img.height());
    if width <= MAX_PROTOCOL_DIM && height <= MAX_PROTOCOL_DIM {
        return img;
    }
    img.resize(
        MAX_PROTOCOL_DIM,
        MAX_PROTOCOL_DIM,
        ::image::imageops::FilterType::Lanczos3,
    )
}

fn image_meta_line(
    width_px: u32,
    height_px: u32,
    format: ::image::ImageFormat,
    bytes_on_disk: u64,
    animated: bool,
) -> String {
    let fmt = image_format_name(format);
    let size = human_bytes(bytes_on_disk);
    if animated {
        format!("{width_px}×{height_px} · {fmt} · {size} · animated (first frame shown)")
    } else {
        format!("{width_px}×{height_px} · {fmt} · {size}")
    }
}

fn image_format_name(format: ::image::ImageFormat) -> &'static str {
    match format {
        ::image::ImageFormat::Png => "PNG",
        ::image::ImageFormat::Jpeg => "JPEG",
        ::image::ImageFormat::Gif => "GIF",
        ::image::ImageFormat::WebP => "WebP",
        ::image::ImageFormat::Bmp => "BMP",
        ::image::ImageFormat::Tiff => "TIFF",
        ::image::ImageFormat::Ico => "ICO",
        _ => "image",
    }
}
