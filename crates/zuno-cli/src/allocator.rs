//! Allocator selection and the safe Linux GNU system-allocator fallback.

use std::process::ExitCode;

/// The configuration embedded into jemalloc by `.cargo/config.toml`.
///
/// Keeping the value visible to Rust lets a deterministic test pin the exact
/// parameters while `tikv-jemalloc-sys` consumes the same environment variable
/// in its build script before jemalloc is compiled.
pub(crate) const JEMALLOC_MALLOC_CONF: &str = env!("JEMALLOC_SYS_WITH_MALLOC_CONF");

// Frozen G1/G2 measurement on 2026-08-18: the 931-message W-real median fell
// from 1,653,348 KiB with tuned glibc to 1,198,872 KiB (-27.49%); W-idle rose
// from 26,204 to 27,340 KiB (+4.34%). One-second decay can trade throughput on
// repeated large alloc/free cycles for earlier page return; this interactive,
// long-running process chooses the lower W-real peak. Full runs are documented
// in `docs/perf-methodology.md`.
const EXPECTED_JEMALLOC_MALLOC_CONF: &str = "dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4";

/// Ensure the active allocator has its tuning before normal process startup.
///
/// Jemalloc receives compile-time tuning, so that branch needs no restart.
/// Linux GNU system builds restart once with glibc's documented environment
/// equivalents of `mallopt(M_ARENA_MAX, 4)` and
/// `mallopt(M_MMAP_THRESHOLD, 256 * 1024)`. `Command::env` plus `exec` keeps the
/// workspace's `unsafe_code = "forbid"` contract intact.
pub(crate) fn ensure_tuned_allocator() -> Option<ExitCode> {
    debug_assert_eq!(JEMALLOC_MALLOC_CONF, EXPECTED_JEMALLOC_MALLOC_CONF);
    restart_with_glibc_tuning()
}

#[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "jemalloc")))]
fn restart_with_glibc_tuning() -> Option<ExitCode> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    const BOOTSTRAP_MARKER: &str = "ZUNO_GLIBC_ALLOCATOR_TUNED";
    const MALLOC_ARENA_MAX: (&str, &str) = ("MALLOC_ARENA_MAX", "4");
    const MALLOC_MMAP_THRESHOLD: (&str, &str) = ("MALLOC_MMAP_THRESHOLD_", "262144");

    if std::env::var_os(BOOTSTRAP_MARKER).is_some() {
        return None;
    }

    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("failed to locate zuno for glibc allocator tuning: {error}");
            return Some(ExitCode::FAILURE);
        }
    };
    let mut command = Command::new(executable);
    command
        .args(std::env::args_os().skip(1))
        .env(BOOTSTRAP_MARKER, "1")
        .env(MALLOC_ARENA_MAX.0, MALLOC_ARENA_MAX.1)
        .env(MALLOC_MMAP_THRESHOLD.0, MALLOC_MMAP_THRESHOLD.1)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let error = command.exec();
    eprintln!("failed to restart zuno with glibc allocator tuning: {error}");
    Some(ExitCode::FAILURE)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu", not(feature = "jemalloc"))))]
const fn restart_with_glibc_tuning() -> Option<ExitCode> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Selection {
        Jemalloc,
        TunedGlibc,
        System,
    }

    const fn selection(
        jemalloc_feature: bool,
        jemalloc_supported: bool,
        linux_gnu: bool,
    ) -> Selection {
        if jemalloc_feature && jemalloc_supported {
            Selection::Jemalloc
        } else if !jemalloc_feature && linux_gnu {
            Selection::TunedGlibc
        } else {
            Selection::System
        }
    }

    #[test]
    fn jemalloc_parameter_string_is_the_embedded_build_configuration() {
        assert_eq!(JEMALLOC_MALLOC_CONF, EXPECTED_JEMALLOC_MALLOC_CONF);
    }

    #[test]
    fn glibc_fallback_is_only_selected_for_non_jemalloc_linux_gnu_builds() {
        assert_eq!(selection(false, true, true), Selection::TunedGlibc);
        assert_eq!(selection(true, true, true), Selection::Jemalloc);
        assert_eq!(selection(false, true, false), Selection::System);
        assert_eq!(selection(true, false, false), Selection::System);
    }
}
