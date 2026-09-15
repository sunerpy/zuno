use super::LocalWorkflowLoadLimits;
use super::LocalWorkflowRoot;
use super::RegisteredWorkflow;
use super::WorkflowDiagnostic;
use super::WorkflowDiagnosticCode;
use super::WorkflowDiagnosticLevel;
use super::WorkflowRegistryLoadOutcome;
use super::WorkflowSource;
use super::WorkflowSourceScope;
use super::normalized_path;
use super::resolve_candidates;
use super::script::BoundedReadError;
use super::script::executable_digest;
use super::script::load_external_script;
use super::script::read_bounded_utf8;
use crate::WorkflowDefinition;
use crate::WorkflowFormat;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Loads a fresh local registry snapshot. It never mutates an existing snapshot.
///
/// Plugin runtimes with a non-local filesystem should materialize authority-checked files into
/// their installed cache before passing roots here. Discovery never follows symlinks.
pub fn load_local_workflow_registry(
    roots: impl IntoIterator<Item = LocalWorkflowRoot>,
    limits: LocalWorkflowLoadLimits,
) -> WorkflowRegistryLoadOutcome {
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort_by(|left, right| root_sort_key(left).cmp(&root_sort_key(right)));
    let mut deduplicated = Vec::<LocalWorkflowRoot>::with_capacity(roots.len());
    for root in roots {
        if let Some(previous) = deduplicated.last_mut()
            && previous.source == root.source
            && previous.path == root.path
        {
            previous.required |= root.required;
        } else {
            deduplicated.push(root);
        }
    }
    let mut roots = deduplicated;

    let mut diagnostics = Vec::new();
    if roots.len() > limits.max_roots {
        for root in roots.drain(limits.max_roots..) {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::RootLimit,
                &root.source,
                &root.path,
                format!("workflow root limit of {} exceeded", limits.max_roots),
            ));
        }
    }

    let mut candidates = Vec::new();
    let mut seen_documents = BTreeSet::new();
    let mut discovered_files = 0usize;
    for root in roots {
        load_root(
            &root,
            limits,
            &mut discovered_files,
            &mut seen_documents,
            &mut candidates,
            &mut diagnostics,
        );
    }

    resolve_candidates(candidates, diagnostics)
}

fn load_root(
    root: &LocalWorkflowRoot,
    limits: LocalWorkflowLoadLimits,
    discovered_files: &mut usize,
    seen_documents: &mut BTreeSet<(WorkflowSourceScope, String, PathBuf)>,
    candidates: &mut Vec<Arc<RegisteredWorkflow>>,
    diagnostics: &mut Vec<WorkflowDiagnostic>,
) {
    let metadata = match fs::symlink_metadata(&root.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !root.required => return,
        Err(error) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                if error.kind() == std::io::ErrorKind::NotFound {
                    WorkflowDiagnosticCode::MissingRoot
                } else {
                    WorkflowDiagnosticCode::Inspect
                },
                &root.source,
                &root.path,
                error.to_string(),
            ));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        diagnostics.push(diagnostic(
            WorkflowDiagnosticLevel::Error,
            WorkflowDiagnosticCode::SymlinkRejected,
            &root.source,
            &root.path,
            "workflow roots must not be symlinks".to_string(),
        ));
        return;
    }

    let canonical_root = if metadata.is_dir() {
        match root.path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::Inspect,
                    &root.source,
                    &root.path,
                    error.to_string(),
                ));
                return;
            }
        }
    } else if metadata.is_file() {
        match root.path.parent().and_then(|path| path.canonicalize().ok()) {
            Some(path) => path,
            None => {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::Inspect,
                    &root.source,
                    &root.path,
                    "workflow file root has no readable parent".to_string(),
                ));
                return;
            }
        }
    } else {
        diagnostics.push(diagnostic(
            WorkflowDiagnosticLevel::Error,
            WorkflowDiagnosticCode::Inspect,
            &root.source,
            &root.path,
            "workflow root must be a file or directory".to_string(),
        ));
        return;
    };

    let mut files = if metadata.is_file() {
        if workflow_format(&root.path).is_some() {
            vec![root.path.clone()]
        } else {
            Vec::new()
        }
    } else {
        discover_files(root, limits, diagnostics)
    };
    files.sort_by_key(|path| normalized_path(path));

    for file in files {
        if *discovered_files >= limits.max_files {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::FileLimit,
                &root.source,
                &file,
                format!("workflow file limit of {} exceeded", limits.max_files),
            ));
            break;
        }
        *discovered_files += 1;

        let canonical_file = match file.canonicalize() {
            Ok(path) if path.starts_with(&canonical_root) => path,
            Ok(_) => {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::DocumentOutsideRoot,
                    &root.source,
                    &file,
                    "workflow document resolved outside its declared root".to_string(),
                ));
                continue;
            }
            Err(error) => {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::Inspect,
                    &root.source,
                    &file,
                    error.to_string(),
                ));
                continue;
            }
        };
        if !seen_documents.insert((
            root.source.scope,
            root.source.id.clone(),
            canonical_file.clone(),
        )) {
            continue;
        }
        load_document(
            root,
            &canonical_root,
            canonical_file,
            limits,
            candidates,
            diagnostics,
        );
    }
}

