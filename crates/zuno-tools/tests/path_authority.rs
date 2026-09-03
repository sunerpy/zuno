//! Path authority: what the user authorized is what the write reaches.
//!
//! Every test here schedules its interference *inside the authorization window* —
//! the [`PermissionAsker`] is the only hook a test can use that runs after the tool
//! has resolved a path and before it touches the filesystem, which is exactly where
//! a real attacker or a real concurrent process lives. A test that swaps the
//! directory before the call proves nothing: the pre-existing symlink is already
//! resolved by `FileToolRuntime::resolve` and already produces an
//! `external_directory` prompt.

use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zuno_error::ToolError;
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
use zuno_tools::FileTools;

/// A permission asker that replaces a directory with a link while it answers.
///
/// `PermissionAsker::ask` is called from `FileToolRuntime::authorize`, so anything
/// this does happens strictly after the tool decided the path was inside the
/// workspace and strictly before the tool writes. That is the whole window the
/// time-of-check/time-of-use defect lives in.
///
/// `plant` is the kind of link the attacker leaves behind, because the two Windows
/// shapes are not equally available: a symlink needs a privilege, a junction does not.
struct SwapDirectoryForLink {
    directory: PathBuf,
    target: PathBuf,
    plant: fn(&Path, &Path) -> std::io::Result<()>,
    asks: Mutex<Vec<PermissionAsk>>,
    swapped: Mutex<bool>,
}

impl SwapDirectoryForLink {
    fn new(directory: PathBuf, target: PathBuf) -> Self {
        Self {
            directory,
            target,
            plant: symlink_dir,
            asks: Mutex::new(Vec::new()),
            swapped: Mutex::new(false),
        }
    }

    /// The same swap, planted as a directory junction rather than a symlink.
    #[cfg(windows)]
    fn junction(directory: PathBuf, target: PathBuf) -> Self {
        Self {
            plant: junction_dir,
            ..Self::new(directory, target)
        }
    }

    fn permissions(&self) -> Vec<String> {
        self.asks
            .lock()
            .expect("permission lock")
            .iter()
            .map(|ask| ask.permission.clone())
            .collect()
    }
}

#[async_trait]
impl PermissionAsker for SwapDirectoryForLink {
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.asks.lock().expect("permission lock").push(ask);
        let mut swapped = self.swapped.lock().expect("swap lock");
        if !*swapped {
            *swapped = true;
            std::fs::remove_dir_all(&self.directory).expect("remove the authorized directory");
            (self.plant)(&self.target, &self.directory).expect("plant the escaping link");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// A directory junction, which is what an unprivileged Windows attacker actually has.
///
/// `std` cannot create one, and `mklink /J` is a `cmd` builtin rather than a program,
/// so this shells out. A failure here is a broken fixture, not a skipped test: the
/// caller unwraps it.
#[cfg(windows)]
fn junction_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "mklink /J {} {} failed with {}: {}{}",
        link.display(),
        target.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    )))
}

fn context(permission: Arc<dyn PermissionAsker>) -> ToolContext {
    ToolContext::new(
        "session-path-authority",
        "message-path-authority",
        "call-path-authority",
        "build",
        permission,
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn file_write_refuses_a_directory_swapped_for_a_symlink_after_authorization() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let authorized = workspace.path().join("docs");
    std::fs::create_dir(&authorized).expect("the directory the user authorizes");
    let tools = FileTools::new(workspace.path()).expect("file tools");
    let permission = Arc::new(SwapDirectoryForLink::new(
        authorized.clone(),
        outside.path().to_owned(),
    ));

    let result = tools
        .write
        .execute(
            json!({
                "filePath": authorized.join("escape.txt"),
                "content": "exfiltrated\n",
            }),
            context(permission.clone()),
        )
        .await;

    let escaped = outside.path().join("escape.txt");
    assert!(
        !escaped.exists(),
        "write escaped the workspace: {} exists after only a workspace authorization; \
         permissions asked were {:?}",
        escaped.display(),
        permission.permissions()
    );
    let error = result.expect_err("a swapped ancestor must refuse the write");
    assert!(
        matches!(error, ToolError::InvalidArgs { .. }),
        "an ancestor that is not the authorized directory is a refusal, not a lost write: {error:?}"
    );
}

/// The same swap as above, planted as a junction, which needs no Windows privilege.
///
/// A symlink swap is the shape the portable test uses, but on Windows it requires
/// Developer Mode or `SeCreateSymbolicLinkPrivilege`. A directory junction requires
/// neither, so it is the swap an ordinary process can actually perform, and it is the
/// one the reparse-point refusal has to cover.
#[cfg(windows)]
#[tokio::test]
async fn file_write_refuses_a_directory_swapped_for_a_junction_after_authorization() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let authorized = workspace.path().join("docs");
    std::fs::create_dir(&authorized).expect("the directory the user authorizes");
    let tools = FileTools::new(workspace.path()).expect("file tools");
    let permission = Arc::new(SwapDirectoryForLink::junction(
        authorized.clone(),
        outside.path().to_owned(),
    ));

    let result = tools
        .write
        .execute(
            json!({
                "filePath": authorized.join("escape.txt"),
                "content": "exfiltrated\n",
            }),
            context(permission.clone()),
        )
        .await;

    let escaped = outside.path().join("escape.txt");
    assert!(
        !escaped.exists(),
        "write escaped the workspace through a junction: {} exists after only a \
         workspace authorization; permissions asked were {:?}",
        escaped.display(),
        permission.permissions()
    );
    let error = result.expect_err("a swapped ancestor must refuse the write");
    assert!(
        matches!(error, ToolError::InvalidArgs { .. }),
        "an ancestor that is not the authorized directory is a refusal, not a lost write: {error:?}"
    );
}

