//! Durable, provider-neutral image attachment objects.
//!
//! Admission is the only place that accepts caller-provided bytes. It validates and
//! normalizes one still image, writes a content-addressed object atomically, and returns
//! a small durable reference. Provider request assembly resolves that reference late,
//! keeping object lifetime and corruption handling outside individual providers.

use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{
    ColorType, DynamicImage, GenericImageView as _, ImageEncoder as _, ImageFormat, Limits,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const POLICY_VERSION: u32 = 1;
const JPEG_QUALITIES: [u8; 5] = [90, 80, 70, 60, 50];
const SHRINK_NUMERATOR: u32 = 85;
const SHRINK_DENOMINATOR: u32 = 100;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

/// Content identity of one normalized object.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AttachmentId(String);

impl AttachmentId {
    /// Parse `sha256:<64 lowercase hex digits>`.
    pub fn parse(value: impl Into<String>) -> Result<Self, AttachmentError> {
        let value = value.into();
        let Some(digest) = value.strip_prefix("sha256:") else {
            return Err(AttachmentError::InvalidId);
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AttachmentError::InvalidId);
        }
        Ok(Self(value))
    }

    /// Hex digest without the algorithm prefix.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.0
            .strip_prefix("sha256:")
            .expect("validated attachment id")
    }
}

impl fmt::Debug for AttachmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AttachmentId")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for AttachmentId {
    type Error = AttachmentError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<AttachmentId> for String {
    fn from(value: AttachmentId) -> Self {
        value.0
    }
}

/// Durable image metadata stored in a session part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentRef {
    /// Content-addressed normalized object.
    pub id: AttachmentId,
    /// Original display name, never a source path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Normalized wire type (`image/png` or `image/jpeg`).
    pub media_type: String,
    /// Normalized pixel width.
    pub width: u32,
    /// Normalized pixel height.
    pub height: u32,
    /// Encoded normalized object size.
    pub encoded_bytes: u64,
}

/// Host policy applied while admitting untrusted source bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageAdmissionPolicy {
    pub auto_resize: bool,
    pub max_source_bytes: u64,
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub max_encoded_bytes: u64,
}

impl Default for ImageAdmissionPolicy {
    fn default() -> Self {
        Self {
            auto_resize: true,
            max_source_bytes: 20 * 1024 * 1024,
            max_width: 2_000,
            max_height: 2_000,
            max_pixels: 4_000_000,
            max_encoded_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Route-specific image budget applied only while assembling a provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRequestPolicy {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub max_encoded_bytes: u64,
}

impl Default for ImageRequestPolicy {
    fn default() -> Self {
        let policy = ImageAdmissionPolicy::default();
        Self {
            max_width: policy.max_width,
            max_height: policy.max_height,
            max_pixels: policy.max_pixels,
            max_encoded_bytes: policy.max_encoded_bytes,
        }
    }
}

impl ImageRequestPolicy {
    fn digest(self) -> String {
        let mut digest = Sha256::new();
        digest.update(POLICY_VERSION.to_le_bytes());
        digest.update(self.max_width.to_le_bytes());
        digest.update(self.max_height.to_le_bytes());
        digest.update(self.max_pixels.to_le_bytes());
        digest.update(self.max_encoded_bytes.to_le_bytes());
        hex::encode(digest.finalize())
    }
}

/// Resolved provider-bound image bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub media_type: String,
    pub data: String,
}

/// One database-scoped object store.
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    admission: ImageAdmissionPolicy,
}

impl AttachmentStore {
    /// Construct `$DATA/attachments/v1/<database-identity>`.
    pub fn new(
        data_root: impl AsRef<Path>,
        database_identity: &str,
        admission: ImageAdmissionPolicy,
    ) -> Result<Self, AttachmentError> {
        if database_identity.is_empty()
            || !database_identity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(AttachmentError::InvalidDatabaseIdentity);
        }
        let root = data_root
            .as_ref()
            .join("attachments")
            .join("v1")
            .join(database_identity);
        create_private_dir(&root)?;
        create_private_dir(&root.join("objects"))?;
        create_private_dir(&root.join("derived"))?;
        Ok(Self { root, admission })
    }

