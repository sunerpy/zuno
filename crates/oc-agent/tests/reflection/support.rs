use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oc_agent::reflection::{
    ReflectionConfig, ReflectionError, ReflectionFork, ReflectionRequest, ReflectionRunner,
    ReflectionToolCall, ReflectionTools, ReflectionTurn, TurnDelivery, TurnTranscript,
};
use oc_error::ToolError;
use oc_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, ToolOutput};
use serde_json::{Value, json};

#[derive(Clone, Default)]
pub struct MemoryProbe {
    calls: Arc<Mutex<Vec<Value>>>,
}

impl MemoryProbe {
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("memory calls lock").len()
    }
}

#[async_trait]
impl Tool for MemoryProbe {
    fn id(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Test memory sink."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.lock().expect("memory calls lock").push(args);
        Ok(ToolOutput::text("memory", "saved"))
    }
}

#[derive(Default)]
pub struct WritingRunner {
    reviews: AtomicUsize,
}

impl WritingRunner {
    pub fn review_count(&self) -> usize {
        self.reviews.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ReflectionRunner for WritingRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        self.reviews.fetch_add(1, Ordering::SeqCst);
        tools
            .dispatch(ReflectionToolCall::new(
                "reflection-memory-call",
                "memory",
                json!({ "action": "add", "content": "durable correction" }),
            ))
            .await?;
        Ok(())
    }
}

#[derive(Default)]
pub struct CaptureRunner {
    requests: Mutex<Vec<ReflectionRequest>>,
}

impl CaptureRunner {
    pub fn take_request(&self) -> ReflectionRequest {
        self.requests
            .lock()
            .expect("reflection requests lock")
            .pop()
            .expect("one reflection request")
    }
}

#[async_trait]
impl ReflectionRunner for CaptureRunner {
    async fn review(
        &self,
        request: ReflectionRequest,
        _tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        self.requests
            .lock()
            .expect("reflection requests lock")
            .push(request);
        Ok(())
    }
}

#[derive(Default)]
pub struct DeniedToolRunner {
    denial: Mutex<Option<String>>,
}

impl DeniedToolRunner {
    pub fn denial(&self) -> String {
        self.denial
            .lock()
            .expect("denial lock")
            .clone()
            .expect("non-memory call is denied")
    }
}

#[async_trait]
impl ReflectionRunner for DeniedToolRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        let error = tools
            .dispatch(ReflectionToolCall::new(
                "reflection-bash-call",
                "bash",
                json!({ "command": "pwd" }),
            ))
            .await
            .expect_err("bash must be outside the reflection whitelist");
        *self.denial.lock().expect("denial lock") = Some(error.to_string());
        Ok(())
    }
}

pub struct PanickingRunner;

#[async_trait]
impl ReflectionRunner for PanickingRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        _tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        panic!("reflection panic")
    }
}

pub struct FailingRunner;

#[async_trait]
impl ReflectionRunner for FailingRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        _tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        Err(std::io::Error::other("reflection failed").into())
    }
}

pub fn fork<R>(turn_interval: u64, runner: Arc<R>, memory: &MemoryProbe) -> ReflectionFork
where
    R: ReflectionRunner + 'static,
{
    ReflectionFork::new(
        ReflectionConfig { turn_interval },
        runner,
        Arc::new(memory.clone()),
    )
    .expect("memory probe has the required id")
}

pub fn turn(delivery: TurnDelivery, transcript: TurnTranscript) -> ReflectionTurn {
    ReflectionTurn::new(delivery, transcript, context())
}

pub fn delivered(transcript: TurnTranscript) -> ReflectionTurn {
    turn(TurnDelivery::new(true, false), transcript)
}

pub fn context() -> ToolContext {
    ToolContext::new(
        "ses_reflection",
        "msg_reflection",
        "call_reflection",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

pub async fn await_spawned(task: Option<tokio::task::JoinHandle<()>>) {
    task.expect("reflection should be spawned")
        .await
        .expect("reflection task failures are swallowed")
}
