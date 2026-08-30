//! Deterministic provider, filesystem, tool, and machine fixtures.

use std::path::Path;

use crate::error::{Result, TestkitError};
use crate::{Oracle, ScriptedEnv};

use super::baseline::{BaselineReport, MachineFacts};

const SOAK_FILE_COUNT: usize = 50_000;

pub(crate) fn provider_config(base_url: &str) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "permission": { "mode": "allow_all", "rules": {} },
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": { "context": 100000, "output": 10000 },
                        "cost": { "input": 0, "output": 0 },
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    })
    .to_string()
}

pub(crate) fn write_memory_driver_tool(env: &ScriptedEnv, soak: bool) -> Result<()> {
    let local_config = env.project().join(".opencode");
    write_offline_dependency_state(&local_config)?;
    write_offline_dependency_state(&env.xdg_config().join("opencode"))?;
    let directory = local_config.join("tools");
    std::fs::create_dir_all(&directory).map_err(|source| {
        TestkitError::io("create baseline custom-tool directory", &directory, source)
    })?;
    let body = if soak {
        include_str!("soak_tool.ts.txt")
    } else {
        include_str!("single_turn_tool.ts.txt")
    };
    let path = directory.join("get_weather.ts");
    std::fs::write(&path, body)
        .map_err(|source| TestkitError::io("write baseline custom tool", path, source))
}

fn write_offline_dependency_state(directory: &Path) -> Result<()> {
    let node_modules = directory.join("node_modules");
    std::fs::create_dir_all(&node_modules).map_err(|source| {
        TestkitError::io(
            "create baseline node_modules sentinel",
            &node_modules,
            source,
        )
    })?;
    let lockfile = directory.join("package-lock.json");
    let body = serde_json::json!({
        "lockfileVersion": 3,
        "packages": {
            "": { "dependencies": { "@opencode-ai/plugin": "*" } }
        }
    });
    std::fs::write(&lockfile, body.to_string())
        .map_err(|source| TestkitError::io("write baseline dependency lockfile", lockfile, source))
}

/// Create the frozen W-soak tree of 50,000 files under `project`.
pub fn create_watcher_tree(project: &Path) -> Result<()> {
    let root = project.join("watch-tree");
    for index in 0..SOAK_FILE_COUNT {
        let directory = root.join(format!("d{:03}", index / 500));
        if index % 500 == 0 {
            std::fs::create_dir_all(&directory).map_err(|source| {
                TestkitError::io("create W-soak watcher directory", &directory, source)
            })?;
        }
        let path = directory.join(format!("f{index:05}.txt"));
        std::fs::write(&path, b"watched\n")
            .map_err(|source| TestkitError::io("create W-soak watcher file", path, source))?;
    }
    Ok(())
}

pub(crate) fn machine_facts(oracle: &Oracle) -> Result<MachineFacts> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo")
        .map_err(|source| TestkitError::io("read CPU facts", "/proc/cpuinfo", source))?;
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|source| TestkitError::io("read RAM facts", "/proc/meminfo", source))?;
    Ok(MachineFacts {
        kernel: read_trimmed("/proc/sys/kernel/osrelease")?,
        hostname: read_trimmed("/proc/sys/kernel/hostname")?,
        cpu_model: field_value(&cpuinfo, "model name")
            .unwrap_or("unknown")
            .to_owned(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        ram_kib: field_value(&meminfo, "MemTotal")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| TestkitError::BaselineInvariant {
                detail: "MemTotal is absent from /proc/meminfo".to_owned(),
            })?,
        typescript_binary: oracle.program().to_path_buf(),
        typescript_version: oracle.reported_version().to_owned(),
    })
}

pub(crate) fn write_report(path: &Path, report: &BaselineReport) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(report).map_err(|source| TestkitError::BaselineDecode {
            path: path.to_path_buf(),
            source,
        })?;
    std::fs::write(path, bytes)
        .map_err(|source| TestkitError::io("write TypeScript baseline", path, source))
}

fn read_trimmed(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|source| TestkitError::io("read machine fact", path, source))
}

fn field_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim())
    })
}