    /// Stable opaque identity for a database location or pool target.
    #[must_use]
    pub fn database_identity(value: impl AsRef<[u8]>) -> String {
        let digest = Sha256::digest(value.as_ref());
        hex::encode(&digest[..16])
    }

    /// Admit an encoded PNG/JPEG/GIF/WebP source.
    pub fn admit(
        &self,
        source: &[u8],
        filename: Option<String>,
    ) -> Result<ImageAttachmentRef, AttachmentError> {
        let normalized = normalize(source, self.admission)?;
        let id = id_for(&normalized.bytes);
        let path = self.object_path(&id);
        write_atomic_private(&path, &normalized.bytes)?;
        Ok(ImageAttachmentRef {
            id,
            filename: filename.map(sanitize_filename),
            media_type: normalized.media_type.to_owned(),
            width: normalized.width,
            height: normalized.height,
            encoded_bytes: u64::try_from(normalized.bytes.len()).unwrap_or(u64::MAX),
        })
    }

    /// Decode and admit a standard base64 payload.
    pub fn admit_base64(
        &self,
        data: &str,
        filename: Option<String>,
    ) -> Result<ImageAttachmentRef, AttachmentError> {
        self.admit_base64_typed(data, None, filename)
    }

    /// Decode and admit base64 while checking the caller-declared source MIME.
    pub fn admit_base64_typed(
        &self,
        data: &str,
        expected_media_type: Option<&str>,
        filename: Option<String>,
    ) -> Result<ImageAttachmentRef, AttachmentError> {
        let estimated = data.len().saturating_mul(3) / 4;
        if u64::try_from(estimated).unwrap_or(u64::MAX) > self.admission.max_source_bytes {
            return Err(AttachmentError::SourceTooLarge {
                limit: self.admission.max_source_bytes,
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(AttachmentError::Base64)?;
        if let Some(expected) = expected_media_type {
            let detected = source_media_type(&bytes)?;
            if detected != expected {
                return Err(AttachmentError::MediaTypeMismatch {
                    expected: expected.to_owned(),
                    detected: detected.to_owned(),
                });
            }
        }
        self.admit(&bytes, filename)
    }

    /// Read and verify the canonical normalized object.
    pub fn read(&self, reference: &ImageAttachmentRef) -> Result<Vec<u8>, AttachmentError> {
        let path = self.object_path(&reference.id);
        let bytes = fs::read(&path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => AttachmentError::MissingObject {
                id: reference.id.clone(),
            },
            _ => AttachmentError::Io { path, source },
        })?;
        verify_digest(&reference.id, &bytes)?;
        let inspected = inspect_normalized(&bytes)?;
        if reference.media_type != inspected.media_type
            || reference.width != inspected.width
            || reference.height != inspected.height
            || reference.encoded_bytes != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        {
            return Err(AttachmentError::InvalidReference);
        }
        Ok(bytes)
    }

    /// Resolve an object under one provider route policy, caching derived encodings.
    pub fn resolve(
        &self,
        reference: &ImageAttachmentRef,
        policy: ImageRequestPolicy,
    ) -> Result<ResolvedImage, AttachmentError> {
        let original = self.read(reference)?;
        if reference.width <= policy.max_width
            && reference.height <= policy.max_height
            && u64::from(reference.width) * u64::from(reference.height) <= policy.max_pixels
            && u64::try_from(original.len()).unwrap_or(u64::MAX) <= policy.max_encoded_bytes
        {
            return Ok(ResolvedImage {
                media_type: reference.media_type.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(original),
            });
        }

        let policy_digest = policy.digest();
        let cache_key = format!("{}-{policy_digest}", reference.id.digest());
        let derived_path = self.derived_path(&cache_key);
        let bytes = match fs::read(&derived_path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let derived = normalize(
                    &original,
                    ImageAdmissionPolicy {
                        auto_resize: true,
                        max_source_bytes: u64::try_from(original.len()).unwrap_or(u64::MAX),
                        max_width: policy.max_width,
                        max_height: policy.max_height,
                        max_pixels: policy.max_pixels,
                        max_encoded_bytes: policy.max_encoded_bytes,
                    },
                )?;
                write_atomic_private(&derived_path, &derived.bytes)?;
                derived.bytes
            }
            Err(source) => {
                return Err(AttachmentError::Io {
                    path: derived_path,
                    source,
                });
            }
        };
        let inspected = inspect_normalized(&bytes)?;
        let pixels = u64::from(inspected.width).saturating_mul(u64::from(inspected.height));
        if inspected.width > policy.max_width
            || inspected.height > policy.max_height
            || pixels > policy.max_pixels
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > policy.max_encoded_bytes
        {
            return Err(AttachmentError::InvalidDerivedPath);
        }
        Ok(ResolvedImage {
            media_type: inspected.media_type.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    fn object_path(&self, id: &AttachmentId) -> PathBuf {
        self.root
            .join("objects")
            .join(&id.digest()[..2])
            .join(id.digest())
    }

    fn derived_path(&self, cache_key: &str) -> PathBuf {
        self.root
            .join("derived")
            .join(&cache_key[..2])
            .join(cache_key)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("attachment id must be sha256 followed by 64 lowercase hex digits")]
    InvalidId,
    #[error("attachment database identity is invalid")]
    InvalidDatabaseIdentity,
    #[error("durable attachment reference is invalid")]
    InvalidReference,
    #[error("durable attachment store is unavailable")]
    StoreUnavailable,
    #[error("image source exceeds the {limit}-byte admission limit")]
    SourceTooLarge { limit: u64 },
    #[error("image source format is not PNG, JPEG, GIF, or WebP")]
    UnsupportedFormat,
    #[error("image MIME {expected} does not match detected {detected}")]
    MediaTypeMismatch { expected: String, detected: String },
    #[error("image dimensions {width}x{height} exceed the admission policy")]
    Dimensions { width: u32, height: u32 },
    #[error("image contains {pixels} pixels, exceeding the {limit}-pixel policy")]
    PixelLimit { pixels: u64, limit: u64 },
    #[error("normalized image cannot fit the {limit}-byte encoded limit")]
    EncodedTooLarge { limit: u64 },
    #[error("image base64 is invalid")]
    Base64(#[source] base64::DecodeError),
    #[error("image decoding or encoding failed")]
    Image(#[source] image::ImageError),
    #[error("attachment object {id} is missing")]
    MissingObject { id: AttachmentId },
    #[error("attachment object {id} does not match its digest")]
    DigestMismatch { id: AttachmentId },
    #[error("derived attachment object has an invalid cache filename")]
    InvalidDerivedPath,
    #[error("attachment filesystem operation failed at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

struct Normalized {
    bytes: Vec<u8>,
    media_type: &'static str,
    width: u32,
    height: u32,
}

fn normalize(source: &[u8], policy: ImageAdmissionPolicy) -> Result<Normalized, AttachmentError> {
    if u64::try_from(source.len()).unwrap_or(u64::MAX) > policy.max_source_bytes {
        return Err(AttachmentError::SourceTooLarge {
            limit: policy.max_source_bytes,
        });
    }
    let format = image::guess_format(source).map_err(|_| AttachmentError::UnsupportedFormat)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(AttachmentError::UnsupportedFormat);
    }

    let dimensions = image::ImageReader::with_format(Cursor::new(source), format)
        .into_dimensions()
        .map_err(AttachmentError::Image)?;
    validate_dimensions(dimensions.0, dimensions.1, policy, false)?;

    let max_alloc = policy
        .max_pixels
        .saturating_mul(4)
        .saturating_add(16 * 1024 * 1024);
    let mut reader = image::ImageReader::with_format(Cursor::new(source), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let mut image = reader.decode().map_err(AttachmentError::Image)?;
    image = apply_orientation(image, exif_orientation(source, format));
    image = fit_dimensions(image, policy)?;

    let has_transparency = has_transparency(&image);
    image = if has_transparency {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    };
    loop {
        let encoded = if has_transparency {
            encode_png(&image)?
        } else {
            encode_jpeg_to_budget(&image, policy.max_encoded_bytes)?
        };
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) <= policy.max_encoded_bytes {
            let (width, height) = image.dimensions();
            return Ok(Normalized {
                bytes: encoded,
                media_type: if has_transparency {
                    "image/png"
                } else {
                    "image/jpeg"
                },
                width,
                height,
            });
        }
        if !policy.auto_resize || image.width() == 1 && image.height() == 1 {
            return Err(AttachmentError::EncodedTooLarge {
                limit: policy.max_encoded_bytes,
            });
        }
        image = shrink(image);
    }
}

fn validate_dimensions(
    width: u32,
    height: u32,
    policy: ImageAdmissionPolicy,
    allow_resize: bool,
) -> Result<(), AttachmentError> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if !allow_resize
        && !policy.auto_resize
        && (width > policy.max_width || height > policy.max_height)
    {
        return Err(AttachmentError::Dimensions { width, height });
    }
    if !allow_resize && !policy.auto_resize && pixels > policy.max_pixels {
        return Err(AttachmentError::PixelLimit {
            pixels,
            limit: policy.max_pixels,
        });
    }
    // A hostile header must not make the decoder allocate before the pixel budget is
    // considered. Auto-resize permits larger source dimensions, but caps the decode
    // allocation to a bounded multiple of the final policy.
    let decode_ceiling = policy.max_pixels.saturating_mul(64);
    if pixels > decode_ceiling {
        return Err(AttachmentError::PixelLimit {
            pixels,
            limit: decode_ceiling,
        });
    }
    Ok(())
}

fn fit_dimensions(
    image: DynamicImage,
    policy: ImageAdmissionPolicy,
) -> Result<DynamicImage, AttachmentError> {
    let (width, height) = image.dimensions();
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width <= policy.max_width && height <= policy.max_height && pixels <= policy.max_pixels {
        return Ok(to_eight_bit(image));
    }
    if !policy.auto_resize {
        validate_dimensions(width, height, policy, false)?;
    }
    let scale_width = f64::from(policy.max_width) / f64::from(width);
    let scale_height = f64::from(policy.max_height) / f64::from(height);
    let scale_pixels = (policy.max_pixels as f64 / pixels as f64).sqrt();
    let scale = scale_width.min(scale_height).min(scale_pixels).min(1.0);
    let next_width = (f64::from(width) * scale).floor().max(1.0) as u32;
    let next_height = (f64::from(height) * scale).floor().max(1.0) as u32;
    Ok(to_eight_bit(image.resize_exact(
        next_width,
        next_height,
        FilterType::Lanczos3,
    )))
}

fn to_eight_bit(image: DynamicImage) -> DynamicImage {
    if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    }
}

fn has_transparency(image: &DynamicImage) -> bool {
    image.color().has_alpha() && image.to_rgba8().pixels().any(|pixel| pixel.0[3] != u8::MAX)
}

fn shrink(image: DynamicImage) -> DynamicImage {
    let width = (image.width().saturating_mul(SHRINK_NUMERATOR) / SHRINK_DENOMINATOR).max(1);
    let height = (image.height().saturating_mul(SHRINK_NUMERATOR) / SHRINK_DENOMINATOR).max(1);
    image.resize_exact(width, height, FilterType::Lanczos3)
}

fn encode_jpeg_to_budget(image: &DynamicImage, limit: u64) -> Result<Vec<u8>, AttachmentError> {
    let rgb = image.to_rgb8();
    let mut smallest = Vec::new();
    for quality in JPEG_QUALITIES {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ColorType::Rgb8.into(),
            )
            .map_err(AttachmentError::Image)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limit {
            return Ok(bytes);
        }
        smallest = bytes;
    }
    Ok(smallest)
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, AttachmentError> {
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ColorType::Rgba8.into(),
        )
        .map_err(AttachmentError::Image)?;
    Ok(bytes)
}

fn exif_orientation(source: &[u8], format: ImageFormat) -> u32 {
    if format != ImageFormat::Jpeg {
        return 1;
    }
    exif::Reader::new()
        .read_from_container(&mut BufReader::new(Cursor::new(source)))
        .ok()
        .and_then(|metadata| {
            metadata
                .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|field| field.value.get_uint(0))
        })
        .unwrap_or(1)
}

fn apply_orientation(image: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.fliph().rotate270(),
        6 => image.rotate90(),
        7 => image.fliph().rotate90(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn source_media_type(bytes: &[u8]) -> Result<&'static str, AttachmentError> {
    match image::guess_format(bytes).map_err(|_| AttachmentError::UnsupportedFormat)? {
        ImageFormat::Png => Ok("image/png"),
        ImageFormat::Jpeg => Ok("image/jpeg"),
        ImageFormat::Gif => Ok("image/gif"),
        ImageFormat::WebP => Ok("image/webp"),
        _ => Err(AttachmentError::UnsupportedFormat),
    }
}

struct InspectedImage {
    media_type: &'static str,
    width: u32,
    height: u32,
}

fn inspect_normalized(bytes: &[u8]) -> Result<InspectedImage, AttachmentError> {
    let format = image::guess_format(bytes).map_err(|_| AttachmentError::UnsupportedFormat)?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        _ => return Err(AttachmentError::UnsupportedFormat),
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(AttachmentError::Image)?;
    Ok(InspectedImage {
        media_type,
        width,
        height,
    })
}

fn id_for(bytes: &[u8]) -> AttachmentId {
    AttachmentId(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn verify_digest(id: &AttachmentId, bytes: &[u8]) -> Result<(), AttachmentError> {
    if id.digest() == hex::encode(Sha256::digest(bytes)) {
        Ok(())
    } else {
        Err(AttachmentError::DigestMismatch { id: id.clone() })
    }
}

fn sanitize_filename(filename: String) -> String {
    Path::new(&filename)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("image")
        .to_owned()
}

fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), AttachmentError> {
    if path.exists() {
        let existing = fs::read(path).map_err(|source| AttachmentError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if existing == bytes {
            return Ok(());
        }
        return Err(AttachmentError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "content-addressed object path contains different bytes",
            ),
        });
    }
    let parent = path.parent().ok_or_else(|| AttachmentError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "object has no parent"),
    })?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("object"),
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| AttachmentError::Io {
            path: temporary.clone(),
            source,
        })?;
    set_private_file(&file, &temporary)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| AttachmentError::Io {
            path: temporary.clone(),
            source,
        })?;
    // Windows does not permit every rename/read race while the creating handle
    // is still open. The bytes are durable already, so close that handle before
    // publishing the content-addressed name.
    drop(file);
    match fs::rename(&temporary, path) {
        Ok(()) => {
            sync_directory(parent)?;
            Ok(())
        }
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            // Different platforms report a no-clobber rename collision with
            // different error kinds. The authoritative fact is whether the
            // content-addressed destination now contains the same bytes.
            if path.exists() {
                let existing = fs::read(path).map_err(|source| AttachmentError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                if existing == bytes {
                    return Ok(());
                }
            }
            Err(AttachmentError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

fn create_private_dir(path: &Path) -> Result<(), AttachmentError> {
    fs::create_dir_all(path).map_err(|source| AttachmentError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    set_private_dir(path)
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> Result<(), AttachmentError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        AttachmentError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) -> Result<(), AttachmentError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(file: &File, path: &Path) -> Result<(), AttachmentError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| AttachmentError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn set_private_file(_file: &File, _path: &Path) -> Result<(), AttachmentError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), AttachmentError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| AttachmentError::Io {
            path: path.to_path_buf(),
            source,
        })
}

// Rust's portable file API cannot open a Windows directory for FlushFileBuffers.
// The object file itself is synced before the atomic rename; directory fsync is
// an additional Unix durability barrier, not a reason to reject a valid object
// on platforms that do not expose it.
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), AttachmentError> {
    Ok(())
}

