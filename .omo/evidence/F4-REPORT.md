# F4 Scope Fidelity Review

## Verdict: REJECT

The implementation contains substantial work that is inside the approved scope, but it does not faithfully close the contract frozen in `.omo/plans/opencode-rust.md`. The blockers below are direct contradictions between the plan and the checked-in artifact; they are not style preferences or requests for extra features.

## Blockers

### 1. The required `/api` compatibility surface is incomplete and the required behavior matrix does not exist

The plan requires every upstream path+method to exist and behave, with a per-group differential matrix comparing status, normalized body, and observable side effects (`.omo/plans/opencode-rust.md:41,59,1127`). The artifact explicitly exempts two upstream operations:

- `GET /api/event`
- `GET /api/session/{sessionID}/event`

`crates/oc-testkit/tests/compat_suite.rs:71-82,693-740` reports only 56 of 58 captured upstream operations as served and classifies the two omissions as known gaps. This is honest reporting, but a known gap still fails the required-subset contract. The same suite marks the API surface `PartiallyCompared` (`:124-131`) and explicitly normalizes away OpenAPI schema bodies (`:305-309`).

The cited replacement, `crates/oc-server/tests/api.rs`, is a set of local implementation tests, not the required operation-by-operation oracle differential. It checks the path set and selected local behaviors; it does not drive both binaries for every operation and compare status, normalized response body, and side effects. It also deliberately verifies that a registered upstream endpoint may return `501 not_implemented` (`api.rs:233-243`), which is reachability rather than behavioral parity. Therefore success criterion 4 is not met even if the current test suite is green.

**Required resolution:** register the two `/api` SSE routes and add the promised per-operation differential behavior matrix. If any operation is intentionally different, declare and document the behavior as a divergence rather than treating omission or a `501` stub as compatibility.

### 2. The divergence allow-list knowingly omits implemented behavioral differences

Success criterion 17 requires every intentional upstream divergence to be allow-listed, reasoned, documented, and asserted (`.omo/plans/opencode-rust.md:1149-1150`). `docs/divergences.toml` contains eight entries, but `crates/oc-testkit/tests/compat_suite.rs:1055-1100` separately records six “nominated divergences” and the test at `:774-789` positively asserts that they remain outside the allow-list:

- `subpath-is-implemented`
- `subpath-matches-literally`
- `context-md-excluded`
- `malformed-auth-json-is-an-error`
- `failed-format-restores-pre-format-bytes`
- `memory-subsystem`

The memory nomination overlaps the declared cross-session-memory divergence, but the other five are distinct observable decisions. Keeping them in a second reporting structure does not satisfy the single allow-list contract; the suite currently institutionalizes the omission instead of failing on it.

**Required resolution:** reconcile each nomination with `docs/divergences.toml`: add the distinct behavioral differences with reasons and behavioral assertions, merge any true duplicate into the existing entry, regenerate the divergence documentation, and make the compatibility gate fail when a nominated behavioral difference is not declared.

### 3. The closed 34-crate workspace roster has silently expanded to 36 crates

The plan deliberately freezes an exact 34-crate roster and says that any addition must be an explicit roster change with a matching count and fixture update (`.omo/plans/opencode-rust.md:100-103`). `crates.expected` still lists those 34 crates. However, the workspace uses `members = ["crates/*"]` (`Cargo.toml:1-3`) and current `cargo metadata --format-version 1 --no-deps` reports two additional members:

- `oc-process`
- `oc-reaping-fixture`

Thus the fixture and real workspace disagree: 34 expected versus 36 actual. Both extra crates correctly inherit workspace lints, so this is not an `unsafe` loophole. `oc-process` also implements the OS containment explicitly required by G6, so the capability itself is in scope. The fidelity defect is structural: a closed, deliberately enumerated roster was bypassed rather than amended, and the test-only fixture crate became a production workspace member through the wildcard.

**Required resolution:** either fold this implementation into approved crates, exclude a test-only package from workspace membership where appropriate, or deliberately amend the plan and `crates.expected` to the justified final roster. Add a gate that compares actual `cargo metadata` output to the fixture so this cannot remain silently green.

### 4. The C8 deletion contract implements 10 related tables while the frozen acceptance contract requires 12

Success criterion 13 and todo 82 require preview/delete coverage across twelve related tables (`.omo/plans/opencode-rust.md:876-879,1140-1143`). The implementation defines `PRUNE_TABLES` and `DELETE_ORDER` as ten tables (`crates/oc-db/src/prune.rs:14-32`). More importantly, its test explicitly asserts that “the plan's 12-table count is stale” (`crates/oc-db/tests/prune.rs:542-568`) rather than updating or satisfying the accepted contract.

This may reflect a legitimate schema correction, but F4 cannot treat an implementation-side assertion as authorization to rewrite the contract. The same area has a user-surface shortfall: `restore_archive` exists and is tested as a Rust library operation, while `docs/session-retention.md:21-24` admits that neither CLI nor HTTP can clear an archive marker. That weakens the product-facing promise that archive is the reversible housekeeping mode, although the library-level reversal itself is real.

**Required resolution:** reconcile the authoritative table set with the plan and schema, then make preview/delete tests cover the agreed complete set. Expose archive restoration through an operator-facing surface, or narrow the user-facing reversibility claim and obtain explicit approval for that limitation.

## Scope-creep assessment

- **Approved additive scope:** C8 retention, slimmed agents, durable goals, and persistent memory are all explicitly in the plan. Memory being enabled by default is not undeclared creep: `docs/compatibility-matrix.md` states the 5,200-character resident budget and documents `memory: false` as the strict-parity switch.
- **Necessary internal hardening:** the bounded ACP queue, prompt-fingerprint hashing, and shared child-process containment support named non-functional requirements. They are not separate product features.
- **Unapproved structural expansion:** the two extra workspace crates are the only clear implementation-surface expansion found. `oc-process` serves a required mechanism, but adding it outside the closed roster is still contract drift; `oc-reaping-fixture` is test infrastructure that should not silently alter the production roster.
- **No evidence of prohibited product expansion:** this review found no added web/desktop/cloud product, hosted billing/share control plane, bundled Node/Bun runtime, OpenSSL default, or first-party `unsafe` implementation.

## Non-blocking observations

1. The plan text still says “61 `/api/*` endpoints,” while the committed oracle document and tests establish 58 operations. The executable oracle is more credible, but the plan and generated documentation should use one number.
2. `README.md` links to “the seven deliberate differences,” while the live allow-list contains eight. The generated divergence page is current; the README sentence is stale.
3. Provider cassette coverage is stronger than the compatibility report's `NotCompared` wording suggests: task-87 evidence records a 5-family × 8-scenario matrix replayed through production decoders. The report should distinguish “not live-provider differential” from “not protocol-tested.”
4. The latest G1/G2 figures and the completed G3-G6 evidence are consistent with the planned performance intent. Per instruction, this F4 review did not rerun the approximately 100-minute memory gate or two-hour soak.

## Review basis

This was a read-only fidelity audit of the frozen plan, source, tests, generated documentation, and committed evidence. No source, test, documentation, plan, commit, branch, or remote state was modified; this report is the sole output.

F4 VERDICT: REJECT
