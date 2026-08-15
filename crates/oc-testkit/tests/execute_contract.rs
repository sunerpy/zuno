//! The `execute` tool's model-facing parameter contract.
//!
//! This file is what survived the removal of the differential compatibility suite.
//! Fifteen of that suite's sixteen tests compared Zuno against a checked-out
//! `opencode` oracle tree, or verified the machinery that did the comparing. This
//! one does neither: it reads Zuno's **own** live JSON schema for the `execute`
//! tool and asserts it still matches the contract recorded in
//! `docs/divergences.toml`.
//!
//! That entry is worth keeping even now that upstream parity is not a goal, because
//! the property it protects is Zuno's, not upstream's: `execute` takes structured
//! sub-calls rather than a `code` string, and the model sees that schema. Changing
//! the parameter set silently changes what every model is told it may send. The
//! recorded `upstream_properties` is retained as provenance for *why* the surface
//! looks the way it does — the reason the divergence was taken in the first place.

use std::collections::BTreeSet;

use oc_testkit::{DivergenceList, divergence};

#[test]
fn the_execute_tools_live_schema_matches_its_divergence_entry() {
    let list = DivergenceList::load().expect("docs/divergences.toml must load");
    let entry = list
        .find(divergence::EXECUTE_CONTRACT_ID)
        .unwrap_or_else(|| {
            panic!(
                "{} must declare {:?}; this divergence is required to be verified, not merely \
                 mentioned",
                list.path().display(),
                divergence::EXECUTE_CONTRACT_ID
            )
        });
    let contract = entry
        .contract
        .as_ref()
        .expect("the execute entry must carry a [divergence.contract] table");

    let schema = oc_tool::schema::params_schema::<oc_tools::ExecuteParams>();
    let properties: Vec<String> = schema["properties"]
        .as_object()
        .expect("the execute schema must be an object schema")
        .keys()
        .cloned()
        .collect();
    let mut required: Vec<String> = schema["required"]
        .as_array()
        .expect("the execute schema must declare required properties")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("a required entry must be a string")
                .to_owned()
        })
        .collect();
    required.sort();

    assert_eq!(
        properties, contract.properties,
        "the `execute` tool's live top-level properties no longer match what \
         docs/divergences.toml declares. The model-facing contract changed; update the entry in \
         the same commit or revert the schema change."
    );
    assert_eq!(
        required, contract.required,
        "the `execute` tool's live required properties no longer match its divergence entry"
    );

    let subcall = oc_tool::schema::derive_params_schema::<oc_tools::batch::Subcall>();
    let subcall_properties: BTreeSet<String> = subcall["properties"]
        .as_object()
        .expect("the sub-call schema must be an object schema")
        .keys()
        .cloned()
        .collect();
    let declared: BTreeSet<String> = contract.subcall_properties.iter().cloned().collect();
    assert!(
        declared.is_subset(&subcall_properties),
        "the divergence entry declares sub-call control properties the live schema does not have: \
         {:?}",
        declared.difference(&subcall_properties).collect::<Vec<_>>()
    );

    assert_eq!(
        contract.upstream_properties,
        ["code"],
        "the entry records `{{ code: string }}` as the contract this surface diverged from; that \
         provenance is why the divergence exists and must keep being stated"
    );
    assert!(
        !properties.contains(&"code".to_owned()),
        "if `execute` grew a `code` parameter the divergence would no longer exist and the entry \
         must be removed"
    );
    eprintln!(
        "execute-parameter-contract: recorded-origin={:?} live={:?} required={:?}",
        contract.upstream_properties, properties, required
    );
}
