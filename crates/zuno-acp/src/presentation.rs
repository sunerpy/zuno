//! Tool-specific ACP presentation that stays independent of tool execution.
//!
//! Raw arguments and raw output remain available for details/debugging. These
//! helpers only add the human-facing card and typed Zuno metadata a client can
//! render without reverse-engineering a tool's JSON envelope.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Map, Value, json};
use zuno_engine::r#loop::ToolDiff;
use zuno_tool::{
    METADATA_MUTATION_CONFLICT_KEY, MutationConflictPresentation, QuestionResultPresentation,
    ToolResultPresentation,
};

pub(crate) fn decorate_tool_call(update: &mut Value, name: &str, raw_input: Option<&Value>) {
    decorate_file_tool_call(update, name, raw_input);
    match name {
        "question" => {
            let Some(presentation) = question_presentation(raw_input, None, "pending") else {
                return;
            };
            update["title"] = Value::String(presentation.title);
            update["content"] = json!([text_content(&presentation.card)]);
            merge_zuno_metadata(update, "question", presentation.metadata);
        }
        "task" => {
            let Some(presentation) = subagent_presentation(raw_input, None, None, "pending") else {
                return;
            };
            update["title"] = Value::String(presentation.title);
            update["content"] = json!([text_content(&presentation.card)]);
            merge_zuno_metadata(update, "subagent", presentation.metadata);
        }
        _ => {}
    }
}

/// Project intended targets only after the native argument envelope is complete.
///
/// This is presentation, not path resolution or evidence that an effect ran. In
/// particular, a relative patch target can name a card but cannot become an ACP
/// location by borrowing the adapter process's working directory.
pub(crate) fn decorate_file_tool_call(update: &mut Value, name: &str, raw_input: Option<&Value>) {
    decorate_file_tool_paths(update, name, &file_input_paths(name, raw_input));
}

pub(crate) fn decorate_file_tool_result(
    update: &mut Value,
    name: &str,
    raw_input: Option<&Value>,
    written_paths: &[String],
    diff: Option<&ToolDiff>,
    presentation: Option<&ToolResultPresentation>,
    metadata: Option<&Map<String, Value>>,
) {
    if file_action(name).is_none() {
        return;
    }
    // Resolved result paths take precedence over input aliases. Do not union
    // unobserved requested targets into a partially applied mutation's locations.
    let mut paths = normalized_paths(
        written_paths.iter().map(String::as_str).chain(
            diff.into_iter()
                .flat_map(ToolDiff::files)
                .map(zuno_tool::FileDiff::path),
        ),
    );
    if paths.is_empty() {
        let presented = uncertain_mutation_paths(presentation, metadata).or_else(|| {
            mutation_conflict_presentation(presentation, metadata)
                .map(|conflict| vec![conflict.resource().to_owned()])
        });
        if let Some(presented) = presented {
            paths = normalized_paths(presented.iter().map(String::as_str));
        }
    }
    if paths.is_empty() {
        paths = file_input_paths(name, raw_input);
    }
    decorate_file_tool_paths(update, name, &paths);
}

/// Shared by the tool stream, durable replay and native file permission cards.
pub(crate) fn decorate_file_tool_paths(update: &mut Value, name: &str, paths: &[String]) {
    let Some(action) = file_action(name) else {
        return;
    };
    let paths = normalized_paths(paths.iter().map(String::as_str));
    if paths.is_empty() {
        return;
    }
    update["title"] = Value::String(file_title(action, &paths));
    update["locations"] = Value::Array(
        paths
            .iter()
            .filter(|path| absolute_wire_path(path))
            .map(|path| json!({ "path": path }))
            .collect(),
    );
}

fn file_action(name: &str) -> Option<&'static str> {
    match name {
        "write" | "edit" | "apply_patch" => Some("Editing"),
        "move" => Some("Moving"),
        "delete" => Some("Deleting"),
        _ => None,
    }
}

