use super::*;
use zuno_catalog::skill::Skill;

use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn skill(name: &str, description: Option<&str>, content: &str) -> Skill {
    Skill {
        name: name.to_owned(),
        description: description.map(str::to_owned),
        location: format!("/skills/{name}/SKILL.md"),
        content: content.to_owned(),
    }
}

fn tool(skills: Vec<Skill>) -> SkillTool {
    SkillTool::new(Arc::new(Skills::from_loaded(skills)))
}

fn ctx() -> ToolContext {
    ToolContext::new(
        "ses_skill",
        "msg_skill",
        "call_skill",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn a_named_skill_answers_with_its_whole_body() {
    let body = "# Heading\n\nStep one.\n  indented\n\nStep two.\n";
    let subject = tool(vec![skill("deploy", Some("Ship it."), body)]);
    assert_eq!(subject.replay_policy(), ToolReplayPolicy::Safe);

    let output = subject
        .run(
            SkillParams {
                name: "deploy".to_owned(),
            },
            ctx(),
        )
        .await
        .expect("a known skill loads");

    assert_eq!(output.output, body);
    assert_eq!(output.title, "Skill: deploy");
    assert_eq!(
        output
            .metadata
            .get("location")
            .and_then(|value| value.as_str()),
        Some("/skills/deploy/SKILL.md")
    );
}

#[tokio::test]
async fn an_unknown_name_is_refused_with_the_names_that_exist() {
    let subject = tool(vec![
        skill("deploy", Some("Ship it."), "body"),
        skill("audit", Some("Check it."), "body"),
    ]);

    let error = subject
        .run(
            SkillParams {
                name: "deployy".to_owned(),
            },
            ctx(),
        )
        .await
        .expect_err("a misspelled name is refused");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("Unknown skill `deployy`"), "{rendered}");
    assert!(rendered.contains("audit, deploy"), "{rendered}");
    assert!(
        error.is_model_correctable(),
        "the fix is in the arguments, so the model must be told to send a different name"
    );
}

#[tokio::test]
async fn the_refusal_bounds_how_many_names_it_pastes_back() {
    let many: Vec<Skill> = (0..SUGGESTION_LIMIT + 5)
        .map(|at| skill(&format!("skill-{at:03}"), Some("described"), "body"))
        .collect();
    let subject = tool(many);

    let error = subject
        .run(
            SkillParams {
                name: "absent".to_owned(),
            },
            ctx(),
        )
        .await
        .expect_err("an absent name is refused");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("(and 5 more)"), "{rendered}");
    assert!(
        !rendered.contains("skill-044"),
        "the tail past the limit must not be pasted: {rendered}"
    );
}

#[tokio::test]
async fn an_undescribed_skill_is_loadable_but_not_suggested() {
    let subject = tool(vec![
        skill("quiet", None, "body of the quiet skill"),
        skill("loud", Some("described"), "body"),
    ]);

    let loaded = subject
        .run(
            SkillParams {
                name: "quiet".to_owned(),
            },
            ctx(),
        )
        .await
        .expect("a description-less skill still has a body to load");
    assert_eq!(loaded.output, "body of the quiet skill");

    let error = subject
        .run(
            SkillParams {
                name: "absent".to_owned(),
            },
            ctx(),
        )
        .await
        .expect_err("an absent name is refused");
    let rendered = format!("{}", source_of(&error));
    assert!(
        !rendered.contains("quiet"),
        "a skill the prompt never advertised must not be suggested: {rendered}"
    );
}

#[tokio::test]
async fn an_empty_catalog_says_so_instead_of_listing_nothing() {
    let subject = tool(Vec::new());

    let error = subject
        .run(
            SkillParams {
                name: "anything".to_owned(),
            },
            ctx(),
        )
        .await
        .expect_err("no skills means nothing to load");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("No skills are available"), "{rendered}");
    assert!(rendered.contains("skills.paths[]"), "{rendered}");
}

#[tokio::test]
async fn a_discovered_skill_with_an_empty_body_names_its_file() {
    let subject = tool(vec![skill("hollow", Some("described"), "   \n\n")]);

    let error = subject
        .run(
            SkillParams {
                name: "hollow".to_owned(),
            },
            ctx(),
        )
        .await
        .expect_err("a bodyless skill cannot be followed");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("/skills/hollow/SKILL.md"), "{rendered}");
}

fn source_of(error: &ToolError) -> &(dyn std::error::Error + 'static) {
    std::error::Error::source(error).expect("a rejection carries its reason")
}
