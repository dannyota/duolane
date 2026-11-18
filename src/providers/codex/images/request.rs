use bytes::Bytes;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    IMAGE_MODEL, ImageOperation, ImageRequestError, MAX_EDIT_IMAGE_BYTES, MAX_EDIT_IMAGES,
    MAX_SINGLE_IMAGE_BYTES,
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationRequest {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u8>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageUrl {
    image_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    prompt: String,
    images: Vec<ImageUrl>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u8>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug)]
pub struct UploadedImage {
    pub bytes: Bytes,
}

#[derive(Debug, Default)]
pub struct MultipartEditInput {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub background: Option<String>,
    pub n: Option<u8>,
    pub quality: Option<String>,
    pub size: Option<String>,
    pub images: Vec<UploadedImage>,
}

#[derive(Debug)]
pub struct PreparedImageRequest {
    pub body: Value,
    pub model: String,
    pub image_count: usize,
}

pub fn prepare_json_request(
    operation: ImageOperation,
    bytes: &[u8],
) -> Result<PreparedImageRequest, ImageRequestError> {
    match operation {
        ImageOperation::Generation => prepare_generation_request(bytes),
        ImageOperation::Edit => prepare_edit_request(bytes),
    }
}

fn prepare_generation_request(bytes: &[u8]) -> Result<PreparedImageRequest, ImageRequestError> {
    let mut request: GenerationRequest = serde_json::from_slice(bytes).map_err(|error| {
        ImageRequestError::invalid(format!("Invalid JSON image request: {error}"), None)
    })?;
    validate_and_default_common(
        &request.prompt,
        &mut request.model,
        &mut request.background,
        request.n,
        &mut request.quality,
        &mut request.size,
    )?;
    let model = request.model.clone().expect("model defaulted");
    let body = serde_json::to_value(request).map_err(|error| ImageRequestError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("Failed to serialize image request: {error}"),
        param: None,
        code: Some("internal_error"),
    })?;
    Ok(PreparedImageRequest {
        body,
        model,
        image_count: 0,
    })
}

pub fn prepare_multipart_edit(
    input: MultipartEditInput,
) -> Result<PreparedImageRequest, ImageRequestError> {
    use base64::Engine as _;

    if input.images.is_empty() || input.images.len() > MAX_EDIT_IMAGES {
        return Err(ImageRequestError::invalid(
            format!("'image' must contain between 1 and {MAX_EDIT_IMAGES} files"),
            Some("image"),
        ));
    }
    let total_bytes = input.images.iter().try_fold(0usize, |total, image| {
        if image.bytes.len() > MAX_SINGLE_IMAGE_BYTES {
            return Err(ImageRequestError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: format!("Each image must be at most {MAX_SINGLE_IMAGE_BYTES} bytes"),
                param: Some("image"),
                code: Some("request_too_large"),
            });
        }
        total
            .checked_add(image.bytes.len())
            .ok_or(ImageRequestError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: "Combined image payload is too large".to_string(),
                param: Some("image"),
                code: Some("request_too_large"),
            })
    })?;
    if total_bytes > MAX_EDIT_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Combined images must be at most {MAX_EDIT_IMAGE_BYTES} bytes"),
            param: Some("image"),
            code: Some("request_too_large"),
        });
    }

    let images = input
        .images
        .into_iter()
        .map(|image| {
            let mime = detect_image_mime(&image.bytes).ok_or_else(|| {
                ImageRequestError::invalid("Unsupported or malformed image file", Some("image"))
            })?;
            let mut data_url = format!("data:{mime};base64,");
            base64::engine::general_purpose::STANDARD.encode_string(&image.bytes, &mut data_url);
            Ok(ImageUrl {
                image_url: data_url,
            })
        })
        .collect::<Result<Vec<_>, ImageRequestError>>()?;
    let request = EditRequest {
        prompt: input.prompt.ok_or_else(|| {
            ImageRequestError::invalid("Missing required 'prompt' field", Some("prompt"))
        })?,
        images,
        model: input.model,
        background: input.background,
        n: input.n,
        quality: input.quality,
        size: input.size,
    };
    prepare_edit_value(request)
}