// Reclaiming objects is deliberately not a capability of this crate. Object lifetime is
// decided by the database that references them, so the shipped collector lives in
// `zuno-db::artifact_gc`, where liveness is read inside the same transaction that
// publishes references and a filename is only a candidate when it is a bare 64-character
// digest. Directory enumeration therefore only serves this crate's own tests.
#[cfg(test)]
fn walk_files(root: &Path) -> Result<Vec<PathBuf>, AttachmentError> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| AttachmentError::Io {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| AttachmentError::Io {
                path: directory.clone(),
                source,
            })?;
            let kind = entry.file_type().map_err(|source| AttachmentError::Io {
                path: entry.path(),
                source,
            })?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Frame, ImageBuffer, Rgb, Rgba};
    use std::sync::{Arc, Barrier};

    fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        if alpha {
            let image = ImageBuffer::from_fn(width, height, |x, y| {
                Rgba([(x % 255) as u8, (y % 255) as u8, 80, ((x + y) % 255) as u8])
            });
            let mut bytes = Vec::new();
            PngEncoder::new(&mut bytes)
                .write_image(image.as_raw(), width, height, ColorType::Rgba8.into())
                .unwrap();
            bytes
        } else {
            let image =
                ImageBuffer::from_fn(width, height, |x, y| Rgb([(x % 255) as u8, 40, y as u8]));
            let mut bytes = Vec::new();
            PngEncoder::new(&mut bytes)
                .write_image(image.as_raw(), width, height, ColorType::Rgb8.into())
                .unwrap();
            bytes
        }
    }

    fn opaque_rgba_png(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([(x % 255) as u8, (y % 255) as u8, 80, u8::MAX])
        });
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ColorType::Rgba8.into())
            .unwrap();
        bytes
    }

    fn oriented_jpeg(orientation: u16) -> Vec<u8> {
        let image = ImageBuffer::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgb([240, 10, 10])
            } else {
                Rgb([10, 10, 240])
            }
        });
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                ColorType::Rgb8.into(),
            )
            .unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(b"II");
        payload.extend_from_slice(&42_u16.to_le_bytes());
        payload.extend_from_slice(&8_u32.to_le_bytes());
        payload.extend_from_slice(&1_u16.to_le_bytes());
        payload.extend_from_slice(&0x0112_u16.to_le_bytes());
        payload.extend_from_slice(&3_u16.to_le_bytes());
        payload.extend_from_slice(&1_u32.to_le_bytes());
        payload.extend_from_slice(&orientation.to_le_bytes());
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        let length = u16::try_from(payload.len() + 2).unwrap();

        let mut output = Vec::with_capacity(jpeg.len() + payload.len() + 4);
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn animated_gif() -> Vec<u8> {
        let red = ImageBuffer::from_pixel(16, 16, Rgba([240, 10, 10, u8::MAX]));
        let blue = ImageBuffer::from_pixel(16, 16, Rgba([10, 10, 240, u8::MAX]));
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder
            .encode_frames([Frame::new(red), Frame::new(blue)])
            .unwrap();
        drop(encoder);
        bytes
    }

    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        let image = ImageBuffer::from_fn(width, height, |_x, _y| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Rgb([(state >> 24) as u8, (state >> 16) as u8, (state >> 8) as u8])
        });
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ColorType::Rgb8.into())
            .unwrap();
        bytes
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = png(1, 1, false);
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let checksum = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
        bytes
    }

    #[test]
    fn deterministic_admission_deduplicates_and_strips_to_normalized_object() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let source = png(16, 8, false);
        let first = store
            .admit(&source, Some("/private/path/diagram.png".to_owned()))
            .unwrap();
        let second = store
            .admit(&source, Some("diagram.png".to_owned()))
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.filename.as_deref(), Some("diagram.png"));
        assert_eq!(first.media_type, "image/jpeg");
        assert_eq!(store.read(&first).unwrap(), store.read(&second).unwrap());
    }

    #[test]
    fn transparency_stays_png_and_dimensions_are_resized_to_budget() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            root.path(),
            "database",
            ImageAdmissionPolicy {
                max_width: 20,
                max_height: 20,
                max_pixels: 400,
                ..ImageAdmissionPolicy::default()
            },
        )
        .unwrap();
        let admitted = store.admit(&png(100, 50, true), None).unwrap();
        assert_eq!(admitted.media_type, "image/png");
        assert!(admitted.width <= 20);
        assert!(admitted.height <= 20);
        assert!(u64::from(admitted.width) * u64::from(admitted.height) <= 400);
    }

    #[test]
    fn opaque_alpha_channels_are_flattened_to_jpeg() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&opaque_rgba_png(8, 8), None).unwrap();
        assert_eq!(admitted.media_type, "image/jpeg");
        assert_eq!(
            image::guess_format(&store.read(&admitted).unwrap()).unwrap(),
            ImageFormat::Jpeg
        );
    }

    #[test]
    fn exif_orientation_is_applied_and_metadata_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&oriented_jpeg(6), None).unwrap();
        assert_eq!((admitted.width, admitted.height), (1, 2));
        let normalized = store.read(&admitted).unwrap();
        assert!(!normalized.windows(4).any(|window| window == b"Exif"));
        assert_eq!(exif_orientation(&normalized, ImageFormat::Jpeg), 1);
    }

    #[test]
    fn animated_inputs_keep_only_the_first_frame() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&animated_gif(), None).unwrap();
        assert_eq!(admitted.media_type, "image/jpeg");
        let decoded = image::load_from_memory(&store.read(&admitted).unwrap())
            .unwrap()
            .to_rgb8();
        let pixel = decoded.get_pixel(8, 8).0;
        assert!(
            pixel[0] > pixel[2],
            "the first red frame must win: {pixel:?}"
        );
    }

    #[test]
    fn hostile_dimensions_are_rejected_before_decode_allocation() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let error = store
            .admit(&png_with_dimensions(100_000, 100_000), None)
            .expect_err("header-only decompression bomb");
        assert!(matches!(error, AttachmentError::PixelLimit { .. }));
        assert!(walk_files(&store.root.join("objects")).unwrap().is_empty());
    }

    #[test]
    fn declared_source_mime_must_match_detected_bytes() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(png(2, 2, false));
        let error = store
            .admit_base64_typed(&data, Some("image/jpeg"), None)
            .expect_err("mismatched MIME");
        assert!(matches!(
            error,
            AttachmentError::MediaTypeMismatch {
                expected,
                detected
            } if expected == "image/jpeg" && detected == "image/png"
        ));
    }

    #[test]
    fn concurrent_admission_publishes_one_complete_object() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap(),
        );
        let source = Arc::new(noisy_png(64, 64));
        let barrier = Arc::new(Barrier::new(12));
        let handles = (0..12)
            .map(|_| {
                let store = Arc::clone(&store);
                let source = Arc::clone(&source);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.admit(source.as_slice(), None).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let admitted = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert!(
            admitted
                .iter()
                .all(|reference| reference.id == admitted[0].id)
        );
        assert_eq!(walk_files(&store.root.join("objects")).unwrap().len(), 1);
        assert!(store.read(&admitted[0]).is_ok());
    }

    #[test]
    fn admitted_objects_survive_store_reopen() {
        let root = tempfile::tempdir().unwrap();
        let admitted = {
            let store =
                AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default())
                    .unwrap();
            store.admit(&png(7, 5, false), None).unwrap()
        };
        let reopened =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        assert!(reopened.read(&admitted).is_ok());
    }

    #[test]
    fn route_policy_derivations_are_cached_by_attachment_and_policy() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&noisy_png(128, 96), None).unwrap();
        let policy = ImageRequestPolicy {
            max_width: 24,
            max_height: 24,
            max_pixels: 24 * 24,
            max_encoded_bytes: 2_000,
        };
        let first = store.resolve(&admitted, policy).unwrap();
        let second = store.resolve(&admitted, policy).unwrap();
        assert_eq!(first, second);
        assert_eq!(walk_files(&store.root.join("derived")).unwrap().len(), 1);
    }

    #[test]
    fn missing_and_corrupt_objects_are_permanent_typed_failures() {
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&png(4, 4, false), None).unwrap();
        let path = store.object_path(&admitted.id);
        fs::write(&path, b"corrupt").unwrap();
        assert!(matches!(
            store.read(&admitted),
            Err(AttachmentError::DigestMismatch { .. })
        ));
        fs::remove_file(path).unwrap();
        assert!(matches!(
            store.read(&admitted),
            Err(AttachmentError::MissingObject { .. })
        ));
    }

    #[test]
    fn disabled_resize_rejects_oversized_source_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            root.path(),
            "database",
            ImageAdmissionPolicy {
                auto_resize: false,
                max_width: 10,
                max_height: 10,
                max_pixels: 100,
                ..ImageAdmissionPolicy::default()
            },
        )
        .unwrap();
        assert!(matches!(
            store.admit(&png(20, 20, false), None),
            Err(AttachmentError::Dimensions { .. } | AttachmentError::PixelLimit { .. })
        ));
        assert!(walk_files(&store.root.join("objects")).unwrap().is_empty());
    }

    #[test]
    fn disabled_resize_rejects_encoded_overflow_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            root.path(),
            "database",
            ImageAdmissionPolicy {
                auto_resize: false,
                max_encoded_bytes: 100,
                ..ImageAdmissionPolicy::default()
            },
        )
        .unwrap();
        assert!(matches!(
            store.admit(&noisy_png(64, 64), None),
            Err(AttachmentError::EncodedTooLarge { limit: 100 })
        ));
        assert!(walk_files(&store.root.join("objects")).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn store_directories_and_objects_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&png(2, 2, false), None).unwrap();
        assert_eq!(
            fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.object_path(&admitted.id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn an_unfinished_object_is_never_reclaimed_and_never_reads_as_a_finished_one() {
        // `AttachmentStore::garbage_collect` used to live here with no caller anywhere in the
        // workspace. It deleted every file under `objects/` and `derived/` whose name did not
        // match a caller-supplied live digest, so a `.tmp` object another process was still
        // writing was its first victim by construction: a temporary name cannot match a live
        // digest until the rename that publishes it. That reclaim path was deleted instead of
        // rebuilt, and this test pins the two properties the shipped collector in
        // `zuno-db::artifact_gc` depends on: an unfinished name can never be read as a finished
        // object, and nothing in this crate removes a file it did not create.
        let root = tempfile::tempdir().unwrap();
        let store =
            AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default()).unwrap();
        let admitted = store.admit(&png(6, 4, false), None).unwrap();
        let shard = store
            .object_path(&admitted.id)
            .parent()
            .expect("object shard")
            .to_path_buf();

        // Exactly the spelling `write_atomic_private` gives a concurrent writer's object.
        let pending = hex::encode([0xab_u8; 32]);
        let in_progress = shard.join(format!(".{pending}.{}.{}.tmp", 4321, 7));
        fs::write(&in_progress, b"the first half of a normalized object").unwrap();

        let name = in_progress
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temporary name");
        assert!(name.starts_with('.') && name.ends_with(".tmp"));
        assert!(AttachmentId::parse(name).is_err());
        // `zuno-db::artifact_gc` only treats a file as a candidate when the first `-` segment of
        // its name is a bare 64-character digest, which this name can never become.
        let candidate = name
            .strip_prefix("sha256:")
            .unwrap_or(name)
            .split('-')
            .next()
            .unwrap_or_default();
        assert_ne!(candidate.len(), 64);

        // Every public operation runs while the foreign write is still outstanding.
        assert_eq!(
            store.admit(&png(6, 4, false), None).unwrap().id,
            admitted.id
        );
        store.read(&admitted).unwrap();
        store
            .resolve(&admitted, ImageRequestPolicy::default())
            .unwrap();
        AttachmentStore::new(root.path(), "database", ImageAdmissionPolicy::default())
            .unwrap()
            .read(&admitted)
            .unwrap();

        assert!(
            in_progress.exists(),
            "an unfinished object written by another process was reclaimed: {in_progress:?}"
        );
        assert_eq!(
            fs::read(&in_progress).unwrap(),
            b"the first half of a normalized object"
        );
    }
}
