# Durable operation result delivery

Execution admission, result submission and parent consumption are separate facts.
An operation response is not a parent-consumption acknowledgement.

## Admission and late receipts

The gateway authorization path records the immutable operation, assigned gateway,
observed environment and authorized attempt in the same PostgreSQL transaction
that checks the current lease, organization policy and approval. Repeated
authorization cannot replace the logical operation's parameters.

`OperationCompletion` contains the original operation and execution identity,
terminal receipt, ordered output blocks and an explicit truncation flag. Only an
authenticated gateway may publish it. The control plane checks the assigned
gateway, logical coordinates, parameters and an admitted attempt before storing
the fact. The old Worker's lease may already have expired: recording a true late
receipt does not grant execution or revive that lease.

An identical repeated receipt is idempotent. A different receipt/output, wrong
gateway or unadmitted attempt is rejected. Receipt storage, completion event and
readiness of matching operation waits commit atomically. Wait registration checks
already stored results to close the completion-before-wait race.

## Gateway ledger and acknowledgement

Gateway ledger format 2 adds durable delivery state. The execution owner scans
bounded batches in rotating order, observes the original container, captures its
terminal result, publishes it and only then records acknowledgement. Captured
results remain immutable and survive process restart.

`DockerGateway::deliver_completions` and the execution-service wrapper implement
one bounded scan. Host lifecycle code must invoke them with interruptible backoff;
the independent production delivery supervisor and role startup are still
separate implementation work.

The captured output is bounded to 64 KiB and 8,192 blocks so its serialized
completion fits the private transport. Truncation is explicit. Full-log object
storage/retention remains a later adapter; this bounded result is not a promise
that every byte of a truncated log survives environment deletion.

Environment release refuses any operation whose result has not been durably
acknowledged. A lost response after the control plane committed keeps the gateway
entry pending. Repeating that publication retrieves the same stored fact; it does
not rerun the command or append a second completion event.

Uncertain outcomes do not produce successful completion. They retain their
inspection obligation and environment hold.

## Parent consumption and migration

The control plane constructs the tool result from the verified receipt and output.
It uses the existing `WaitCompletionStore` and bounded driver. The parent writes
the original tool result and consumed marker with its next checkpoint. A paused
parent keeps the result without automatically resuming.

Gateway format 1 upgrades atomically while preserving environments, operations and
snapshots; old operations begin unacknowledged. PostgreSQL format 8 adds scoped
operation/admission-attempt records. Supported formats 1–7 migrate forward in one
transaction. Exact old fixtures and injected marker/DDL failures verify retained
rows and rollback.

Run `python3 scripts/check_enterprise_docker.py` for the combined native gateway,
PostgreSQL and HTTPS contracts. Tests exercise post-commit response loss, repeated
publication, expired Worker leases, forged attempts/changed outputs, release
before acknowledgement, restart and migration preservation.

See [中文](OPERATION_RESULTS.zh.md), [gateway](GATEWAY.md) and
[waiting](WAITING.md). These storage/transport pieces do not by themselves enable
an enterprise runtime command or certify the remaining Worker/Workflow/Web work.
