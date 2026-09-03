use async_trait::async_trait;
use std::collections::VecDeque;
use std::future::pending;
use std::sync::atomic::{AtomicBool, Ordering};
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
async fn repeated_shutdown_preserves_an_uncertain_outcome() {
    let runtime = HarnessRuntime::new("root");
    runtime.mount(FailingStop).await.expect("component mounts");
    runtime
        .unmount("failing-stop")
        .await
        .expect_err("first stop is uncertain");
    let before = runtime.snapshot();

    let error = runtime
        .shutdown()
        .await
        .expect_err("uncertainty cannot be upgraded to closed");

    assert!(error.to_string().contains("stop rejected"));
    let after = runtime.snapshot();
    assert_eq!(after.state, LifecycleState::Uncertain);
    assert_eq!(after.components, before.components);
    assert_eq!(after.diagnostics, before.diagnostics);
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
async fn parent_recomposition_rejects_live_child_consumers() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let root = HarnessRuntime::new("root");
    root.mount(greeting("provider", "old", &log))
        .await
        .expect("parent provider");
    let child = root.child("child");
    child
        .mount(summary("consumer", &log))
        .await
        .expect("child consumer");
    let before = log.lock().expect("lifecycle log").clone();

    let error = root
        .replace(greeting("provider", "new", &log))
        .await
        .expect_err("a live child would retain the old provider");

    assert!(error.to_string().contains("live child scope"));
    assert_eq!(
        root.service::<dyn Greeting>().expect("old provider").text(),
        "old"
    );
    assert_eq!(
        child
            .service::<dyn Summary>()
            .expect("old child consumer")
            .greeting(),
        "old"
    );
    assert_eq!(*log.lock().expect("lifecycle log"), before);

    child.shutdown().await.expect("child stops");
    root.replace(greeting("provider", "new", &log))
        .await
        .expect("parent may replace after child quiesces");
    assert_eq!(
        root.service::<dyn Greeting>().expect("new provider").text(),
        "new"
    );
}

#[tokio::test]
async fn an_uncertain_child_keeps_its_parent_uncertain() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let root = HarnessRuntime::new("root");
    root.mount(greeting("parent", "parent", &log))
        .await
        .expect("parent mounts");
    let child = root.child("child");
    child.mount(FailingStop).await.expect("child mounts");

    let error = root
        .shutdown()
        .await
        .expect_err("the descendant did not prove quiescence");

    assert!(error.to_string().contains("stop rejected"));
    assert_eq!(child.snapshot().state, LifecycleState::Uncertain);
    let parent = root.snapshot();
    assert_eq!(parent.state, LifecycleState::Uncertain);
    assert!(parent.components.is_empty());
    assert!(
        parent
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("stop rejected"))
    );
    assert_eq!(
        *log.lock().expect("lifecycle log"),
        ["start:parent", "stop:parent"]
    );
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

type Script = Arc<Mutex<VecDeque<Result<(), &'static str>>>>;

/// A component whose effect start and stop outcomes are scripted per call, so a test
/// can make exactly one transition of a multi-step recovery fail.
struct Scripted {
    id: &'static str,
    starts: Script,
    stops: Script,
}

fn scripted(
    id: &'static str,
    starts: Vec<Result<(), &'static str>>,
    stops: Vec<Result<(), &'static str>>,
) -> Scripted {
    Scripted {
        id,
        starts: Arc::new(Mutex::new(starts.into())),
        stops: Arc::new(Mutex::new(stops.into())),
    }
}

#[async_trait]
impl Component for Scripted {
    fn id(&self) -> &str {
        self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let starts = Arc::clone(&self.starts);
        let stops = Arc::clone(&self.stops);
        context.effect("scripted", move || async move {
            let next_start = starts.lock().expect("start script").pop_front();
            if let Some(Err(message)) = next_start {
                return Err(EffectError::new(message));
            }
            Ok::<_, EffectError>(move || async move {
                let next_stop = stops.lock().expect("stop script").pop_front();
                match next_stop {
                    Some(Err(message)) => Err(EffectError::new(message)),
                    Some(Ok(())) | None => Ok(()),
                }
            })
        })
    }
}

struct SlowStop {
    reclaimed: Arc<AtomicBool>,
    delay: Duration,
}

#[async_trait]
impl Component for SlowStop {
    fn id(&self) -> &str {
        "slow-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let reclaimed = Arc::clone(&self.reclaimed);
        let delay = self.delay;
        context.effect("process-tree", move || async move {
            Ok::<_, EffectError>(move || async move {
                tokio::time::sleep(delay).await;
                reclaimed.store(true, Ordering::SeqCst);
                Ok(())
            })
        })
    }
}

