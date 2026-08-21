use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use zuno_error::{BoxSource, ToolError};
use zuno_tool::{AllowAll, InterruptHandle, NeverInterrupted, Tool, ToolContext, Typed};
use zuno_tools::websearch::{
    SearchExecution, SearchRequest, SearchResult, SearchSource, WebSearchPolicy, WebSearchProvider,
    WebSearchTool,
};

fn context(interrupt: Arc<dyn InterruptHandle>) -> ToolContext {
    ToolContext::new(
        "ses_search",
        "msg_search",
        "call_search",
        "build",
        Arc::new(AllowAll),
        interrupt,
    )
}

async fn run(
    tool: WebSearchTool,
    args: Value,
    interrupt: Arc<dyn InterruptHandle>,
) -> Result<zuno_tool::ToolOutput, ToolError> {
    Tool::execute(&Typed(tool), args, context(interrupt)).await
}

fn result(content: &str, urls: &[&str]) -> SearchResult {
    SearchResult {
        content: (!content.is_empty()).then(|| content.to_owned()),
        sources: urls
            .iter()
            .map(|url| SearchSource {
                url: (*url).to_owned(),
                title: None,
                snippet: None,
                published_at: None,
            })
            .collect(),
        truncated: false,
    }
}

struct OrderedProvider {
    calls: Mutex<Vec<String>>,
    started: AtomicUsize,
    both_started: Notify,
    release_first: Notify,
}

impl OrderedProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
            both_started: Notify::new(),
            release_first: Notify::new(),
        }
    }
}

#[async_trait]
impl WebSearchProvider for OrderedProvider {
    fn id(&self) -> &str {
        "ordered"
    }

    async fn search(
        &self,
        request: SearchRequest,
        _execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(request.query.clone());
        if self.started.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            self.both_started.notify_waiters();
        }
        match request.query.as_str() {
            "one" => {
                self.release_first.notified().await;
                Ok(result(
                    "answer one",
                    &["https://a.test", "https://shared.test"],
                ))
            }
            "two" => Ok(result(
                "answer two",
                &["https://b.test", "https://shared.test"],
            )),
            other => Err(Box::new(io::Error::other(format!(
                "unexpected query {other}"
            )))),
        }
    }
}

#[tokio::test]
async fn distinct_queries_start_concurrently_and_merge_by_query_order() {
    let provider = Arc::new(OrderedProvider::new());
    let tool = WebSearchTool::with_provider(
        Arc::clone(&provider) as Arc<dyn WebSearchProvider>,
        WebSearchPolicy {
            max_queries: 4,
            max_results: 3,
            timeout: Duration::from_secs(5),
        },
    );

    let pending = tokio::spawn(run(
        tool,
        json!({ "queries": ["one", "one", "two"] }),
        Arc::new(NeverInterrupted),
    ));
    tokio::time::timeout(Duration::from_secs(1), provider.both_started.notified())
        .await
        .expect("both distinct queries must start before the first completes");
    provider.release_first.notify_waiters();

    let output = pending
        .await
        .expect("search task")
        .expect("batch search succeeds");
    assert_eq!(
        provider.calls.lock().expect("calls lock").as_slice(),
        ["one", "two"]
    );
    assert!(output.output.contains("### one\n\nanswer one"));
    assert!(output.output.contains("### two\n\nanswer two"));
    assert!(
        output.output.find("### one").expect("one heading")
            < output.output.find("### two").expect("two heading"),
        "rendering must follow query order, not completion order: {}",
        output.output
    );
    assert_eq!(
        output.metadata["sources"],
        json!([
            { "url": "https://a.test" },
            { "url": "https://b.test" },
            { "url": "https://shared.test" },
        ])
    );
    assert_eq!(output.metadata["truncated"], false);
}

struct FailingProvider {
    sibling_started: Notify,
    sibling_cancelled: AtomicBool,
    sibling_settle: Notify,
}

#[async_trait]
impl WebSearchProvider for FailingProvider {
    fn id(&self) -> &str {
        "failing"
    }

