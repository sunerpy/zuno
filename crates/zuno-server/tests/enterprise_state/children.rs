use super::*;
use sqlx_core::query_scalar::query_scalar;
use zuno_application::{
    child::{ChildDefinitionCatalog, ChildDefinitionGrant},
    runtime::{JobInputModel, JobInputSelection},
};
use zuno_engine::{budget::NoopBudgetPolicy, driver::DefaultAgentDriver};
use zuno_worker::{
    child::{ChildToolDispatcher, ChildToolTarget},
    runtime::{
        WorkerError, WorkerObserver, WorkerServiceFactory, WorkerTurnServices, advance_claimed,
    },
};

#[derive(Clone)]
pub(super) struct Catalog {
    parent: ConfigurationRef,
    child: ConfigurationRef,
}
impl Catalog {
    pub(super) fn new(parent: ConfigurationRef) -> Self {
        Self {
            parent,
            child: ConfigurationRef {
                id: ConfigurationId::new("wire-child").unwrap(),
                version: 1,
                sha256: "7".repeat(64),
            },
        }
    }
}
impl ChildDefinitionCatalog for Catalog {
    fn resolve(
        &self,
        parent: &ConfigurationRef,
        agent: &str,
        model: Option<&str>,
    ) -> Option<ChildDefinitionGrant> {
        (parent == &self.parent && agent == "build" && model == Some("wire-test/model")).then(
            || ChildDefinitionGrant {
                parent: self.parent.clone(),
                child: self.child.clone(),
                selection: JobInputSelection {
                    agent: "build".to_owned(),
                    model: JobInputModel {
                        provider_id: "wire-test".to_owned(),
                        model_id: "model".to_owned(),
                    },
                },
                maximum_depth: 2,
                maximum_children: 4,
                workspace: zuno_application::child::ChildWorkspacePolicy::ModelOnly,
            },
        )
    }
}

