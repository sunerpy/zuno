//! Conditional-tool exposure against the measured client/plan-mode matrix.
//!
//! Durable plans and work items are regular Zuno tools now; their database and
//! optimistic-concurrency coverage lives in `work_state`. This file is deliberately
//! limited to the three tools whose model visibility still depends on runtime flags.

use std::sync::Arc;
use zuno_tool::{Tool, erase};
use zuno_tools::exposure::{
    ENV_CLIENT, ENV_ENABLE_QUESTION_TOOL, ENV_EXPERIMENTAL, ENV_EXPERIMENTAL_PLAN_MODE,
    ExposureFlags, exposed_conditional_tools,
};
use zuno_tools::invalid::InvalidTool;
use zuno_tools::plan_exit::{PlanExitTool, RecordingHost};
use zuno_tools::question::{QuestionAsker, QuestionTool, ScriptedAnswers};

fn flags(pairs: &[(&str, &str)]) -> ExposureFlags {
    let owned = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    ExposureFlags::from_lookup(|key| {
        owned
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    })
}

type MeasuredCase = (
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

#[test]
fn conditional_invalid_is_offered_in_every_configuration() {
    for configuration in [
        flags(&[]),
        flags(&[(ENV_CLIENT, "tui")]),
        flags(&[(ENV_CLIENT, "")]),
        flags(&[(ENV_EXPERIMENTAL, "true")]),
        flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "true")]),
    ] {
        assert!(
            exposed_conditional_tools(&configuration).contains(&"invalid"),
            "invalid must be offered for {configuration:?}"
        );
    }
}

#[test]
fn conditional_question_is_offered_only_to_an_interactive_client_or_under_its_flag() {
    for client in ["cli", "app", "desktop"] {
        assert!(
            exposed_conditional_tools(&flags(&[(ENV_CLIENT, client)])).contains(&"question"),
            "{client} must be offered question"
        );
    }
    assert!(
        exposed_conditional_tools(&flags(&[
            (ENV_CLIENT, "tui"),
            (ENV_ENABLE_QUESTION_TOOL, "true"),
        ]))
        .contains(&"question")
    );

    for client in ["tui", "CLI", "", "headless"] {
        assert!(
            !exposed_conditional_tools(&flags(&[(ENV_CLIENT, client)])).contains(&"question"),
            "{client:?} must not be offered question"
        );
    }
}

#[test]
fn conditional_plan_exit_is_offered_only_under_plan_mode_with_a_cli_client() {
    assert!(
        exposed_conditional_tools(&flags(&[
            (ENV_CLIENT, "cli"),
            (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
        ]))
        .contains(&"plan_exit")
    );
    assert!(!exposed_conditional_tools(&flags(&[(ENV_CLIENT, "cli")])).contains(&"plan_exit"));
    for client in ["tui", "app", "desktop", "CLI", ""] {
        assert!(
            !exposed_conditional_tools(&flags(&[
                (ENV_CLIENT, client),
                (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
            ]))
            .contains(&"plan_exit"),
            "{client:?} must not be offered plan_exit"
        );
    }
}

#[test]
fn conditional_the_exposed_set_matches_the_measured_matrix() {
    let cases: &[MeasuredCase] = &[
        (&[], &["invalid", "question"]),
        (&[(ENV_CLIENT, "tui")], &["invalid"]),
        (
            &[(ENV_CLIENT, "tui"), (ENV_ENABLE_QUESTION_TOOL, "true")],
            &["invalid", "question"],
        ),
        (&[(ENV_CLIENT, "app")], &["invalid", "question"]),
        (&[(ENV_CLIENT, "desktop")], &["invalid", "question"]),
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "question", "plan_exit"],
        ),
        (
            &[(ENV_CLIENT, "tui"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid"],
        ),
        (
            &[(ENV_CLIENT, "app"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid", "question"],
        ),
        (
            &[(ENV_EXPERIMENTAL, "true")],
            &["invalid", "question", "plan_exit"],
        ),
        (
            &[
                (ENV_EXPERIMENTAL, "true"),
                (ENV_EXPERIMENTAL_PLAN_MODE, "false"),
            ],
            &["invalid", "question"],
        ),
        (
            &[
                (ENV_EXPERIMENTAL, "false"),
                (ENV_EXPERIMENTAL_PLAN_MODE, "true"),
            ],
            &["invalid", "question", "plan_exit"],
        ),
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "1")],
            &["invalid", "question", "plan_exit"],
        ),
        (
            &[(ENV_EXPERIMENTAL_PLAN_MODE, "0")],
            &["invalid", "question"],
        ),
        (
            &[(ENV_CLIENT, "CLI"), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid"],
        ),
        (
            &[(ENV_CLIENT, ""), (ENV_EXPERIMENTAL_PLAN_MODE, "true")],
            &["invalid"],
        ),
    ];

    assert!(cases.len() >= 15);
    for (environment, expected) in cases {
        let mut offered = exposed_conditional_tools(&flags(environment));
        offered.sort_unstable();
        let mut want = expected.to_vec();
        want.sort_unstable();
        assert_eq!(offered, want, "exposure differs for {environment:?}");
    }
}

#[test]
fn conditional_tools_report_the_same_exposure_as_the_registry_predicates() {
    let plan_mode_cli = flags(&[(ENV_EXPERIMENTAL_PLAN_MODE, "true")]);
    let headless = flags(&[(ENV_CLIENT, "tui")]);

    assert!(QuestionTool::exposed_under(&plan_mode_cli));
    assert!(!QuestionTool::exposed_under(&headless));
    assert!(PlanExitTool::exposed_under(&plan_mode_cli));
    assert!(!PlanExitTool::exposed_under(&headless));
}

#[test]
fn conditional_tools_erase_into_one_list_with_distinct_wire_ids() {
    let asker: Arc<dyn QuestionAsker> = Arc::new(ScriptedAnswers::selecting("Yes"));
    let registry: Vec<Arc<dyn Tool>> = vec![
        erase(InvalidTool::new()),
        erase(QuestionTool::new(Arc::clone(&asker))),
        erase(PlanExitTool::new(asker, Arc::new(RecordingHost::default()))),
    ];

    let ids = registry.iter().map(|tool| tool.id()).collect::<Vec<_>>();
    assert_eq!(ids, vec!["invalid", "question", "plan_exit"]);
    for tool in &registry {
        let definition = tool.definition();
        assert_eq!(definition.parameters["type"], "object");
        assert!(!definition.description.is_empty());
    }
}
