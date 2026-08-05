//! The differential self-test: does the harness actually detect a divergence?
//!
//! Everything else in this crate is scaffolding for one question. These tests run
//! the real `opencode` and this project's binary under one scripted environment,
//! confirm the harness reports *only* the difference that genuinely exists, then
//! inject a perturbation and confirm it is reported.
//!
//! These tests require the oracle. That is deliberate. This project's entire
//! premise is differential compatibility against `opencode` v1.18.13, so a machine
//! without the oracle cannot verify anything, and a harness that quietly skips
//! would report success for a verification it never performed. When the oracle is
//! absent the failure names the path and the remedy — which is itself covered, in
//! `oracle_absence_is_actionable`.

use oc_testkit::{
    DbChoice, Normalizer, Oracle, ScriptedEnv, Subject, TestkitError, diff_normalized, diff_runs,
};

/// A shared closed world, so neither side can see the developer's environment.
fn scripted() -> ScriptedEnv {
    ScriptedEnv::new()
        .expect("scripted env")
        .with_db(DbChoice::Memory)
}

#[test]
fn the_oracle_reports_which_version_it_actually_ran() {
    let oracle = Oracle::discover().expect("an oracle is required to verify anything");
    let version = oracle.reported_version();
    assert!(!version.is_empty(), "the oracle must state a version");

    let gap = oracle.version_gap();
    let described = gap.describe();
    assert!(described.contains(version), "{described}");

    // The gap is data, not an assumption. Whichever of these three is true on this
    // machine, the harness must be able to say which.
    let classified = gap.is_aligned() || gap.is_unversioned() || described.contains("version gap");
    assert!(
        classified,
        "the harness failed to classify the version gap: {described}"
    );

    let label = oracle.provenance().label();
    assert!(
        label.contains(version),
        "the label must carry the running version: {label}"
    );
    assert!(
        label.contains("installed-binary") || label.contains("from-source"),
        "the label must name the flavour: {label}"
    );

    println!("oracle: {label}");
    println!("version gap: {described}");
}

/// The happy QA scenario: run `--version` on both sides and report only the
/// expected known difference, which is that they are different products.
#[test]
fn a_version_differential_reports_only_the_expected_difference() {
    let env = scripted();
    let oracle = Oracle::discover()
        .expect("an oracle is required")
        .with_env(scripted());
    let mut subject = Subject::discover_or_build()
        .expect("the subject binary must be buildable")
        .with_env(env);
    subject.probe_version().expect("probe the subject version");

    let left = oracle.run(["--version"]).expect("run the oracle");
    let right = subject.run(["--version"]).expect("run the subject");

    println!("{}", left.render());
    println!("{}", right.render());

    let report = diff_runs(&left, &right, &Normalizer::default());
    println!("{}", report.render());

    // Exactly one difference, on the version line. Everything else must agree,
    // including exit status.
    assert_eq!(
        report.divergence_count(),
        1,
        "expected exactly the version-string difference:\n{}",
        report.render()
    );
    let divergence = &report.divergences[0];
    assert_eq!(
        divergence.left.as_deref(),
        Some(oracle.reported_version()),
        "the oracle side of the difference must be its version:\n{}",
        report.render()
    );

    // What the subject prints is the *measured* state of this project, not an
    // aspiration. At the time this harness landed, `oc-cli`'s entry point was still
    // the `fn main() {}` stub from todo 1, so `--version` produced nothing at all,
    // and this differential legitimately reduces to "the oracle states a version,
    // the subject states none". Both arms below are correct outcomes; the arms
    // exist so that implementing the CLI tightens this test instead of silently
    // passing under a looser assertion.
    match divergence.right.as_deref() {
        None => assert!(
            right.stdout.is_empty(),
            "the subject printed something the diff did not pair up:\n{}",
            report.render()
        ),
        Some(printed) => assert_ne!(
            printed,
            oracle.reported_version(),
            "the two products reported the same version string, which the diff should have \
             collapsed to zero divergences:\n{}",
            report.render()
        ),
    }

    assert!(left.is_success(), "the oracle failed: {}", left.render());
    assert!(
        right.is_success(),
        "the subject failed, which is a difference beyond the version string: {}",
        right.render()
    );
    assert!(
        report.render().contains("pinned source"),
        "the report must name the pinned oracle version:\n{}",
        report.render()
    );
}

/// The acceptance criterion: perturb the subject's output and confirm the harness
/// reports the divergence rather than absorbing it.
#[test]
fn an_injected_divergence_in_subject_output_is_reported() {
    let oracle = Oracle::discover().expect("an oracle is required");
    let baseline = oracle.run(["--version"]).expect("run the oracle");
    let normalizer = Normalizer::default();

    // Control: the oracle's real output compared against itself is identical, so a
    // reported divergence below cannot be noise.
    let control = diff_normalized(
        baseline.label(),
        &baseline.stdout,
        "subject(replayed oracle output)",
        &baseline.stdout,
        &normalizer,
    );
    assert!(
        control.is_identical(),
        "control diff was not clean:\n{}",
        control.render()
    );

    // One character, in the middle of a value the harness has no rule for.
    let perturbed = perturb(&baseline.stdout);
    assert_ne!(perturbed, baseline.stdout, "the perturbation did nothing");

    let report = diff_normalized(
        baseline.label(),
        &baseline.stdout,
        "subject(perturbed oracle output)",
        &perturbed,
        &normalizer,
    );
    println!("{}", report.render());
    assert!(
        !report.is_identical(),
        "the harness absorbed an injected divergence, which makes it useless:\n{}",
        report.render()
    );
    assert_eq!(report.divergence_count(), 1);
    assert_eq!(
        report.divergences[0].left.as_deref(),
        Some(baseline.stdout.trim_end())
    );
}