#[derive(Debug)]
struct ProviderScript {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Value>>,
}
impl Provider for ProviderScript {
    fn id(&self) -> &str {
        "wire-test"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }
    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().unwrap().push(json!(request.messages));
        Box::pin(stream::iter(
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("one response per expected request")
                .into_iter()
                .map(Ok::<_, ProviderError>),
        ))
    }
}
fn text(value: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(value.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ]
}
fn providers(script: &Arc<ProviderScript>) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();
    let script = script.clone();
    registry.register("wire-test", move |_| script.clone());
    Arc::new(registry)
}
struct Factory {
    client: WorkerClient,
    catalog: Catalog,
    parent: Arc<ProviderRegistry>,
    child: Arc<ProviderRegistry>,
}
#[async_trait]
impl WorkerServiceFactory for Factory {
    fn configurations(&self) -> Vec<ConfigurationRef> {
        vec![self.catalog.parent.clone(), self.catalog.child.clone()]
    }
    async fn resolve(
        &self,
        execution: &zuno_worker::WorkerExecution,
    ) -> Result<WorkerTurnServices, WorkerError> {
        let parent = execution.job.configuration == self.catalog.parent;
        if !parent && execution.job.configuration != self.catalog.child {
            return Err(WorkerError::Configuration);
        }
        let empty = Arc::new(ToolRegistryDispatcher::new(
            vec![],
            vec![],
            Arc::new(AllowAll),
            AuthorizationPolicy::Standard,
            McpToolStatus::Ready,
        ));
        let dispatcher: Arc<dyn zuno_engine::r#loop::ToolDispatcher> = if parent {
            Arc::new(
                ChildToolDispatcher::new(
                    empty,
                    self.client.clone(),
                    execution.clone(),
                    [(
                        "build".to_owned(),
                        ChildToolTarget {
                            model: "wire-test/model".to_owned(),
                            facts: zuno_tools::ModelFacts {
                                family: zuno_llm::effort::ProviderFamily::OpenAi,
                                reasoning: false,
                                effort: Default::default(),
                                variants: Default::default(),
                            },
                        },
                    )]
                    .into(),
                    2,
                )
                .map_err(|_| WorkerError::Configuration)?,
            )
        } else {
            empty
        };
        Ok(WorkerTurnServices {
            configuration: execution.job.configuration.clone(),
            providers: if parent {
                self.parent.clone()
            } else {
                self.child.clone()
            },
            resolver: Arc::new(Resolver),
            dispatcher,
            driver: Arc::new(DefaultAgentDriver),
            budget: Arc::new(NoopBudgetPolicy),
            dynamic_context: DynamicContext::default(),
            dynamic_context_refresher: None,
            executor_directory: "/workspace".to_owned(),
            steps_per_advance: NonZeroU32::MIN,
            context_limit: None,
        })
    }
}
struct Observer;
impl WorkerObserver for Observer {
    fn event(&self, _job: &JobId, _event: zuno_engine::r#loop::TurnEvent) {}
    fn settled(&self, _job: &JobId, _outcome: &Result<AdvanceOutcome, WorkerError>) {}
}

pub(super) async fn exercise(
    backend: &PostgresBackend,
    admin: &sqlx_postgres::PgPool,
    client: &WorkerClient,
    actor: &PrincipalScope,
    parent_configuration: &ConfigurationRef,
) {
    let session = AgentApplication::new(Arc::new(backend.sessions(actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("child-parent").unwrap(),
            workspace_id: WorkspaceId::new("workspace").unwrap(),
            title: "Delegate over HTTP".to_owned(),
        })
        .await
        .unwrap();
    let runtime = backend.runtime(actor.tenant_id().clone());
    let parent = runtime
        .submit(
            actor,
            JobSubmission {
                session_id: session.id.clone(),
                request_id: RequestId::new("child-parent-input").unwrap(),
                expected_input_version: 0,
                text: "Inspect through a child".to_owned(),
                configuration: parent_configuration.clone(),
                selection: Some(JobInputSelection {
                    agent: "build".to_owned(),
                    model: JobInputModel {
                        provider_id: "wire-test".to_owned(),
                        model_id: "model".to_owned(),
                    },
                }),
            },
        )
        .await
        .unwrap();
    let arguments = json!({"agent":"build","objective":"Inspect the fixture","deliverable":"A checked answer",
        "instructions":"Read the fixture","success_evidence":"Return its finding"});
    let parent_script = Arc::new(ProviderScript {
        replies: Mutex::new(
            vec![
                vec![
                    StreamEvent::ToolUseStart {
                        id: "remote-child".to_owned(),
                        name: "task".to_owned(),
                    },
                    StreamEvent::ToolInputDelta {
                        id: "remote-child".to_owned(),
                        delta: arguments.to_string(),
                    },
                    StreamEvent::ToolUseEnd {
                        id: "remote-child".to_owned(),
                    },
                    StreamEvent::MessageEnd {
                        stop_reason: Some(FinishReason::ToolCalls),
                    },
                ],
                text("Parent received the child result"),
            ]
            .into(),
        ),
        requests: Mutex::new(Vec::new()),
    });
    let child_script = Arc::new(ProviderScript {
        replies: Mutex::new(vec![text("Child verified the fixture")].into()),
        requests: Mutex::new(Vec::new()),
    });
    let factory = Factory {
        client: client.clone(),
        catalog: Catalog::new(parent_configuration.clone()),
        parent: providers(&parent_script),
        child: providers(&child_script),
    };
    let configurations = factory.configurations();
    let first = client
        .claim(
            WorkerInstanceId::new("delegating-worker").unwrap(),
            &configurations,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.job.id, parent.id);
    let outcome = advance_claimed(
        client,
        &first,
        &factory,
        &Observer,
        std::time::Duration::from_millis(100),
    )
    .await
    .unwrap();
    let AdvanceOutcome::Waiting { waits, .. } = outcome else {
        panic!("foreground task must release its slot and remain pending: {outcome:?}");
    };
    assert_eq!(waits.len(), 1);
    assert_eq!(parent_script.requests.lock().unwrap().len(), 1);
    let child = client
        .claim(
            WorkerInstanceId::new("other-child-worker").unwrap(),
            &configurations,
        )
        .await
        .unwrap()
        .unwrap();
    assert_ne!(child.job.session_id, parent.session_id);
    let child_id = child.job.session_id.clone();
    assert!(matches!(
        advance_claimed(
            client,
            &child,
            &factory,
            &Observer,
            std::time::Duration::from_millis(100)
        )
        .await
        .unwrap(),
        AdvanceOutcome::Completed { .. }
    ));
    let mut completed = false;
    for step in 0..3 {
        let next = client
            .claim(
                WorkerInstanceId::new(format!("replacement-parent-{step}")).unwrap(),
                &configurations,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.job.id, parent.id);
        match advance_claimed(
            client,
            &next,
            &factory,
            &Observer,
            std::time::Duration::from_millis(100),
        )
        .await
        .unwrap()
        {
            AdvanceOutcome::Completed { .. } => {
                completed = true;
                break;
            }
            AdvanceOutcome::Progressed { .. } => {}
            other => panic!("unexpected resumed parent result: {other:?}"),
        }
    }
    assert!(completed);
    assert_eq!(parent_script.requests.lock().unwrap().len(), 2);
    assert_eq!(child_script.requests.lock().unwrap().len(), 1);
    let sent = parent_script.requests.lock().unwrap()[1].to_string();
    assert!(sent.contains("Child verified the fixture"));
    assert!(
        sent.contains(child_id.as_str()),
        "the native task result must identify the resumable child session"
    );
    let tools:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND kind='tool'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(parent.session_id.as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(
        tools, 1,
        "the original invocation gets one durable result across Worker replacement"
    );
}