fn prepare_edit_request(bytes: &[u8]) -> Result<PreparedImageRequest, ImageRequestError> {
    let request: EditRequest = serde_json::from_slice(bytes).map_err(|error| {
        ImageRequestError::invalid(format!("Invalid JSON image edit request: {error}"), None)
    })?;
    prepare_edit_value(request)
}

fn prepare_edit_value(mut request: EditRequest) -> Result<PreparedImageRequest, ImageRequestError> {
    if request.images.is_empty() || request.images.len() > MAX_EDIT_IMAGES {
        return Err(ImageRequestError::invalid(
            format!("'images' must contain between 1 and {MAX_EDIT_IMAGES} items"),
            Some("images"),
        ));
    }
    let total_bytes = request.images.iter().try_fold(0usize, |total, image| {
        let image_bytes = validate_data_url(&image.image_url)?;
        total.checked_add(image_bytes).ok_or(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Combined image payload is too large".to_string(),
            param: Some("images"),
            code: Some("request_too_large"),
        })
    })?;
    if total_bytes > MAX_EDIT_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Combined images must be at most {MAX_EDIT_IMAGE_BYTES} bytes"),
            param: Some("images"),
            code: Some("request_too_large"),
        });
    }
    validate_and_default_common(
        &request.prompt,
        &mut request.model,
        &mut request.background,
        request.n,
        &mut request.quality,
        &mut request.size,
    )?;
    let model = request.model.clone().expect("model defaulted");
    let image_count = request.images.len();
    let body = serde_json::to_value(request).map_err(|error| ImageRequestError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("Failed to serialize image edit request: {error}"),
        param: None,
        code: Some("internal_error"),
    })?;
    Ok(PreparedImageRequest {
        body,
        model,
        image_count,
    })
}

fn validate_data_url(value: &str) -> Result<usize, ImageRequestError> {
    use base64::Engine as _;

    let (metadata, encoded) = value.split_once(',').ok_or_else(|| {
        ImageRequestError::invalid("Image must be a base64 data URL", Some("images"))
    })?;
    let mime = metadata
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or_else(|| {
            ImageRequestError::invalid("Image must be a base64 data URL", Some("images"))
        })?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            ImageRequestError::invalid("Image data is not valid base64", Some("images"))
        })?;
    let detected = detect_image_mime(&decoded).ok_or_else(|| {
        ImageRequestError::invalid("Unsupported or malformed image data", Some("images"))
    })?;
    if mime != detected {
        return Err(ImageRequestError::invalid(
            format!("Image media type '{mime}' does not match '{detected}' data"),
            Some("images"),
        ));
    }
    if decoded.len() > MAX_SINGLE_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Each image must be at most {MAX_SINGLE_IMAGE_BYTES} bytes"),
            param: Some("images"),
            code: Some("request_too_large"),
        });
    }
    Ok(decoded.len())
}

pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn validate_and_default_common(
    prompt: &str,
    model: &mut Option<String>,
    background: &mut Option<String>,
    n: Option<u8>,
    quality: &mut Option<String>,
    size: &mut Option<String>,
) -> Result<(), ImageRequestError> {
    if prompt.trim().is_empty() {
        return Err(ImageRequestError::invalid(
            "'prompt' must not be empty",
            Some("prompt"),
        ));
    }
    match model.as_deref() {
        Some(IMAGE_MODEL) | None => {}
        Some(other) => {
            return Err(ImageRequestError::invalid(
                format!("Unsupported image model '{other}'; expected '{IMAGE_MODEL}'"),
                Some("model"),
            ));
        }
    }
    if n.is_some_and(|n| !(1..=10).contains(&n)) {
        return Err(ImageRequestError::invalid(
            "'n' must be between 1 and 10",
            Some("n"),
        ));
    }
    validate_choice(
        "background",
        background.as_deref(),
        &["auto", "transparent", "opaque"],
    )?;
    validate_choice(
        "quality",
        quality.as_deref(),
        &["auto", "low", "medium", "high"],
    )?;
    if size.as_deref().is_some_and(str::is_empty) {
        return Err(ImageRequestError::invalid(
            "'size' must not be empty",
            Some("size"),
        ));
    }
    model.get_or_insert_with(|| IMAGE_MODEL.to_string());
    background.get_or_insert_with(|| "auto".to_string());
    quality.get_or_insert_with(|| "auto".to_string());
    size.get_or_insert_with(|| "auto".to_string());
    Ok(())
}