#[tokio::test]
async fn a_failed_restore_whose_cleanup_fails_is_uncertain_not_failed() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    // p1 stops cleanly when the candidate is rolled back and fails when the restore is.
    let p1 = scripted(
        "p1",
        vec![],
        vec![Ok(()), Ok(()), Err("p1 disposer lost its child")],
    );
    // p2 starts for the profile and for the candidate, then refuses the restore's start.
    let p2 = scripted(
        "p2",
        vec![Ok(()), Ok(()), Err("p2 cannot start again")],
        vec![],
    );
    runtime
        .activate_profile(
            HarnessProfile::new("base").with_bundle(
                ProfileBundle::new("core")
                    .with_component(p1)
                    .with_component(p2),
            ),
        )
        .await
        .expect("base profile activates");

    let error = runtime
        .mount(FailingStart { id: "candidate" })
        .await
        .expect_err("the candidate fails to start and the restore fails too");

    assert!(
        matches!(error, RuntimeError::RestoreFailed { .. }),
        "{error:?}"
    );
    let snapshot = runtime.snapshot();
    assert_eq!(
        snapshot.state,
        LifecycleState::Uncertain,
        "p1's disposer failed during the restore's rollback, so an effect may still be \
         live; reporting a clean failure hides that: {:#?}",
        snapshot.diagnostics
    );
    assert!(
        snapshot
            .components
            .iter()
            .all(|component| component.state == LifecycleState::Uncertain),
        "{:#?}",
        snapshot.components
    );
    assert!(
        snapshot.diagnostics.iter().any(|diagnostic| {
            diagnostic.component_id == "p1"
                && diagnostic.phase == LifecyclePhase::Stop
                && diagnostic.kind == LifecycleFailureKind::Uncertain
        }),
        "the cleanup failure is retained as evidence: {:#?}",
        snapshot.diagnostics
    );
    assert!(
        snapshot.diagnostics.iter().any(|diagnostic| {
            diagnostic.component_id == "p2" && diagnostic.phase == LifecyclePhase::Restore
        }),
        "the restore failure is retained as evidence: {:#?}",
        snapshot.diagnostics
    );
    assert_eq!(
        runtime
            .mount(greeting("later", "later", &log))
            .await
            .expect_err("an uncertain runtime accepts no composition change"),
        RuntimeError::NotOperational(LifecycleState::Uncertain)
    );
}

#[tokio::test(start_paused = true)]
async fn an_overrunning_disposer_is_reported_but_still_runs_to_completion() {
    let reclaimed = Arc::new(AtomicBool::new(false));
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default().with_stop_timeout(Duration::from_millis(25)),
    );
    runtime
        .mount(SlowStop {
            reclaimed: Arc::clone(&reclaimed),
            delay: Duration::from_millis(100),
        })
        .await
        .expect("component mounts");

    let error = runtime
        .unmount("slow-stop")
        .await
        .expect_err("an overrun is reported, not hidden");

    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        !reclaimed.load(Ordering::SeqCst),
        "the budget elapsed before the reclaim finished"
    );
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.state, LifecycleState::Uncertain);
    assert_eq!(snapshot.diagnostics[0].kind, LifecycleFailureKind::TimedOut);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        reclaimed.load(Ordering::SeqCst),
        "the runtime cancelled the disposer at its budget instead of letting it finish \
         reclaiming what it owns"
    );
}

struct PatientStop {
    delay: Duration,
    budget: zuno_runtime::StopBudget,
}

#[async_trait]
impl Component for PatientStop {
    fn id(&self) -> &str {
        "patient-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let delay = self.delay;
        context.effect("reaper", move || async move {
            Ok::<_, EffectError>(move || async move {
                tokio::time::sleep(delay).await;
                Ok(())
            })
        })
    }

    fn stop_budget(&self) -> zuno_runtime::StopBudget {
        self.budget
    }
}

#[tokio::test(start_paused = true)]
async fn a_component_declared_stop_budget_outranks_the_runtime_timeout() {
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default().with_stop_timeout(Duration::from_millis(25)),
    );
    runtime
        .mount(PatientStop {
            delay: Duration::from_secs(1),
            budget: zuno_runtime::StopBudget::Bounded(Duration::from_secs(2)),
        })
        .await
        .expect("component mounts");

    runtime
        .unmount("patient-stop")
        .await
        .expect("a disposer inside its own declared budget is not an overrun");

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.state, LifecycleState::Stopped);
    assert!(
        snapshot.diagnostics.is_empty(),
        "{:#?}",
        snapshot.diagnostics
    );
}

