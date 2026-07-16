//! Raster ⇄ text QR code helpers. No Naraka-specific knowledge lives here —
//! see `rewrite::parse_foreign_wrapper_url` / `rewrite::local_share_url` for
//! the Photo Booth wrapper-URL logic that sits on top of this.

use std::io::Cursor;

use anyhow::{Context, Result};

/// Decode the first QR code found in an image (PNG/JPEG bytes), returning
/// its text content.
pub fn decode(image_bytes: &[u8]) -> Result<String> {
	let gray = image::load_from_memory(image_bytes)
		.context("not a readable image")?
		.to_luma8();

	let mut prepared = rqrr::PreparedImage::prepare(gray);
	let grids = prepared.detect_grids();
	let grid = grids.first().context("no QR code found in that image")?;
	let (_meta, content) = grid.decode().context("found a QR code but couldn't decode it")?;
	Ok(content)
}

/// Encode `text` as a QR code and return it as PNG bytes.
pub fn encode_png(text: &str) -> Result<Vec<u8>> {
	let code = qrcode::QrCode::new(text).context("encoding QR code")?;
	let image = code.render::<image::Luma<u8>>().build();

	let mut png_bytes = Vec::new();
	image
		.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
		.context("encoding QR image as PNG")?;
	Ok(png_bytes)
}
