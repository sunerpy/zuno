use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use zuno_runtime::{
    Component, HarnessProfile, HarnessRuntime, MountContext, ProfileBundle, RuntimeError,
};

trait Greeting: Send + Sync {
    fn text(&self) -> &'static str;
}

struct GreetingService(&'static str);

impl Greeting for GreetingService {
    fn text(&self) -> &'static str {
        self.0
    }
}

struct GreetingComponent {
    id: &'static str,
    value: &'static str,
    cleanup: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Component for GreetingComponent {
    fn id(&self) -> &str {
        self.id
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService(self.value)))?;
        let id = self.id.to_owned();
        let cleanup = Arc::clone(&self.cleanup);
        context.on_close(move || async move {
            cleanup.lock().expect("cleanup log").push(id);
        });
        Ok(())
    }
}

struct FailingComponent {
    id: &'static str,
    cleanup: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Component for FailingComponent {
    fn id(&self) -> &str {
        self.id
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        context.provide::<dyn Greeting>(Arc::new(GreetingService("candidate")))?;
        let id = self.id.to_owned();
        let cleanup = Arc::clone(&self.cleanup);
        context.on_close(move || async move {
            cleanup.lock().expect("cleanup log").push(id);
        });
        Err(RuntimeError::Component("candidate rejected".to_owned()))
    }
}

struct RequiresGreeting;

#[async_trait]
impl Component for RequiresGreeting {
    fn id(&self) -> &str {
        "consumer"
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        let _greeting = context.require::<dyn Greeting>()?;
        Ok(())
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

struct SummaryComponent {
    cleanup: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Component for SummaryComponent {
    fn id(&self) -> &str {
        "summary"
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        let greeting = context.require::<dyn Greeting>()?;
        context.provide::<dyn Summary>(Arc::new(SummaryService(greeting.text())))?;
        let cleanup = Arc::clone(&self.cleanup);
        context.on_close(move || async move {
            cleanup
                .lock()
                .expect("cleanup log")
                .push("summary".to_owned());
        });
        Ok(())
    }
}

struct GateComponent {
    started: Arc<Notify>,
    release: Arc<Notify>,
    cleanup: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Component for GateComponent {
    fn id(&self) -> &str {
        "gate"
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        let cleanup = Arc::clone(&self.cleanup);
        context.on_close(move || async move {
            cleanup.lock().expect("cleanup log").push("gate".to_owned());
        });
        started.notify_one();
        release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn mounted_component_exposes_a_typed_trait_service() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");

    runtime
        .mount(GreetingComponent {
            id: "greeting",
            value: "hello",
            cleanup,
        })
        .await
        .expect("component mounts");

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("greeting service")
            .text(),
        "hello"
    );
}

#[tokio::test]
async fn child_scope_shadows_then_reveals_its_parent_service() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let root = HarnessRuntime::new("root");
    root.mount(GreetingComponent {
        id: "root-greeting",
        value: "root",
        cleanup: Arc::clone(&cleanup),
    })
    .await
    .expect("root component mounts");
    let child = root.child("session");
    child
        .mount(GreetingComponent {
            id: "session-greeting",
            value: "session",
            cleanup,
        })
        .await
        .expect("child component mounts");

    assert_eq!(
        child
            .service::<dyn Greeting>()
            .expect("child service")
            .text(),
        "session"
    );
    child
        .unmount("session-greeting")
        .await
        .expect("child component unmounts");
    assert_eq!(
        child
            .service::<dyn Greeting>()
            .expect("parent service is visible again")
            .text(),
        "root"
    );
}

#[tokio::test]
async fn failed_mount_rolls_back_staged_services_and_effects() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");

    let error = runtime
        .mount(FailingComponent {
            id: "candidate",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect_err("candidate must fail");

    assert_eq!(
        error,
        RuntimeError::Component("candidate rejected".to_owned())
    );
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert_eq!(*cleanup.lock().expect("cleanup log"), ["candidate"]);
}

#[tokio::test]
async fn failed_replace_leaves_the_previous_component_live() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(GreetingComponent {
            id: "greeting",
            value: "old",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect("old component mounts");

    runtime
        .replace(FailingComponent {
            id: "greeting",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect_err("candidate replacement must fail");

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("old service remains")
            .text(),
        "old"
    );
    assert_eq!(*cleanup.lock().expect("cleanup log"), ["greeting"]);
}

#[tokio::test]
async fn shutdown_cleans_components_in_reverse_mount_order_and_closes_the_scope() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    for id in ["first", "second", "third"] {
        runtime
            .mount(GreetingComponent {
                id,
                value: id,
                cleanup: Arc::clone(&cleanup),
            })
            .await
            .expect("component mounts");
    }

    runtime.shutdown().await.expect("runtime shuts down");

    assert_eq!(
        *cleanup.lock().expect("cleanup log"),
        ["third", "second", "first"]
    );
    assert_eq!(
        runtime.mount(RequiresGreeting).await,
        Err(RuntimeError::Closed)
    );
}

#[tokio::test]
async fn missing_required_service_fails_at_mount_time() {
    let runtime = HarnessRuntime::new("root");

    let error = runtime
        .mount(RequiresGreeting)
        .await
        .expect_err("missing service must reject the component");

    assert!(matches!(error, RuntimeError::MissingService(name) if name.contains("Greeting")));
}

#[tokio::test]
async fn successful_replace_publishes_the_candidate_then_cleans_the_previous_component() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(GreetingComponent {
            id: "greeting",
            value: "old",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect("old component mounts");

    runtime
        .replace(GreetingComponent {
            id: "greeting",
            value: "new",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect("replacement mounts");

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("new service is live")
            .text(),
        "new"
    );
    assert_eq!(*cleanup.lock().expect("cleanup log"), ["greeting"]);
    runtime.shutdown().await.expect("runtime shuts down");
    assert_eq!(
        *cleanup.lock().expect("cleanup log"),
        ["greeting", "greeting"]
    );
}

#[tokio::test]
async fn unmount_reveals_the_previous_provider_in_the_same_scope() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    for (id, value) in [("first", "one"), ("second", "two")] {
        runtime
            .mount(GreetingComponent {
                id,
                value,
                cleanup: Arc::clone(&cleanup),
            })
            .await
            .expect("component mounts");
    }

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("latest provider")
            .text(),
        "two"
    );
    runtime.unmount("second").await.expect("provider unmounts");
    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("previous provider")
            .text(),
        "one"
    );
}

#[tokio::test]
async fn duplicate_component_ids_fail_before_the_candidate_mounts() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .mount(GreetingComponent {
            id: "greeting",
            value: "old",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect("first component mounts");

    let error = runtime
        .mount(GreetingComponent {
            id: "greeting",
            value: "new",
            cleanup,
        })
        .await
        .expect_err("duplicate component id must fail");

    assert_eq!(
        error,
        RuntimeError::DuplicateComponent("greeting".to_owned())
    );
    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("first provider remains")
            .text(),
        "old"
    );
}

#[tokio::test]
async fn parent_shutdown_closes_children_before_parent_components() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let root = HarnessRuntime::new("root");
    root.mount(GreetingComponent {
        id: "parent",
        value: "parent",
        cleanup: Arc::clone(&cleanup),
    })
    .await
    .expect("parent mounts");
    let child = root.child("child");
    child
        .mount(GreetingComponent {
            id: "child",
            value: "child",
            cleanup: Arc::clone(&cleanup),
        })
        .await
        .expect("child mounts");

    root.shutdown().await.expect("root shuts down");

    assert_eq!(*cleanup.lock().expect("cleanup log"), ["child", "parent"]);
    assert!(root.service::<dyn Greeting>().is_none());
    assert!(child.service::<dyn Greeting>().is_none());
    assert_eq!(
        child.mount(RequiresGreeting).await,
        Err(RuntimeError::Closed)
    );
}

#[tokio::test]
async fn dynamically_selected_components_use_the_same_lifecycle() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    let component: Arc<dyn Component> = Arc::new(GreetingComponent {
        id: "dynamic",
        value: "dynamic",
        cleanup,
    });

    runtime
        .mount_shared(component)
        .await
        .expect("dynamic component mounts");

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("dynamic provider")
            .text(),
        "dynamic"
    );
}

#[tokio::test]
async fn a_profile_stages_cross_bundle_dependencies_without_exposing_partial_services() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .activate_profile(HarnessProfile::new("old").with_bundle(
            ProfileBundle::new("core").with_component(GreetingComponent {
                id: "greeting",
                value: "old",
                cleanup: Arc::clone(&cleanup),
            }),
        ))
        .await
        .expect("old profile activates");

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let started_wait = started.notified();
    let candidate = HarnessProfile::new("new")
        .with_bundle(
            ProfileBundle::new("core").with_component(GreetingComponent {
                id: "greeting",
                value: "new",
                cleanup: Arc::clone(&cleanup),
            }),
        )
        .with_bundle(
            ProfileBundle::new("features")
                .with_component(SummaryComponent {
                    cleanup: Arc::clone(&cleanup),
                })
                .with_component(GateComponent {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                    cleanup: Arc::clone(&cleanup),
                }),
        );
    let activating = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.activate_profile(candidate).await })
    };
    started_wait.await;

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("old greeting stays visible while staging")
            .text(),
        "old"
    );
    assert!(
        runtime.service::<dyn Summary>().is_none(),
        "candidate-only services are not published before commit"
    );

    release.notify_one();
    activating
        .await
        .expect("activation task")
        .expect("candidate activates");
    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("new greeting")
            .text(),
        "new"
    );
    assert_eq!(
        runtime
            .service::<dyn Summary>()
            .expect("summary resolved the candidate greeting")
            .greeting(),
        "new"
    );
    assert_eq!(runtime.active_profile_id().as_deref(), Some("new"));
    assert_eq!(*cleanup.lock().expect("cleanup log"), ["greeting"]);
}

