use async_trait::async_trait;
use std::future::pending;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use zuno_runtime::{
    Component, EffectError, HarnessProfile, HarnessRuntime, LifecycleFailureKind, LifecyclePhase,
    LifecycleState, PrepareContext, ProfileBundle, RuntimeError, RuntimeOptions,
};

type Log = Arc<Mutex<Vec<String>>>;

trait Greeting: Send + Sync {
    fn text(&self) -> &'static str;
}

struct GreetingService(&'static str);

impl Greeting for GreetingService {
    fn text(&self) -> &'static str {
        self.0
    }
}

trait Summary: Send + Sync {
    fn greeting(&self) -> &'static str;
}

struct SummaryService(&'static str);

impl Summary for SummaryService {
    fn greeting(&self) -> &'static str {
        self.0
    }
}

struct GreetingComponent {
    id: &'static str,
    value: &'static str,
    log: Log,
}

#[async_trait]
impl Component for GreetingComponent {
    fn id(&self) -> &str {
        self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService(self.value)))?;
        let id = self.id.to_owned();
        let start_log = Arc::clone(&self.log);
        context.effect("lifecycle", move || async move {
            start_log
                .lock()
                .expect("lifecycle log")
                .push(format!("start:{id}"));
            let stop_id = id.clone();
            let stop_log = Arc::clone(&start_log);
            Ok::<_, EffectError>(move || async move {
                stop_log
                    .lock()
                    .expect("lifecycle log")
                    .push(format!("stop:{stop_id}"));
                Ok(())
            })
        })
    }
}

struct SummaryComponent {
    id: &'static str,
    log: Log,
}

#[async_trait]
impl Component for SummaryComponent {
    fn id(&self) -> &str {
        self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let greeting = context.require::<dyn Greeting>()?;
        context.provide::<dyn Summary>(Arc::new(SummaryService(greeting.text())))?;
        let id = self.id.to_owned();
        let start_log = Arc::clone(&self.log);
        context.effect("lifecycle", move || async move {
            start_log
                .lock()
                .expect("lifecycle log")
                .push(format!("start:{id}"));
            let stop_id = id.clone();
            let stop_log = Arc::clone(&start_log);
            Ok::<_, EffectError>(move || async move {
                stop_log
                    .lock()
                    .expect("lifecycle log")
                    .push(format!("stop:{stop_id}"));
                Ok(())
            })
        })
    }
}

struct FailingPrepare {
    started: Arc<Mutex<bool>>,
}

#[async_trait]
impl Component for FailingPrepare {
    fn id(&self) -> &str {
        "failing-prepare"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let started = Arc::clone(&self.started);
        context.effect("must-not-start", move || async move {
            *started.lock().expect("started flag") = true;
            Ok::<_, EffectError>(|| async { Ok(()) })
        })?;
        Err(RuntimeError::Component("candidate rejected".to_owned()))
    }
}

struct BlockingPrepare {
    prepared: Arc<Notify>,
    release: Arc<Notify>,
    started: Arc<Mutex<bool>>,
}

#[async_trait]
impl Component for BlockingPrepare {
    fn id(&self) -> &str {
        "blocking-prepare"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("candidate")))?;
        let started = Arc::clone(&self.started);
        context.effect("deferred", move || async move {
            *started.lock().expect("started flag") = true;
            Ok::<_, EffectError>(|| async { Ok(()) })
        })?;
        self.prepared.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

struct BlockingStart {
    start_entered: Arc<Notify>,
    start_release: Arc<Notify>,
}

#[async_trait]
impl Component for BlockingStart {
    fn id(&self) -> &str {
        "blocking-start"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("candidate")))?;
        let entered = Arc::clone(&self.start_entered);
        let release = Arc::clone(&self.start_release);
        context.effect("blocking", move || async move {
            entered.notify_one();
            release.notified().await;
            Ok::<_, EffectError>(|| async { Ok(()) })
        })
    }
}

struct FailingStart {
    id: &'static str,
}