fn validate_choice(
    field: &'static str,
    value: Option<&str>,
    allowed: &[&str],
) -> Result<(), ImageRequestError> {
    if let Some(value) = value
        && !allowed.contains(&value)
    {
        return Err(ImageRequestError::invalid(
            format!("Invalid '{field}' value '{value}'"),
            Some(field),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_edit_enforces_decoded_image_size_limits() {
        use base64::Engine as _;

        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(MAX_SINGLE_IMAGE_BYTES + 1, 0);
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "prompt": "x",
            "images": [{"image_url": data_url}]
        }))
        .unwrap();
        let error = prepare_json_request(ImageOperation::Edit, &body).unwrap_err();
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn request_validation_rejects_unsupported_and_unsafe_inputs() {
        for body in [
            br#"{"prompt":"x","model":"gpt-image-1"}"#.as_slice(),
            br#"{"prompt":"x","n":0}"#,
            br#"{"prompt":"x","response_format":"url"}"#,
            br#"{"prompt":" "}"#,
        ] {
            assert!(prepare_json_request(ImageOperation::Generation, body).is_err());
        }
        assert!(
            prepare_json_request(
                ImageOperation::Edit,
                br#"{"prompt":"x","images":[{"image_url":"https://example.com/x.png"}]}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn multipart_edit_is_translated_to_codex_data_urls() {
        let prepared = prepare_multipart_edit(MultipartEditInput {
            prompt: Some("make it blue".to_string()),
            model: None,
            background: None,
            n: None,
            quality: None,
            size: None,
            images: vec![UploadedImage {
                bytes: Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            }],
        })
        .expect("multipart edit should be valid");

        assert_eq!(prepared.image_count, 1);
        assert_eq!(
            prepared.body["images"][0]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn json_edit_request_accepts_data_urls_and_applies_defaults() {
        let prepared = prepare_json_request(
            ImageOperation::Edit,
            br#"{"prompt":"make it blue","images":[{"image_url":"data:image/png;base64,iVBORw0KGgo="}]}"#,
        )
        .expect("edit request should be valid");

        assert_eq!(prepared.model, IMAGE_MODEL);
        assert_eq!(prepared.image_count, 1);
        assert_eq!(
            prepared.body["images"][0]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert_eq!(prepared.body["background"], "auto");
        assert_eq!(prepared.body["quality"], "auto");
        assert_eq!(prepared.body["size"], "auto");
    }

    #[test]
    fn generation_request_applies_safe_defaults() {
        let prepared =
            prepare_json_request(ImageOperation::Generation, br#"{"prompt":"draw a fox"}"#)
                .expect("generation request should be valid");

        assert_eq!(prepared.model, IMAGE_MODEL);
        assert_eq!(prepared.image_count, 0);
        assert_eq!(prepared.body["prompt"], "draw a fox");
        assert_eq!(prepared.body["model"], IMAGE_MODEL);
        assert_eq!(prepared.body["background"], "auto");
        assert_eq!(prepared.body["quality"], "auto");
        assert_eq!(prepared.body["size"], "auto");
        assert!(prepared.body.get("n").is_none());
    }
}