fn file_input_paths(name: &str, input: Option<&Value>) -> Vec<String> {
    if !matches!(name, "write" | "edit" | "apply_patch") {
        return Vec::new();
    }
    let Some(mut input) = input.filter(|input| input.is_object()).cloned() else {
        return Vec::new();
    };
    zuno_tool::guard::strip_cross_cutting(&mut input);
    let paths = match name {
        "write" => serde_json::from_value::<zuno_tools::write::WriteParams>(input)
            .ok()
            .map(|params| vec![params.file_path]),
        "edit" => serde_json::from_value::<zuno_tools::edit::EditParams>(input)
            .ok()
            .map(|params| vec![params.file_path]),
        "apply_patch" => serde_json::from_value::<zuno_tools::apply_patch::ApplyPatchParams>(input)
            .ok()
            .and_then(|params| zuno_tools::apply_patch::intended_file_paths(&params.patch_text)),
        _ => None,
    };
    paths.map_or_else(Vec::new, |paths| {
        normalized_paths(paths.iter().map(String::as_str))
    })
}

fn normalized_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter_map(|path| {
            if path.trim().is_empty() || path.contains('\0') {
                return None;
            }
            let path = zuno_paths::wire_path(Path::new(path));
            // Durable inputs may come from a different platform. wire_path handles
            // native Windows paths; also strip their verbatim prefix on replay on
            // another host, before choosing a filename or an absolute location.
            let path = if let Some(rest) = path.strip_prefix("//?/UNC/") {
                format!("//{rest}")
            } else {
                path.strip_prefix("//?/").unwrap_or(&path).to_owned()
            };
            seen.insert(path.clone()).then_some(path)
        })
        .collect()
}

fn absolute_wire_path(path: &str) -> bool {
    path.starts_with('/')
        || matches!(
            path.as_bytes(),
            [drive, b':', b'/', ..] if drive.is_ascii_alphabetic()
        )
}

fn file_title(action: &str, paths: &[String]) -> String {
    const MAX_CHARS: usize = 160;
    const MAX_NAMES: usize = 3;
    let shown = paths.len().min(MAX_NAMES);
    let suffix = if paths.len() > shown {
        format!(" (+{} more)", paths.len() - shown)
    } else {
        String::new()
    };
    let separators = (shown - 1) * 2;
    let name_budget =
        (MAX_CHARS - action.chars().count() - 1 - suffix.chars().count() - separators) / shown;
    let names = paths
        .iter()
        .take(shown)
        .map(|path| {
            let filename = path
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(path);
            let filename = filename
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let count = filename.chars().count();
            if count > name_budget {
                format!(
                    "…{}",
                    filename
                        .chars()
                        .skip(count - (name_budget - 1))
                        .collect::<String>()
                )
            } else {
                filename
            }
        })
        .collect::<Vec<_>>();
    format!("{action} {}{suffix}", names.join(", "))
}

pub(crate) fn decorate_completed_tool_update(
    update: &mut Value,
    name: &str,
    raw_input: Option<&Value>,
    result_presentation: Option<&ToolResultPresentation>,
    metadata: Option<&Map<String, Value>>,
    output: &str,
    is_error: bool,
) {
    if let Some(conflict) = mutation_conflict_presentation(result_presentation, metadata) {
        merge_zuno_metadata(
            update,
            "mutationConflict",
            serde_json::to_value(conflict)
                .expect("mutation conflict presentation is JSON-serializable"),
        );
    }
    if let Some(paths) = uncertain_mutation_paths(result_presentation, metadata) {
        merge_zuno_metadata(update, "outcome", json!("uncertain"));
        merge_zuno_metadata(update, "uncertain", json!(true));
        merge_zuno_metadata(update, "appliedPaths", json!(paths));
    }
    match name {
        "question" => {
            let live_metadata = result_presentation.and_then(|presentation| match presentation {
                ToolResultPresentation::Question(question) => {
                    Some(question_result_metadata(question))
                }
                ToolResultPresentation::MutationConflict(_)
                | ToolResultPresentation::UncertainMutation(_) => None,
            });
            let metadata = live_metadata.as_ref().or(metadata);
            let status = metadata
                .and_then(|metadata| metadata.get("questionStatus"))
                .and_then(Value::as_str)
                .unwrap_or(if is_error { "failed" } else { "completed" });
            let Some(presentation) = question_presentation(raw_input, metadata, status) else {
                return;
            };
            if update
                .get("title")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                update["title"] = Value::String(presentation.title);
            }
            if metadata.is_some_and(|metadata| metadata.contains_key("answers")) {
                replace_primary_content(update, &presentation.card);
            } else {
                // Live turn events deliberately do not expose arbitrary tool metadata.
                // Keep the model-visible question result beside the card until replay can
                // rebuild the exact selected answers from durable metadata.
                prepend_content(update, &presentation.card);
            }
            merge_zuno_metadata(update, "question", presentation.metadata);
        }
        "task" => {
            let output_state = task_output_attribute(output, "state");
            let status = metadata
                .and_then(|metadata| metadata.get("subagent"))
                .and_then(Value::as_object)
                .and_then(|subagent| subagent.get("state"))
                .and_then(Value::as_str)
                .or(output_state.as_deref())
                .unwrap_or(if is_error { "failed" } else { "completed" });
            let Some(presentation) =
                subagent_presentation(raw_input, metadata, Some(output), status)
            else {
                return;
            };
            prepend_content(update, &presentation.card);
            merge_zuno_metadata(update, "subagent", presentation.metadata);
        }
        _ => {}
    }
}

