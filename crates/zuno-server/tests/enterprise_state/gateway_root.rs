use super::*;
use zuno_application::runtime::{JobInputModel, JobInputSelection};
use zuno_engine::{budget::NoopBudgetPolicy, driver::DefaultAgentDriver};
use zuno_types::wait::WaitTarget;
use zuno_worker::{
    WorkerExecution,
    runtime::{
        WorkerError, WorkerObserver, WorkerServiceFactory, WorkerTurnServices, advance_claimed,
    },
    tools::{ENVIRONMENT_COMMAND, GatewayToolDispatcher},
};

pub(super) struct Context<'a> {
    pub backend: &'a PostgresBackend,
    pub actor: &'a PrincipalScope,
    pub worker: &'a WorkerClient,
    pub delivery: &'a GatewayExecutionService,
    pub certificate: reqwest::Certificate,
    pub configuration: &'a ConfigurationRef,
}

struct Factory {
    client: WorkerClient,
    gateway: Arc<GatewayClient>,
    configuration: ConfigurationRef,
    providers: Arc<ProviderRegistry>,
}
#[async_trait]
impl WorkerServiceFactory for Factory {
    fn configurations(&self) -> Vec<ConfigurationRef> {
        vec![self.configuration.clone()]
    }
    async fn resolve(
        &self,
        execution: &WorkerExecution,
    ) -> Result<WorkerTurnServices, WorkerError> {
        Ok(WorkerTurnServices {
            configuration: self.configuration.clone(),
            providers: self.providers.clone(),
            resolver: Arc::new(Resolver),
            dispatcher: Arc::new(GatewayToolDispatcher::new(
                self.client.clone(),
                execution.clone(),
                self.gateway.clone(),
            )),
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
    fn settled(&self, _job: &JobId, _result: &Result<AdvanceOutcome, WorkerError>) {}
}

async fn claim(context: &Context<'_>, worker: &str) -> WorkerExecution {
    context
        .worker
        .claim(
            WorkerInstanceId::new(worker).unwrap(),
            std::slice::from_ref(context.configuration),
        )
        .await
        .unwrap()
        .unwrap()
}
async fn run(
    context: &Context<'_>,
    execution: &WorkerExecution,
    factory: &Factory,
) -> AdvanceOutcome {
    advance_claimed(
        context.worker,
        execution,
        factory,
        &Observer,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap()
}

pub(super) async fn exercise(context: Context<'_>) {
    let session = AgentApplication::new(Arc::new(context.backend.sessions(context.actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("kernel-session").unwrap(),
            workspace_id: WorkspaceId::new("workspace").unwrap(),
            title: "Kernel gateway".to_owned(),
        })
        .await
        .unwrap();
    let runtime = context.backend.runtime(context.actor.tenant_id().clone());
    let job = runtime
        .submit(
            context.actor,
            JobSubmission {
                session_id: session.id.clone(),
                request_id: RequestId::new("kernel-job").unwrap(),
                expected_input_version: 0,
                text: "Execute the approved command once.".to_owned(),
                configuration: context.configuration.clone(),
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
    let script=Arc::new(Script {responses:Mutex::new(VecDeque::from([
        vec![
            StreamEvent::ToolUseStart {id:"kernel-command".to_owned(),name:ENVIRONMENT_COMMAND.to_owned()},
            StreamEvent::ToolInputDelta {id:"kernel-command".to_owned(),delta:json!({
                "argv":["sh","-c","printf 'once\\n' >> /workspace/kernel-once; cat /workspace/kernel-once"],
            }).to_string()},
            StreamEvent::ToolUseEnd {id:"kernel-command".to_owned()},
            StreamEvent::MessageEnd {stop_reason:Some(FinishReason::ToolCalls)},
        ],
        vec![StreamEvent::TextDelta("Command complete.".to_owned()),
            StreamEvent::MessageEnd {stop_reason:Some(FinishReason::Stop)}],
    ])),calls:AtomicUsize::new(0)});
    let mut providers = ProviderRegistry::new();
    let provider = script.clone();
    providers.register("wire-test", move |_| provider.clone());
    let factory = Factory {
        client: context.worker.clone(),
        gateway: Arc::new(GatewayClient::new(Some(context.certificate.clone())).unwrap()),
        configuration: context.configuration.clone(),
        providers: Arc::new(providers),
    };

    let first = claim(&context, "kernel-first").await;
    assert_eq!(first.job.id, job.id);
    let AdvanceOutcome::Waiting { waits, .. } = run(&context, &first, &factory).await else {
        panic!("approval wait")
    };
    let WaitTarget::Approval { approval_id } = &waits[0].target else {
        panic!("approval target")
    };
    let visible = zuno_server::enterprise_application::JobView::from(
        context
            .backend
            .client_job(context.actor, &job.id)
            .await
            .unwrap(),
    );
    assert_eq!(visible.waits.len(), 1);
    assert_eq!(visible.waits[0].target, waits[0].target);
    assert_eq!(
        runtime
            .get(&context.actor.owner(), &job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Waiting
    );
    assert!(
        context
            .worker
            .claim(
                WorkerInstanceId::new("idle").unwrap(),
                std::slice::from_ref(context.configuration)
            )
            .await
            .unwrap()
            .is_none(),
        "waiting releases execution authority without making the Job runnable"
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    context
        .backend
        .organizations(context.actor.tenant_id().clone())
        .answer(
            context.actor,
            AnswerApproval {
                request_id: RequestId::new("kernel-approve").unwrap(),
                approval_id: approval_id.clone(),
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    let second = claim(&context, "kernel-second").await;
    assert!(matches!(
        run(&context, &second, &factory).await,
        AdvanceOutcome::Progressed { .. }
    ));
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);

    let third = claim(&context, "kernel-third").await;
    let AdvanceOutcome::Waiting { waits, .. } = run(&context, &third, &factory).await else {
        panic!("operation wait")
    };
    let WaitTarget::Operation { operation_id } = &waits[0].target else {
        panic!("operation target")
    };
    assert_eq!(
        runtime
            .get(&context.actor.owner(), &job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Waiting
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while runtime
        .get(&context.actor.owner(), &job.id)
        .await
        .unwrap()
        .phase
        == JobPhase::Waiting
    {
        context.delivery.deliver_completions(128).await.unwrap();
        assert!(
            tokio::time::Instant::now() < deadline,
            "operation receipt did not wake its parent"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let fourth = claim(&context, "kernel-fourth").await;
    assert!(matches!(
        run(&context, &fourth, &factory).await,
        AdvanceOutcome::Progressed { .. }
    ));
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    let fifth = claim(&context, "kernel-fifth").await;
    let history = context
        .worker
        .persistence(&fifth, "/workspace".to_owned())
        .unwrap()
        .history(&TurnStateScope {
            owner: context.actor.owner(),
            session_id: session.id.to_string(),
        })
        .await
        .unwrap();
    let part = history
        .iter()
        .flat_map(|message| &message.parts)
        .find(|part| part.data.get("callID").and_then(Value::as_str) == Some("kernel-command"))
        .unwrap();
    assert_eq!(part.data["state"]["status"], "completed");
    let output: Value =
        serde_json::from_str(part.data["state"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(output["exitCode"], 0);
    assert_eq!(output["operationId"], operation_id.as_str());
    assert_eq!(
        output["output"],
        json!([{"channel":"stdout","text":"once\n"}])
    );
    assert!(part.data.get("completionID").is_some());
    assert!(matches!(
        run(&context, &fifth, &factory).await,
        AdvanceOutcome::Completed { .. }
    ));
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime
            .get(&context.actor.owner(), &job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Completed
    );
    assert_eq!(context.delivery.deliver_completions(128).await.unwrap(), 0);
    let environment = context
        .delivery
        .environments()
        .get(
            &context.actor.owner(),
            &EnvironmentId::new(session.id.as_str()).unwrap(),
        )
        .await
        .unwrap();
    context
        .delivery
        .environments()
        .release(
            &context.actor.owner(),
            &environment.spec.id,
            environment.revision,
        )
        .await
        .unwrap();
}