#[async_trait]
impl Component for FailingStart {
    fn id(&self) -> &str {
        self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("candidate")))?;
        context.effect("start", || async {
            Err::<fn() -> std::future::Ready<Result<(), EffectError>>, _>(EffectError::new(
                "start rejected",
            ))
        })
    }
}

struct BlockingStop {
    stop_entered: Arc<Notify>,
    stop_release: Arc<Notify>,
}

#[async_trait]
impl Component for BlockingStop {
    fn id(&self) -> &str {
        "blocking-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("blocking")))?;
        let entered = Arc::clone(&self.stop_entered);
        let release = Arc::clone(&self.stop_release);
        context.effect("worker", move || async move {
            Ok::<_, EffectError>(move || async move {
                entered.notify_one();
                release.notified().await;
                Ok(())
            })
        })
    }
}

struct HangingStop;

#[async_trait]
impl Component for HangingStop {
    fn id(&self) -> &str {
        "hanging-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("hanging")))?;
        context.effect("worker", || async {
            Ok::<_, EffectError>(|| async {
                pending::<()>().await;
                Ok(())
            })
        })
    }
}

struct FailingStop;

#[async_trait]
impl Component for FailingStop {
    fn id(&self) -> &str {
        "failing-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("failing")))?;
        context.effect("worker", || async {
            Ok::<_, EffectError>(|| async { Err(EffectError::new("stop rejected")) })
        })
    }
}

fn greeting(id: &'static str, value: &'static str, log: &Log) -> GreetingComponent {
    GreetingComponent {
        id,
        value,
        log: Arc::clone(log),
    }
}

fn summary(id: &'static str, log: &Log) -> SummaryComponent {
    SummaryComponent {
        id,
        log: Arc::clone(log),
    }
}

#[tokio::test]
async fn prepare_is_side_effect_free_and_candidate_services_stay_hidden() {
    let prepared = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let started = Arc::new(Mutex::new(false));
    let runtime = HarnessRuntime::new("root");
    let preparing = {
        let runtime = runtime.clone();
        let component = BlockingPrepare {
            prepared: Arc::clone(&prepared),
            release: Arc::clone(&release),
            started: Arc::clone(&started),
        };
        tokio::spawn(async move { runtime.mount(component).await })
    };

    prepared.notified().await;
    assert!(!*started.lock().expect("started flag"));
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert_eq!(runtime.snapshot().state, LifecycleState::Preparing);

    release.notify_one();
    preparing
        .await
        .expect("mount task")
        .expect("component mounts");
    assert!(*started.lock().expect("started flag"));
    assert_eq!(
        runtime.service::<dyn Greeting>().expect("service").text(),
        "candidate"
    );
}

#[tokio::test]
async fn a_prepare_failure_drops_unstarted_effects() {
    let started = Arc::new(Mutex::new(false));
    let runtime = HarnessRuntime::new("root");

    let error = runtime
        .mount(FailingPrepare {
            started: Arc::clone(&started),
        })
        .await
        .expect_err("prepare must fail");

    assert_eq!(
        error,
        RuntimeError::Component("candidate rejected".to_owned())
    );
    assert!(!*started.lock().expect("started flag"));
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert_eq!(runtime.snapshot().state, LifecycleState::Stopped);
}

#[tokio::test]
async fn candidate_services_are_not_published_until_effect_start_finishes() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = HarnessRuntime::new("root");
    let mounting = {
        let runtime = runtime.clone();
        let component = BlockingStart {
            start_entered: Arc::clone(&entered),
            start_release: Arc::clone(&release),
        };
        tokio::spawn(async move { runtime.mount(component).await })
    };

    entered.notified().await;
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert_eq!(runtime.snapshot().state, LifecycleState::Preparing);

    release.notify_one();
    mounting.await.expect("mount task").expect("mount succeeds");
    assert_eq!(
        runtime.service::<dyn Greeting>().expect("service").text(),
        "candidate"
    );
}

