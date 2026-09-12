# Child workspace preparation

Configured enterprise children now run with a separate Docker workspace cloned
from a named, immutable parent snapshot. Workers may execute on different Linux
machines. The first Docker fork adapter requires parent and child environments to
belong to the same configured gateway; cross-gateway snapshot transfer is not
advertised by this adapter.

## Initialize a project

An authenticated user in a configured approval application can initialize a new
root session before its first input. Create the session, then call
`POST /sessions/{session}/workspace/imports` with `requestId`,
`expectedInputVersion: "0"`, the archive `sha256`, and decimal-string `bytes`.
Upload the same uncompressed tar bytes to
`PUT /sessions/{session}/workspace/imports/{import}/archive` using
`Content-Type: application/x-tar`. API and BFF prefixes share this contract.
`GET /sessions/{session}/workspace/imports/{import}` returns the typed state.

The archive must place its entries under `workspace/`, contain at most 512 MiB
and use supported UTF-8 file, directory, symlink or hardlink entries. Special files,
set-id modes, escaping links, duplicates and structurally inconsistent trees are
rejected. Initial import normalizes UID/GID to the runtime user (`0:0`) while
preserving content, ordinary permissions and safe links. It uses private streaming
files and never extracts into the gateway host.

Import admission serializes with first-input admission. While upload or restore
is pending, no Job is accepted for that session. The control plane fixes owner,
profile and environment; a separate short-lived upload ticket cannot execute
commands or read other resources. After byte validation, the gateway rechecks
current initialization authority, publishes through the existing durable fork
mechanism and commits a matching receipt. A ready workspace is always opened as
existing; a missing volume cannot silently become empty.

Identical requests/uploads reuse the same result, including concurrent retries;
different bytes or metadata conflict. `DELETE` on the import resource cancels an
upload before initialization begins. Once restoring, retry the same archive or
inspect the import state. Ready data cannot be replaced through initialization.
A cancelled upload can be replaced before any input is admitted. This initial
adapter pins the selected configuration; profile/environment changes need an
explicit migration. Later Agent commands still require their own approval.

The SDK provides `beginWorkspaceImport`, `workspaceImport`,
`uploadWorkspaceArchive` and `cancelWorkspaceImport`. A ready import projects as
an artifact with `ViewWorkspaceImport`, without private assignment or credentials.
PostgreSQL format 18 preserves import state across control-plane restarts.

## Configure a target

Install the child definition on the control plane and compatible Workers. Obtain
its normalized reference without starting a service or loading credentials:

```sh
zuno-enterprise --definition-ref /etc/zuno-enterprise-preview/helper.json
```

Add the returned object to the parent's optional `delegation.targets`:

```json
{
  "delegation": {
    "targets": [
      {"id": "helper", "version": 1, "sha256": "<digest from --definition-ref>"}
    ],
    "maximumDepth": 2,
    "maximumChildren": 4
  }
}
```

This fragment is a template. A target must match its exact ID, version and digest,
workspace and gateway. Changing child bytes requires updating the parent
reference and version. Each Agent name resolves to one installed child definition.
The child uses that definition's native provider/model and credentials binding;
ordinary task calls cannot supply arbitrary model/effort option overrides.

The Worker installs native `task` only for a definition with valid targets.
Task planning retains the native contract and target checks. Delegation depth is
persisted in the child session and can only narrow on further delegation or
resume. A child's configuration cannot raise the parent's effective limit.

## Admission and recovery

The first child request records intent and returns its stable Job/session IDs.
A child requiring a workspace remains unclaimable until preparation is durably
confirmed. Background dispatch also waits for this preparation before returning
an executable Job handle.

Worker protocol 11 requests `PrepareChildWorkspace` through gateway protocol 4.
The request carries only a staged child Job ID. The control plane verifies the
parent lease and child relation, resolves both environment specifications and
records the workspace admission. The Worker cannot choose another volume, image,
gateway or path.

The gateway captures a snapshot named from the stable child Job, then restores it
to the target workspace. Ledger format 3 records fork intent before Docker writes,
including a random ownership nonce on the target volume. Publication of the
environment and fork receipt is atomic. A retry of a committed fork returns the
existing environment, preserving later child edits.

Interrupted unpublished restoration can discard and reconstruct only its own
nonce-marked volume and never-started transfer helper. Matching names or ordinary
environment labels alone do not authorize deletion. `acquire` cannot turn a
partially restored target into an executable empty environment.

