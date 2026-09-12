# Durable enterprise workflows

Enterprise definitions may install named workflow templates. The model selects a
template and supplies its task; the definition fixes node identities, dependency
edges, child Agent/model references and logical parallelism. Native `WorkflowTool`
and `WorkflowHost` remain the consumer/host boundary. A foreground invocation
returns a typed pending wait until its original result can be consumed.

## Definition

Add `workflows` to the parent [definition](DEPLOYMENT.md):

```json
{
  "workflows": [{
    "name": "inspection",
    "sourceId": "organization:inspection:v1",
    "maxParallel": 2,
    "maxAgents": 3,
    "nodes": [
      {"id": "scan", "agent": "helper", "prompt": "Collect evidence.", "description": null, "dependsOn": []},
      {"id": "review", "agent": "helper", "prompt": "Review constraints.", "description": null, "dependsOn": []},
      {"id": "conclude", "agent": "helper", "prompt": "Combine the findings.", "description": null, "dependsOn": ["scan", "review"]}
    ]
  }]
}
```

Every node Agent must occur in `delegation.targets`. The parent must allow at
least two delegation levels for the coordinator and its Agent nodes.
`maxAgents` must fit `delegation.maximumChildren`, with at most 64 nodes and
32 templates per definition. Cycles, duplicate identities, missing dependencies,
unknown Agents and capacity expansion fail during configuration. Changing a graph
changes the parent definition digest and requires a new definition version.

## Ownership and execution

The workflow owns a native child Job and an internal coordination session. This
Job is never claimed by an Agent Worker or sent to a provider. The control plane
holds short coordination transactions to inspect completed dependencies and admit
eligible native child Jobs. Nodes use the existing Worker leases, bounded driver,
approval and operation gateways. Waiting releases Worker capacity while retaining
one logical node slot. A completed branch immediately makes capacity available
to its eligible successor, without waiting for an unrelated slow branch.

The original parent stages the fixed plan, forks an immutable coordination
workspace and prepares independent node workspaces. This bounded preparation
still holds the original Worker slot; it performs no node model/tool execution.
Nodes may select another configured gateway; authenticated snapshot transfer
prepares their branches before execution. After preparation,
foreground activation and the original parent's waiting checkpoint commit
together. Background workflows activate through preparation and retain the
existing `nextStep`/`quiet` behavior.

Each node starts from the coordination workspace. Writes stay in its branch;
dependency ordering does not merge filesystem changes. Approved patch/commit
integration remains a separate capability under the implementation plan.

Only successfully completed dependencies supply input to a successor. Their
bounded text, stable identities and completion digests are recorded with the
exact admitted node input. Result text is marked as data, not additional authority.
Local and enterprise workflow hosts share this input renderer and DAG decisions.
Provider-private reasoning is excluded from child results.

Node identity and input do not change when another Worker takes over. Group
completion, the native Job result and the original parent's completion envelope
commit together. Consumption uses the existing stable completion marker and
checkpoint transaction. An uncertain child result is retained and consumed once;
the shared driver then requires inspection before another tool or provider call.
Failure/cancellation stops sibling Jobs through the same durable tree-cancellation
and gateway-stop machinery.

The coordination session is absent from the conversation list and rejects user
input and arbitrary child-session continuation. It is internal state, not another
interactive Agent. Current organization authorization is checked before node
admission; stored definitions do not restore revoked permissions.

## Client view and protocol

`GET /jobs/{job}/workflow`, relative to the existing API or BFF prefix, returns
`WorkflowRunView`: run identity/state and ordered typed node views with dependency
IDs, Job IDs and public wait targets. It omits private plans, prompts, configuration
snapshots, leases and credentials. Reads recheck the current authenticated owner.
`UiAction::ViewWorkflow` is projected from the data-owner relation between the
original message Job and invocation; it is not inferred from a tool name.

The typed view is the contract for a future App client. App design and UI delivery are deferred to the Penpot design phase; experimental UI code is not part of this backend delivery. The server remains authoritative for approval eligibility and the exact operation binding.

PostgreSQL preview format 15 adds workflow/node coordination and frozen dependency
inputs. The exact format-14 fixture preserves sessions, messages, Memory, committed
frames and live rows through migration and rollback. Earlier supported formats
advance in the same guarded transaction. Worker protocol 11 carries workflow
commands; checkpoint schema 4 is unchanged and gateway protocol 5 validates Workflow workspace preparation. Use matching
control-plane/Worker versions and regenerate the public SDK when updating these contracts.

