use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use zuno_db::job::{AgentJob, AgentJobStore, JobSubject};
use zuno_db::pool::Pool;
use zuno_error::ToolError;
use zuno_tool::{ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

pub const TASK_REPORT_TOOL_ID: &str = "task_report";
pub const WIRE_ID: &str = TASK_REPORT_TOOL_ID;
pub const DESCRIPTION: &str = include_str!("description/task-report.txt");
const MAX_REPORT_BYTES: usize = 8 * 1_024;
const MIN_REPORT_BYTES: usize = 512;
const MAX_ERROR_CHARS: usize = 1_000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskReportParams {
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Clone)]
pub struct TaskReportTool {
    jobs: AgentJobStore,
}

impl TaskReportTool {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            jobs: AgentJobStore::new(pool),
        }
    }
}

#[async_trait]
impl TypedTool for TaskReportTool {
    type Params = TaskReportParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    async fn run(
        &self,
        params: TaskReportParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let job_id = visible(params.job_id.as_deref());
        let task_id = visible(params.task_id.as_deref());
        if job_id.is_some() == task_id.is_some() {
            return Err(invalid("supply exactly one of jobID or taskID"));
        }
        if params
            .max_bytes
            .is_some_and(|bytes| bytes > 0 && (bytes as usize) < MIN_REPORT_BYTES)
        {
            return Err(invalid(&format!(
                "maxBytes must be at least {MIN_REPORT_BYTES} when supplied"
            )));
        }
        let jobs = self.jobs.clone();
        let parent = ctx.session_id;
        let selected = tokio::task::spawn_blocking(move || -> Result<AgentJob, ToolError> {
            let job = match (job_id, task_id) {
                (Some(job_id), None) => jobs.get(&job_id).map_err(|_| not_found())?,
                (None, Some(task_id)) => jobs
                    .list_for_parent(&parent)
                    .map_err(failed)?
                    .into_iter()
                    .rev()
                    .find(|job| {
                        matches!(
                            &job.subject,
                            JobSubject::ChildSession { session_id } if session_id == &task_id
                        )
                    })
                    .ok_or_else(not_found)?,
                _ => unreachable!("validated selector"),
            };
            if job.parent_session_id != parent {
                return Err(not_found());
            }
            Ok(job)
        })
        .await
        .map_err(failed)??;
        let max_bytes = params
            .max_bytes
            .filter(|value| *value > 0)
            .map_or(MAX_REPORT_BYTES, |value| value as usize)
            .min(MAX_REPORT_BYTES);
        let body = bounded_job(&selected, max_bytes);
        Ok(ToolOutput::text(
            format!("{}: {}", selected.id, selected.status.as_str()),
            body,
        ))
    }
}

fn bounded_job(job: &AgentJob, max_bytes: usize) -> String {
    let result = job.result.clone();
    let full = job_json(job, result.clone());
    let encoded = full.to_string();
    if encoded.len() <= max_bytes {
        return encoded;
    }
    let omitted_bytes = result
        .as_ref()
        .map(|value| value.to_string().len())
        .unwrap_or_default();
    let bounded = job_json(
        job,
        Some(json!({
            "omitted": true,
            "bytes": omitted_bytes,
            "reason": "terminal result exceeds task_report byte limit"
        })),
    );
    let encoded = bounded.to_string();
    if encoded.len() <= max_bytes {
        return encoded;
    }
    let fallback = json!({
        "jobID": job.id,
        "status": job.status.as_str(),
        "result": {
            "omitted": true,
            "bytes": omitted_bytes
        }
    })
    .to_string();
    if fallback.len() <= max_bytes {
        fallback
    } else {
        "{\"omitted\":true}".to_owned()
    }
}

fn job_json(job: &AgentJob, result: Option<Value>) -> Value {
    json!({
        "jobID": job.id,
        "parentSessionID": job.parent_session_id,
        "taskID": match &job.subject {
            JobSubject::ChildSession { session_id } => Some(session_id.as_str()),
            JobSubject::ProductAgent { .. } | JobSubject::Workflow { .. } => None,
        },
        "subject": job.subject.as_json(),
        "status": job.status.as_str(),
        "reportDelivery": job.report_delivery.as_str(),
        "result": result,
        "error": job.error.as_deref().map(|error| clip(error, MAX_ERROR_CHARS)),
        "reportInputID": job.report_input_id,
        "timeCompleted": job.time_completed,
    })
}