fn mutation_conflict_presentation(
    presentation: Option<&ToolResultPresentation>,
    metadata: Option<&Map<String, Value>>,
) -> Option<MutationConflictPresentation> {
    presentation
        .and_then(|presentation| match presentation {
            ToolResultPresentation::MutationConflict(conflict) => Some(conflict.clone()),
            ToolResultPresentation::Question(_) | ToolResultPresentation::UncertainMutation(_) => {
                None
            }
        })
        .or_else(|| {
            metadata
                .and_then(|metadata| metadata.get(METADATA_MUTATION_CONFLICT_KEY))
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
        })
}

fn uncertain_mutation_paths(
    presentation: Option<&ToolResultPresentation>,
    metadata: Option<&Map<String, Value>>,
) -> Option<Vec<String>> {
    presentation
        .and_then(|presentation| match presentation {
            ToolResultPresentation::UncertainMutation(uncertain) => {
                Some(uncertain.applied_paths().to_vec())
            }
            ToolResultPresentation::Question(_) | ToolResultPresentation::MutationConflict(_) => {
                None
            }
        })
        .or_else(|| {
            metadata
                .filter(|metadata| {
                    metadata.get("outcome").and_then(Value::as_str) == Some("uncertain")
                        || metadata
                            .get("uncertain")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                })
                .map(|metadata| {
                    metadata
                        .get("writtenPaths")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
        })
}

fn question_result_metadata(result: &QuestionResultPresentation) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert(
        "questionStatus".to_owned(),
        Value::String(result.status().as_str().to_owned()),
    );
    metadata.insert("questionCount".to_owned(), json!(result.question_count()));
    metadata.insert("elapsedMs".to_owned(), json!(result.elapsed_ms()));
    if let Some(answers) = result.answers() {
        metadata.insert("answers".to_owned(), json!(answers));
    }
    metadata
}

fn replace_primary_content(update: &mut Value, card: &str) {
    if let Some(content) = update.get_mut("content").and_then(Value::as_array_mut) {
        if content.is_empty() {
            content.push(text_content(card));
        } else {
            content[0] = text_content(card);
        }
    } else {
        update["content"] = json!([text_content(card)]);
    }
}

fn prepend_content(update: &mut Value, card: &str) {
    if let Some(content) = update.get_mut("content").and_then(Value::as_array_mut) {
        content.insert(0, text_content(card));
    } else {
        update["content"] = json!([text_content(card)]);
    }
}

struct QuestionPresentation {
    title: String,
    card: String,
    metadata: Value,
}

struct SubagentPresentation {
    title: String,
    card: String,
    metadata: Value,
}

fn question_presentation(
    raw_input: Option<&Value>,
    metadata: Option<&Map<String, Value>>,
    status: &str,
) -> Option<QuestionPresentation> {
    let questions = raw_input?.as_object()?.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let answers = metadata
        .and_then(|metadata| metadata.get("answers"))
        .and_then(Value::as_array);
    let title = match questions.as_slice() {
        [question] => question
            .get("header")
            .and_then(Value::as_str)
            .filter(|header| !header.is_empty())
            .map_or_else(
                || "Question".to_owned(),
                |header| format!("Question · {header}"),
            ),
        _ => format!("Questions · {}", questions.len()),
    };
    let mut sections = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        sections.push(render_question(
            question,
            answers.and_then(|answers| answers.get(index)),
            answers.is_some(),
            status,
        ));
    }
    let mut typed = Map::new();
    typed.insert("status".to_owned(), Value::String(status.to_owned()));
    typed.insert("questions".to_owned(), Value::Array(questions.clone()));
    if let Some(answers) = answers {
        typed.insert("answers".to_owned(), Value::Array(answers.clone()));
    }
    for key in ["questionCount", "elapsedMs"] {
        if let Some(value) = metadata.and_then(|metadata| metadata.get(key)) {
            typed.insert(key.to_owned(), value.clone());
        }
    }
    Some(QuestionPresentation {
        title,
        card: sections.join("\n\n---\n\n"),
        metadata: Value::Object(typed),
    })
}

