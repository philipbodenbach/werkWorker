//! Bounded, portable image attachments. No paths or remote URLs enter the prompt.
use crate::{Result, WorkerError};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::ImageReader;
use serde::{Deserialize, Serialize};
use std::io::Cursor;

pub const CLIPBOARD_IMAGE_LIMIT: usize = 16 * 1024 * 1024;
pub const MAX_IMAGES: usize = 4;
pub const IMAGE_TOKEN_RESERVE: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data_base64: String,
    pub width: u32,
    pub height: u32,
}
impl ImageAttachment {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > CLIPBOARD_IMAGE_LIMIT {
            return Err(WorkerError::new(
                "image_limit",
                "Clipboard image exceeds 16 MiB",
            ));
        }
        let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|_| {
            WorkerError::new("invalid_image", "Cannot decode bounded PNG/JPEG image")
        })?;
        let mut image = if decoded.width() > 1024 || decoded.height() > 1024 {
            decoded.thumbnail(1024, 1024)
        } else {
            decoded
        }
        .to_rgb8();
        loop {
            let mut encoded = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 85)
                .encode_image(&image)
                .map_err(|_| WorkerError::new("invalid_image", "Cannot encode image"))?;
            if encoded.len() <= 128 * 1024 {
                return Ok(Self {
                    mime_type: "image/jpeg".into(),
                    data_base64: STANDARD.encode(encoded),
                    width: image.width(),
                    height: image.height(),
                });
            }
            let w = (image.width() * 3 / 4).max(1);
            let h = (image.height() * 3 / 4).max(1);
            image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.mime_type != "image/jpeg" || self.data_base64.len() > 175_000 {
            return Err(WorkerError::new(
                "image_limit",
                "Expected a bounded JPEG attachment",
            ));
        }
        let bytes = STANDARD
            .decode(&self.data_base64)
            .map_err(|_| WorkerError::new("invalid_image", "Invalid image encoding"))?;
        let reader = ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg);
        let dimensions = reader
            .into_dimensions()
            .map_err(|_| WorkerError::new("invalid_image", "Invalid JPEG image"))?;
        if dimensions != (self.width, self.height)
            || self.width == 0
            || self.height == 0
            || self.width > 1024
            || self.height > 1024
        {
            return Err(WorkerError::new(
                "image_limit",
                "Image dimensions must match and fit 1024 pixels",
            ));
        }
        Ok(())
    }
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data_base64)
    }
}