/// A perturbation the normalizer has no rule for: change a digit inside the
/// version string.
fn perturb(output: &str) -> String {
    let mut chars: Vec<char> = output.chars().collect();
    for c in &mut chars {
        if c.is_ascii_digit() {
            *c = if *c == '9' { '8' } else { '9' };
            break;
        }
    }
    chars.into_iter().collect()
}

/// A perturbation the harness *would* be entitled to mask must still be reported
/// when it is not in a masked position. This is the anti-widening check at the
/// integration level.
#[test]
fn a_volatile_looking_value_outside_a_masked_position_still_diverges() {
    let normalizer = Normalizer::default();
    let left = r#"{"port":4096,"host":"api.anthropic.com:443","createdAt":"2026-04-28T21:18:45Z"}"#;
    let right =
        r#"{"port":4097,"host":"api.anthropic.com:443","createdAt":"2026-08-05T09:00:00Z"}"#;
    let report = diff_normalized("oracle", left, "subject", right, &normalizer);
    assert!(
        !report.is_identical(),
        "a configured port difference must survive normalization:\n{}",
        report.render()
    );
    assert_eq!(report.rules_fired.get("iso8601-timestamp"), Some(&2));
    assert!(!report.rules_fired.contains_key("loopback-port"));
}

/// The failure QA scenario, at the integration level: an oracle that is not there
/// fails with a message naming the path it wanted.
#[test]
fn oracle_absence_is_actionable() {
    let missing = "/nonexistent/oc-testkit/oracle/bin/opencode";
    let err = Oracle::at_binary(missing).expect_err("a nonexistent oracle cannot be used");
    let rendered = err.to_string();
    println!("{rendered}");
    assert!(
        matches!(err, TestkitError::BinaryNotFound { role: "oracle", .. }),
        "{err:?}"
    );
    assert!(
        rendered.contains(missing),
        "the message must name the path: {rendered}"
    );
    assert!(
        rendered.contains("remedy:"),
        "the message must be actionable: {rendered}"
    );
}

/// The second oracle flavour has to work, not merely be offered. Running the pinned
/// tree directly is the only way to compare against 1.18.13's *code* rather than
/// against whatever release happens to be installed, so it is exercised here — and
/// its self-reported `local` version is asserted, because that is the property that
/// makes it unsuitable as the default.
#[test]
fn the_from_source_oracle_runs_the_pinned_tree_and_reports_local() {
    let tree = oc_testkit::oracle::locate_source_tree().expect("the pinned oracle tree");
    let oracle = Oracle::from_source(&tree)
        .expect("the pinned tree must be runnable; it has node_modules installed")
        .with_env(scripted());

    assert_eq!(oracle.flavour(), &oc_testkit::OracleFlavour::FromSource);
    assert_eq!(
        oracle.reported_version(),
        "local",
        "a from-source run cannot state a version: it is a build-time define"
    );

    let gap = oracle.version_gap();
    assert!(gap.is_unversioned(), "{}", gap.describe());
    assert_eq!(
        gap.pinned.as_deref(),
        Some("1.18.13"),
        "the pinned tree must be the version this project targets"
    );

    let outcome = oracle.run(["--version"]).expect("run the pinned tree");
    println!("{}", outcome.render());
    assert!(outcome.is_success(), "{}", outcome.render());
    assert_eq!(outcome.stdout_trimmed(), "local");
    assert!(
        outcome.label().contains("from-source"),
        "the flavour must be visible in the label: {}",
        outcome.label()
    );

    // Both flavours are reachable from one machine, which is what lets a later
    // differential failure be re-run against the pinned code to tell a version gap
    // apart from a real defect.
    let installed = Oracle::installed_binary().expect("the installed release");
    assert_ne!(installed.reported_version(), oracle.reported_version());
    assert_eq!(
        installed.flavour(),
        &oc_testkit::OracleFlavour::InstalledBinary
    );
}

/// Both sides must receive byte-identical environments apart from the binary being
/// run, or a differential is comparing two different worlds.
#[test]
fn both_sides_can_share_one_scripted_world() {
    let env = scripted();
    let expected = env.env_vars();
    let oracle = Oracle::discover()
        .expect("an oracle is required")
        .with_env(env);
    let subject = Subject::discover_or_build()
        .expect("subject")
        .with_env(scripted());

    let left = oracle.run(["--version"]).expect("oracle runs");
    assert_eq!(left.env, expected, "the oracle saw a different environment");

    // The two fixtures differ only in their temp roots, so every key must match and
    // every scripted value must be rooted in its own tree.
    let right = subject.run(["--version"]).expect("subject runs");
    assert_eq!(
        left.env.keys().collect::<Vec<_>>(),
        right.env.keys().collect::<Vec<_>>(),
        "the two sides saw different variable sets"
    );
    assert_eq!(
        left.env.get("OPENCODE_DB").map(String::as_str),
        Some(":memory:"),
        "the differential must not touch a real database"
    );
}