fn subagent_presentation(
    raw_input: Option<&Value>,
    metadata: Option<&Map<String, Value>>,
    output: Option<&str>,
    status: &str,
) -> Option<SubagentPresentation> {
    let input = raw_input?.as_object()?;
    let agent = input
        .get("agent")
        .and_then(Value::as_str)
        .filter(|agent| !agent.is_empty())
        .unwrap_or("subagent");
    let objective = input
        .get("objective")
        .and_then(Value::as_str)
        .filter(|objective| !objective.is_empty());
    let deliverable = input
        .get("deliverable")
        .and_then(Value::as_str)
        .filter(|deliverable| !deliverable.is_empty());
    let instructions = input
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|instructions| !instructions.is_empty());
    let success_evidence = input
        .get("success_evidence")
        .and_then(Value::as_str)
        .filter(|evidence| !evidence.is_empty());
    let scope = input.get("scope").and_then(Value::as_object);
    let constraints = input.get("constraints").and_then(Value::as_object);
    let dependencies = input.get("dependencies").and_then(Value::as_array);
    let stored = metadata
        .and_then(|metadata| metadata.get("subagent"))
        .and_then(Value::as_object);
    let mut typed = stored.cloned().unwrap_or_default();
    typed.insert("agent".to_owned(), Value::String(agent.to_owned()));
    typed.insert(
        "background".to_owned(),
        Value::Bool(
            input
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    let mut contract = Map::new();
    for (wire, projected, value) in [
        ("objective", "objective", objective),
        ("deliverable", "deliverable", deliverable),
        ("instructions", "instructions", instructions),
        ("success_evidence", "successEvidence", success_evidence),
    ] {
        if let Some(value) = value {
            let value = Value::String(value.to_owned());
            contract.insert(wire.to_owned(), value.clone());
            typed.insert(projected.to_owned(), value);
        }
    }
    if let Some(scope) = scope {
        contract.insert("scope".to_owned(), Value::Object(scope.clone()));
    }
    if let Some(constraints) = constraints {
        contract.insert("constraints".to_owned(), Value::Object(constraints.clone()));
    }
    if let Some(dependencies) = dependencies {
        contract.insert(
            "dependencies".to_owned(),
            Value::Array(dependencies.clone()),
        );
    }
    typed.insert("contract".to_owned(), Value::Object(contract));
    if let Some(output) = output {
        for (key, attribute) in [
            ("sessionId", "id"),
            ("jobId", "job"),
            ("reportDelivery", "reportDelivery"),
        ] {
            if let Some(value) = task_output_attribute(output, attribute) {
                typed
                    .entry(key.to_owned())
                    .or_insert_with(|| Value::String(value));
            }
        }
    }
    typed.insert("state".to_owned(), Value::String(status.to_owned()));
    let title = objective.map_or_else(
        || format!("Delegate · {agent}"),
        |objective| format!("Delegate · {agent} · {objective}"),
    );
    let heading = objective.unwrap_or("Delegated task");
    let mut lines = vec![format!("### {heading}"), format!("Agent: {agent}")];
    if let Some(deliverable) = deliverable {
        lines.push(format!("Deliverable: {deliverable}"));
    }
    if let Some(instructions) = instructions {
        lines.push(format!("Instructions: {instructions}"));
    }
    if let Some(success_evidence) = success_evidence {
        lines.push(format!("Success evidence: {success_evidence}"));
    }
    push_card_list(
        &mut lines,
        "Include",
        scope
            .and_then(|scope| scope.get("include"))
            .and_then(Value::as_array)
            .map(Vec::as_slice),
    );
    push_card_list(
        &mut lines,
        "Exclude",
        scope
            .and_then(|scope| scope.get("exclude"))
            .and_then(Value::as_array)
            .map(Vec::as_slice),
    );
    push_card_list(
        &mut lines,
        "Must",
        constraints
            .and_then(|constraints| constraints.get("must"))
            .and_then(Value::as_array)
            .map(Vec::as_slice),
    );
    push_card_list(
        &mut lines,
        "Must not",
        constraints
            .and_then(|constraints| constraints.get("must_not"))
            .and_then(Value::as_array)
            .map(Vec::as_slice),
    );
    push_card_list(&mut lines, "Dependencies", dependencies.map(Vec::as_slice));
    lines.push(format!("State: {status}"));
    for (label, key) in [
        ("Session", "sessionId"),
        ("Job", "jobId"),
        ("Model", "model"),
        ("Effort", "effort"),
        ("Delivery", "reportDelivery"),
    ] {
        if let Some(value) = typed
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            lines.push(format!("{label}: {value}"));
        }
    }
    Some(SubagentPresentation {
        title,
        card: lines.join("\n"),
        metadata: Value::Object(typed),
    })
}