    async fn search(
        &self,
        request: SearchRequest,
        execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource> {
        match request.query.as_str() {
            "one" => {
                self.sibling_started.notified().await;
                Err(Box::new(io::Error::other("first search failed")))
            }
            "two" => {
                self.sibling_started.notify_waiters();
                execution.interrupt.notified().await;
                self.sibling_cancelled.store(true, Ordering::SeqCst);
                self.sibling_settle.notified().await;
                Err(Box::new(io::Error::other("sibling search stopped")))
            }
            other => Err(Box::new(io::Error::other(format!(
                "unexpected query {other}"
            )))),
        }
    }
}

#[tokio::test]
async fn first_failure_cancels_siblings_and_waits_for_settlement() {
    let provider = Arc::new(FailingProvider {
        sibling_started: Notify::new(),
        sibling_cancelled: AtomicBool::new(false),
        sibling_settle: Notify::new(),
    });
    let tool = WebSearchTool::with_provider(
        Arc::clone(&provider) as Arc<dyn WebSearchProvider>,
        WebSearchPolicy::default(),
    );
    let pending = tokio::spawn(run(
        tool,
        json!({ "queries": ["one", "two"] }),
        Arc::new(NeverInterrupted),
    ));

    tokio::time::timeout(Duration::from_secs(1), async {
        while !provider.sibling_cancelled.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the sibling observes batch cancellation");
    assert!(
        !pending.is_finished(),
        "the call must wait for every sibling to settle"
    );

    provider.sibling_settle.notify_waiters();
    let error = pending
        .await
        .expect("search task")
        .expect_err("the first failure is returned");
    let ToolError::Failed { source, .. } = error else {
        panic!("expected provider failure, got {error:?}");
    };
    assert_eq!(source.to_string(), "first search failed");
}

#[derive(Default)]
struct RecordingProvider {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl WebSearchProvider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }

    async fn search(
        &self,
        request: SearchRequest,
        _execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource> {
        self.calls.lock().expect("calls lock").push(request.query);
        Ok(SearchResult::default())
    }
}

#[tokio::test]
async fn queries_is_the_only_model_facing_input_and_bounds_precede_deduplication() {
    let provider = Arc::new(RecordingProvider::default());
    let tool = WebSearchTool::with_provider(
        Arc::clone(&provider) as Arc<dyn WebSearchProvider>,
        WebSearchPolicy {
            max_queries: 2,
            ..WebSearchPolicy::default()
        },
    );
    let schema = Tool::raw_parameters_schema(&Typed(tool.clone()));
    assert_eq!(schema["required"], json!(["queries"]));
    assert_eq!(
        schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .collect::<Vec<_>>(),
        vec!["queries"]
    );

    for args in [
        json!({ "query": "legacy" }),
        json!({ "queries": [] }),
        json!({ "queries": ["ok", " "] }),
        json!({ "queries": ["same", "same", "same"] }),
    ] {
        let error = run(
            tool.clone(),
            args,
            Arc::new(NeverInterrupted) as Arc<dyn InterruptHandle>,
        )
        .await
        .expect_err("invalid native search arguments are rejected");
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    }
    assert!(
        provider.calls.lock().expect("calls lock").is_empty(),
        "validation must finish before provider execution"
    );
}

struct ExternalInterrupt {
    set: AtomicBool,
    notify: Notify,
}

impl ExternalInterrupt {
    fn fire(&self) {
        self.set.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl InterruptHandle for ExternalInterrupt {
    fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    async fn notified(&self) {
        if self.is_set() {
            return;
        }
        self.notify.notified().await;
    }
}

struct CancellingProvider {
    started: AtomicUsize,
    observed: Mutex<BTreeMap<String, bool>>,
}

#[async_trait]
impl WebSearchProvider for CancellingProvider {
    fn id(&self) -> &str {
        "cancelling"
    }

    async fn search(
        &self,
        request: SearchRequest,
        execution: SearchExecution,
    ) -> Result<SearchResult, BoxSource> {
        self.started.fetch_add(1, Ordering::SeqCst);
        execution.interrupt.notified().await;
        self.observed
            .lock()
            .expect("observed lock")
            .insert(request.query, execution.interrupt.is_set());
        Err(Box::new(io::Error::other("search cancelled")))
    }
}

#[tokio::test]
async fn caller_interrupt_cascades_to_every_query() {
    let provider = Arc::new(CancellingProvider {
        started: AtomicUsize::new(0),
        observed: Mutex::new(BTreeMap::new()),
    });
    let signal = Arc::new(ExternalInterrupt {
        set: AtomicBool::new(false),
        notify: Notify::new(),
    });
    let tool = WebSearchTool::with_provider(
        Arc::clone(&provider) as Arc<dyn WebSearchProvider>,
        WebSearchPolicy::default(),
    );
    let pending = tokio::spawn(run(
        tool,
        json!({ "queries": ["one", "two"] }),
        Arc::clone(&signal) as Arc<dyn InterruptHandle>,
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while provider.started.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both queries start");
    signal.fire();
    pending
        .await
        .expect("search task")
        .expect_err("caller cancellation stops the batch");

    assert_eq!(
        provider.observed.lock().expect("observed lock").clone(),
        BTreeMap::from([("one".to_owned(), true), ("two".to_owned(), true)])
    );
}
