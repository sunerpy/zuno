# Child workspace preparation

Configured enterprise children now run with a separate Docker workspace cloned
from a named, immutable parent snapshot. Workers may execute on different Linux
machines. The first Docker fork adapter requires parent and child environments to
belong to the same configured gateway; cross-gateway snapshot transfer is not
advertised by this adapter.

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

Worker protocol 7 requests `PrepareChildWorkspace` through gateway protocol 2.
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
provider fixtures. Full cancellation/approved merge, cross-gateway transfer,
Workflow/Council and the remaining P5–P6 acceptance continue under the main plan.

See [中文](WORKSPACES.zh.md), [child dispatch](CHILDREN.md),
[deployment](DEPLOYMENT.md) and [platforms](PLATFORMS.md).
