# Enterprise admission quotas

`QuotaStore` is a data-owner policy port. PostgreSQL keeps a tenant policy and
applies its limits independently to each authenticated owner. Counts come from
durable Jobs and database-time leases; process restarts do not reset them.

| Resource | Default | What counts |
| --- | ---: | --- |
| rootSessions | 2048 | Retained sessions without a parent |
| rootJobs | 64 | User-input Jobs that are not completed, failed or cancelled |
| childJobs | 1024 | Staged or unfinished child/Workflow/Council Jobs |
| executions | 8 | Unexpired foreground/child execution leases |
| learningJobs | 64 | Queued, running or uncertain learning Jobs |
| learningExecutions | 2 | Unexpired background learning leases |

Paused and uncertain work retains its logical admission cost. Waiting releases
execution capacity. Children use their own logical limit, so a parent waiting on
a child does not consume the child's execution slot. Durable completion delivery
is not a new user admission and cannot lose a child result because the user queue
is full. Existing request receipts are returned before quota admission checks.

`GET /api/v1/quotas` returns the current policy and only the caller's usage.
`PUT /api/v1/quotas` requires an active organization administrator using a trusted
review application. The request contains `requestId`, `expectedRevision` and the
complete `limits` object. Policy updates, audit and idempotent receipts commit
atomically. Lowering a limit never cancels accepted work or rewrites its budget;
new admission waits for capacity to become available. Session quota counts retained
roots and is not a retention or deletion policy.

Quota rejection returns HTTP 429 with `quota_exceeded`. It does not consume an
input version, create a Job or register a successful request receipt. Clients must
not mechanically repeat a write. Inspect usage or explicitly adjust/cancel work.
The SDK offers `quotas` and `replaceQuotas`; revisions use exact decimal strings.

Claims are serialized per owner around capacity checks and lease admission. A
capped owner does not prevent another owner from being selected. Background
learning has a separate durable round-robin clock; continuous learning for one
owner cannot monopolize the next eligible claim merely because foreground queues
are empty. When learning admission is full, unprocessed source watermarks remain
pending; completed extraction may settle while its maintenance wake is deferred.

PostgreSQL preview format 29 adds quota policy/audit and the learning scheduling
clock. Existing organizations receive the default policy atomically with migration;
new organizations receive it during bootstrap. These quotas complement per-Job
token/tool/time budgets and gateway limits. They do not claim tenant billing,
aggregate storage retention or external-provider cost enforcement.
