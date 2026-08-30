use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOperation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateChunk {
    pub(crate) header: Option<String>,
    pub(crate) lines: Vec<ChunkLine>,
    pub(crate) end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchParseError {
    Interrupted,
    Invalid(String),
}

impl fmt::Display for PatchParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted => formatter.write_str("operation interrupted"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PatchParseError {}

pub(crate) fn parse_patch(
    patch: &str,
    interrupted: impl Fn() -> bool,
) -> Result<Vec<PatchOperation>, PatchParseError> {
    let normalized = patch.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err(PatchParseError::Invalid(
            "patch must start with `*** Begin Patch` and end with `*** End Patch`".to_owned(),
        ));
    }
    if lines.len() == 2 {
        return Err(PatchParseError::Invalid(
            "patch rejected: empty patch".to_owned(),
        ));
    }

    let mut operations = Vec::new();
    let mut index = 1usize;
    while index + 1 < lines.len() {
        if interrupted() {
            return Err(PatchParseError::Interrupted);
        }
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = required_path(path)?;
            index += 1;
            let mut content = Vec::new();
            while index + 1 < lines.len() && !is_operation_header(lines[index]) {
                if interrupted() {
                    return Err(PatchParseError::Interrupted);
                }
                let Some(value) = lines[index].strip_prefix('+') else {
                    return Err(PatchParseError::Invalid(format!(
                        "added file line must start with `+`: {}",
                        lines[index]
                    )));
                };
                content.push(value);
                index += 1;
            }
            let content = if content.is_empty() {
                String::new()
            } else {
                format!("{}\n", content.join("\n"))
            };
            operations.push(PatchOperation::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            let path = required_path(path)?;
            index += 1;
            if index + 1 < lines.len() && !is_operation_header(lines[index]) {
                return Err(PatchParseError::Invalid(
                    "delete sections cannot contain patch lines".to_owned(),
                ));
            }
            operations.push(PatchOperation::Delete { path });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = required_path(path)?;
            index += 1;
            let move_to = if index + 1 < lines.len() {
                lines[index]
                    .strip_prefix("*** Move to: ")
                    .map(required_path)
                    .transpose()?
            } else {
                None
            };
            if move_to.is_some() {
                index += 1;
            }
            let mut chunks = Vec::new();
            while index + 1 < lines.len() && !is_operation_header(lines[index]) {
                if interrupted() {
                    return Err(PatchParseError::Interrupted);
                }
                let Some(raw_header) = lines[index].strip_prefix("@@") else {
                    return Err(PatchParseError::Invalid(format!(
                        "update section expected a `@@` hunk header, found: {}",
                        lines[index]
                    )));
                };
                let header = raw_header.trim().trim_end_matches("@@").trim().to_owned();
                index += 1;
                let mut chunk_lines = Vec::new();
                let mut end_of_file = false;
                while index + 1 < lines.len()
                    && !lines[index].starts_with("@@")
                    && !is_operation_header(lines[index])
                {
                    if interrupted() {
                        return Err(PatchParseError::Interrupted);
                    }
                    if lines[index] == "*** End of File" {
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    let line = lines[index];
                    let Some(prefix) = line.chars().next() else {
                        return Err(PatchParseError::Invalid(
                            "empty update lines must include a context, add, or remove prefix"
                                .to_owned(),
                        ));
                    };
                    let value = line[prefix.len_utf8()..].to_owned();
                    let parsed = match prefix {
                        ' ' => ChunkLine::Context(value),
                        '-' => ChunkLine::Remove(value),
                        '+' => ChunkLine::Add(value),
                        _ => {
                            return Err(PatchParseError::Invalid(format!(
                                "update line must start with ` `, `+`, or `-`: {line}"
                            )));
                        }
                    };
                    chunk_lines.push(parsed);
                    index += 1;
                }
                if chunk_lines.is_empty() {
                    return Err(PatchParseError::Invalid(
                        "update hunk must contain at least one patch line".to_owned(),
                    ));
                }
                chunks.push(UpdateChunk {
                    header: (!header.is_empty()).then_some(header),
                    lines: chunk_lines,
                    end_of_file,
                });
            }
            if chunks.is_empty() {
                return Err(PatchParseError::Invalid(
                    "update section must contain at least one `@@` hunk".to_owned(),
                ));
            }
            operations.push(PatchOperation::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(PatchParseError::Invalid(format!(
            "unknown patch operation header: {line}"
        )));
    }
    Ok(operations)
}

fn required_path(path: &str) -> Result<String, PatchParseError> {
    let path = path.trim();
    if path.is_empty() {
        Err(PatchParseError::Invalid(
            "patch operation path must not be empty".to_owned(),
        ))
    } else {
        Ok(path.to_owned())
    }
}

fn is_operation_header(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}
