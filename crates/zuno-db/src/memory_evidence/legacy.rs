use super::{MemoryEvidenceReference, MemoryEvidenceStore, query_error, reference_for, same_kind};
use crate::{
    Connection,
    experience::ExperienceEvidenceRecord,
    learning_source::{LearningSource, LearningSourceSnapshot, digest, snapshot_current_on},
    open,
};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};
use zuno_error::DbError;
use zuno_types::ExperienceStatus;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyEvidenceRepair {
    pub examined: usize,
    pub revalidated: usize,
    pub unverified: usize,
    pub forgotten: usize,
}

impl MemoryEvidenceStore {
    /// Revalidate at most one extraction's bounded evidence against a closed
    /// durable source. The original model input and result remain intact.
    pub fn revalidate_legacy_job(
        &self,
        job_id: &str,
        snapshot: &LearningSourceSnapshot,
        sources: &[LearningSource],
        dry_run: bool,
        now: i64,
    ) -> Result<LegacyEvidenceRepair, DbError> {
        self.pool.transaction(|transaction| {
            revalidate_on(transaction, job_id, snapshot, sources, dry_run, now)
        })
    }
}

fn revalidate_on(
    connection: &Connection,
    job_id: &str,
    snapshot: &LearningSourceSnapshot,
    sources: &[LearningSource],
    dry_run: bool,
    now: i64,
) -> Result<LegacyEvidenceRepair, DbError> {
    if !snapshot_current_on(connection, snapshot, sources)? {
        return Err(invalid(
            job_id,
            "legacy repair source is unavailable or changed",
        ));
    }
    let payload = connection
        .query_row(
            "SELECT payload FROM learning_job
         WHERE id=?1 AND project_id=?2 AND session_id=?3 AND source_message_id=?4
           AND kind='extraction' AND status='completed'
           AND COALESCE(length(CAST(payload AS BLOB)),0)<=2097152
           AND NOT EXISTS(SELECT 1 FROM session_memory_policy p
             WHERE p.session_id=learning_job.session_id AND p.generation<>'enabled')",
            params![
                job_id,
                snapshot.project_id,
                snapshot.session_id,
                snapshot.source_message_id
            ],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| {
            invalid(
                job_id,
                "legacy repair requires a completed, generation-enabled job",
            )
        })?;
    let mut payload: Value = payload
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(query_error)?
        .unwrap_or_else(|| json!({}));
    if !payload.is_object() {
        return Err(invalid(
            job_id,
            "legacy extraction payload is not an object",
        ));
    }
    let original_sources: Vec<LearningSource> = payload
        .get("request")
        .unwrap_or(&payload)
        .get("sources")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(query_error)?
        .unwrap_or_default();
    if original_sources.len() > 256 {
        return Err(invalid(job_id, "legacy manifest exceeds its bound"));
    }
    let mut query = connection
        .prepare(
            "SELECT id FROM experience_record WHERE extraction_job_id=?1
         ORDER BY extraction_ordinal,id LIMIT 33",
        )
        .map_err(open::map_error)?;
    let ids = query
        .query_map([job_id], |row| row.get::<_, String>(0))
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    if ids.len() > 32 {
        return Err(invalid(
            job_id,
            "legacy extraction exceeds the record bound",
        ));
    }
    let mut report = LegacyEvidenceRepair::default();
    for id in ids {
        let bounded: bool = connection
            .query_row(
                "SELECT length(CAST(title AS BLOB))<=512
                 AND length(CAST(summary AS BLOB))<=8192
                 AND COALESCE(length(CAST(resolution AS BLOB)),0)<=8192
                 AND (SELECT count(*) FROM experience_evidence WHERE experience_id=?1)<=16
                 AND NOT EXISTS(SELECT 1 FROM experience_evidence
                   WHERE experience_id=?1 AND length(CAST(excerpt AS BLOB))>4096)
             FROM experience_record WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .map_err(open::map_error)?;
        report.examined += 1;
        if !bounded {
            report.unverified += 1;
            continue;
        }
        let record = crate::experience::read_required(connection, &id)?;
        if record.projection.project_id != snapshot.project_id
            || record.projection.session_id.as_deref() != Some(snapshot.session_id.as_str())
            || record.projection.source_message_id.as_deref()
                != Some(snapshot.source_message_id.as_str())
        {
            return Err(invalid(
                job_id,
                "legacy experience provenance differs from its source job",
            ));
        }
        if record.projection.status == ExperienceStatus::Forgotten {
            report.forgotten += 1;
            continue;
        }
        let before = reference_for(&record);
        let matches = record
            .evidence
            .iter()
            .map(|evidence| {
                let mut matches = sources
                    .iter()
                    .filter(|source| evidence_matches(evidence, source, &original_sources));
                let first = matches.next();
                if matches.next().is_some() {
                    None
                } else {
                    first
                }
            })
            .collect::<Vec<_>>();
        let verified = !matches.is_empty() && matches.iter().all(Option::is_some);
        if verified {
            report.revalidated += 1;
        } else {
            report.unverified += 1;
        }
        if dry_run {
            continue;
        }
        for (evidence, source) in record.evidence.iter().zip(matches) {
            if let Some(source) = source {
                connection
                    .execute(
                        "UPDATE experience_evidence SET source_id=?2,source_digest=?3,verified=1,
                       promotion_eligible=?4
                     WHERE id=?1 AND experience_id=?5 AND digest=?6",
                        params![
                            evidence.id,
                            source.source_id,
                            source.source_digest,
                            source.proves_success,
                            id,
                            evidence.digest
                        ],
                    )
                    .map_err(open::map_error)?;
            } else {
                connection
                    .execute(
                        "UPDATE experience_evidence SET verified=0,promotion_eligible=0
                     WHERE id=?1 AND experience_id=?2",
                        params![evidence.id, id],
                    )
                    .map_err(open::map_error)?;
            }
        }
        connection
            .execute(
                "UPDATE experience_record SET evidence_verified=?2,time_updated=?3
             WHERE id=?1 AND status<>'forgotten'",
                params![id, verified, now],
            )
            .map_err(open::map_error)?;
        if verified {
            let after = reference_for(&crate::experience::read_required(connection, &id)?);
            refresh_provenance(connection, &before, &after, now)?;
        }
    }
    if !dry_run {
        payload["sourceRevalidation"] = json!({
            "sourceSnapshot":snapshot,"sources":sources,"time":now
        });
        connection
            .execute(
                "UPDATE learning_job SET payload=?2,time_updated=?3
             WHERE id=?1 AND status='completed'",
                params![job_id, payload.to_string(), now],
            )
            .map_err(open::map_error)?;
    }
    Ok(report)
}

fn evidence_matches(
    evidence: &ExperienceEvidenceRecord,
    source: &LearningSource,
    original_sources: &[LearningSource],
) -> bool {
    let Some(address) = evidence.source_id.as_deref() else {
        return false;
    };
    if evidence.excerpt.is_empty()
        || digest(&evidence.excerpt) != evidence.digest
        || !same_kind(evidence.kind, source.kind)
        || !source.content.contains(evidence.excerpt.as_str())
        || !(address == source.source_id
            || address == source.reference_id
            || address == source.message_id)
        || evidence
            .source_digest
            .as_ref()
            .is_some_and(|digest| digest != &source.source_digest)
    {
        return false;
    }
    // An old recorded source digest cannot be replaced merely because an edited
    // source still happens to contain the quoted substring.
    !original_sources.iter().any(|original| {
        (original.source_id == source.source_id || original.reference_id == source.reference_id)
            && original.source_digest != source.source_digest
    })
}

fn refresh_provenance(
    connection: &Connection,
    before: &MemoryEvidenceReference,
    after: &MemoryEvidenceReference,
    now: i64,
) -> Result<(), DbError> {
    if before == after {
        return Ok(());
    }
    let mut query = connection
        .prepare(
            "SELECT path,content,evidence FROM resident_memory_provenance
         WHERE length(CAST(evidence AS BLOB))<=2097152
           AND EXISTS(SELECT 1 FROM json_each(evidence)
             WHERE json_extract(value,'$.experience_id')=?1)
         ORDER BY path,content LIMIT 129",
        )
        .map_err(open::map_error)?;
    let rows = query
        .query_map([&before.experience_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    if rows.len() > 128 {
        return Err(invalid(
            &before.experience_id,
            "legacy provenance exceeds its bound",
        ));
    }
    for (path, content, encoded) in rows {
        let mut references: Vec<MemoryEvidenceReference> =
            serde_json::from_str(&encoded).map_err(query_error)?;
        if references.len() > 4096 {
            return Err(invalid(
                &before.experience_id,
                "legacy references exceed their bound",
            ));
        }
        let mut changed = false;
        for reference in &mut references {
            if reference == before {
                *reference = after.clone();
                changed = true;
            }
        }
        if changed {
            connection
                .execute(
                    "UPDATE resident_memory_provenance SET evidence=?4,time_updated=?5
                 WHERE path=?1 AND content=?2 AND evidence=?3",
                    params![
                        path,
                        content,
                        encoded,
                        serde_json::to_string(&references).map_err(query_error)?,
                        now
                    ],
                )
                .map_err(open::map_error)?;
        }
    }
    Ok(())
}

fn invalid(id: &str, detail: &str) -> DbError {
    DbError::Conflict {
        table: "learning_job".to_owned(),
        id: id.to_owned(),
        detail: detail.to_owned(),
    }
}
