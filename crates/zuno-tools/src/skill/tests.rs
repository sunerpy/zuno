use super::*;
use zuno_catalog::skill::Skill;

use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn skill(name: &str, description: Option<&str>, content: &str) -> Skill {
    Skill::embedded_at_path(
        name,
        description.map(str::to_owned),
        PathBuf::from(format!("/skills/{name}/SKILL.md")),
        content,
    )
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

#[test]
fn the_provider_schema_advertises_progressive_discovery_and_resource_reads() {
    let definition = zuno_tool::erase(tool(Vec::new())).definition();
    let properties = definition.parameters["properties"]
        .as_object()
        .expect("skill parameters are an object schema");

    for field in [
        "action", "query", "name", "source", "path", "cursor", "limit",
    ] {
        assert!(
            properties.contains_key(field),
            "provider schema omitted `{field}`: {}",
            definition.parameters
        );
    }
    assert_eq!(
        properties["action"]["enum"],
        serde_json::json!(["list", "search", "load", "read_resource"])
    );
    assert_eq!(properties["limit"]["minimum"], 1);
    assert_eq!(properties["limit"]["maximum"], 20);
}

#[tokio::test]
async fn same_named_skills_require_the_source_returned_by_discovery() {
    let subject = tool(vec![
        Skill::embedded_at_path(
            "deploy",
            Some(String::from("First deployment workflow.")),
            PathBuf::from("/skills/first/SKILL.md"),
            "FIRST BODY",
        ),
        Skill::embedded_at_path(
            "deploy",
            Some(String::from("Second deployment workflow.")),
            PathBuf::from("/skills/second/SKILL.md"),
            "SECOND BODY",
        ),
    ]);

    let erased = zuno_tool::erase(subject);
    let error = erased
        .invoke(
            serde_json::json!({"action": "load", "name": "deploy"}),
            ctx(),
        )
        .await
        .expect_err("plain duplicate name must be refused");
    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("ambiguous"), "{rendered}");
    assert!(rendered.contains("/skills/first/SKILL.md"), "{rendered}");
    assert!(rendered.contains("/skills/second/SKILL.md"), "{rendered}");

    let output = erased
        .invoke(
            serde_json::json!({
                "action": "load",
                "name": "deploy",
                "source": "/skills/second/SKILL.md"
            }),
            ctx(),
        )
        .await
        .expect("source disambiguates the skill");
    assert!(output.output.contains("SECOND BODY"), "{}", output.output);
    assert!(!output.output.contains("FIRST BODY"), "{}", output.output);
}

#[tokio::test]
async fn list_and_search_return_the_source_needed_for_an_exact_load() {
    let subject = tool(vec![skill(
        "release-rust",
        Some("Release and publish a Rust CLI."),
        "body",
    )]);
    let erased = zuno_tool::erase(subject);

    for arguments in [
        serde_json::json!({"action": "list"}),
        serde_json::json!({"action": "search", "query": "publish Rust"}),
    ] {
        let output = erased
            .invoke(arguments, ctx())
            .await
            .expect("metadata discovery succeeds");
        assert!(
            output.output.contains("/skills/release-rust/SKILL.md"),
            "{}",
            output.output
        );
    }
}

#[tokio::test]
async fn a_filesystem_skill_body_is_read_after_selection_not_cached_at_discovery() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("SKILL.md");
    std::fs::write(
        &path,
        "---\nname: live\ndescription: Live instructions.\n---\nOLD BODY\n",
    )
    .expect("initial skill");
    let discovered = zuno_catalog::skill::parse_file(&path).expect("metadata discovery");
    std::fs::write(
        &path,
        "---\nname: live\ndescription: Live instructions.\n---\nNEW BODY\n",
    )
    .expect("updated skill");

    let output = tool(vec![discovered])
        .run(SkillParams::load("live"), ctx())
        .await
        .expect("selected body reads");

    assert!(output.output.contains("NEW BODY"), "{}", output.output);
    assert!(!output.output.contains("OLD BODY"), "{}", output.output);
}