fn discover_files(
    root: &LocalWorkflowRoot,
    limits: LocalWorkflowLoadLimits,
    diagnostics: &mut Vec<WorkflowDiagnostic>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(root.path.clone(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Error,
                    WorkflowDiagnosticCode::Read,
                    &root.source,
                    &directory,
                    error.to_string(),
                ));
                continue;
            }
        };
        let mut readable_entries = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => readable_entries.push(entry),
                Err(error) => diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Warning,
                    WorkflowDiagnosticCode::Read,
                    &root.source,
                    &directory,
                    error.to_string(),
                )),
            }
        }
        readable_entries.sort_by_key(|entry| normalized_path(&entry.path()));
        for entry in readable_entries.into_iter().rev() {
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        WorkflowDiagnosticLevel::Warning,
                        WorkflowDiagnosticCode::Inspect,
                        &root.source,
                        &path,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                diagnostics.push(diagnostic(
                    WorkflowDiagnosticLevel::Warning,
                    WorkflowDiagnosticCode::SymlinkRejected,
                    &root.source,
                    &path,
                    "workflow discovery does not follow symlinks".to_string(),
                ));
            } else if metadata.is_dir() {
                if depth >= limits.max_depth {
                    diagnostics.push(diagnostic(
                        WorkflowDiagnosticLevel::Warning,
                        WorkflowDiagnosticCode::DepthLimit,
                        &root.source,
                        &path,
                        format!(
                            "workflow discovery depth limit of {} reached",
                            limits.max_depth
                        ),
                    ));
                } else {
                    pending.push((path, depth + 1));
                }
            } else if metadata.is_file() && workflow_format(&path).is_some() {
                files.push(path);
            }
        }
    }
    files
}

fn load_document(
    root: &LocalWorkflowRoot,
    canonical_root: &Path,
    document_path: PathBuf,
    limits: LocalWorkflowLoadLimits,
    candidates: &mut Vec<Arc<RegisteredWorkflow>>,
    diagnostics: &mut Vec<WorkflowDiagnostic>,
) {
    let Some(format) = workflow_format(&document_path) else {
        return;
    };
    let metadata = match fs::metadata(&document_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::Inspect,
                &root.source,
                &document_path,
                error.to_string(),
            ));
            return;
        }
    };
    if metadata.len() > limits.max_document_bytes {
        diagnostics.push(diagnostic(
            WorkflowDiagnosticLevel::Error,
            WorkflowDiagnosticCode::DocumentTooLarge,
            &root.source,
            &document_path,
            format!(
                "workflow document exceeds the {}-byte limit",
                limits.max_document_bytes
            ),
        ));
        return;
    }
    let raw = match read_bounded_utf8(&document_path, limits.max_document_bytes) {
        Ok(raw) => raw,
        Err(BoundedReadError::TooLarge { limit }) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::DocumentTooLarge,
                &root.source,
                &document_path,
                format!("workflow document exceeds the {limit}-byte limit"),
            ));
            return;
        }
        Err(BoundedReadError::Io(error)) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::Read,
                &root.source,
                &document_path,
                error.to_string(),
            ));
            return;
        }
    };
    let relative = document_path
        .strip_prefix(canonical_root)
        .unwrap_or(document_path.as_path());
    let source_id = format!(
        "{}://{}/{}",
        root.source.scope.scheme(),
        root.source.id,
        normalized_path(relative)
    );
    let workflow = match WorkflowDefinition::parse(&raw, format, source_id) {
        Ok(workflow) => workflow,
        Err(error) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                WorkflowDiagnosticCode::Parse,
                &root.source,
                &document_path,
                error.to_string(),
            ));
            return;
        }
    };
    let resolved_script = match load_external_script(
        canonical_root,
        &document_path,
        &workflow,
        limits.max_script_bytes,
    ) {
        Ok(script) => script,
        Err((code, message)) => {
            diagnostics.push(diagnostic(
                WorkflowDiagnosticLevel::Error,
                code,
                &root.source,
                &document_path,
                message,
            ));
            return;
        }
    };
    let executable_digest = executable_digest(&workflow, resolved_script.as_deref());
    candidates.push(Arc::new(RegisteredWorkflow {
        source: root.source.clone(),
        document_path,
        workflow: Arc::new(workflow),
        resolved_script,
        executable_digest,
    }));
}

fn workflow_format(path: &Path) -> Option<WorkflowFormat> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => Some(WorkflowFormat::Json),
        Some("yaml" | "yml") => Some(WorkflowFormat::Yaml),
        _ => None,
    }
}

fn root_sort_key(root: &LocalWorkflowRoot) -> (Reverse<u8>, &str, String) {
    (
        Reverse(root.source.scope.precedence()),
        root.source.id.as_str(),
        normalized_path(&root.path),
    )
}

pub(super) fn diagnostic_sort_key(diagnostic: &WorkflowDiagnostic) -> (u8, &str, &str, &str) {
    (
        diagnostic.source.scope.precedence(),
        diagnostic.source.id.as_str(),
        diagnostic.path.as_str(),
        diagnostic.message.as_str(),
    )
}

pub(super) fn diagnostic(
    level: WorkflowDiagnosticLevel,
    code: WorkflowDiagnosticCode,
    source: &WorkflowSource,
    path: &Path,
    message: String,
) -> WorkflowDiagnostic {
    WorkflowDiagnostic {
        level,
        code,
        source: source.clone(),
        path: normalized_path(path),
        message,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