/// A permission asker that records every ask and allows them all.
struct Allowing {
    asks: Mutex<Vec<PermissionAsk>>,
}

impl Allowing {
    fn new() -> Self {
        Self {
            asks: Mutex::new(Vec::new()),
        }
    }

    fn permissions(&self) -> Vec<String> {
        self.asks
            .lock()
            .expect("permission lock")
            .iter()
            .map(|ask| ask.permission.clone())
            .collect()
    }

    fn patterns_for(&self, permission: &str) -> Vec<String> {
        self.asks
            .lock()
            .expect("permission lock")
            .iter()
            .filter(|ask| ask.permission == permission)
            .flat_map(|ask| ask.patterns.clone())
            .collect()
    }
}

#[async_trait]
impl PermissionAsker for Allowing {
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.asks.lock().expect("permission lock").push(ask);
        Ok(())
    }
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[tokio::test]
async fn file_edit_refuses_a_directory_swapped_for_a_symlink_after_authorization() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let authorized = workspace.path().join("docs");
    std::fs::create_dir(&authorized).expect("the directory the user authorizes");
    let edited = authorized.join("notes.md");
    std::fs::write(&edited, "secret\n").expect("the file the user is editing");
    // The attacker's directory holds a same-named decoy, so following the swapped link
    // would find a readable file and the edit would appear to succeed.
    std::fs::write(outside.path().join("notes.md"), "secret\n").expect("decoy");

    let tools = FileTools::new(workspace.path()).expect("file tools");
    // Read first: `edit` requires a current read receipt, and the receipt is taken
    // against the authorized file, before any swap.
    tools
        .read
        .execute(
            json!({ "filePath": edited }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("read the file the edit is authorized against");

    let permission = Arc::new(SwapDirectoryForLink::new(
        authorized.clone(),
        outside.path().to_owned(),
    ));
    let result = tools
        .edit
        .execute(
            json!({
                "filePath": edited,
                "edits": [{ "oldString": "secret", "newString": "exfiltrated" }],
            }),
            context(permission.clone()),
        )
        .await;

    assert_eq!(
        std::fs::read_to_string(outside.path().join("notes.md")).expect("the decoy"),
        "secret\n",
        "edit escaped the workspace; permissions asked were {:?}",
        permission.permissions()
    );
    result.expect_err("a swapped ancestor must refuse the edit");
}

#[tokio::test]
async fn file_read_refuses_a_directory_swapped_for_a_symlink_after_authorization() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let authorized = workspace.path().join("docs");
    std::fs::create_dir(&authorized).expect("the directory the user authorizes");
    let target = authorized.join("notes.md");
    std::fs::write(&target, "workspace content\n").expect("the file the user authorizes");
    std::fs::write(outside.path().join("notes.md"), "private content\n").expect("the secret");

    let tools = FileTools::new(workspace.path()).expect("file tools");
    let permission = Arc::new(SwapDirectoryForLink::new(
        authorized.clone(),
        outside.path().to_owned(),
    ));

    let result = tools
        .read
        .execute(json!({ "filePath": target }), context(permission.clone()))
        .await;

    match result {
        Ok(output) => assert!(
            !output.output.contains("private content"),
            "read disclosed a file outside the workspace after only a workspace authorization; \
             permissions asked were {:?}\n{}",
            permission.permissions(),
            output.output
        ),
        Err(error) => assert!(
            matches!(
                error,
                ToolError::InvalidArgs { .. } | ToolError::Failed { .. }
            ),
            "unexpected refusal: {error:?}"
        ),
    }
}

#[tokio::test]
async fn apply_patch_refuses_a_directory_swapped_for_a_symlink_after_authorization() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let authorized = workspace.path().join("docs");
    std::fs::create_dir(&authorized).expect("the directory the user authorizes");
    let tools = FileTools::new(workspace.path()).expect("file tools");
    let permission = Arc::new(SwapDirectoryForLink::new(
        authorized.clone(),
        outside.path().to_owned(),
    ));

    let result = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Add File: docs/escape.txt\n",
                    "+exfiltrated\n",
                    "*** End Patch\n",
                ),
            }),
            context(permission.clone()),
        )
        .await;

    assert!(
        !outside.path().join("escape.txt").exists(),
        "apply_patch escaped the workspace; permissions asked were {:?}",
        permission.permissions()
    );
    result.expect_err("a swapped ancestor must refuse the patch");
}