#[tokio::test]
async fn replacement_stops_the_old_effect_before_starting_the_candidate() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(greeting("greeting", "old", &log))
        .await
        .expect("old mounts");
    runtime
        .replace(greeting("greeting", "new", &log))
        .await
        .expect("replacement mounts");

    assert_eq!(
        *log.lock().expect("lifecycle log"),
        ["start:greeting", "stop:greeting", "start:greeting"]
    );
    assert_eq!(
        runtime.service::<dyn Greeting>().expect("service").text(),
        "new"
    );
}

#[tokio::test]
async fn a_failed_candidate_start_restores_the_previous_composition() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(greeting("greeting", "old", &log))
        .await
        .expect("old mounts");

    let error = runtime
        .replace(FailingStart { id: "greeting" })
        .await
        .expect_err("candidate start fails");

    assert!(error.to_string().contains("start rejected"));
    assert_eq!(
        runtime.service::<dyn Greeting>().expect("restored").text(),
        "old"
    );
    assert_eq!(
        *log.lock().expect("lifecycle log"),
        ["start:greeting", "stop:greeting", "start:greeting"]
    );
    assert_eq!(runtime.snapshot().state, LifecycleState::Active);
}

#[tokio::test]
async fn replacement_reprepares_consumers_against_the_new_provider() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(greeting("greeting", "old", &log))
        .await
        .expect("provider mounts");
    runtime
        .mount(summary("summary", &log))
        .await
        .expect("consumer mounts");
    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "old"
    );

    runtime
        .replace(greeting("greeting", "new", &log))
        .await
        .expect("provider replaces");

    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "new"
    );
    assert_eq!(
        *log.lock().expect("lifecycle log"),
        [
            "start:greeting",
            "stop:greeting",
            "start:greeting",
            "start:summary",
            "stop:summary",
            "stop:greeting",
            "start:greeting",
            "start:summary",
        ]
    );
}

#[tokio::test]
async fn unmount_reprepares_consumers_against_the_revealed_provider() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(greeting("first", "one", &log))
        .await
        .expect("first provider");
    runtime
        .mount(greeting("second", "two", &log))
        .await
        .expect("second provider");
    runtime
        .mount(summary("summary", &log))
        .await
        .expect("consumer");
    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "two"
    );

    runtime.unmount("second").await.expect("second unmounts");

    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "one"
    );
}

#[tokio::test]
async fn stop_waits_for_quiescence_and_projects_stopping_state() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(BlockingStop {
            stop_entered: Arc::clone(&entered),
            stop_release: Arc::clone(&release),
        })
        .await
        .expect("component mounts");
    let stopping = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.unmount("blocking-stop").await })
    };

    entered.notified().await;
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.state, LifecycleState::Stopping);
    assert_eq!(snapshot.components[0].state, LifecycleState::Stopping);
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert!(!stopping.is_finished());

    release.notify_one();
    stopping
        .await
        .expect("unmount task")
        .expect("component stops");
    assert_eq!(runtime.snapshot().state, LifecycleState::Stopped);
}

#[tokio::test]
async fn a_hanging_stop_is_bounded_and_becomes_uncertain() {
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default().with_stop_timeout(Duration::from_millis(25)),
    );
    runtime.mount(HangingStop).await.expect("component mounts");

    let error = runtime
        .unmount("hanging-stop")
        .await
        .expect_err("timeout is not a successful unmount");

    assert!(error.to_string().contains("timed out"));
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.state, LifecycleState::Uncertain);
    assert_eq!(snapshot.components[0].state, LifecycleState::Uncertain);
    assert_eq!(snapshot.diagnostics.len(), 1);
    assert_eq!(snapshot.diagnostics[0].kind, LifecycleFailureKind::TimedOut);
    assert_eq!(snapshot.diagnostics[0].phase, LifecyclePhase::Stop);
    assert!(runtime.service::<dyn Greeting>().is_none());
}

#[tokio::test]
async fn an_explicit_stop_failure_becomes_uncertain() {
    let runtime = HarnessRuntime::new("root");
    runtime.mount(FailingStop).await.expect("component mounts");

    let error = runtime
        .unmount("failing-stop")
        .await
        .expect_err("failed disposer is not stopped");

    assert!(error.to_string().contains("stop rejected"));
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.state, LifecycleState::Uncertain);
    assert_eq!(
        snapshot.diagnostics[0].kind,
        LifecycleFailureKind::Uncertain
    );
}