The gateway sends an authenticated preparation receipt to the control plane.
Late truthful receipts may be stored after the admitting lease expires; they do
not restore that Worker's authority. An identical retry returns the saved receipt
without another copy. Parent checkpoint admission (or the final background
admission) still verifies current authority before starting the child.

Existing child sessions retain their workspace on `task_id` continuation. A
prepared child uses `get`, not an empty-volume fallback, when it acquires an
execution environment. Lost environments require explicit recovery.

Workspace inheritance does not approve commands. Child command preparation and
execution use the same current-user approval and fencing checks as root commands.
Child writes stay in the child volume; integrating them into the parent remains
a separate approved merge operation.

## Approved workspace merge

Definitions with an installed child catalog expose `workspace_merge`. Arguments
are `childJobId` and optional `resolutions`, a map of logical paths to `parent`
or `child`. The source must be a completed descendant of this Job with no newer
input and a provable fork baseline on the assigned gateway. Workflow nodes use
the original group's fork as their baseline. Unproven legacy/resumed-child
origins fail closed.

The gateway compares baseline, parent and child snapshots. It preserves
parent-only edits, accepts independent child edits and reports simultaneous
changes to one entry as conflicts. Comparison is per entry; it does not perform
line-level text merging. File/directory and link conflicts require consistent
explicit resolutions before approval. Changing choices changes the binding.
Empty changes return without mutation.

Plans retain file sizes/digests, permissions and numeric ownership. Binary files,
symlinks, hardlinks and root metadata are preserved without host extraction.
Limits are 512 MiB per snapshot, 50,000 tree entries, 1,024 changed entries and a
512 KiB manifest. UTF-8 paths are logical names; `.` identifies the workspace root.

Every applied merge requires current human approval. Ownership/mode changes and
sensitive repository metadata require a designated approver. Under either API
prefix, `GET /approvals/{approval}/merge` returns the typed plan.
`GET /approvals/{approval}/merge/content?side=parent&path=...` streams exact
`base`, `parent` or `child` bytes for a changed entry. An independent short-lived
read ticket is redeemed by the authenticated gateway and rechecks viewer policy.
It cannot execute tools and never enters a Web DTO. Downloads are no-store/nosniff
attachments with an immutable SHA-256; the control plane checks size and digest.

Approval binds the Job, original invocation, plan, lineage and resource versions.
Approval checking and execution admission commit together. The Worker then waits
durably and releases its slot. A bounded gateway task restores a fresh volume,
reads it back and compares the tree, then atomically publishes its pointer,
revision and receipt. The parent is never partially overwritten.

The journal preserves reservations, retry deadlines and receipt acknowledgements.
Restart reconstructs execution; a lost response returns the original receipt.
Cancellation before submit creates a never-startable tombstone without seizing
another operation's slot. Cancellation and publication compete atomically;
committed facts remain authoritative. Truthful receipts remain deliverable after
Worker lease loss and are consumed once.

Gateway ledger 4 and PostgreSQL preview format 17 add guarded migrations.
Worker protocol 11 and gateway protocol 4 require matching roles. The SDK exposes
`mergeReview` and streaming `mergeContent`; `UiAction::ViewWorkspaceMerge` identifies
review. App UI remains deferred to Penpot. Cross-gateway transfer, remote artifact
storage and full retention/backup/rolling-upgrade acceptance remain separate work.

## Migrations and evidence

PostgreSQL format 11 adds scoped workspace admission/receipts and inherited depth
limits, preserving formats 1–10. Legacy children do not infer a wider delegation
grant. Exact old-format tests retain session, Memory and staged-child values and
roll back before the marker on injected failure. Gateway format 3 upgrades formats
1–2 atomically, preserving environments, command receipts and snapshots.

The native Docker tests cover fork retry after child edits and restart, failed
publication, refusal to acquire unpublished targets and refusal to adopt another
ledger's volume. The authenticated gateway test drops a workspace acknowledgement,
retrieves the same receipt, verifies inherited files and proves child writes leave
the parent unchanged.

The executable fixture runs one control plane, one gateway and two Workers with
two users, real parent/child Jobs, fourteen provider requests, four human command
approvals, private Memory and clean SIGTERM shutdown. These are deterministic
provider fixtures. Approved merge, cross-gateway transfer,
Workflow/Council and the remaining P5–P6 acceptance continue under the main plan.

See [中文](WORKSPACES.zh.md), [child dispatch](CHILDREN.md),
[deployment](DEPLOYMENT.md) and [platforms](PLATFORMS.md).
