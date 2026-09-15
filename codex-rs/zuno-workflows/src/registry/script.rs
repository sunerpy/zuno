use super::WorkflowDiagnosticCode;
use crate::ValidatedWorkflow;
use crate::sha256_hex;
use std::fs;
use std::io;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

pub(super) fn load_external_script(
    canonical_root: &Path,
    document_path: &Path,
    workflow: &ValidatedWorkflow,
    max_bytes: u64,
) -> Result<Option<Arc<str>>, (WorkflowDiagnosticCode, String)> {
    let Some(relative_script) = workflow.definition().spec.script_file.as_deref() else {
        return Ok(None);
    };
    let Some(document_parent) = document_path.parent() else {
        return Err((
            WorkflowDiagnosticCode::ScriptMissing,
            "workflow document has no parent for scriptFile resolution".to_string(),
        ));
    };
    let script_path = document_parent.join(relative_script);
    let metadata = fs::symlink_metadata(&script_path).map_err(|error| {
        (
            WorkflowDiagnosticCode::ScriptMissing,
            format!(
                "failed to inspect scriptFile {}: {error}",
                script_path.display()
            ),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err((
            WorkflowDiagnosticCode::SymlinkRejected,
            format!(
                "scriptFile {} must be a regular non-symlink file",
                script_path.display()
            ),
        ));
    }
    if metadata.len() > max_bytes {
        return Err((
            WorkflowDiagnosticCode::ScriptTooLarge,
            format!("workflow script exceeds the {max_bytes}-byte limit"),
        ));
    }
    let canonical_script = script_path.canonicalize().map_err(|error| {
        (
            WorkflowDiagnosticCode::ScriptMissing,
            format!(
                "failed to resolve scriptFile {}: {error}",
                script_path.display()
            ),
        )
    })?;
    if !canonical_script.starts_with(canonical_root) {
        return Err((
            WorkflowDiagnosticCode::ScriptOutsideRoot,
            format!(
                "scriptFile {} resolves outside the declared workflow root",
                script_path.display()
            ),
        ));
    }
    let script = read_bounded_utf8(&canonical_script, max_bytes).map_err(|error| match error {
        BoundedReadError::TooLarge { limit } => (
            WorkflowDiagnosticCode::ScriptTooLarge,
            format!("workflow script exceeds the {limit}-byte limit"),
        ),
        BoundedReadError::Io(error) => (
            WorkflowDiagnosticCode::Read,
            format!(
                "failed to read scriptFile {}: {error}",
                script_path.display()
            ),
        ),
    })?;
    Ok(Some(Arc::from(script)))
}

#[derive(Debug)]
pub(super) enum BoundedReadError {
    Io(io::Error),
    TooLarge { limit: u64 },
}

pub(super) fn read_bounded_utf8(path: &Path, max_bytes: u64) -> Result<String, BoundedReadError> {
    let file = fs::File::open(path).map_err(BoundedReadError::Io)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() as u64 > max_bytes {
        return Err(BoundedReadError::TooLarge { limit: max_bytes });
    }
    String::from_utf8(bytes)
        .map_err(|error| BoundedReadError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))
}

pub(super) fn executable_digest(
    workflow: &ValidatedWorkflow,
    resolved_script: Option<&str>,
) -> String {
    let Some(script) = resolved_script else {
        return workflow.identity().digest.clone();
    };
    let mut material = Vec::with_capacity(workflow.identity().digest.len() + script.len() + 1);
    material.extend_from_slice(workflow.identity().digest.as_bytes());
    material.push(0);
    material.extend_from_slice(script.as_bytes());
    sha256_hex(&material)
}