fn push_card_list(lines: &mut Vec<String>, label: &str, values: Option<&[Value]>) {
    let Some(values) = values else {
        return;
    };
    let values = values
        .iter()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    lines.push(format!("{label}:\n- {}", values.join("\n- ")));
}

fn task_output_attribute(output: &str, name: &str) -> Option<String> {
    let tag = output.lines().find(|line| line.starts_with("<task "))?;
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

fn render_question(
    question: &Value,
    answer: Option<&Value>,
    answers_known: bool,
    status: &str,
) -> String {
    let header = question
        .get("header")
        .and_then(Value::as_str)
        .filter(|header| !header.is_empty())
        .unwrap_or("Question");
    let prompt = question
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut lines = vec![format!("### {header}")];
    if !prompt.is_empty() {
        lines.push(prompt.to_owned());
    }
    if let Some(options) = question.get("options").and_then(Value::as_array)
        && !options.is_empty()
    {
        lines.push(String::new());
        lines.extend(options.iter().filter_map(|option| {
            let label = option.get("label")?.as_str()?;
            let description = option
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.is_empty());
            Some(description.map_or_else(
                || format!("- {label}"),
                |description| format!("- {label} — {description}"),
            ))
        }));
    }
    if answers_known {
        let selected = answer
            .and_then(Value::as_array)
            .map(|answers| {
                answers
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|answer| !answer.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        lines.push(String::new());
        lines.push(if selected.is_empty() {
            "Selected: none".to_owned()
        } else {
            format!("Selected: {}", selected.join(", "))
        });
    }
    lines.push(format!("Status: {status}"));
    lines.join("\n")
}

fn text_content(text: &str) -> Value {
    json!({
        "type": "content",
        "content": { "type": "text", "text": text },
    })
}

fn merge_zuno_metadata(update: &mut Value, key: &str, value: Value) {
    if !update.is_object() {
        return;
    }
    if !update["_meta"].is_object() {
        update["_meta"] = json!({});
    }
    if !update["_meta"]["zuno"].is_object() {
        update["_meta"]["zuno"] = json!({});
    }
    update["_meta"]["zuno"][key] = value;
}
