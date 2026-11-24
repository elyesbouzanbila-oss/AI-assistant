//! Screenshot post-processing: shrink and re-encode before it goes to the model.
//!
//! A 4K PNG is several megabytes; the same frame as a 1400px-wide JPEG is
//! ~150 KB, which matters for latency and cost. This part is platform
//! independent and unit tested.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, ImageFormat};
use std::io::Cursor;

/// Widest edge sent to the model. Enough to read UI text, small enough to be
/// quick and cheap.
pub const MAX_WIDTH: u32 = 1400;
const JPEG_QUALITY: u8 = 72;

pub fn to_data_url(bytes: &[u8]) -> Result<String> {
    let image = image::load_from_memory(bytes).context("screenshot was not a readable image")?;
    let resized = downscale(image, MAX_WIDTH);

    let mut buffer = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY);
    encoder
        .encode_image(&resized)
        .context("could not encode the screenshot as JPEG")?;

    Ok(format!(
        "data:image/jpeg;base64,{}",
        STANDARD.encode(buffer)
    ))
}

/// Pure encode path, used by tests to round-trip a known image.
pub fn to_data_url_from_image(image: DynamicImage) -> Result<String> {
    let resized = downscale(image, MAX_WIDTH);
    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);
    resized
        .write_to(&mut cursor, ImageFormat::Png)
        .context("could not encode image")?;
    to_data_url(&buffer)
}

fn downscale(image: DynamicImage, max_width: u32) -> DynamicImage {
    if image.width() <= max_width {
        return image;
    }
    let ratio = max_width as f32 / image.width() as f32;
    let height = (image.height() as f32 * ratio).round().max(1.0) as u32;
    image.resize_exact(max_width, height, FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn solid(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([20, 40, 60])))
    }

    #[test]
    fn produces_a_jpeg_data_url() {
        let url = to_data_url_from_image(solid(800, 600)).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"));

        let payload = url.trim_start_matches("data:image/jpeg;base64,");
        let bytes = STANDARD.decode(payload).unwrap();
        // JPEG magic number.
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert!(bytes.len() > 100);
    }

    #[test]
    fn huge_frames_are_downscaled() {
        let url = to_data_url_from_image(solid(3840, 2160)).unwrap();
        let bytes = STANDARD
            .decode(url.trim_start_matches("data:image/jpeg;base64,"))
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), MAX_WIDTH);
        assert_eq!(decoded.height(), 788); // 2160 * 1400/3840, rounded
                                           // Well under the multi-megabyte PNG it came from.
        assert!(bytes.len() < 400_000);
    }

    #[test]
    fn small_frames_are_left_alone() {
        let url = to_data_url_from_image(solid(320, 200)).unwrap();
        let bytes = STANDARD
            .decode(url.trim_start_matches("data:image/jpeg;base64,"))
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), 320);
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_data_url(b"not an image").is_err());
    }
}
