mod support;

pub use support::{FileFormatter, NoopFormatter};
pub(crate) use support::{
    FileToolRuntime, PathKind, ResolvedPath, check_interrupt, decode_text, encode_text, failed,
    interrupted, invalid, report_diff, report_formatting, slash, write_with_dirs,
};

use async_trait::async_trait;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_tool::{Attachment, ToolContext, ToolOutput, TypedTool};

const DEFAULT_READ_LIMIT: usize = 2_000;
const MAX_LINE_LENGTH: usize = 2_000;
const MAX_BYTES: usize = 50 * 1_024;
const SAMPLE_BYTES: usize = 4_096;

/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/read.txt");

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadParams {
    /// The absolute path to the file or directory to read.
    pub file_path: String,
    /// The line number to start reading from (1-indexed).
    #[serde(default)]
    pub offset: Option<usize>,
    /// The maximum number of lines to read (defaults to 2000).
    #[serde(default)]
    pub limit: Option<usize>,
}

pub struct ReadTool {
    runtime: Arc<FileToolRuntime>,
}

impl ReadTool {
    pub(crate) fn new(runtime: Arc<FileToolRuntime>) -> Self {
        Self { runtime }
    }

    fn read_directory(
        &self,
        path: &Path,
        offset: usize,
        limit: usize,
        ctx: &ToolContext,
    ) -> Result<(String, Map<String, Value>), ToolError> {
        let entries = std::fs::read_dir(path).map_err(|error| failed("read", error))?;
        let mut names = Vec::new();
        for entry in entries {
            check_interrupt("read", ctx)?;
            let entry = entry.map_err(|error| failed("read", error))?;
            let mut name = entry.file_name().to_string_lossy().into_owned();
            let is_directory = entry
                .file_type()
                .map(|kind| kind.is_dir())
                .or_else(|_| entry.metadata().map(|metadata| metadata.is_dir()))
                .unwrap_or(false);
            if is_directory {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        let start = offset.saturating_sub(1);
        if start > names.len() {
            return Err(invalid(
                "read",
                format!(
                    "Offset {offset} is out of range for this directory ({} entries)",
                    names.len()
                ),
            ));
        }
        let shown: Vec<String> = names.iter().skip(start).take(limit).cloned().collect();
        let truncated = start.saturating_add(shown.len()) < names.len();
        let suffix = if truncated {
            format!(
                "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
                shown.len(),
                names.len(),
                offset.saturating_add(shown.len())
            )
        } else {
            format!("\n({} entries)", names.len())
        };
        let output = format!(
            "<path>{}</path>\n<type>directory</type>\n<entries>\n{}{}\n</entries>",
            path.display(),
            shown.join("\n"),
            suffix
        );
        let mut metadata = Map::new();
        metadata.insert(
            "preview".to_owned(),
            Value::String(
                shown
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );
        metadata.insert("truncated".to_owned(), Value::Bool(truncated));
        metadata.insert("loaded".to_owned(), Value::Array(Vec::new()));
        metadata.insert(
            "display".to_owned(),
            json!({
                "type": "directory",
                "path": slash(path),
                "entries": shown,
                "offset": offset,
                "totalEntries": names.len(),
                "truncated": truncated,
            }),
        );
        Ok((output, metadata))
    }

    fn read_text(
        &self,
        path: &Path,
        text: &str,
        offset: usize,
        limit: usize,
        ctx: &ToolContext,
    ) -> Result<(String, Map<String, Value>), ToolError> {
        let normalized = text.replace("\r\n", "\n");
        let lines: Vec<&str> = if normalized.is_empty() {
            Vec::new()
        } else {
            normalized.split_terminator('\n').collect()
        };
        let start = offset.saturating_sub(1);
        if start >= lines.len() && !(lines.is_empty() && offset == 1) {
            return Err(invalid(
                "read",
                format!(
                    "Offset {offset} is out of range for this file ({} lines)",
                    lines.len()
                ),
            ));
        }

        let mut shown = Vec::new();
        let mut bytes = 0usize;
        let mut cut = false;
        for line in lines.iter().skip(start).take(limit) {
            check_interrupt("read", ctx)?;
            let rendered = truncate_line(line);
            let extra = rendered.len() + usize::from(!shown.is_empty());
            if bytes.saturating_add(extra) > MAX_BYTES {
                cut = true;
                break;
            }
            bytes += extra;
            shown.push(rendered);
        }
        let more = start.saturating_add(shown.len()) < lines.len();
        let truncated = cut || more;
        let last = if shown.is_empty() {
            offset.saturating_sub(1)
        } else {
            offset + shown.len() - 1
        };
        let next = last.saturating_add(1);
        let body = shown
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}: {line}", offset + index))
            .collect::<Vec<_>>()
            .join("\n");
        let suffix = if cut {
            format!(
                "\n\n(Output capped at 50 KB. Showing lines {offset}-{last}. Use offset={next} to continue.)"
            )
        } else if more {
            format!(
                "\n\n(Showing lines {offset}-{last} of {}. Use offset={next} to continue.)",
                lines.len()
            )
        } else {
            format!("\n\n(End of file - total {} lines)", lines.len())
        };
        let output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n{body}{suffix}\n</content>",
            path.display()
        );
        let preview = shown
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let mut metadata = Map::new();
        metadata.insert("preview".to_owned(), Value::String(preview));
        metadata.insert("truncated".to_owned(), Value::Bool(truncated));
        metadata.insert("loaded".to_owned(), Value::Array(Vec::new()));
        metadata.insert(
            "display".to_owned(),
            json!({
                "type": "file",
                "path": slash(path),
                "text": shown.join("\n"),
                "lineStart": offset,
                "lineEnd": last,
                "totalLines": lines.len(),
                "truncated": truncated,
            }),
        );
        Ok((output, metadata))
    }
}

#[async_trait]
impl TypedTool for ReadTool {
    type Params = ReadParams;

