//! Linux total-process-tree RSS collection without unsafe system calls.

use std::collections::{BTreeSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TestkitError};

/// One total-process-tree RSS observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RssSample {
    /// Milliseconds since the workload process started.
    pub elapsed_ms: u64,
    /// Sum of resident KiB across root and all transitive children.
    pub total_rss_kib: u64,
    /// Every PID included in the sum, sorted and deduplicated.
    pub pids: Vec<u32>,
}

pub(crate) fn sample(root: u32, started: Instant) -> Result<RssSample> {
    let pids = collect_transitive_pids(root, linux_children)?;
    let mut total_rss_kib = 0_u64;
    let mut included = Vec::with_capacity(pids.len());
    for pid in pids {
        match linux_rss_kib(pid) {
            Ok(rss) => {
                total_rss_kib = total_rss_kib.saturating_add(rss);
                included.push(pid);
            }
            Err(TestkitError::ProcessVanished { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(RssSample {
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        total_rss_kib,
        pids: included,
    })
}

pub(crate) fn collect_transitive_pids<F>(root: u32, mut children: F) -> Result<Vec<u32>>
where
    F: FnMut(u32) -> Result<Vec<u32>>,
{
    let mut seen = BTreeSet::new();
    let mut pending = VecDeque::from([root]);
    while let Some(pid) = pending.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        for child in children(pid)? {
            if !seen.contains(&child) {
                pending.push_back(child);
            }
        }
    }
    Ok(seen.into_iter().collect())
}

pub(crate) fn find_oracle_descendant(launcher: u32) -> Result<Option<u32>> {
    let pids = collect_transitive_pids(launcher, linux_children)?;
    for pid in pids.into_iter().rev() {
        let comm = PathBuf::from(format!("/proc/{pid}/comm"));
        match std::fs::read_to_string(&comm) {
            Ok(name) if name.trim() == "opencode" => return Ok(Some(pid)),
            Ok(_) => {}
            Err(source) if source.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(TestkitError::ProcessTreeRead {
                    pid,
                    path: comm,
                    source,
                });
            }
        }
    }
    Ok(None)
}

fn linux_children(pid: u32) -> Result<Vec<u32>> {
    let task_dir = PathBuf::from(format!("/proc/{pid}/task"));
    let entries = match std::fs::read_dir(&task_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(TestkitError::ProcessTreeRead {
                pid,
                path: task_dir,
                source,
            });
        }
    };
    let mut children = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| TestkitError::ProcessTreeRead {
            pid,
            path: task_dir.clone(),
            source,
        })?;
        let child_path = entry.path().join("children");
        let text = match std::fs::read_to_string(&child_path) {
            Ok(text) => text,
            Err(source) if source.kind() == ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(TestkitError::ProcessTreeRead {
                    pid,
                    path: child_path,
                    source,
                });
            }
        };
        for token in text.split_whitespace() {
            let child = token
                .parse::<u32>()
                .map_err(|source| TestkitError::ProcessTreeParse {
                    pid,
                    path: child_path.clone(),
                    value: token.to_owned(),
                    source,
                })?;
            children.insert(child);
        }
    }
    Ok(children.into_iter().collect())
}

fn linux_rss_kib(pid: u32) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let text = std::fs::read_to_string(&path).map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            TestkitError::ProcessVanished { pid }
        } else {
            TestkitError::ProcessTreeRead {
                pid,
                path: path.clone(),
                source,
            }
        }
    })?;
    parse_status_rss(pid, &path, &text)
}

fn parse_status_rss(pid: u32, path: &Path, text: &str) -> Result<u64> {
    let line = text
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .ok_or_else(|| TestkitError::ProcessTreeFormat {
            pid,
            path: path.to_path_buf(),
            detail: "VmRSS field is absent".to_owned(),
        })?;
    let value = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| TestkitError::ProcessTreeFormat {
            pid,
            path: path.to_path_buf(),
            detail: format!("VmRSS field has no numeric value: {line}"),
        })?;
    value
        .parse::<u64>()
        .map_err(|source| TestkitError::ProcessTreeParse {
            pid,
            path: path.to_path_buf(),
            value: value.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_walk_includes_transitive_children() {
        // Given: a synthetic parent-child map with two generations.
        let tree: std::collections::BTreeMap<u32, Vec<u32>> = [
            (100_u32, vec![200_u32, 300_u32]),
            (200_u32, vec![400_u32]),
            (300_u32, Vec::new()),
            (400_u32, Vec::new()),
        ]
        .into_iter()
        .collect();

        // When: the complete process tree is enumerated.
        let pids =
            collect_transitive_pids(100, |pid| Ok(tree.get(&pid).cloned().unwrap_or_default()))
                .expect("synthetic tree is readable");

        // Then: the root, children, and grandchild all appear exactly once.
        assert_eq!(pids, vec![100, 200, 300, 400]);
    }

    #[test]
    fn status_parser_reads_rss_in_kib() {
        // Given: the kernel's status-file shape.
        let status = "Name:\topencode\nVmPeak:\t200 kB\nVmRSS:\t12345 kB\nThreads:\t4\n";

        // When: VmRSS is parsed.
        let rss = parse_status_rss(7, Path::new("/proc/7/status"), status).expect("valid status");

        // Then: the numeric KiB value is retained without unit conversion.
        assert_eq!(rss, 12_345);
    }
}
