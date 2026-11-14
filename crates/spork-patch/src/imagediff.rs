//! Image decoding and pixel comparison for the image diff viewers (docs/specs/M08 §3).
//!
//! Pure byte-in/struct-out helpers: decode both sides of an image change, and compute
//! which pixels differ, so the UI can render side-by-side, swipe, onion-skin, and
//! highlight-pixel modes from the results.

use std::fmt;
use std::io::Cursor;
use std::path::Path;

/// The file extensions the image diff viewers treat as images (M08 §3), any case.
/// `svg` is in the set because the spec lists it with images, but [`decode_image`]
/// rejects vector files — the UI renders those separately.
pub fn is_image_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg")
    )
}

/// A decoded image: `width * height * 4` straight-alpha RGBA8 bytes, row-major with
/// rows of `width * 4` bytes and pixel `(x, y)` at index `(y * width + x) * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decoding image bytes failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageError {
    message: String,
}

impl ImageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ImageError {}

/// Decode image bytes (png, jpg, gif, webp, bmp, ico) into straight-alpha RGBA8.
///
/// The format is sniffed from the bytes. Garbage or empty input is an error, never a
/// panic. `svg` is not decodable here by design.
pub fn decode_image(data: &[u8]) -> Result<DecodedImage, ImageError> {
    let rgba = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| ImageError::new(format!("not a recognizable image: {e}")))?
        .decode()
        .map_err(|e| ImageError::new(format!("image decode failed: {e}")))?
        .into_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Decode both sides of a change, each independently. `None` is an absent side (new
/// file or deleted file). A side that fails to decode fails the whole call — callers
/// that want per-side degradation use [`decode_image`] directly.
pub fn decode_image_pair(
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Result<(Option<DecodedImage>, Option<DecodedImage>), ImageError> {
    let old = old.map(decode_image).transpose()?;
    let new = new.map(decode_image).transpose()?;
    Ok((old, new))
}

/// The result of comparing two images pixel by pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelDiff {
    /// How many pixels differ by more than the tolerance.
    pub differing: u64,
    /// One entry per pixel of the common size: `mask[y * width + x]`, where width and
    /// height are the larger of the two inputs (see [`pixel_diff`]).
    pub mask: Vec<bool>,
}

/// Compare two images pixel by pixel over their common size
/// `max(old.width, new.width) × max(old.height, new.height)`: the smaller image is
/// padded with transparent black `(0, 0, 0, 0)`, and the padding counts as a
/// difference where the larger image is not transparent.
///
/// A pixel differs when any of its RGBA channels differs by more than `tolerance`
/// (0 = exact; the UI may pass a small tolerance to absorb anti-aliasing).
pub fn pixel_diff(old: &DecodedImage, new: &DecodedImage, tolerance: u8) -> PixelDiff {
    let width = old.width.max(new.width) as usize;
    let height = old.height.max(new.height) as usize;
    let mut mask = vec![false; width * height];
    for y in 0..height {
        for x in 0..width {
            if pixels_differ(pixel_at(old, x, y), pixel_at(new, x, y), tolerance) {
                mask[y * width + x] = true;
            }
        }
    }
    let differing = mask.iter().filter(|d| **d).count() as u64;
    PixelDiff { differing, mask }
}

/// The pixel at `(x, y)`, or transparent black outside the image (the padding used
/// by [`pixel_diff`]).
fn pixel_at(img: &DecodedImage, x: usize, y: usize) -> [u8; 4] {
    const PAD: [u8; 4] = [0, 0, 0, 0];
    if x >= img.width as usize || y >= img.height as usize {
        return PAD;
    }
    let i = (y * img.width as usize + x) * 4;
    [
        img.rgba[i],
        img.rgba[i + 1],
        img.rgba[i + 2],
        img.rgba[i + 3],
    ]
}

/// Whether two RGBA pixels differ in any channel by more than `tolerance`.
fn pixels_differ(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter().zip(b).any(|(x, y)| x.abs_diff(y) > tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};

    fn img(width: u32, height: u32, rgba: Vec<u8>) -> DecodedImage {
        DecodedImage {
            width,
            height,
            rgba,
        }
    }

    /// 2×2: red, green, blue, opaque black — one byte per channel, in order.
    fn quad() -> DecodedImage {
        img(
            2,
            2,
            vec![1, 0, 0, 255, 0, 1, 0, 255, 0, 0, 1, 255, 0, 0, 0, 255],
        )
    }

    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let image = RgbaImage::from_vec(width, height, rgba.to_vec()).unwrap();
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        image.write_to(&mut cursor, ImageFormat::Png).unwrap();
        buf
    }

    #[test]
    fn is_image_path_matches_only_image_extensions() {
        for path in [
            "a.png",
            "a.jpg",
            "a.jpeg",
            "a.gif",
            "a.webp",
            "a.bmp",
            "a.ico",
            "a.svg",
            "deep/dir/file.PNG",
        ] {
            assert!(is_image_path(path), "{path} should be an image");
        }
        for path in ["a.txt", "a.pngx", "a", ".hidden", "a.svgz"] {
            assert!(!is_image_path(path), "{path} should not be an image");
        }
    }

    #[test]
    fn identical_images_differ_nowhere() {
        let d = pixel_diff(&quad(), &quad(), 0);
        assert_eq!(d.differing, 0);
        assert_eq!(d.mask, vec![false, false, false, false]);
    }

    #[test]
    fn a_single_changed_pixel_is_found_at_its_position() {
        let mut new = quad();
        new.rgba[7] = 100; // alpha of pixel (1, 0)
        let d = pixel_diff(&quad(), &new, 0);
        assert_eq!(d.differing, 1);
        assert_eq!(d.mask, vec![false, true, false, false]);
    }

    #[test]
    fn tolerance_ignores_deltas_up_to_itself() {
        let mut new = quad();
        new.rgba[0] = 6; // R of pixel (0, 0): 1 → 6, a delta of 5
        assert_eq!(pixel_diff(&quad(), &new, 5).differing, 0);
        assert_eq!(pixel_diff(&quad(), &new, 4).differing, 1);
        assert_eq!(pixel_diff(&quad(), &new, 0).differing, 1);
    }

    #[test]
    fn alpha_only_difference_counts() {
        let mut new = quad();
        new.rgba[3] = 0; // pixel (0, 0) becomes fully transparent
        assert_eq!(pixel_diff(&quad(), &new, 0).differing, 1);
        assert_eq!(
            pixel_diff(&quad(), &new, 255).differing,
            0,
            "an alpha delta of 255 is within a tolerance of 255"
        );
    }

    #[test]
    fn size_mismatch_is_padded_with_transparent_black() {
        let small = img(1, 1, vec![255, 255, 255, 255]); // white 1×1
        let big = img(
            2,
            2,
            vec![
                255, 255, 255, 255, // (0,0) matches the small pixel
                0, 0, 0, 0, // (1,0) equals the small image's padding
                0, 0, 0, 0, // (0,1) too
                255, 0, 0, 255, // (1,1) red: differs from transparent padding
            ],
        );
        let d = pixel_diff(&small, &big, 0);
        assert_eq!(d.differing, 1);
        assert_eq!(d.mask, vec![false, false, false, true]);
    }

    #[test]
    fn png_encode_decode_round_trip() {
        let buf = encode_png(
            2,
            2,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ],
        );
        let decoded = decode_image(&buf).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.rgba.len(), 16);
        assert_eq!(
            decoded.rgba,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ]
        );
    }

    #[test]
    fn bmp_encode_decode_round_trip() {
        let image = RgbaImage::from_vec(1, 1, vec![9, 8, 7, 6]).unwrap();
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        image.write_to(&mut cursor, ImageFormat::Bmp).unwrap();
        let decoded = decode_image(&buf).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.rgba, vec![9, 8, 7, 6]);
    }

    #[test]
    fn decode_image_pair_decodes_each_present_side() {
        let buf = encode_png(1, 1, &[1, 2, 3, 4]);
        let (old, new) = decode_image_pair(Some(&buf), None).unwrap();
        let old = old.unwrap();
        assert_eq!((old.width, old.height), (1, 1));
        assert_eq!(old.rgba, vec![1, 2, 3, 4]);
        assert!(new.is_none());
        assert_eq!(decode_image_pair(None, None).unwrap(), (None, None));
    }

    #[test]
    fn garbage_bytes_are_an_error_not_a_panic() {
        assert!(decode_image(b"this is not an image at all").is_err());
        assert!(decode_image(b"").is_err());
        assert!(decode_image_pair(None, Some(b"garbage")).is_err());
        assert!(decode_image_pair(Some(b"garbage"), None).is_err());
    }
}
