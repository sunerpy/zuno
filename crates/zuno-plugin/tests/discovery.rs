use std::fs;

use zuno_config::Config;
use zuno_config::schema::plugin::PluginSpec;
use zuno_plugin::{ConfigDirectory, ConfigLayer, PluginOrigin, PluginScope, discover_plugins};

#[test]
fn relative_specs_resolve_against_each_declaring_config_file() {
    // Given
    let root = tempfile::tempdir().expect("tempdir");
    let global_file = root.path().join("global/zuno.json");
    let local_file = root.path().join("repo/.zuno/zuno.json");
    fs::create_dir_all(global_file.parent().expect("global parent")).expect("global directory");
    fs::create_dir_all(local_file.parent().expect("local parent")).expect("local directory");
    let global = Config {
        plugin: Some(vec![PluginSpec::Name("./plugin.ts".to_owned())]),
        ..Config::default()
    };
    let local = Config {
        plugin: Some(vec![PluginSpec::WithOptions(
            "./plugin.ts".to_owned(),
            serde_json::Map::from_iter([("mode".to_owned(), serde_json::json!("local"))]),
        )]),
        ..Config::default()
    };
    let layers = [
        ConfigLayer::new(&global_file, PluginScope::Global, &global),
        ConfigLayer::new(&local_file, PluginScope::Local, &local),
    ];

    // When
    let discovered = discover_plugins(&layers, &[]).expect("discover config plugins");

    // Then
    assert_eq!(discovered.len(), 2);
    assert_eq!(
        discovered[0].spec.name(),
        url::Url::from_file_path(root.path().join("global/plugin.ts"))
            .expect("global file URL")
            .as_str()
    );
    assert_eq!(
        discovered[1].spec.name(),
        url::Url::from_file_path(root.path().join("repo/.zuno/plugin.ts"))
            .expect("local file URL")
            .as_str()
    );
    assert_eq!(
        discovered[1]
            .spec
            .options()
            .and_then(|options| options.get("mode")),
        Some(&serde_json::json!("local"))
    );
    assert!(matches!(
        &discovered[1].origin,
        PluginOrigin::Config { source, scope: PluginScope::Local } if source == &local_file
    ));
}

#[test]
fn auto_discovery_scans_both_directories_in_stable_order() {
    // Given
    let root = tempfile::tempdir().expect("tempdir");
    let config_dir = root.path().join(".opencode");
    fs::create_dir_all(config_dir.join("plugin")).expect("plugin directory");
    fs::create_dir_all(config_dir.join("plugins")).expect("plugins directory");
    fs::write(config_dir.join("plugin/z.ts"), "export default {};").expect("z plugin");
    fs::write(config_dir.join("plugin/.hidden.js"), "export default {};").expect("hidden plugin");
    fs::write(config_dir.join("plugins/a.js"), "export default {};").expect("a plugin");
    fs::write(config_dir.join("plugins/ignored.txt"), "ignored").expect("ignored file");
    let directories = [ConfigDirectory::new(&config_dir, PluginScope::Local)];

    // When
    let discovered = discover_plugins(&[], &directories).expect("auto discover plugins");

    // Then
    let names: Vec<&str> = discovered.iter().map(|plugin| plugin.spec.name()).collect();
    let expected = [
        config_dir.join("plugin/.hidden.js"),
        config_dir.join("plugin/z.ts"),
        config_dir.join("plugins/a.js"),
    ]
    .map(|path| {
        url::Url::from_file_path(path)
            .expect("file URL")
            .to_string()
    });
    assert_eq!(
        names,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(discovered.iter().all(|plugin| matches!(
        plugin.origin,
        PluginOrigin::AutoDiscovered {
            scope: PluginScope::Local,
            ..
        }
    )));
}

#[cfg(unix)]
fn write_mode(path: &std::path::Path, body: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    fs::write(path, body).expect("write file");
    let mut permissions = fs::metadata(path).expect("stat").permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).expect("chmod");
}

#[cfg(unix)]
#[test]
fn only_executable_non_script_files_are_process_plugin_candidates() {
    // Given
    let root = tempfile::tempdir().expect("tempdir");
    let config_dir = root.path().join("repo/.zuno");
    write_mode(&config_dir.join("plugin/zeta"), "#!/bin/sh\n", 0o755);
    write_mode(&config_dir.join("plugin/alpha"), "#!/bin/sh\n", 0o755);
    write_mode(
        &config_dir.join("plugin/not-executable"),
        "#!/bin/sh\n",
        0o644,
    );
    write_mode(&config_dir.join("plugin/README.md"), "docs\n", 0o644);
    // Executable and a script: the JavaScript tier owns it, and starting it here too
    // would run one plugin twice under two protocols.
    write_mode(
        &config_dir.join("plugin/dual.js"),
        "#!/usr/bin/env node\n",
        0o755,
    );
    write_mode(&config_dir.join("plugins/beta"), "#!/bin/sh\n", 0o755);

    // When
    let discovered = zuno_plugin::discover_process_plugins(&[ConfigDirectory::new(
        &config_dir,
        PluginScope::Local,
    )])
    .expect("discover process plugins");

    // Then
    let names: Vec<&str> = discovered
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["alpha", "zeta", "beta"],
        "`plugin` sorted, then `plugins` sorted, matching the script tier's order"
    );
    assert!(
        discovered
            .iter()
            .all(|plugin| plugin.scope == PluginScope::Local)
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_an_executable_is_a_process_plugin_candidate() {
    // Given
    let root = tempfile::tempdir().expect("tempdir");
    let config_dir = root.path().join("repo/.zuno");
    let real = root.path().join("build/my-plugin");
    write_mode(&real, "#!/bin/sh\n", 0o755);
    fs::create_dir_all(config_dir.join("plugin")).expect("plugin directory");
    std::os::unix::fs::symlink(&real, config_dir.join("plugin/linked")).expect("symlink");
    std::os::unix::fs::symlink(
        root.path().join("build/absent"),
        config_dir.join("plugin/broken"),
    )
    .expect("broken symlink");

    // When
    let discovered = zuno_plugin::discover_process_plugins(&[ConfigDirectory::new(
        &config_dir,
        PluginScope::Local,
    )])
    .expect("discover process plugins");

    // Then
    let names: Vec<&str> = discovered
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["linked"],
        "a symlink into a build directory is how a plugin author iterates; a broken \
         one resolves to nothing executable"
    );
}

#[test]
fn an_absent_plugin_directory_is_not_an_error_for_the_process_tier() {
    // Given
    let root = tempfile::tempdir().expect("tempdir");

    // When
    let discovered = zuno_plugin::discover_process_plugins(&[ConfigDirectory::new(
        root.path(),
        PluginScope::Global,
    )])
    .expect("an absent directory is the common case, not a failure");

    // Then
    assert!(discovered.is_empty());
}
