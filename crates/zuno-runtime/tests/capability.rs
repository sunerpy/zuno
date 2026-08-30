use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use zuno_runtime::{
    CapabilityAvailability, CapabilityContract, CapabilityKey, CapabilityProvenance,
    CapabilityScope, CapabilityVersion, Component, EffectError, HarnessProfile, HarnessRuntime,
    PrepareContext, ProfileBundle, RuntimeError,
};

fn key(name: &str) -> CapabilityKey {
    CapabilityKey::new(
        "zuno.tool",
        name,
        CapabilityVersion::new(1, 0),
        CapabilityScope::new("profile").expect("valid capability scope"),
    )
    .expect("valid capability key")
}

fn contract(digest: &str) -> CapabilityContract {
    CapabilityContract::new("zuno.tool/v1", Some(digest)).expect("valid capability contract")
}

fn provenance(source: &str) -> CapabilityProvenance {
    CapabilityProvenance::new(source, None::<String>).expect("valid capability provenance")
}

struct Provider {
    id: &'static str,
    key: CapabilityKey,
    contract: CapabilityContract,
    starts: Option<Arc<AtomicUsize>>,
}

#[async_trait]
impl Component for Provider {
    fn id(&self) -> &str {
        self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide_capability(self.key.clone(), self.contract.clone(), provenance(self.id))?;
        if let Some(starts) = self.starts.as_ref().map(Arc::clone) {
            context.effect("probe", move || async move {
                starts.fetch_add(1, Ordering::SeqCst);
                Ok::<_, EffectError>(|| async { Ok(()) })
            })?;
        }
        Ok(())
    }
}

struct Consumer {
    key: CapabilityKey,
}

#[async_trait]
impl Component for Consumer {
    fn id(&self) -> &str {
        "consumer"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let _ = context.require_capability(&self.key)?;
        Ok(())
    }
}

struct WithdrawalProbe {
    runtime: HarnessRuntime,
    key: CapabilityKey,
    withdrawn_before_stop: Arc<AtomicBool>,
}

#[async_trait]
impl Component for WithdrawalProbe {
    fn id(&self) -> &str {
        "withdrawal-probe"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide_capability(
            self.key.clone(),
            contract("sha256:withdrawal"),
            provenance("withdrawal-fixture"),
        )?;
        let runtime = self.runtime.clone();
        let key = self.key.clone();
        let observed = Arc::clone(&self.withdrawn_before_stop);
        context.effect("lease", move || async move {
            Ok::<_, EffectError>(move || async move {
                observed.store(runtime.capability(&key).is_none(), Ordering::SeqCst);
                Ok(())
            })
        })
    }
}

struct FailingPrepareProvider {
    key: CapabilityKey,
}

#[async_trait]
impl Component for FailingPrepareProvider {
    fn id(&self) -> &str {
        "candidate"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide_capability(
            self.key.clone(),
            contract("sha256:candidate"),
            provenance("candidate"),
        )?;
        Err(RuntimeError::Component("candidate rejected".to_owned()))
    }
}

struct FailingStartProvider {
    key: CapabilityKey,
}

#[async_trait]
impl Component for FailingStartProvider {
    fn id(&self) -> &str {
        "provider"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide_capability(
            self.key.clone(),
            contract("sha256:candidate"),
            provenance("candidate"),
        )?;
        context.effect("start", || async {
            Err::<fn() -> std::future::Ready<Result<(), EffectError>>, _>(EffectError::new(
                "candidate start rejected",
            ))
        })
    }
}

#[test]
fn capability_identity_rejects_ambiguous_empty_fields() {
    assert!(CapabilityScope::new(" ").is_err());
    assert!(
        CapabilityKey::new(
            "",
            "search",
            CapabilityVersion::new(1, 0),
            CapabilityScope::new("profile").expect("scope"),
        )
        .is_err()
    );
    assert!(CapabilityContract::new("", None::<String>).is_err());
    assert!(CapabilityProvenance::new("", None::<String>).is_err());
}

#[tokio::test]
async fn named_capability_is_published_with_owner_contract_generation_and_provenance() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    let contract = contract("sha256:first");
    runtime
        .mount(Provider {
            id: "provider",
            key: key.clone(),
            contract: contract.clone(),
            starts: None,
        })
        .await
        .expect("provider mounts");

    let descriptor = runtime.capability(&key).expect("capability published");
    assert_eq!(descriptor.key(), &key);
    assert_eq!(descriptor.owner(), "provider");
    assert_eq!(descriptor.runtime_scope(), "root");
    assert_eq!(descriptor.generation(), 1);
    assert_eq!(descriptor.contract(), &contract);
    assert_eq!(descriptor.provenance(), &provenance("provider"));
    assert_eq!(descriptor.availability(), CapabilityAvailability::Available);
    assert!(runtime.capability_is_current(&descriptor));
    assert_eq!(runtime.snapshot().capabilities, [descriptor]);
}

