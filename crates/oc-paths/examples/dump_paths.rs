//! Print the same nine lines as `opencode debug paths`.
//!
//! This is the shell-visible half of the differential comparison: it lets a
//! script diff the Rust layout against the real binary without going through the
//! test harness, which is how the QA transcripts in
//! `.omo/evidence/task-4-opencode-rust.txt` are produced. Todo 6's `Subject`
//! harness needs exactly this shape too.
//!
//! ```sh
//! diff <(opencode debug paths) <(cargo run -q -p oc-paths --example dump_paths)
//! ```

fn main() {
    print!("{}", oc_paths::global().debug_paths_dump());
}
