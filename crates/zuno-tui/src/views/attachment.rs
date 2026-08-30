//! Local image attachments owned by a prompt draft.
//!
//! The visible `[Image #N]` token is presentation state, never attachment
//! identity. Actual bytes, MIME, and filename travel separately and become a
//! typed [`RequestContentBlock::Image`] only when the draft is submitted.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use zuno_llm::event::RequestContentBlock;

/// Maximum decoded size of one local image attachment.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// One validated local image ready to enter a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub filename: String,
    pub media_type: String,
    pub data: String,
}

/// Display metadata appended to the user message in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentLabel {
    pub filename: String,
    pub mime: String,
}

/// Rich prompt material produced when a draft containing image tokens is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachedPrompt {
    pub content: Vec<RequestContentBlock>,
    pub labels: Vec<AttachmentLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingImage {
    placeholder: String,
    image: LoadedImage,
}

/// Images attached to one composer draft.
#[derive(Debug, Default)]
pub(crate) struct AttachmentDraft {
    next_image: usize,
    images: Vec<PendingImage>,
}

impl AttachmentDraft {
    /// Whether the visible draft still contains at least one owned image token.
    pub(crate) fn has_attached_prompt(&self, text: &str) -> bool {
        self.images
            .iter()
            .any(|image| text.contains(&image.placeholder))
    }

    /// Treat a whole pasted string as an image path when it resolves to a supported image.
    pub(crate) fn attach_pasted_path(&mut self, text: &str) -> Result<Option<String>, String> {
        let Some(path) = normalize_pasted_path(text) else {
            return Ok(None);
        };
        let Some(image) = load_image_file(&path)? else {
            return Ok(None);
        };
        Ok(Some(self.push(image)))
    }

    /// Attach image bytes already supplied by the system clipboard as base64.
    pub(crate) fn attach_clipboard_image(
        &mut self,
        media_type: &str,
        data: &str,
    ) -> Result<String, String> {
        let image = load_encoded_image(media_type, data, None)?;
        Ok(self.push(image))
    }

    /// Consume images whose placeholders remain in `text` and build provider content.
    pub(crate) fn take_prompt(&mut self, text: &str) -> Option<AttachedPrompt> {
        let images = std::mem::take(&mut self.images)
            .into_iter()
            .filter(|image| text.contains(&image.placeholder))
            .collect::<Vec<_>>();
        if images.is_empty() {
            return None;
        }
        let mut content = vec![RequestContentBlock::Text {
            text: text.to_owned(),
        }];
        let mut labels = Vec::with_capacity(images.len());
        for pending in images {
            labels.push(AttachmentLabel {
                filename: pending.image.filename.clone(),
                mime: pending.image.media_type.clone(),
            });
            content.push(RequestContentBlock::Image {
                filename: Some(pending.image.filename),
                media_type: pending.image.media_type,
                data: pending.image.data,
            });
        }
        Some(AttachedPrompt { content, labels })
    }

    fn push(&mut self, image: LoadedImage) -> String {
        self.next_image = self.next_image.saturating_add(1);
        let placeholder = format!("[Image #{}]", self.next_image);
        self.images.push(PendingImage {
            placeholder: placeholder.clone(),
            image,
        });
        placeholder
    }
}

/// Load one local image, returning `None` for a regular non-image file.
pub fn load_image_file(path: &Path) -> Result<Option<LoadedImage>, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "cannot inspect image attachment `{}`: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut file = File::open(path)
        .map_err(|error| format!("cannot read image attachment `{}`: {error}", path.display()))?;
    let mut header = [0_u8; 12];
    let header_len = file
        .read(&mut header)
        .map_err(|error| format!("cannot read image attachment `{}`: {error}", path.display()))?;
    let Some(media_type) = image_media_type(&header[..header_len]) else {
        return Ok(None);
    };
    if metadata.len() > u64::try_from(MAX_IMAGE_BYTES).unwrap_or(u64::MAX) {
        return Err(format!(
            "image attachment `{}` exceeds the {} MiB limit",
            path.display(),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    bytes.extend_from_slice(&header[..header_len]);
    file.take(u64::try_from(MAX_IMAGE_BYTES.saturating_sub(header_len)).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read image attachment `{}`: {error}", path.display()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image attachment `{}` exceeds the {} MiB limit",
            path.display(),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
        .to_owned();
    Ok(Some(LoadedImage {
        filename,
        media_type: media_type.to_owned(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

fn load_encoded_image(
    declared_media_type: &str,
    data: &str,
    filename: Option<String>,
) -> Result<LoadedImage, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| format!("clipboard image is not valid base64: {error}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "clipboard image exceeds the {} MiB limit",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let detected = image_media_type(&bytes)
        .ok_or_else(|| "clipboard image is not PNG, JPEG, GIF, or WebP".to_owned())?;
    if declared_media_type != detected {
        return Err(format!(
            "clipboard image MIME `{declared_media_type}` does not match detected `{detected}`"
        ));
    }
    Ok(LoadedImage {
        filename: filename.unwrap_or_else(|| default_clipboard_name(detected).to_owned()),
        media_type: detected.to_owned(),
        data: data.to_owned(),
    })
}

/// Normalize one whole paste into an existing local path.
pub fn normalize_pasted_path(text: &str) -> Option<PathBuf> {
    let mut candidate = text.trim();
    if candidate.is_empty() || candidate.contains(['\n', '\r']) {
        return None;
    }
    if ((candidate.starts_with('"') && candidate.ends_with('"'))
        || (candidate.starts_with('\'') && candidate.ends_with('\'')))
        && candidate.len() >= 2
    {
        candidate = &candidate[1..candidate.len() - 1];
    }
    if candidate.starts_with("file://") {
        return url::Url::parse(candidate)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .filter(|path| path.is_file());
    }

    let expanded_home = candidate.strip_prefix("~/").and_then(|relative| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
    });
    if let Some(path) = expanded_home.filter(|path| path.is_file()) {
        return Some(path);
    }

    let unescaped;
    let candidate = if !looks_windows_path(candidate) && candidate.contains("\\ ") {
        unescaped = candidate.replace("\\ ", " ");
        unescaped.as_str()
    } else {
        candidate
    };
    let path = PathBuf::from(candidate);
    if path.is_file() {
        return Some(path);
    }
    wsl_path(candidate).filter(|path| path.is_file())
}

fn looks_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with(r"\\")
}

#[cfg(unix)]
fn wsl_path(path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = path[3..].replace('\\', "/");
    Some(PathBuf::from(format!("/mnt/{drive}/{rest}")))
}

#[cfg(not(unix))]
fn wsl_path(_path: &str) -> Option<PathBuf> {
    None
}

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

const fn default_clipboard_name(media_type: &str) -> &'static str {
    match media_type.as_bytes() {
        b"image/png" => "clipboard-image.png",
        b"image/jpeg" => "clipboard-image.jpg",
        b"image/gif" => "clipboard-image.gif",
        b"image/webp" => "clipboard-image.webp",
        _ => "clipboard-image",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_and_file_url_paths_normalize_to_the_same_image() {
        let root = tempfile::tempdir().expect("tempdir");
        let image = root.path().join("image with space.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").expect("image");
        assert_eq!(
            normalize_pasted_path(&format!("\"{}\"", image.display())),
            Some(image.clone())
        );
        assert_eq!(
            normalize_pasted_path(url::Url::from_file_path(&image).expect("file URL").as_str()),
            Some(image)
        );
    }
}
