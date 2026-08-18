//! §8.2 — the linker, and why this workspace configures none.
//!
//! # What was measured
//!
//! The plan quotes the reference implementation's `mold` result (2.9s with lld,
//! 2.0s with mold) and §10.1 requires it be re-measured here rather than adopted.
//! Re-measured on 2026-08-18 against this workspace's `zuno` binary
//! (203,921,136 bytes, 84,452,158 bytes of `.text`), five interleaved runs of
//! `touch crates/zuno-cli/src/main.rs && cargo rustc --offline -p zuno-cli --bin
//! zuno`:
//!
//! | linker | five runs (s) | min / median / max | max/min |
//! | --- | --- | --- | --- |
//! | toolchain default | 1.34; 1.45; 1.22; 1.19; 1.17 | 1.17 / 1.22 / 1.45 | 1.2393x |
//! | explicit `-fuse-ld=bfd` | 7.39; 5.99; 6.28; 6.48; 5.79 | 5.79 / 6.28 / 7.39 | 1.2764x |
//!
//! # The finding
//!
//! **The toolchain default already is lld.** Rust 1.96 passes
//! `-B<sysroot>/lib/rustlib/<triple>/bin/gcc-ld -fuse-ld=lld` on
//! `x86_64-unknown-linux-gnu` with no configuration at all, which is why the
//! default column above is 5.15x faster than bfd. The first version of this
//! measurement compared the default against an explicit `-fuse-ld=lld` and found
//! no difference — correctly, because both were lld.
//!
//! So the change §8.2 contemplates is already in effect, and `mold` is **not
//! installed on this host** (`which mold` finds nothing; `ld.lld`, `lld` and
//! `clang` are all present). Its remaining headroom over lld on this binary is
//! therefore unmeasured here, and §10.1's rule — no optimisation without a
//! measurement taken on this project — means it is not adopted on the strength of
//! someone else's number.
//!
//! # Why no `.cargo/config.toml` linker entry
//!
//! Two costs and no measured benefit. An unconditional `-fuse-ld=mold` fails the
//! build outright on every machine without mold, and cargo config has no way to
//! express "if the tool exists". And any `RUSTFLAGS`-carried linker flag changes
//! every crate's fingerprint, so a `make build` that sets it and a bare
//! `cargo test` that does not would rebuild the whole workspace against each
//! other.
//!
//! What this file does instead is pin the measured fact, so the 5.15x is not lost
//! silently: a toolchain that stops shipping `rust-lld`, or a config edit that
//! overrides the linker back to bfd, fails here.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("zuno-cli is two levels under the workspace root")
        .to_path_buf()
}

/// The active toolchain's sysroot.
fn sysroot() -> PathBuf {
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("rustc must be runnable to report its sysroot");
    assert!(output.status.success(), "rustc --print sysroot failed");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("a sysroot path is UTF-8")
            .trim(),
    )
}

fn host_triple() -> String {
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-vV")
        .output()
        .expect("rustc must be runnable to report its host triple");
    String::from_utf8(output.stdout)
        .expect("rustc -vV output is UTF-8")
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV reports a host triple")
        .trim()
        .to_owned()
}

#[test]
fn build_the_toolchain_still_ships_the_lld_that_makes_linking_five_times_faster() {
    let triple = host_triple();
    if !triple.contains("linux") {
        eprintln!(
            "SKIP: the lld default measured in this module is a Linux finding; host is {triple}"
        );
        return;
    }

    let sysroot = sysroot();
    let rust_lld = sysroot
        .join("lib/rustlib")
        .join(&triple)
        .join("bin/rust-lld");
    let shim = sysroot
        .join("lib/rustlib")
        .join(&triple)
        .join("bin/gcc-ld/ld.lld");

    // Then: both halves of the mechanism are present. `rust-lld` is the linker;
    // the `gcc-ld/ld.lld` shim is what `-B<dir> -fuse-ld=lld` resolves through,
    // and rustc emits both without configuration.
    assert!(
        rust_lld.is_file(),
        "{} is absent, so this toolchain no longer bundles lld and every link \
         falls back to the system linker — measured at 6.28s median against \
         lld's 1.22s on this binary. Either restore a toolchain that ships \
         rust-lld or re-measure and record the new figure.",
        rust_lld.display()
    );
    assert!(
        shim.is_file(),
        "{} is absent, so `-fuse-ld=lld` cannot resolve to the bundled linker",
        shim.display()
    );
}

#[test]
fn build_no_cargo_config_overrides_the_measured_linker() {
    let path = workspace_root().join(".cargo/config.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    // A linker override is the one edit that would silently undo the 5.15x, so it
    // is named rather than reviewed. `fuse-ld` is checked separately from
    // `linker` because either spelling reaches the same outcome.
    for forbidden in ["linker =", "linker=", "fuse-ld", "-Clinker", "-C linker"] {
        assert!(
            !text.contains(forbidden),
            "{} contains `{forbidden}`. The toolchain default is already lld and \
             measured 5.15x faster than bfd on this binary, so an override here \
             can only be a regression unless it comes with its own measurement \
             recorded in docs/perf-methodology.md.",
            path.display()
        );
    }

    // And the file has to still be the one that carries the allocator tuning, so
    // this test cannot pass by reading an empty or renamed file.
    assert!(
        text.contains("JEMALLOC_SYS_WITH_MALLOC_CONF"),
        "{} no longer carries the jemalloc tuning, so this assertion is reading \
         the wrong file and would pass vacuously",
        path.display()
    );
}

#[test]
fn build_mold_is_absent_and_therefore_unmeasured_rather_than_rejected() {
    // Given: the host's linker inventory.
    let mold = which::which("mold").ok();

    // Then: state the position rather than assert an outcome. This is a null
    // result with a reason, not a preference: §10.1 forbids adopting the
    // reference implementation's 2.9s->2.0s number without a local measurement,
    // and a measurement needs the tool.
    match mold {
        None => eprintln!(
            "mold is not installed on this host, so its headroom over the \
             toolchain's bundled lld is UNMEASURED on this workspace. lld itself \
             is already the default and measured 1.22s median against bfd's \
             6.28s. To close this, install mold, re-run the interleaved \
             measurement in this module's docs, and record the result in \
             docs/perf-methodology.md before configuring anything."
        ),
        Some(path) => eprintln!(
            "mold is now available at {}. §8.2 is still open: measure it against \
             the bundled lld with the interleaved procedure in this module's docs \
             before adding any linker configuration, and note that a \
             RUSTFLAGS-carried linker flag invalidates every crate fingerprint.",
            path.display()
        ),
    }
}
