//! [`zuno_catalog::skill::load`] must return a `Send` future.
//!
//! # Why this needs a test of its own
//!
//! This was broken and invisible. `skill::remote::download_all` mapped over
//! `files.iter()`, which made its closure `fn(&'0 String) -> impl Future` for one
//! inferred lifetime instead of `for<'a>`, and that made `load`'s whole future
//! `!Send`. Every existing test and the only production caller awaited it from a
//! `!Send` context, so nothing failed — until it was awaited inside
//! `TurnPlan::resolve`, whose future `zuno serve` boxes into a `Send` trait object
//! and the TUI hands to `tokio::spawn`.
//!
//! The failure mode is what makes it worth pinning: rustc reports it at the *caller*
//! ("implementation of `Send` is not general enough", pointing at `serve.rs` and
//! `tui.rs`), naming neither this crate nor the closure. A regression here would
//! surface as an unexplained compile error two crates away.
//!
//! Compile-time only — there is nothing to assert at runtime.

use std::path::PathBuf;
use zuno_catalog::skill::{SkillOptions, load};
use zuno_paths::Env;

fn assert_send<T: Send>(_value: T) {}

#[test]
fn loading_skills_can_be_awaited_from_a_send_context() {
    let env = Env::from_pairs(Vec::<(&str, String)>::new());
    let options = SkillOptions::new(
        PathBuf::from("/nonexistent"),
        None::<PathBuf>,
        &env,
        Vec::new(),
        Vec::new(),
    );

    assert_send(async move {
        let _skills = load(&options).await;
    });
}