#[tokio::test]
async fn a_large_skill_body_is_paginated_and_the_cursor_is_content_bound() {
    let body = "abcdefghij".repeat(10_000);
    let source = "/skills/large/SKILL.md";
    let erased = zuno_tool::erase(tool(vec![Skill::embedded_at_path(
        "large",
        Some(String::from("Large instructions.")),
        PathBuf::from(source),
        body,
    )]));

    let first = erased
        .invoke(
            serde_json::json!({"action": "load", "name": "large"}),
            ctx(),
        )
        .await
        .expect("first page");
    let cursor = first
        .metadata
        .get("next_cursor")
        .and_then(serde_json::Value::as_str)
        .expect("large body has a continuation")
        .to_owned();
    assert_eq!(first.metadata["complete"], false);
    assert!(!first.output.contains("--- END SKILL BODY ---"));

    let second = erased
        .invoke(
            serde_json::json!({
                "action": "load",
                "name": "large",
                "source": source,
                "cursor": cursor
            }),
            ctx(),
        )
        .await
        .expect("second page");
    assert_eq!(second.metadata["complete"], true);
    assert!(second.output.contains("--- END SKILL BODY ---"));

    let other = zuno_tool::erase(tool(vec![Skill::embedded_at_path(
        "large",
        Some(String::from("Large instructions.")),
        PathBuf::from(source),
        "different body".repeat(10_000),
    )]));
    let error = other
        .invoke(
            serde_json::json!({
                "action": "load",
                "name": "large",
                "source": source,
                "cursor": first.metadata["next_cursor"]
            }),
            ctx(),
        )
        .await
        .expect_err("a cursor cannot be replayed against changed content");
    assert!(
        format!("{}", source_of(&error)).contains("stale `cursor`"),
        "{error}"
    );
}

#[tokio::test]
async fn a_named_skill_answers_with_its_whole_body() {
    let body = "# Heading\n\nStep one.\n  indented\n\nStep two.\n";
    let subject = tool(vec![skill("deploy", Some("Ship it."), body)]);
    assert_eq!(subject.replay_policy(), ToolReplayPolicy::Safe);

    let output = subject
        .run(SkillParams::load("deploy"), ctx())
        .await
        .expect("a known skill loads");

    assert!(output.output.contains(body), "{}", output.output);
    assert!(
        output.output.contains("Resource root: `/skills/deploy`"),
        "{}",
        output.output
    );
    assert!(
        output.output.contains("action `read_resource`")
            && output.output.contains("do not search the filesystem"),
        "{}",
        output.output
    );
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
        .run(SkillParams::load("deployy"), ctx())
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
        .run(SkillParams::load("absent"), ctx())
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
        .run(SkillParams::load("quiet"), ctx())
        .await
        .expect("a description-less skill still has a body to load");
    assert!(
        loaded.output.contains("body of the quiet skill"),
        "{}",
        loaded.output
    );

    let error = subject
        .run(SkillParams::load("absent"), ctx())
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
        .run(SkillParams::load("anything"), ctx())
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
        .run(SkillParams::load("hollow"), ctx())
        .await
        .expect_err("a bodyless skill cannot be followed");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("/skills/hollow/SKILL.md"), "{rendered}");
}

#[tokio::test]
async fn search_ranks_matching_descriptions_without_loading_skill_bodies() {
    let subject = tool(vec![
        skill(
            "release-rust",
            Some("Release and publish a Rust CLI to GitHub."),
            "SECRET RELEASE BODY",
        ),
        skill(
            "release-web",
            Some("Release a web application to a hosting provider."),
            "SECRET WEB BODY",
        ),
        skill(
            "rust-review",
            Some("Review Rust code without publishing it."),
            "SECRET REVIEW BODY",
        ),
    ]);

    let output = subject
        .run(SkillParams::search("publish Rust CLI", Some(2)), ctx())
        .await
        .expect("search succeeds");

    assert_eq!(output.title, "Skill search: publish Rust CLI");
    assert!(output.output.contains("release-rust"), "{}", output.output);
    assert!(output.output.contains("rust-review"), "{}", output.output);
    assert!(!output.output.contains("release-web"), "{}", output.output);
    assert!(
        !output.output.contains("SECRET"),
        "search must disclose descriptions, not load executable guidance: {}",
        output.output
    );
    assert!(
        output.metadata.get("name").is_none(),
        "a search result must not make the TUI mark a skill as loaded"
    );
}