## Durable Council

An Agent definition can install `councils`. Each entry contains a `preset`,
`synthesis` configuration reference and a `repairs` map from seat Agent name to
configuration reference. Generate exact references with `--definition-ref`.
The preset uses the native Council fields: `name`, `sourceId`, `seats`,
`quorum`, `maxParallel`, `deadlineMs`, `seatOutputBytes`,
`retryPolicy.maxRetries`, and `synthesisPolicy.{timeoutMs,maxInputBytes}`.
The native tool ID remains `council_run`; callers choose a preset and question.

All seat, repair and synthesis definitions must be installed and explicitly
allowed by the parent's `delegation.targets`, with the same logical workspace.
Repair and synthesis definitions require `agent.mode: "completion"` and no
environment, delegation, Workflow or Council catalog. A repair must retain its
seat's exact model binding, including credential reference and provider options.
Changing the model during format correction is rejected at configuration time.
The child limit must cover `seats × (maxRetries + 1) + 1`; at least two delegation
levels are required. There are at most 12 seats, 3 format corrections per seat,
32 presets per definition and 10 minutes per run.

Council reuses native Workflow coordination, child Jobs, workspace forks,
durable waits and completion consumption. The original caller releases its
Worker slot after preparation. Seat waits still occupy logical seat capacity.
Every initial seat gets its own workspace branch. Only completed public answer
text enters validation; private reasoning and tool transcripts do not enter
repair or synthesis prompts. An invalid answer may produce a bounded model-only
format-correction Job; it never replays the original Agent's commands.
Repair attempts and source completion digests remain durable across restarts.

The database establishes the run deadline at activation after workspace
preparation. Seat time includes queueing, approval waits and repairs; synthesis
time is reserved inside that deadline. Claims and renewals cannot extend leases
past the applicable deadline. Expired seats are cancelled through the same
native task tree and gateway outbox. Pending external receipts hold synthesis
for at most five seconds, bounded by the remaining run deadline. Unconfirmed
effects leave the Council `uncertain`; a later legitimate receipt is retained
without silently continuing an uncertain parent.

All seats settle before synthesis, retaining order and dissent. A quorum counts
only validated answers completed before the seat deadline. Missing, timed-out,
invalid or failed seats are recorded but cannot vote. The synthesis Job has no
tools or resident Memory. Its frozen input is bounded; overflow, insufficient
quorum or synthesis timeout produces failure rather than fabricated success.
Cancellation fences the tree, and a new Worker rechecks current authority.

PostgreSQL format 16 adds scoped Council state, seat outcomes, attempt provenance
and optional Job deadlines. Exact format-15 migration preserves existing Jobs,
Workflow/node rows, messages, Memory and activity frames, with rollback before
the marker. Worker protocol 11 adds internal Council admission; gateway protocol
5 supports remote node workspaces; checkpoint schema 4 remains unchanged. `WorkflowRunView.kind` distinguishes
`workflow` and `council`; its optional `council` view carries typed phases, seat
states, attempt counts and exact decimal deadlines. Private configuration,
prompts, leases and credentials remain outside this public view.

Generic remote Council does not advertise native review binding. Review-specific
evidence acceptance and approved workspace merge remain separate capabilities.

## Acceptance scope

Database tests cover activation rollback, concurrent claims, waiting/refill,
dependency input, replacement Workers, terminal consumption, failure, uncertainty,
cancellation and owner boundaries. Native executable tests exercise a control
plane, two gateways and two Workers with a three-node DAG, four explicit command
approvals, isolated workspace forks and a dependent node starting while another
node still waits for approval. Identity and model providers in these tests are
fixtures; native Linux amd64/arm64 CI supplies platform evidence.

Council database cases cover model-only repair, quorum, bounded deadlines,
cancelled waits, lease fencing, late operation receipts and uncertain outcomes.
The native test adds two isolated, explicitly approved seat commands, one
format correction and a model-only synthesis before consuming the original
parent call. No browser is required for this backend evidence.

Approved workspace merges and cross-gateway transfer use the configured workspace
providers. Full P5–P6 operational acceptance continues under [the plan](PLAN.zh.md).
The independent preview release remains disabled until its acceptance is complete.

See [中文](WORKFLOW.zh.md), [children](CHILDREN.md), [workspace forks](WORKSPACES.md),
[Web](WEB.md) and [status](STATUS.md).
