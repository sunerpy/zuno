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
The current Docker adapter requires one assigned gateway. After preparation,
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
advance in the same guarded transaction. Worker protocol 9 carries workflow
commands; checkpoint schema 4 is unchanged and gateway protocol 3 validates Workflow workspace preparation. Use matching
control-plane/Worker versions and regenerate the public SDK with the Web bundle.

## Acceptance scope

Database tests cover activation rollback, concurrent claims, waiting/refill,
dependency input, replacement Workers, terminal consumption, failure, uncertainty,
cancellation and owner boundaries. Native executable tests exercise a control
plane, gateway and two Workers with a three-node DAG, four explicit command
approvals, isolated workspace forks and a dependent node starting while another
node still waits for approval. Identity and model providers in these tests are
fixtures; native Linux amd64/arm64 CI supplies platform evidence.

Distributed Council synthesis/quorum/deadline execution, approved workspace
merges, cross-gateway transfer and the remaining P5–P6 operational acceptance
continue under [the plan](PLAN.zh.md). They are not enabled by a workflow template.
The independent preview release remains disabled until its acceptance is complete.

See [中文](WORKFLOW.zh.md), [children](CHILDREN.md), [workspace forks](WORKSPACES.md),
[Web](WEB.md) and [status](STATUS.md).