#[tokio::test]
async fn profile_replacement_reprepares_cross_bundle_dependencies() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .activate_profile(
            HarnessProfile::new("old")
                .with_bundle(
                    ProfileBundle::new("provider")
                        .with_component(greeting("greeting", "old", &log)),
                )
                .with_bundle(
                    ProfileBundle::new("consumer").with_component(summary("summary", &log)),
                ),
        )
        .await
        .expect("old profile");
    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "old"
    );

    runtime
        .activate_profile(
            HarnessProfile::new("new")
                .with_bundle(
                    ProfileBundle::new("provider")
                        .with_component(greeting("greeting", "new", &log)),
                )
                .with_bundle(
                    ProfileBundle::new("consumer").with_component(summary("summary", &log)),
                ),
        )
        .await
        .expect("new profile");

    assert_eq!(runtime.active_profile_id().as_deref(), Some("new"));
    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary")
            .greeting(),
        "new"
    );
}

#[tokio::test]
async fn failed_profile_prepare_keeps_the_previous_profile_live() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(Mutex::new(false));
    let runtime = HarnessRuntime::new("root");
    runtime
        .activate_profile(HarnessProfile::new("old").with_bundle(
            ProfileBundle::new("core").with_component(greeting("greeting", "old", &log)),
        ))
        .await
        .expect("old profile");

    runtime
        .activate_profile(
            HarnessProfile::new("candidate").with_bundle(
                ProfileBundle::new("core")
                    .with_component(greeting("candidate", "new", &log))
                    .with_component(FailingPrepare {
                        started: Arc::clone(&started),
                    }),
            ),
        )
        .await
        .expect_err("candidate profile prepare fails");

    assert!(!*started.lock().expect("started flag"));
    assert_eq!(runtime.active_profile_id().as_deref(), Some("old"));
    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("old greeting")
            .text(),
        "old"
    );
}

#[tokio::test]
async fn parent_shutdown_closes_children_before_parent_components() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let root = HarnessRuntime::new("root");
    root.mount(greeting("parent", "parent", &log))
        .await
        .expect("parent mounts");
    let child = root.child("child");
    child
        .mount(greeting("child", "child", &log))
        .await
        .expect("child mounts");

    root.shutdown().await.expect("root shuts down");

    assert_eq!(
        *log.lock().expect("lifecycle log"),
        ["start:parent", "start:child", "stop:child", "stop:parent"]
    );
    assert_eq!(root.snapshot().state, LifecycleState::Closed);
    assert_eq!(child.snapshot().state, LifecycleState::Closed);
    assert_eq!(root.mount(FailingStop).await, Err(RuntimeError::Closed));
    root.shutdown().await.expect("repeated shutdown is a no-op");
}

#[tokio::test]
async fn duplicate_component_ids_fail_before_prepare() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(greeting("same", "one", &log))
        .await
        .expect("first mounts");

    let error = runtime
        .mount(greeting("same", "two", &log))
        .await
        .expect_err("duplicate id fails");

    assert_eq!(error, RuntimeError::DuplicateComponent("same".to_owned()));
    assert_eq!(
        runtime.service::<dyn Greeting>().expect("first").text(),
        "one"
    );
}

#[tokio::test]
async fn duplicate_profile_component_ids_fail_without_starting_effects() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");

    let error = runtime
        .activate_profile(
            HarnessProfile::new("duplicate")
                .with_bundle(
                    ProfileBundle::new("one").with_component(greeting("same", "one", &log)),
                )
                .with_bundle(
                    ProfileBundle::new("two").with_component(greeting("same", "two", &log)),
                ),
        )
        .await
        .expect_err("duplicate ids fail");

    assert_eq!(error, RuntimeError::DuplicateComponent("same".to_owned()));
    assert!(log.lock().expect("lifecycle log").is_empty());
}
