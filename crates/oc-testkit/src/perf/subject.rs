//! The pinned `W-real` measurement subject.
//!
//! `W-real` used to be defined as *whichever* session held the most `part.data`
//! bytes in whatever database `OPENCODE_DB` happened to resolve to. That made the
//! workload a moving target: the G2 ceiling is `0.50 x` the **TypeScript median
//! measured for one particular session** and does not scale with a different one,
//! so growing or deleting sessions moved the gate without any code change.
//! Measured on 2026-08-08: todo 88's subject had been deleted from the live
//! database entirely and the then-largest session was `2.85x` heavier, against an
//! unchanged ceiling.
//!
//! The subject is therefore **data the repository owns**, not something the
//! machine discovers. It is recorded here as a constant, compared against the
//! resolved database before any measurement starts, and a mismatch is a hard
//! failure that names [`W_REAL_RECAPTURE`] — never a silent substitution.
//!
//! # Why this is not a `PERF_METHODOLOGY_REVISION` bump
//!
//! Pinning *which* session is measured does not change *how* it is measured. The
//! four frozen formulas, the `0.50` factor, the five repetitions, the 2-second
//! sampling interval, the process-tree rule and the warm-up scoping are all
//! untouched, so the hashed formula section in `docs/perf-methodology.md` is
//! byte-identical and revision 2 still describes this measurement. Bumping the
//! revision here would only make
//! [`crate::perf::BaselineReport::validate`](crate::perf::BaselineReport)
//! reject the committed `benchmarks/ts-baseline.json`, which records revision 2 —
//! destroying the measured G1/G2 results rather than protecting them.
//!
//! What *would* require a bump is changing the formulas. What requires
//! re-measuring the baseline is changing this pin; see [`W_REAL_RECAPTURE`].

/// Session content and source-database identity that fix what `W-real` measures.
///
/// The four session fields are a content fingerprint: an id plus its exact
/// message count, part count and summed `LENGTH(part.data)`. The three database
/// fields identify the immutable snapshot the fingerprint was taken from. The
/// path is where to *look*; the byte length and digest are what make it the right
/// file, so a copy at another path is accepted and a mutated database is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedSubject {
    /// Exact session the workload hydrates, restores and takes one turn in.
    pub session_id: &'static str,
    /// `COUNT(DISTINCT part.message_id)` for that session.
    pub message_count: u64,
    /// `COUNT(*)` of that session's parts.
    pub part_count: u64,
    /// `SUM(LENGTH(part.data))` for that session.
    pub part_data_bytes: u64,
    /// Where the pinned snapshot was read from when the baseline was measured.
    pub database_path: &'static str,
    /// Byte length of that snapshot, checked first because it is free.
    pub database_bytes: u64,
    /// SHA-256 of that snapshot, which is what actually pins the database.
    pub database_sha256: &'static str,
}

/// The subject `benchmarks/ts-baseline.json` and both memory gates measure.
///
/// Provenance: `/config/.local/share/opencode/opencode.db.bak.20260408`, an
/// immutable April snapshot. In it this session genuinely is the largest by
/// `SUM(LENGTH(part.data))`, which is how todo 88 originally selected it and why
/// the committed TypeScript medians describe exactly this workload.
pub const W_REAL_SUBJECT: PinnedSubject = PinnedSubject {
    session_id: "ses_2bcaee257ffeFZNJrmtpi3ZglR",
    message_count: 931,
    part_count: 3_620,
    part_data_bytes: 105_118_812,
    database_path: "/config/.local/share/opencode/opencode.db.bak.20260408",
    database_bytes: 2_630_582_272,
    database_sha256: "e2cde4df08cd580d0a4f03068b2d861275ca8aef983fef6578968f7f7a2a18a7",
};

/// The procedure every pin failure names, because re-pinning is not free.
///
/// The G2 ceiling and the pin come from **one** measurement. Changing the pin
/// without re-measuring silently compares a new workload against an old ceiling,
/// which is the exact defect the pin exists to close.
pub const W_REAL_RECAPTURE: &str = "\
To re-pin W-real: (1) choose an immutable database snapshot and record its byte \
length and `sha256sum`; (2) read its heaviest session with `sqlite3 -readonly \
<db> \"SELECT p.session_id, COUNT(DISTINCT p.message_id), COUNT(*), \
SUM(LENGTH(p.data)) FROM part AS p GROUP BY p.session_id ORDER BY \
SUM(LENGTH(p.data)) DESC LIMIT 1;\"`; (3) update all seven fields of \
`W_REAL_SUBJECT` in `crates/oc-testkit/src/perf/subject.rs`; (4) re-measure the \
TypeScript baseline and regenerate `benchmarks/ts-baseline.json`, because G2's \
ceiling is 0.50 x the TypeScript median for the pinned subject and does not \
scale to a different one. Never change the pin without step (4): the subject and \
the ceiling must come from the same measurement.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_subject_is_fully_specified() {
        // Given/When: the committed pin.
        let pin = W_REAL_SUBJECT;

        // Then: every field carries a usable value, so no part of the subject is
        // left for the machine to discover at measurement time.
        assert!(pin.session_id.starts_with("ses_"), "{}", pin.session_id);
        assert!(pin.message_count > 0);
        assert!(pin.part_count >= pin.message_count);
        assert!(pin.part_data_bytes > 0);
        assert!(pin.database_bytes > 0);
        assert_eq!(pin.database_sha256.len(), 64, "{}", pin.database_sha256);
        assert!(
            pin.database_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "{}",
            pin.database_sha256
        );
    }

    #[test]
    fn the_recapture_procedure_names_the_baseline_it_invalidates() {
        // Given: the text every pin failure prints.
        let procedure = W_REAL_RECAPTURE;

        // Then: it names where the pin lives and that the committed baseline must
        // be re-measured with it, which is the step that keeps the ceiling and the
        // subject from drifting apart.
        assert!(procedure.contains("W_REAL_SUBJECT"), "{procedure}");
        assert!(
            procedure.contains("crates/oc-testkit/src/perf/subject.rs"),
            "{procedure}"
        );
        assert!(
            procedure.contains("benchmarks/ts-baseline.json"),
            "{procedure}"
        );
        assert!(procedure.contains("0.50"), "{procedure}");
    }
}