#[tokio::test]
async fn duplicate_local_capabilities_fail_before_any_effect_starts() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    let starts = Arc::new(AtomicUsize::new(0));
    let error = runtime
        .activate_profile(
            HarnessProfile::new("duplicate").with_bundle(
                ProfileBundle::new("providers")
                    .with_component(Provider {
                        id: "first",
                        key: key.clone(),
                        contract: contract("sha256:first"),
                        starts: Some(Arc::clone(&starts)),
                    })
                    .with_component(Provider {
                        id: "second",
                        key: key.clone(),
                        contract: contract("sha256:second"),
                        starts: None,
                    }),
            ),
        )
        .await
        .expect_err("duplicate capability must fail");

    assert_eq!(error, RuntimeError::DuplicateCapability(key));
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert!(runtime.snapshot().capabilities.is_empty());
}

#[tokio::test]
async fn child_scope_shadows_then_reveals_the_parent_capability() {
    let root = HarnessRuntime::new("root");
    let key = key("search");
    root.mount(Provider {
        id: "root-provider",
        key: key.clone(),
        contract: contract("sha256:root"),
        starts: None,
    })
    .await
    .expect("root provider");
    let child = root.child("child");
    child
        .mount(Provider {
            id: "child-provider",
            key: key.clone(),
            contract: contract("sha256:child"),
            starts: None,
        })
        .await
        .expect("child provider");

    assert_eq!(
        child.capability(&key).expect("child capability").owner(),
        "child-provider"
    );
    child
        .unmount("child-provider")
        .await
        .expect("child provider unmounts");
    let revealed = child.capability(&key).expect("parent capability revealed");
    assert_eq!(revealed.owner(), "root-provider");
    assert_eq!(revealed.runtime_scope(), "root");

    child.shutdown().await.expect("child shuts down");
    root.shutdown().await.expect("root shuts down");
}

#[tokio::test]
async fn withdrawal_precedes_the_component_disposer() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    let observed = Arc::new(AtomicBool::new(false));
    runtime
        .mount(WithdrawalProbe {
            runtime: runtime.clone(),
            key: key.clone(),
            withdrawn_before_stop: Arc::clone(&observed),
        })
        .await
        .expect("probe mounts");

    runtime
        .unmount("withdrawal-probe")
        .await
        .expect("probe unmounts");

    assert!(observed.load(Ordering::SeqCst));
    assert!(runtime.capability(&key).is_none());
}

#[tokio::test]
async fn replacement_advances_generation_and_invalidates_the_old_descriptor() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    runtime
        .mount(Provider {
            id: "provider",
            key: key.clone(),
            contract: contract("sha256:first"),
            starts: None,
        })
        .await
        .expect("first provider");
    let first = runtime.capability(&key).expect("first descriptor");

    runtime
        .replace(Provider {
            id: "provider",
            key: key.clone(),
            contract: contract("sha256:second"),
            starts: None,
        })
        .await
        .expect("provider replaced");
    let second = runtime.capability(&key).expect("second descriptor");

    assert_eq!(first.generation(), 1);
    assert_eq!(second.generation(), 2);
    assert!(!runtime.capability_is_current(&first));
    assert!(runtime.capability_is_current(&second));
    assert_eq!(second.contract(), &contract("sha256:second"));
}

#[tokio::test]
async fn failed_prepare_keeps_the_current_descriptor_and_generation() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    runtime
        .mount(Provider {
            id: "provider",
            key: key.clone(),
            contract: contract("sha256:stable"),
            starts: None,
        })
        .await
        .expect("stable provider");
    let before = runtime.capability(&key).expect("stable descriptor");

    runtime
        .mount(FailingPrepareProvider { key: key.clone() })
        .await
        .expect_err("candidate prepare fails");

    let after = runtime.capability(&key).expect("stable descriptor remains");
    assert_eq!(after, before);
    assert!(runtime.capability_is_current(&before));
}

#[tokio::test]
async fn failed_start_restores_the_previous_provider_with_a_fresh_generation() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    runtime
        .mount(Provider {
            id: "provider",
            key: key.clone(),
            contract: contract("sha256:stable"),
            starts: None,
        })
        .await
        .expect("stable provider");
    let before = runtime.capability(&key).expect("first descriptor");

    runtime
        .replace(FailingStartProvider { key: key.clone() })
        .await
        .expect_err("candidate start fails");

    let restored = runtime.capability(&key).expect("provider restored");
    assert_eq!(restored.contract(), &contract("sha256:stable"));
    assert_eq!(restored.generation(), before.generation() + 1);
    assert!(!runtime.capability_is_current(&before));
    assert!(runtime.capability_is_current(&restored));
}

#[tokio::test]
async fn named_requirement_records_the_observed_provider_generation() {
    let runtime = HarnessRuntime::new("root");
    let key = key("search");
    runtime
        .activate_profile(
            HarnessProfile::new("required").with_bundle(
                ProfileBundle::new("capabilities")
                    .with_component(Provider {
                        id: "provider",
                        key: key.clone(),
                        contract: contract("sha256:stable"),
                        starts: None,
                    })
                    .with_component(Consumer { key: key.clone() }),
            ),
        )
        .await
        .expect("profile activates");

    let snapshot = runtime.snapshot();
    let consumer = snapshot
        .components
        .iter()
        .find(|component| component.id == "consumer")
        .expect("consumer snapshot");
    assert_eq!(
        consumer.requires,
        [format!("{} generation 1 <- provider", key)]
    );
}