#[tokio::test]
async fn a_failed_profile_rolls_back_every_candidate_and_keeps_the_previous_profile() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .activate_profile(HarnessProfile::new("old").with_bundle(
            ProfileBundle::new("core").with_component(GreetingComponent {
                id: "greeting",
                value: "old",
                cleanup: Arc::clone(&cleanup),
            }),
        ))
        .await
        .expect("old profile activates");

    let error = runtime
        .activate_profile(
            HarnessProfile::new("candidate").with_bundle(
                ProfileBundle::new("core")
                    .with_component(GreetingComponent {
                        id: "candidate-greeting",
                        value: "new",
                        cleanup: Arc::clone(&cleanup),
                    })
                    .with_component(FailingComponent {
                        id: "candidate-failure",
                        cleanup: Arc::clone(&cleanup),
                    }),
            ),
        )
        .await
        .expect_err("candidate profile fails");

    assert_eq!(
        error,
        RuntimeError::Component("candidate rejected".to_owned())
    );
    assert_eq!(runtime.active_profile_id().as_deref(), Some("old"));
    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("old greeting remains")
            .text(),
        "old"
    );
    assert_eq!(
        *cleanup.lock().expect("cleanup log"),
        ["candidate-failure", "candidate-greeting"]
    );
}

#[tokio::test]
async fn profile_replacement_cleans_the_previous_profile_in_reverse_component_order() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    runtime
        .activate_profile(
            HarnessProfile::new("old").with_bundle(
                ProfileBundle::new("core")
                    .with_component(GreetingComponent {
                        id: "first",
                        value: "first",
                        cleanup: Arc::clone(&cleanup),
                    })
                    .with_component(GreetingComponent {
                        id: "second",
                        value: "second",
                        cleanup: Arc::clone(&cleanup),
                    }),
            ),
        )
        .await
        .expect("old profile activates");

    runtime
        .activate_profile(HarnessProfile::new("new").with_bundle(
            ProfileBundle::new("core").with_component(GreetingComponent {
                id: "next",
                value: "next",
                cleanup: Arc::clone(&cleanup),
            }),
        ))
        .await
        .expect("new profile activates");

    assert_eq!(
        runtime
            .service::<dyn Greeting>()
            .expect("new profile service")
            .text(),
        "next"
    );
    assert_eq!(*cleanup.lock().expect("cleanup log"), ["second", "first"]);
}

#[tokio::test]
async fn duplicate_component_ids_across_bundles_fail_before_mounting_any_candidate() {
    let cleanup = Arc::new(Mutex::new(Vec::new()));
    let runtime = HarnessRuntime::new("root");
    let error = runtime
        .activate_profile(
            HarnessProfile::new("duplicate")
                .with_bundle(ProfileBundle::new("one").with_component(GreetingComponent {
                    id: "same",
                    value: "one",
                    cleanup: Arc::clone(&cleanup),
                }))
                .with_bundle(ProfileBundle::new("two").with_component(GreetingComponent {
                    id: "same",
                    value: "two",
                    cleanup: Arc::clone(&cleanup),
                })),
        )
        .await
        .expect_err("duplicate component ids fail");

    assert_eq!(error, RuntimeError::DuplicateComponent("same".to_owned()));
    assert!(runtime.service::<dyn Greeting>().is_none());
    assert!(cleanup.lock().expect("cleanup log").is_empty());
}