#[tokio::test]
async fn search_bounds_large_descriptions_and_rejects_an_empty_query() {
    let subject = tool(vec![skill(
        "large",
        Some(&"large trigger ".repeat(1_000)),
        "body",
    )]);

    let output = subject
        .run(SkillParams::search("large trigger", None), ctx())
        .await
        .expect("search succeeds");
    assert!(
        output.output.len() < 4_000,
        "one pathological frontmatter description escaped the search bound"
    );

    let error = subject
        .run(SkillParams::search("   ", None), ctx())
        .await
        .expect_err("an empty search is invalid");
    assert!(error.is_model_correctable());
}

#[tokio::test]
async fn a_referenced_skill_resource_is_read_without_filesystem_discovery() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("github-project-scaffold");
    let references = root.join("references");
    std::fs::create_dir_all(&references).expect("reference directory");
    let skill_file = root.join("SKILL.md");
    std::fs::write(&skill_file, "body").expect("skill file");
    std::fs::write(references.join("ci.md"), "# CI contract\nPinned actions.\n")
        .expect("reference file");
    let subject = tool(vec![Skill::embedded_at_path(
        "github-project-scaffold",
        Some(String::from("Prepare a public repository.")),
        skill_file,
        "Read `references/ci.md`.",
    )]);

    let output = subject
        .run(
            SkillParams::read_resource("github-project-scaffold", "references/ci.md"),
            ctx(),
        )
        .await
        .expect("resource read");

    assert_eq!(output.output, "# CI contract\nPinned actions.\n");
    let expected_path = zuno_paths::wire_path(
        &references
            .join("ci.md")
            .canonicalize()
            .expect("canonical reference path"),
    );
    assert_eq!(
        output
            .metadata
            .get("path")
            .and_then(serde_json::Value::as_str),
        Some(expected_path.as_str())
    );
}

#[tokio::test]
async fn a_skill_resource_cannot_escape_its_skill_root() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("skill");
    std::fs::create_dir_all(&root).expect("skill directory");
    let skill_file = root.join("SKILL.md");
    std::fs::write(&skill_file, "body").expect("skill file");
    std::fs::write(dir.path().join("secret.md"), "outside").expect("outside file");
    let subject = tool(vec![Skill::embedded_at_path(
        "bounded",
        Some(String::from("Bounded resource reads.")),
        skill_file,
        "body",
    )]);

    let error = subject
        .run(SkillParams::read_resource("bounded", "../secret.md"), ctx())
        .await
        .expect_err("parent traversal must be rejected");

    let rendered = format!("{}", source_of(&error));
    assert!(
        rendered.contains("relative path") && rendered.contains("cannot contain"),
        "{rendered}"
    );
    assert!(error.is_model_correctable());
}

#[cfg(unix)]
#[tokio::test]
async fn a_skill_resource_symlink_cannot_escape_its_skill_root() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("skill");
    let references = root.join("references");
    std::fs::create_dir_all(&references).expect("reference directory");
    let skill_file = root.join("SKILL.md");
    std::fs::write(&skill_file, "body").expect("skill file");
    let outside = dir.path().join("secret.md");
    std::fs::write(&outside, "outside").expect("outside file");
    symlink(&outside, references.join("escape.md")).expect("resource symlink");
    let subject = tool(vec![Skill::embedded_at_path(
        "bounded",
        Some(String::from("Bounded resource reads.")),
        skill_file,
        "body",
    )]);

    let error = subject
        .run(
            SkillParams::read_resource("bounded", "references/escape.md"),
            ctx(),
        )
        .await
        .expect_err("a symlink outside the skill root must be rejected");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("resolves outside"), "{rendered}");
    assert!(error.is_model_correctable());
}

#[tokio::test]
async fn action_specific_fields_are_rejected_instead_of_silently_ignored() {
    let subject = tool(vec![skill("deploy", Some("Ship it."), "body")]);
    let error = subject
        .run(
            SkillParams {
                action: SkillAction::Load,
                query: Some("deploy".to_owned()),
                name: Some("deploy".to_owned()),
                source: None,
                path: None,
                cursor: None,
                limit: None,
            },
            ctx(),
        )
        .await
        .expect_err("load must not silently ignore search-only fields");

    let rendered = format!("{}", source_of(&error));
    assert!(rendered.contains("does not accept `query`"), "{rendered}");
    assert!(error.is_model_correctable());
}

fn source_of(error: &ToolError) -> &(dyn std::error::Error + 'static) {
    std::error::Error::source(error).expect("a rejection carries its reason")
}