#[tokio::test]
async fn a_write_through_a_symlink_inside_the_workspace_reaches_its_destination_and_keeps_the_link()
{
    // The documented decision: `resolve` follows a symlinked leaf exactly once, before
    // it asks for permission, so the user authorizes the link's *destination*. The link
    // itself is never rewritten and never destroyed.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let real = workspace.path().join("real.txt");
    std::fs::write(&real, "before\n").expect("the real file");
    let link = workspace.path().join("link.txt");
    symlink_file(&real, &link).expect("a link the user legitimately writes through");

    let tools = FileTools::new(workspace.path()).expect("file tools");
    tools
        .read
        .execute(
            json!({ "filePath": link.clone() }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("read through the link");

    let permission = Arc::new(Allowing::new());
    tools
        .write
        .execute(
            json!({ "filePath": link, "content": "after\n" }),
            context(permission.clone()),
        )
        .await
        .expect("writing through a link inside the workspace keeps working");

    assert_eq!(
        std::fs::read_to_string(&real).expect("the destination"),
        "after\n",
        "the bytes must reach the link's destination"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("metadata")
            .is_symlink(),
        "the link must survive the write"
    );
    assert_eq!(
        permission.permissions(),
        vec!["edit".to_owned()],
        "a link that stays inside the workspace needs no external grant"
    );
}

#[tokio::test]
async fn a_write_through_a_symlink_leaving_the_workspace_requires_an_external_directory_grant() {
    // The other half of the decision: a link is followed, but the destination is what
    // gets authorized, so a link out of the workspace cannot launder an outside write
    // into a plain workspace prompt.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("temporary external directory");
    let destination = outside.path().join("real.txt");
    std::fs::write(&destination, "before\n").expect("the real file");
    let link = workspace.path().join("link.txt");
    symlink_file(&destination, &link).expect("a link out of the workspace");

    let tools = FileTools::new(workspace.path()).expect("file tools");
    tools
        .read
        .execute(
            json!({ "filePath": link.clone() }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("read through the link");

    let permission = Arc::new(Allowing::new());
    tools
        .write
        .execute(
            json!({ "filePath": link, "content": "after\n" }),
            context(permission.clone()),
        )
        .await
        .expect("an explicitly granted external write succeeds");

    assert_eq!(
        permission.permissions(),
        vec!["external_directory".to_owned(), "edit".to_owned()],
        "the destination outside the workspace must be prompted for by itself"
    );
    assert_eq!(
        permission.patterns_for("external_directory"),
        vec![format!(
            "{}/*",
            zuno_paths::wire_path(&outside.path().canonicalize().expect("canonical"))
        )],
        "the grant names the destination's directory, not the link's"
    );
    assert_eq!(
        std::fs::read_to_string(&destination).expect("the destination"),
        "after\n"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("metadata")
            .is_symlink(),
        "the link must survive the write"
    );
}

#[tokio::test]
async fn a_write_is_never_observable_as_a_torn_or_empty_file() {
    // A publication renames a completed temporary file over the destination, so a
    // concurrent reader sees either the whole previous content or the whole new
    // content. The truncate-then-write it replaced exposed an empty and then a
    // partial file for as long as the write took.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let path = workspace.path().join("large.txt");
    let old = "o".repeat(2 * 1024 * 1024);
    let new = "n".repeat(2 * 1024 * 1024);
    std::fs::write(&path, &old).expect("the previous content");

    let tools = FileTools::new(workspace.path()).expect("file tools");
    tools
        .read
        .execute(
            json!({ "filePath": path.clone(), "limit": 1 }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("read for the write receipt");

    let observed = Arc::new(Mutex::new(Vec::<usize>::new()));
    let watcher = {
        let observed = Arc::clone(&observed);
        let path = path.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(bytes) = std::fs::read(&path) {
                    observed.lock().expect("observed lock").push(bytes.len());
                }
            }
        });
        (handle, stop)
    };

    tools
        .write
        .execute(
            json!({ "filePath": path.clone(), "content": new.clone() }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("write");

    watcher.1.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher.0.join().expect("watcher");

    let lengths = observed.lock().expect("observed lock").clone();
    assert!(
        !lengths.is_empty(),
        "the watcher must have observed the file"
    );
    for length in lengths {
        assert!(
            length == old.len() || length == new.len(),
            "a reader observed {length} bytes, which is neither the previous {} nor the new {}",
            old.len(),
            new.len()
        );
    }
    assert_eq!(std::fs::read_to_string(&path).expect("final"), new);
}

#[tokio::test]
async fn a_write_leaves_no_temporary_file_behind() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let path = workspace.path().join("notes.txt");
    let tools = FileTools::new(workspace.path()).expect("file tools");

    tools
        .write
        .execute(
            json!({ "filePath": path.clone(), "content": "one\n" }),
            context(Arc::new(Allowing::new())),
        )
        .await
        .expect("write");

    let entries: Vec<String> = std::fs::read_dir(workspace.path())
        .expect("list the workspace")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(entries, vec!["notes.txt".to_owned()]);
}