fn visible(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn clip(value: &str, max: usize) -> String {
    let mut output = value.chars().take(max).collect::<String>();
    if value.chars().count() > max {
        output.push('…');
    }
    output
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn not_found() -> ToolError {
    ToolError::NotFound {
        tool: WIRE_ID.to_owned(),
    }
}

fn failed(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use zuno_db::job::{JobSubject, NewAgentJob, ReportDelivery};
    use zuno_paths::DbLocation;
    use zuno_tool::{AllowAll, NeverInterrupted};

    fn fixture() -> (tempfile::TempDir, Arc<Pool>) {
        let root = tempfile::tempdir().expect("task report root");
        let location = DbLocation::File(root.path().join("task-report.db"));
        let mut connection = zuno_db::open::open(&location).expect("database");
        zuno_db::migration::apply(&mut connection).expect("migration");
        connection
            .execute(
                "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
                 VALUES ('proj', '/tmp/proj', NULL, 1, 1, '[]')",
                (),
            )
            .expect("project");
        let transaction = connection.transaction().expect("transaction");
        zuno_db::session::create(
            &transaction,
            &zuno_db::session::SessionCreate::new(
                "ses_parent",
                "ses_parent",
                "proj",
                "/tmp/proj",
                "/tmp/proj",
                "parent",
                "0.0.0",
            )
            .at(1),
        )
        .expect("parent");
        transaction.commit().expect("commit");
        drop(connection);
        let pool = Arc::new(Pool::open(&location).expect("pool"));
        AgentJobStore::new(Arc::clone(&pool))
            .create(NewAgentJob::new(
                "job_1",
                "ses_parent",
                JobSubject::child_session("ses_child"),
                ReportDelivery::NextStep,
                2,
            ))
            .expect("job");
        (root, pool)
    }

    fn context(session_id: &str) -> ToolContext {
        ToolContext::new(
            session_id,
            "msg_1",
            "call_1",
            "review",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[tokio::test]
    async fn job_and_task_selectors_resolve_the_same_parent_owned_report() {
        let (_root, pool) = fixture();
        let tool = TaskReportTool::new(pool);
        let by_job = tool
            .run(
                TaskReportParams {
                    job_id: Some("job_1".to_owned()),
                    task_id: None,
                    max_bytes: None,
                },
                context("ses_parent"),
            )
            .await
            .expect("job selector");
        let by_task = tool
            .run(
                TaskReportParams {
                    job_id: None,
                    task_id: Some("ses_child".to_owned()),
                    max_bytes: None,
                },
                context("ses_parent"),
            )
            .await
            .expect("task selector");
        let job: Value = serde_json::from_str(&by_job.output).expect("job json");
        let task: Value = serde_json::from_str(&by_task.output).expect("task json");
        assert_eq!(job["jobID"], "job_1");
        assert_eq!(task["jobID"], "job_1");
        assert_eq!(task["taskID"], "ses_child");
    }

    #[tokio::test]
    async fn another_parent_cannot_read_a_report_by_job_id() {
        let (_root, pool) = fixture();
        let error = TaskReportTool::new(pool)
            .run(
                TaskReportParams {
                    job_id: Some("job_1".to_owned()),
                    task_id: None,
                    max_bytes: None,
                },
                context("ses_other"),
            )
            .await
            .expect_err("foreign parent");
        assert!(matches!(error, ToolError::NotFound { .. }));
    }

    #[test]
    fn a_large_terminal_result_is_omitted_as_one_bounded_value() {
        let job = AgentJob {
            id: "job_large".to_owned(),
            parent_session_id: "ses_parent".to_owned(),
            logical_key: "large".to_owned(),
            subject: JobSubject::child_session("ses_child"),
            work_context: None,
            orchestration_snapshot: None,
            evidence_start_rowid: 0,
            status: zuno_db::job::JobStatus::Completed,
            report_delivery: ReportDelivery::NextStep,
            result: Some(json!({"text":"x".repeat(MAX_REPORT_BYTES * 2)})),
            error: None,
            report_input_id: None,
            created_sequence: 1,
            settled_sequence: Some(2),
            time_created: 1,
            time_updated: 2,
            time_completed: Some(2),
        };
        let output = bounded_job(&job, 512);
        assert!(output.len() <= 512, "{}", output.len());
        let value: Value = serde_json::from_str(&output).expect("bounded JSON");
        assert_eq!(value["result"]["omitted"], true);
    }
}
