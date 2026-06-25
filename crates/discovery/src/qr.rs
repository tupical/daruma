//! QR code generation for pairing URLs.
//!
//! Encodes a `daruma://pair?…` URL into a PNG image returned as raw bytes,
//! and optionally renders an ASCII-art representation for terminal display.

use anyhow::{Context, Result};
use qrcode::QrCode;

/// Encode `url` as a QR code PNG image.
///
/// Returns the PNG bytes, ready to be served as `image/png` or written to
/// a file.
pub fn encode_png(url: &str) -> Result<Vec<u8>> {
    let code = QrCode::new(url.as_bytes()).context("QrCode::new")?;

    let image = code.render::<qrcode::render::svg::Color>().build();
    // We want PNG, not SVG — use the `image` crate renderer.
    let img = code
        .render::<image::Luma<u8>>()
        .min_dimensions(200, 200)
        .build();

    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("write QR PNG")?;

    let _ = image; // suppress unused warning from SVG path above

    Ok(buf)
}

/// Render `url` as an ASCII-art QR code string suitable for terminal output.
pub fn encode_ascii(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes()).context("QrCode::new")?;
    Ok(code
        .render::<char>()
        .quiet_zone(false)
        .module_dimensions(2, 1)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_starts_with_png_magic() {
        let url = "daruma://pair?host=localhost%3A8443&token=abc&fpr=sha256%3Adef";
        let png = encode_png(url).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn ascii_is_non_empty() {
        let url = "daruma://pair?host=localhost%3A8443&token=abc&fpr=sha256%3Adef";
        let art = encode_ascii(url).unwrap();
        assert!(!art.is_empty());
    }
}
