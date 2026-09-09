use std::sync::{Arc, Barrier};
use tempfile::TempDir;
use zuno_memory::{MemoryStore, Scope};

#[test]
fn concurrent_replacements_of_one_snapshot_cannot_both_succeed() {
    let directory = TempDir::new().expect("temporary memory directory");
    for iteration in 0..24 {
        let path = directory.path().join(format!("memory-{iteration}.md"));
        let first = MemoryStore::open(Scope::Project, path.clone()).expect("first reader");
        let second = MemoryStore::open(Scope::Project, path.clone()).expect("second reader");
        let barrier = Arc::new(Barrier::new(2));
        let workers = [first, second]
            .into_iter()
            .enumerate()
            .map(|(writer, mut store)| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let replacement = format!(
                        "Writer {writer}: {}",
                        "Preserve verified task evidence. ".repeat(30),
                    );
                    barrier.wait();
                    store.replace_exact(&[], &[replacement]).is_ok()
                })
            })
            .collect::<Vec<_>>();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().expect("memory writer"))
            .filter(|success| *success)
            .count();
        assert_eq!(
            successes, 1,
            "both writers compared the same empty snapshot"
        );
        let observed = MemoryStore::open(Scope::Project, path).expect("published memory");
        assert_eq!(observed.entries().len(), 1);
    }
}