    fn id(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        check_interrupt("read", &ctx)?;
        let input = Path::new(&params.file_path);
        let preliminary = self
            .runtime
            .resolve(input, PathKind::File)
            .map_err(|error| failed("read", error))?;
        let metadata = std::fs::metadata(&preliminary.canonical);
        let target = match metadata.as_ref() {
            Ok(info) if info.is_dir() => self
                .runtime
                .resolve(input, PathKind::Directory)
                .map_err(|error| failed("read", error))?,
            _ => preliminary,
        };
        self.runtime
            .authorize("read", "read", &target, &ctx)
            .await?;
        let metadata = metadata.map_err(|error| failed("read", error))?;
        let offset = params.offset.unwrap_or(1).max(1);
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let title = self.runtime.title(&target);

        if metadata.is_dir() {
            let (output, metadata) = self.read_directory(&target.canonical, offset, limit, &ctx)?;
            return Ok(ToolOutput {
                title,
                output,
                metadata,
                attachments: Vec::new(),
            });
        }

        check_interrupt("read", &ctx)?;
        let bytes = std::fs::read(&target.canonical).map_err(|error| failed("read", error))?;
        check_interrupt("read", &ctx)?;
        let mime = sniff_attachment_mime(&bytes, &target.canonical);
        if let Some(mime) = mime {
            self.runtime
                .state
                .record_read(&ctx.session_id, &target.canonical, &bytes);
            let message = if mime == "application/pdf" {
                "PDF read successfully"
            } else {
                "Image read successfully"
            };
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(ToolOutput::text(title, message)
                .with_attachment(Attachment::new(mime, format!("data:{mime};base64,{data}"))));
        }
        if is_binary(&bytes, &target.canonical) {
            return Err(invalid(
                "read",
                format!("Cannot read binary file: {}", target.canonical.display()),
            ));
        }
        let decoded = decode_text(&bytes).map_err(|error| failed("read", error))?;
        let (output, metadata) =
            self.read_text(&target.canonical, &decoded.text, offset, limit, &ctx)?;
        self.runtime
            .state
            .record_read(&ctx.session_id, &target.canonical, &bytes);
        Ok(ToolOutput {
            title,
            output,
            metadata,
            attachments: Vec::new(),
        })
    }
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line.to_owned();
    }
    let prefix: String = line.chars().take(MAX_LINE_LENGTH).collect();
    format!("{prefix}... (line truncated to {MAX_LINE_LENGTH} chars)")
}

fn sniff_attachment_mime<'a>(bytes: &[u8], path: &'a Path) -> Option<&'a str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("pdf") => Some("application/pdf"),
        _ => None,
    }
}

fn is_binary(bytes: &[u8], path: &Path) -> bool {
    const BINARY_EXTENSIONS: &[&str] = &[
        "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war", "7z", "doc", "docx", "xls",
        "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm",
        "pyc", "pyo",
    ];
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BINARY_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
    {
        return true;
    }
    let sample = &bytes[..bytes.len().min(SAMPLE_BYTES)];
    if sample.is_empty() {
        return false;
    }
    let mut non_printable = 0usize;
    for byte in sample {
        if *byte == 0 {
            return true;
        }
        if *byte < 9 || (*byte > 13 && *byte < 32) {
            non_printable += 1;
        }
    }
    non_printable * 10 > sample.len() * 3
}
