# Bounded Agent Worker host

`zuno-worker::runtime::WorkerRuntime` owns bounded execution slots, compatible
claims, grant renewal and draining. A `WorkerServiceFactory` provides the exact
installed configuration, provider registry, model/Agent resolver, tool dispatcher,
shared `AgentDriver`, budget policy and executor-visible directory for each Job.
The runtime calls the existing `AgentDriver::advance`; it does not copy the agent
loop or create a second task identity.

The `zuno-enterprise` binary now composes this library with the native provider
factories and gateway tools; see [deployment](DEPLOYMENT.md). The personal TUI/ACP
platform contract is unchanged; see [platform boundaries](PLATFORMS.md).

## Claims and lifetime

Internal Worker protocol 9 requires the protocol version and a bounded, nonempty
set of configuration references on every claim. The PostgreSQL claim query
matches the exact ID, version and SHA before acquiring the session. A Worker with
an older definition leaves incompatible work eligible for another Worker.
Protocol mismatch is rejected before a claim. This is compatibility routing, not
authentication; workload identity, tenant and current organization policy are
still checked.

Grant responses include their remaining lifetime. The Worker starts a monotonic
deadline before its HTTP request, conservatively subtracting response latency.
Its renewal interval is capped at one third of the remaining lifetime, including
during service initialization and input materialization. A slow renewal does not
block the advancing future; expiry or renewal failure stops it. No failed POST is
mechanically repeated and no loss of authority is converted into success.
Database time and current epoch remain authoritative for every durable mutation.

Renewal and the final checkpoint/finish POST are serialized for one claim.
The checkpoint transaction releases the database lease before its HTTP response
arrives; a concurrent renewal must not mistake that normal handoff for lease loss.
After a validated acknowledgement the local claim is retired and no further
renewal is sent. An unknown/failed commit response still grants no replay.

Entering the final POST also closes new state and gateway admission locally.
If its response arrives after the old grant deadline, the Worker may wait up to
30 seconds for that one acknowledgement. This wait grants no execution authority:
the kernel has already finished its provider/tool segment, and PostgreSQL still
checks the deadline inside the commit transaction. A missing response remains an
unconfirmed boundary, never a reason to repeat the POST.

The root input's database admission timestamp travels with its stable ID. The
materializer reuses that timestamp and part identity across Worker changes.
Consumed input and provider-applied input retain their separate durable meanings.
Checkpoint schema 4 distinguishes unsubmitted and submitted waits. Schema 3
remains readable as unsubmitted state; accumulated budgets are preserved.

The internal finish route accepts fenced failure, cancellation or uncertainty.
Successful completion must use the kernel's atomic settlement path. It cannot be
manufactured through this route.

## Capacity, draining and recovery

One claimed advance occupies one slot. Progress or durable waiting releases that
slot after the kernel commits the boundary. Another compatible Worker may claim
the next advance; waiting does not require a resident future.

Shutdown stops new claims, including an outstanding claim request, then allows
existing advances to drain within a configured deadline. At the hard deadline
unfinished futures are aborted. Their durable started/operation records remain
available for conservative recovery; shutdown does not replay or declare an
external effect complete.

Live observations belong to the current attempt. Durable history is read from the
state service, and a slow or disconnected UI must not become the authoritative
owner of the Worker lifecycle. Observer callbacks enqueue without network waits.
A profile retaining an event sender cannot delay a committed advance; only the
already buffered live events receive a bounded final drain.

## Verification and remaining assembly

`python3 scripts/check_enterprise_postgres.py` runs two Worker runtime instances
against real PostgreSQL and verified HTTPS. Configuration loading and provider
responses both exceed the initial lease. The test checks renewal, checkpoint
handoff, exactly one original input and tool execution, protocol/configuration
rejection before claiming, and stopping before dispatch when renewal fails.
The instance test runs within one test process; it is not independent-process
deployment evidence.

The Docker runner additionally verifies the executable control plane, gateway and
two independent Workers through the real provider transport and approval/result
chain. Remote cancellation and remaining P3–P6 acceptance are still required.
Preview publication remains disabled.

See [中文](WORKERS.zh.md), [state service](POSTGRES.md) and [status](STATUS.md).