#[tokio::test(start_paused = true)]
async fn a_deployment_ceiling_bounds_a_component_declared_budget() {
    // The deployment is willing to wait 50 ms for any one disposer; the component
    // asks for two seconds. The wait is clamped and reported, and the disposer is
    // still left running to completion rather than cancelled.
    let reclaimed = Arc::new(AtomicBool::new(false));
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default()
            .with_stop_timeout(Duration::from_millis(25))
            .with_max_stop_timeout(Duration::from_millis(50)),
    );
    runtime
        .mount(CountedPatientStop {
            reclaimed: Arc::clone(&reclaimed),
            delay: Duration::from_millis(500),
            budget: zuno_runtime::StopBudget::Bounded(Duration::from_secs(2)),
        })
        .await
        .expect("component mounts");

    let error = runtime
        .unmount("patient-stop")
        .await
        .expect_err("the ceiling, not the declared budget, decides how long we wait");

    assert!(error.to_string().contains("timed out"), "{error}");
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.diagnostics[0].kind, LifecycleFailureKind::TimedOut);
    assert!(
        snapshot.diagnostics[0].message.contains("50 ms"),
        "the diagnostic must report the wait that actually elapsed: {}",
        snapshot.diagnostics[0].message
    );
    assert!(
        !reclaimed.load(Ordering::SeqCst),
        "the clamped wait elapsed before the reclaim finished"
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        reclaimed.load(Ordering::SeqCst),
        "clamping the wait must not cancel the disposer's own work"
    );
}

#[tokio::test(start_paused = true)]
async fn a_ceiling_above_the_declared_budget_changes_nothing() {
    let reclaimed = Arc::new(AtomicBool::new(false));
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default()
            .with_stop_timeout(Duration::from_millis(25))
            .with_max_stop_timeout(Duration::from_secs(30)),
    );
    runtime
        .mount(CountedPatientStop {
            reclaimed: Arc::clone(&reclaimed),
            delay: Duration::from_secs(1),
            budget: zuno_runtime::StopBudget::Bounded(Duration::from_secs(2)),
        })
        .await
        .expect("component mounts");

    runtime
        .unmount("patient-stop")
        .await
        .expect("a disposer inside both its budget and the ceiling is not an overrun");

    assert_eq!(runtime.snapshot().state, LifecycleState::Stopped);
    assert!(reclaimed.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_ceiling_also_bounds_components_that_declare_no_budget() {
    // `StopBudget::Runtime` resolves to the runtime timeout, and the ceiling bounds
    // that too, so one deployment value bounds every disposer's wait.
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default()
            .with_stop_timeout(Duration::from_secs(5))
            .with_max_stop_timeout(Duration::from_millis(20)),
    );
    runtime
        .mount(PatientStop {
            delay: Duration::from_secs(1),
            budget: zuno_runtime::StopBudget::Runtime,
        })
        .await
        .expect("component mounts");

    let error = runtime
        .unmount("patient-stop")
        .await
        .expect_err("the ceiling bounds the runtime default as well");

    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        runtime.snapshot().diagnostics[0].message.contains("20 ms"),
        "{:#?}",
        runtime.snapshot().diagnostics
    );
}

#[test]
#[should_panic(expected = "effect stop ceiling must be positive")]
fn a_zero_ceiling_is_refused_rather_than_read_as_no_wait() {
    let _ = RuntimeOptions::default().with_max_stop_timeout(Duration::ZERO);
}

#[test]
fn no_ceiling_is_the_default() {
    assert_eq!(RuntimeOptions::default().max_stop_timeout(), None);
    assert_eq!(
        RuntimeOptions::default()
            .with_max_stop_timeout(Duration::from_secs(9))
            .max_stop_timeout(),
        Some(Duration::from_secs(9))
    );
}

struct CountedPatientStop {
    reclaimed: Arc<AtomicBool>,
    delay: Duration,
    budget: zuno_runtime::StopBudget,
}

#[async_trait]
impl Component for CountedPatientStop {
    fn id(&self) -> &str {
        "patient-stop"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let delay = self.delay;
        let reclaimed = Arc::clone(&self.reclaimed);
        context.effect("reaper", move || async move {
            Ok::<_, EffectError>(move || async move {
                tokio::time::sleep(delay).await;
                reclaimed.store(true, Ordering::SeqCst);
                Ok(())
            })
        })
    }

    fn stop_budget(&self) -> zuno_runtime::StopBudget {
        self.budget
    }
}

#[tokio::test(start_paused = true)]
async fn a_zero_stop_budget_falls_back_to_the_runtime_timeout() {
    let runtime = HarnessRuntime::with_options(
        "root",
        RuntimeOptions::default().with_stop_timeout(Duration::from_millis(25)),
    );
    runtime
        .mount(PatientStop {
            delay: Duration::from_secs(1),
            budget: zuno_runtime::StopBudget::Bounded(Duration::ZERO),
        })
        .await
        .expect("component mounts");

    let error = runtime
        .unmount("patient-stop")
        .await
        .expect_err("a zero budget cannot prove quiescence and reads as the runtime default");

    assert!(error.to_string().contains("timed out"), "{error}");
    assert_eq!(
        runtime.snapshot().diagnostics[0].kind,
        LifecycleFailureKind::TimedOut
    );
}
