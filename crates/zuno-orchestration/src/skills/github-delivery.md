# GitHub Delivery

Use this Skill for GitHub pull requests, Actions, CI, tags, artifacts, or
releases. It defines evidence and safety, not repository-specific release policy.

1. Freeze the repository and exact commit, ref, or tag before remote work.
   Treat dispatch, approval, rerun, merge, push, tag movement, asset upload, and
   release publication as separate authorized side effects.
2. Inspect machine-readable state and retain stable repository, run, attempt,
   job, artifact, and release identifiers. A green workflow summary is
   insufficient: every required job must actually conclude `success`; skipped,
   cancelled, missing, or unexpanded matrix work is unverified unless explicitly
   optional.
3. When authoring Actions, use repository-owned prompt files, least-privilege
   permissions, full-SHA action pins, bounded outputs or artifacts, and no
   job-wide secrets around untrusted checkout code. Keep analysis and patch
   production read-only; move credentials and writes into a separate gated job.
4. For asynchronous work, when Shell and background tools are available,
   start a Shell execution with `background: true` and
   `backgroundPurpose: "remoteObserver"`, then yield. Its durable report is a
   wake signal. Inspect output and re-query authoritative remote state by
   run/attempt or ref before continuing; never overlap watchers or poll loops.
5. Before release publication, prove artifacts were built from the exact ref,
   required binaries were executed or smoke-tested, checksums match, and the
   published tag, release, and assets correspond. Run a consumer-facing install
   or startup smoke when in scope.
6. Report exact evidence and any gate not executed. Never convert a planned,
   skipped, queued, or merely observed check into a pass.
