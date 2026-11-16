//! Bounded image bytes only; never resolve plugin-supplied paths or URLs.

use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use plugin_api::{MediaCreate, PluginError};
use serde_json::Value;

#[derive(Debug)]
pub(super) struct ValidatedImage {
    pub(super) payload: Value,
    pub(super) data: Vec<u8>,
}

pub(super) async fn validate_create_async(payload: Value) -> Result<ValidatedImage, PluginError> {
    let data = decode_create_async(payload.clone()).await?;
    Ok(ValidatedImage { payload, data })
}

/// The permit stays in the blocking closure if its async caller is cancelled.
pub(super) async fn decode_create_async(payload: Value) -> Result<Vec<u8>, PluginError> {
    let permit = decoder_slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| PluginError::ResourceExhausted("image decoder is unavailable".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_create(&payload)
    })
    .await
    .map_err(|_| PluginError::Permanent("image decoder worker failed".into()))?
}

pub(super) fn decoder_slots() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    SLOTS.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
}

pub(super) fn decode_create(payload: &Value) -> Result<Vec<u8>, PluginError> {
    let invalid = || PluginError::Permanent("invalid media.create image payload".to_owned());
    let request: MediaCreate = serde_json::from_value(payload.clone()).map_err(|_| invalid())?;
    // The complete HandlerOutput also has a 1 MiB limit. Check before Base64 allocation
    // here as well so recovered commands cannot bypass the bound.
    if request.data_base64.len() > super::MAX_OUTPUT_BYTES {
        return Err(PluginError::ResourceExhausted(
            "media.create exceeds output limit".to_owned(),
        ));
    }
    let format = match request.mime_type.as_str() {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => return Err(invalid()),
    };
    let data = STANDARD
        .decode(&request.data_base64)
        .map_err(|_| invalid())?;
    if image::guess_format(&data).map_err(|_| invalid())? != format {
        return Err(invalid());
    }
    let mut reader = ImageReader::with_format(Cursor::new(&data), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => {
            PluginError::ResourceExhausted("media.create image exceeds decoder limits".to_owned())
        }
        _ => invalid(),
    })?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;
    use serde_json::json;

    #[test]
    fn supported_images_are_decoded_and_malicious_payloads_are_rejected() {
        for (mime, format) in [
            ("image/png", ImageFormat::Png),
            ("image/jpeg", ImageFormat::Jpeg),
            ("image/webp", ImageFormat::WebP),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::new_rgb8(1, 1)
                .write_to(&mut encoded, format)
                .unwrap();
            let payload =
                json!({"mime_type":mime,"data_base64":STANDARD.encode(encoded.get_ref())});
            assert_eq!(decode_create(&payload).unwrap(), *encoded.get_ref());
            let mut mismatch = payload.clone();
            mismatch["mime_type"] = json!(if mime == "image/png" {
                "image/jpeg"
            } else {
                "image/png"
            });
            assert!(decode_create(&mismatch).is_err());
            let mut unknown = payload;
            unknown["path"] = json!("/etc/passwd");
            assert!(decode_create(&unknown).is_err());
        }
        for payload in [
            json!({"mime_type":"image/png","data_base64":"%%%"}),
            json!({"mime_type":"image/png","data_base64":STANDARD.encode(b"\x89PNG\r\n\x1a\n")}),
            json!({"mime_type":"image/jpeg","data_base64":STANDARD.encode(b"\x89PNG\r\n\x1a\n")}),
            json!({"mime_type":"image/svg+xml","data_base64":STANDARD.encode(b"<svg/>")}),
            json!({"mime_type":"image/png","data_base64":"file:///etc/passwd"}),
            json!({"mime_type":"image/png","data_base64":"https://example.com/a.png"}),
            json!({"mime_type":"image/png","data_base64":"","path":"/etc/passwd"}),
            json!({"mime_type":"image/png","data_base64":"A".repeat(super::super::MAX_OUTPUT_BYTES+1)}),
        ] {
            assert!(decode_create(&payload).is_err());
        }
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(4097, 1)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        assert!(
            decode_create(
                &json!({"mime_type":"image/png","data_base64":STANDARD.encode(encoded.get_ref())})
            )
            .is_err()
        );
    }
}
