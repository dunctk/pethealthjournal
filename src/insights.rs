//! Deterministic, LLM-free health insights.
//!
//! Every function here is pure arithmetic over already-recorded rows: weight
//! changes, symptom frequencies, adherence gaps and lab-value deltas. Nothing
//! in this module calls a model, contacts the network, or makes a clinical
//! claim. Summaries describe the measured change only ("+6.2% over 28 days"),
//! never a diagnosis, prescription, or urgency judgement.
//!
//! Persistence lives in the `health_insights` table (created in
//! [`crate::db::migrate`]). Findings are upserted by `dedupe_key` so
//! recomputation never creates duplicates.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::Serialize;

/// Version every persisted finding carries. Bump when a calculation changes
/// so old rows can be distinguished from recomputed ones.
pub const INSIGHTS_RULE_VERSION: &str = "v1";

/// A computed, reviewable finding. Returned by [`compute_insights`] and
/// persisted with provenance for the web UI and `get_health_insights`.
#[derive(Clone, Debug, Serialize)]
pub struct HealthFinding {
    pub household_id: i64,
    pub pet_id: i64,
    pub pet_name: String,
    pub finding_type: String,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub summary: String,
    pub detail: String,
    pub rule_version: String,
    pub source_ids: Vec<String>,
    pub dedupe_key: String,
    pub uncertainty: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Recompute deterministic insights for one pet from current rows.
/// Shared by the MCP `get_health_insights` tool and the web Insights tab.
/// Pure reads + arithmetic: no model, no network, no diagnosis.
pub async fn recompute_for_pet(
    db: &sea_orm::DatabaseConnection,
    household_id: i64,
    pet: &crate::domain::Pet,
) -> anyhow::Result<Vec<HealthFinding>> {
    let now = Utc::now();
    let weights = crate::db::list_weights(db, household_id, pet.id).await?;
    let weight_points: Vec<WeightPoint> = weights
        .into_iter()
        .take(200)
        .map(|w| WeightPoint {
            id: w.id,
            weight_kg: w.weight_kg,
            measured_at: w.measured_at,
        })
        .collect();
    let events = crate::db::list_events(db, household_id, Some(pet.id), 200).await?;
    let symptom_points: Vec<SymptomPoint> = events
        .into_iter()
        .filter(|e| e.event_type == "symptom")
        .map(|e| SymptomPoint {
            id: e.id,
            symptom: e.concept.clone(),
            occurred_at: e.occurred_at,
            episode_id: e.symptom.as_ref().map(|s| s.episode_id.clone()),
        })
        .collect();
    let rows = crate::db::list_adherence(db, household_id, pet.id, 100).await?;
    let adherence_points: Vec<AdherencePoint> = rows
        .into_iter()
        .map(|a| AdherencePoint {
            id: a.id,
            prescription_id: a.prescription_id,
            prescription_name: a.prescription_name.clone(),
            scheduled_for: a.scheduled_for.clone(),
            status: a.status.clone(),
        })
        .collect();
    let reports = crate::db::list_lab_reports(db, household_id, pet.id).await?;
    let mut lab_points: Vec<LabPoint> = Vec::new();
    for report in &reports {
        for result in &report.results {
            if let Some(value) = result.value_numeric {
                lab_points.push(LabPoint {
                    report_id: report.id,
                    test_name: result.test_name.clone(),
                    value_numeric: value,
                    unit: result.unit.clone(),
                    test_date: report.test_date.clone(),
                    imported_at: report.imported_at,
                });
            }
        }
    }
    Ok(compute_insights(
        household_id,
        pet.id,
        &pet.name,
        &weight_points,
        &symptom_points,
        &adherence_points,
        &lab_points,
        now,
    ))
}

/// Minimal weight input so the engine stays decoupled from SeaORM row types.
#[derive(Clone, Debug)]
pub struct WeightPoint {
    pub id: i64,
    pub weight_kg: f64,
    pub measured_at: DateTime<Utc>,
}

/// Minimal symptom input: one structured symptom event.
#[derive(Clone, Debug)]
pub struct SymptomPoint {
    pub id: i64,
    pub symptom: String,
    pub occurred_at: DateTime<Utc>,
    pub episode_id: Option<String>,
}

/// Minimal adherence input: one scheduled-dose record.
#[derive(Clone, Debug)]
pub struct AdherencePoint {
    pub id: i64,
    pub prescription_id: i64,
    pub prescription_name: String,
    pub scheduled_for: String,
    pub status: String,
}

/// Minimal lab input: one numeric result with its report date.
#[derive(Clone, Debug)]
pub struct LabPoint {
    pub report_id: i64,
    pub test_name: String,
    pub value_numeric: f64,
    pub unit: Option<String>,
    pub test_date: Option<String>,
    pub imported_at: DateTime<Utc>,
}

/// Compute every deterministic finding for one pet from in-memory rows.
/// Pure function: no I/O, no model. Callers persist the result with
/// [`save_findings`].
pub fn compute_insights(
    household_id: i64,
    pet_id: i64,
    pet_name: &str,
    weights: &[WeightPoint],
    symptoms: &[SymptomPoint],
    adherence: &[AdherencePoint],
    labs: &[LabPoint],
    now: DateTime<Utc>,
) -> Vec<HealthFinding> {
    let mut out = Vec::new();
    if let Some(f) = weight_trend(household_id, pet_id, pet_name, weights, now) {
        out.push(f);
    }
    out.extend(symptom_frequency(
        household_id,
        pet_id,
        pet_name,
        symptoms,
        now,
    ));
    out.extend(adherence_gaps(household_id, pet_id, pet_name, adherence));
    out.extend(lab_deltas(household_id, pet_id, pet_name, labs));
    out
}

/// Latest-vs-previous weight change plus a 90-day longer view when enough
/// history exists. Skips non-positive weights and single-measurement pets.
fn weight_trend(
    household_id: i64,
    pet_id: i64,
    pet_name: &str,
    weights: &[WeightPoint],
    now: DateTime<Utc>,
) -> Option<HealthFinding> {
    let mut sorted: Vec<&WeightPoint> = weights
        .iter()
        .filter(|w| w.weight_kg.is_finite() && w.weight_kg > 0.0)
        .collect();
    if sorted.len() < 2 {
        return None;
    }
    sorted.sort_by(|a, b| b.measured_at.cmp(&a.measured_at));
    // Deduplicate same-day repeats: keep the latest entry per calendar day so
    // a double-weighing does not read as a trend.
    let mut deduped: Vec<&WeightPoint> = Vec::with_capacity(sorted.len());
    for w in sorted {
        let day = w.measured_at.date_naive();
        if deduped
            .last()
            .is_none_or(|last: &&WeightPoint| last.measured_at.date_naive() != day)
        {
            deduped.push(w);
        }
    }
    if deduped.len() < 2 {
        return None;
    }
    let latest = deduped[0];
    let previous = deduped[1];
    let days = (latest.measured_at - previous.measured_at)
        .num_seconds()
        .max(0) as f64
        / 86_400.0;
    let abs = latest.weight_kg - previous.weight_kg;
    let pct = if previous.weight_kg > 0.0 {
        abs / previous.weight_kg * 100.0
    } else {
        0.0
    };
    // Longer view: oldest measurement within the last 120 days.
    let cutoff = now - chrono::Duration::days(120);
    let oldest_in_window = deduped.iter().rev().find(|w| w.measured_at >= cutoff);
    let long_view = oldest_in_window
        .filter(|w| w.id != latest.id)
        .map(|oldest| {
            let long_pct = (latest.weight_kg - oldest.weight_kg) / oldest.weight_kg * 100.0;
            let long_days = ((latest.measured_at - oldest.measured_at).num_seconds() as f64
                / 86_400.0)
                .round() as i64;
            format!(
                " Over the last ~{long_days} days the measured change is {long_pct:+.1}% ({} measurements in window).",
                deduped.iter().filter(|w| w.measured_at >= cutoff).count()
            )
        })
        .unwrap_or_default();
    let direction = if abs > 0.0 {
        "up"
    } else if abs < 0.0 {
        "down"
    } else {
        "unchanged"
    };
    let summary = format!(
        "{}'s latest weight is {:.2} kg ({}{:+.2} kg, {:+.1}% vs {:.2} kg {}).",
        pet_name,
        latest.weight_kg,
        direction,
        abs,
        pct,
        previous.weight_kg,
        previous.measured_at.format("%Y-%m-%d"),
    );
    let uncertainty = if deduped.len() == 2 {
        "Based on 2 measurements after same-day deduplication; a single pair cannot show a sustained trend."
            .to_owned()
    } else if days < 1.0 {
        "The two latest daily measurements are less than a day apart; short gaps reflect scale or timing noise more than body change.".to_owned()
    } else {
        format!(
            "Based on {} deduplicated measurements. Scales, feeding, and time of day affect readings.",
            deduped.len()
        )
    };
    Some(HealthFinding {
        household_id,
        pet_id,
        pet_name: pet_name.to_owned(),
        finding_type: "weight_trend".into(),
        period_start: Some(previous.measured_at.to_rfc3339()),
        period_end: Some(latest.measured_at.to_rfc3339()),
        summary,
        detail: format!(
            "Latest {:.2} kg on {}, previous {:.2} kg on {} ({:.1} days apart).{} This is a measured change, not a health judgement.",
            latest.weight_kg,
            latest.measured_at.format("%Y-%m-%d"),
            previous.weight_kg,
            previous.measured_at.format("%Y-%m-%d"),
            days,
            long_view,
        ),
        rule_version: INSIGHTS_RULE_VERSION.into(),
        source_ids: vec![
            format!("weight:{}", latest.id),
            format!("weight:{}", previous.id),
        ],
        dedupe_key: format!("weight_trend:{pet_id}:{}:{}", previous.id, latest.id),
        uncertainty,
        status: "open".into(),
        created_at: now,
        updated_at: now,
    })
}

/// Compare symptom counts in the last 7 days vs the prior 7 days, per
/// symptom. Episode ids deduplicate repeat rows for the same episode within
/// the 6-hour grouping window. Missing records are never treated as absence.
fn symptom_frequency(
    household_id: i64,
    pet_id: i64,
    pet_name: &str,
    symptoms: &[SymptomPoint],
    now: DateTime<Utc>,
) -> Vec<HealthFinding> {
    use std::collections::{BTreeMap, BTreeSet};
    let recent_cutoff = now - chrono::Duration::days(7);
    let prior_cutoff = now - chrono::Duration::days(14);
    // (symptom, period) -> set of dedupe ids
    let mut counts: BTreeMap<(&str, u8), BTreeSet<String>> = BTreeMap::new();
    // (symptom, period) -> representative event ids for provenance
    let mut examples: BTreeMap<(&str, u8), Vec<i64>> = BTreeMap::new();
    for s in symptoms {
        let period = if s.occurred_at >= recent_cutoff {
            0u8
        } else if s.occurred_at >= prior_cutoff {
            1u8
        } else {
            continue;
        };
        let key = s
            .episode_id
            .clone()
            .unwrap_or_else(|| format!("event:{}", s.id));
        counts
            .entry((s.symptom.as_str(), period))
            .or_default()
            .insert(key);
        examples
            .entry((s.symptom.as_str(), period))
            .or_default()
            .push(s.id);
    }
    let mut symptom_names: BTreeSet<&str> = BTreeSet::new();
    for (name, _) in counts.keys() {
        symptom_names.insert(name);
    }
    let mut out = Vec::new();
    for name in symptom_names {
        let recent = counts.get(&(name, 0)).map_or(0, |s| s.len());
        let prior = counts.get(&(name, 1)).map_or(0, |s| s.len());
        // Only surface a finding when there is something to compare and the
        // counts differ; equal counts (including 0 vs 0, which cannot occur
        // here) carry no information.
        if recent == prior || (recent == 0 && prior == 0) {
            continue;
        }
        // Require at least 2 total episodes before commenting so a single
        // isolated event does not read as a frequency change.
        if recent + prior < 2 {
            continue;
        }
        let direction = if recent > prior { "more" } else { "fewer" };
        let mut source_ids: Vec<String> = examples
            .get(&(name, 0))
            .map(|ids| ids.iter().take(5).map(|id| format!("event:{id}")).collect())
            .unwrap_or_default();
        for id in examples
            .get(&(name, 1))
            .map(|ids| ids.iter().take(5).collect::<Vec<_>>())
            .unwrap_or_default()
        {
            source_ids.push(format!("event:{id}"));
        }
        out.push(HealthFinding {
            household_id,
            pet_id,
            pet_name: pet_name.to_owned(),
            finding_type: "symptom_frequency".into(),
            period_start: Some(prior_cutoff.to_rfc3339()),
            period_end: Some(now.to_rfc3339()),
            summary: format!(
                "{pet_name} had {direction} recorded {name} episodes in the last 7 days ({recent}) than in the prior 7 days ({prior})."
            ),
            detail: format!(
                "Recent 7-day distinct episodes: {recent}; prior 7-day distinct episodes: {prior}. Episodes group repeat rows within 6 hours. Only recorded events are counted."
            ),
            rule_version: INSIGHTS_RULE_VERSION.into(),
            source_ids,
            dedupe_key: format!("symptom_frequency:{pet_id}:{name}:{}", now.date_naive()),
            uncertainty: "Only recorded events are counted. Missing records do not mean the symptom did not occur, and equal recording habits across weeks are assumed, not verified.".into(),
            status: "open".into(),
            created_at: now,
            updated_at: now,
        });
    }
    out
}

/// Surface recorded (non-unknown) adherence states over the last 14
/// scheduled days. Unknown/unrecorded days are listed separately and never
/// counted as missed.
fn adherence_gaps(
    household_id: i64,
    pet_id: i64,
    pet_name: &str,
    adherence: &[AdherencePoint],
) -> Vec<HealthFinding> {
    use std::collections::BTreeMap;
    let mut by_rx: BTreeMap<i64, Vec<&AdherencePoint>> = BTreeMap::new();
    for a in adherence.iter().take(500) {
        by_rx.entry(a.prescription_id).or_default().push(a);
    }
    let mut out = Vec::new();
    for (rx_id, rows) in by_rx {
        let mut rows = rows;
        rows.sort_by(|a, b| b.scheduled_for.cmp(&a.scheduled_for));
        let recent: Vec<&&AdherencePoint> = rows.iter().take(14).collect();
        if recent.is_empty() {
            continue;
        }
        let name = recent[0].prescription_name.clone();
        let mut confirmed_missed = 0usize;
        let mut partial = 0usize;
        let mut unknown = 0usize;
        let mut given = 0usize;
        let mut missed_dates: Vec<String> = Vec::new();
        let mut source_ids: Vec<String> = Vec::new();
        for a in &recent {
            source_ids.push(format!("adherence:{}", a.id));
            match a.status.as_str() {
                "missed" => {
                    confirmed_missed += 1;
                    missed_dates.push(a.scheduled_for.clone());
                }
                "partial" => partial += 1,
                "given" => given += 1,
                _ => unknown += 1,
            }
        }
        if confirmed_missed == 0 && partial == 0 {
            continue;
        }
        let mut parts = Vec::new();
        if confirmed_missed > 0 {
            parts.push(format!("{confirmed_missed} recorded as missed"));
        }
        if partial > 0 {
            parts.push(format!("{partial} recorded as partial"));
        }
        out.push(HealthFinding {
            household_id,
            pet_id,
            pet_name: pet_name.to_owned(),
            finding_type: "medication_adherence".into(),
            period_start: recent.last().map(|a| a.scheduled_for.clone()),
            period_end: recent.first().map(|a| a.scheduled_for.clone()),
            summary: format!(
                "{pet_name}'s {name} record shows {} in the last {} scheduled days ({given} given{}.",
                parts.join(", "),
                recent.len(),
                if unknown > 0 {
                    format!(", {unknown} unrecorded")
                } else {
                    String::new()
                }
            ),
            detail: format!(
                "Recorded states over the last {} scheduled days: {given} given, {confirmed_missed} missed, {partial} partial, {unknown} unknown/unrecorded.{} An unrecorded day is not proof a dose was missed.",
                recent.len(),
                if missed_dates.is_empty() {
                    String::new()
                } else {
                    format!(" Missed dates: {}.", missed_dates.join(", "))
                }
            ),
            rule_version: INSIGHTS_RULE_VERSION.into(),
            source_ids,
            dedupe_key: format!("medication_adherence:{pet_id}:{rx_id}:{}", recent.len()),
            uncertainty: "Only explicitly recorded states are reported. Unrecorded days may have been administered without being logged.".into(),
            status: "open".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
    }
    out
}

/// Compare the two latest numeric values per (test, unit) pair. Different
/// units or different test names are never compared.
fn lab_deltas(
    household_id: i64,
    pet_id: i64,
    pet_name: &str,
    labs: &[LabPoint],
) -> Vec<HealthFinding> {
    use std::collections::BTreeMap;
    let mut by_test: BTreeMap<(String, String), Vec<&LabPoint>> = BTreeMap::new();
    for l in labs {
        if !l.value_numeric.is_finite() {
            continue;
        }
        let unit = l.unit.clone().unwrap_or_default().to_lowercase();
        by_test
            .entry((l.test_name.to_lowercase(), unit))
            .or_default()
            .push(l);
    }
    let mut out = Vec::new();
    for ((test, unit), mut rows) in by_test {
        rows.sort_by(|a, b| {
            b.test_date
                .cmp(&a.test_date)
                .then(b.imported_at.cmp(&a.imported_at))
        });
        if rows.len() < 2 {
            continue;
        }
        let latest = rows[0];
        let previous = rows[1];
        if latest.value_numeric == previous.value_numeric {
            continue;
        }
        let abs = latest.value_numeric - previous.value_numeric;
        let pct = if previous.value_numeric != 0.0 {
            abs / previous.value_numeric.abs() * 100.0
        } else {
            0.0
        };
        let unit_label = if unit.is_empty() {
            String::new()
        } else {
            format!(" {}", latest.unit.clone().unwrap_or_default())
        };
        out.push(HealthFinding {
            household_id,
            pet_id,
            pet_name: pet_name.to_owned(),
            finding_type: "lab_delta".into(),
            period_start: previous.test_date.clone(),
            period_end: latest.test_date.clone(),
            summary: format!(
                "{pet_name}'s {} changed from {} to {}{unit_label} ({:+.1}%).",
                latest.test_name, previous.value_numeric, latest.value_numeric, pct
            ),
            detail: format!(
                "Latest {} on {} vs {} on {}. Same test name and compatible units required; unrelated tests are never compared. This is a numeric change, not a clinical interpretation.",
                latest.test_name,
                latest.test_date.clone().unwrap_or_else(|| "undated report".into()),
                previous.value_numeric,
                previous.test_date.clone().unwrap_or_else(|| "undated report".into()),
            ),
            rule_version: INSIGHTS_RULE_VERSION.into(),
            source_ids: vec![
                format!("lab:{}", latest.report_id),
                format!("lab:{}", previous.report_id),
            ],
            dedupe_key: format!(
                "lab_delta:{pet_id}:{}:{}:{}",
                test, unit, latest.report_id
            ),
            uncertainty: "Lab values vary by assay, handling, and timing. Reference ranges and flags are in the underlying reports; this summary does not interpret them.".into(),
            status: "open".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
    }
    out
}

/// Persist findings idempotently: one row per `dedupe_key`, refreshed when
/// the summary changes. Returns the number of rows written.
pub async fn save_findings<C: ConnectionTrait>(
    db: &C,
    findings: &[HealthFinding],
) -> anyhow::Result<u64> {
    let mut written = 0u64;
    for f in findings {
        let sources = serde_json::to_string(&f.source_ids).unwrap_or_else(|_| "[]".into());
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO health_insights
               (household_id,pet_id,pet_name,finding_type,period_start,period_end,summary,detail,rule_version,source_ids,dedupe_key,uncertainty,status,created_at,updated_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
               ON CONFLICT(dedupe_key) DO UPDATE SET
                 summary=excluded.summary, detail=excluded.detail,
                 source_ids=excluded.source_ids, updated_at=excluded.updated_at
               WHERE health_insights.summary != excluded.summary"#,
            [
                f.household_id.into(),
                f.pet_id.into(),
                f.pet_name.clone().into(),
                f.finding_type.clone().into(),
                f.period_start.clone().into(),
                f.period_end.clone().into(),
                f.summary.clone().into(),
                f.detail.clone().into(),
                f.rule_version.clone().into(),
                sources.into(),
                f.dedupe_key.clone().into(),
                f.uncertainty.clone().into(),
                f.status.clone().into(),
                f.created_at.to_rfc3339().into(),
                f.updated_at.to_rfc3339().into(),
            ],
        ))
        .await?;
        written += 1;
    }
    Ok(written)
}

/// Read persisted findings for one pet, newest first. Household-scoped.
pub async fn list_findings<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    pet_id: i64,
    include_dismissed: bool,
    limit: u64,
) -> anyhow::Result<Vec<HealthFinding>> {
    let sql = if include_dismissed {
        "SELECT household_id,pet_id,pet_name,finding_type,period_start,period_end,summary,detail,rule_version,source_ids,dedupe_key,uncertainty,status,created_at,updated_at FROM health_insights WHERE household_id=? AND pet_id=? ORDER BY updated_at DESC LIMIT ?"
    } else {
        "SELECT household_id,pet_id,pet_name,finding_type,period_start,period_end,summary,detail,rule_version,source_ids,dedupe_key,uncertainty,status,created_at,updated_at FROM health_insights WHERE household_id=? AND pet_id=? AND status != 'dismissed' ORDER BY updated_at DESC LIMIT ?"
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [
                household_id.into(),
                pet_id.into(),
                (limit.clamp(1, 100) as i64).into(),
            ],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let source_raw: String = row.try_get("", "source_ids")?;
            let source_ids: Vec<String> = serde_json::from_str(&source_raw).unwrap_or_default();
            let created_raw: String = row.try_get("", "created_at")?;
            let updated_raw: String = row.try_get("", "updated_at")?;
            Ok(HealthFinding {
                household_id: row.try_get("", "household_id")?,
                pet_id: row.try_get("", "pet_id")?,
                pet_name: row.try_get("", "pet_name")?,
                finding_type: row.try_get("", "finding_type")?,
                period_start: row.try_get("", "period_start")?,
                period_end: row.try_get("", "period_end")?,
                summary: row.try_get("", "summary")?,
                detail: row.try_get("", "detail")?,
                rule_version: row.try_get("", "rule_version")?,
                source_ids,
                dedupe_key: row.try_get("", "dedupe_key")?,
                uncertainty: row.try_get("", "uncertainty")?,
                status: row.try_get("", "status")?,
                created_at: created_raw.parse().unwrap_or_else(|_| Utc::now()),
                updated_at: updated_raw.parse().unwrap_or_else(|_| Utc::now()),
            })
        })
        .collect()
}

/// Mark a finding reviewed or dismissed. Returns true when a row in this
/// household was updated.
pub async fn set_finding_status<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    dedupe_key: &str,
    status: &str,
) -> anyhow::Result<bool> {
    if !matches!(status, "open" | "reviewed" | "dismissed") {
        anyhow::bail!("status must be open, reviewed, or dismissed");
    }
    let res = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE health_insights SET status=?, updated_at=? WHERE household_id=? AND dedupe_key=?",
            [
                status.into(),
                Utc::now().to_rfc3339().into(),
                household_id.into(),
                dedupe_key.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The migration for the findings table. Called from `db::migrate`.
pub const MIGRATION_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS health_insights (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            pet_name TEXT NOT NULL DEFAULT '',
            finding_type TEXT NOT NULL,
            period_start TEXT,
            period_end TEXT,
            summary TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            rule_version TEXT NOT NULL DEFAULT 'v1',
            source_ids TEXT NOT NULL DEFAULT '[]',
            dedupe_key TEXT NOT NULL UNIQUE,
            uncertainty TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open','reviewed','dismissed')),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id)
        );
        CREATE INDEX IF NOT EXISTS idx_insights_tenant_pet
            ON health_insights(household_id, pet_id, updated_at DESC);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(days_ago: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::days(days_ago)
    }

    #[test]
    fn weight_trend_reports_change_without_diagnosis() {
        let weights = vec![
            WeightPoint {
                id: 2,
                weight_kg: 4.25,
                measured_at: t(1),
            },
            WeightPoint {
                id: 1,
                weight_kg: 4.0,
                measured_at: t(30),
            },
        ];
        let f = weight_trend(1, 7, "Velcro", &weights, Utc::now()).unwrap();
        assert!(f.summary.contains("Velcro"));
        assert!(f.summary.contains("+6.2") || f.summary.contains("+6.3"));
        assert!(f.detail.contains("measured change, not a health judgement"));
        assert!(!f.summary.to_lowercase().contains("diagnos"));
        assert_eq!(f.dedupe_key, "weight_trend:7:1:2");
    }

    #[test]
    fn weight_trend_needs_two_distinct_days() {
        let at = t(1);
        let weights = vec![
            WeightPoint {
                id: 2,
                weight_kg: 4.25,
                measured_at: at,
            },
            WeightPoint {
                id: 1,
                weight_kg: 4.0,
                measured_at: at,
            },
        ];
        assert!(weight_trend(1, 7, "Velcro", &weights, Utc::now()).is_none());
        let single = vec![WeightPoint {
            id: 1,
            weight_kg: 4.0,
            measured_at: t(5),
        }];
        assert!(weight_trend(1, 7, "Velcro", &single, Utc::now()).is_none());
    }

    #[test]
    fn symptom_frequency_dedupes_episodes() {
        let symptoms = vec![
            SymptomPoint {
                id: 1,
                symptom: "vomiting".into(),
                occurred_at: t(1),
                episode_id: Some("e1".into()),
            },
            SymptomPoint {
                id: 2,
                symptom: "vomiting".into(),
                occurred_at: t(1),
                episode_id: Some("e1".into()),
            },
            SymptomPoint {
                id: 3,
                symptom: "vomiting".into(),
                occurred_at: t(2),
                episode_id: Some("e2".into()),
            },
            SymptomPoint {
                id: 4,
                symptom: "vomiting".into(),
                occurred_at: t(10),
                episode_id: Some("e3".into()),
            },
        ];
        let out = symptom_frequency(1, 7, "Velcro", &symptoms, Utc::now());
        assert_eq!(out.len(), 1);
        assert!(out[0].summary.contains("(2) than in the prior 7 days (1)"));
        assert!(out[0].uncertainty.contains("Missing records do not mean"));
    }

    #[test]
    fn symptom_frequency_ignores_single_isolated_event() {
        let symptoms = vec![SymptomPoint {
            id: 1,
            symptom: "vomiting".into(),
            occurred_at: t(1),
            episode_id: None,
        }];
        assert!(symptom_frequency(1, 7, "Velcro", &symptoms, Utc::now()).is_empty());
    }

    #[test]
    fn adherence_never_counts_unknown_as_missed() {
        let rows = vec![
            AdherencePoint {
                id: 1,
                prescription_id: 9,
                prescription_name: "Apelka".into(),
                scheduled_for: "2026-11-04".into(),
                status: "given".into(),
            },
            AdherencePoint {
                id: 2,
                prescription_id: 9,
                prescription_name: "Apelka".into(),
                scheduled_for: "2026-11-03".into(),
                status: "unknown".into(),
            },
            AdherencePoint {
                id: 3,
                prescription_id: 9,
                prescription_name: "Apelka".into(),
                scheduled_for: "2026-11-02".into(),
                status: "missed".into(),
            },
        ];
        let out = adherence_gaps(1, 7, "Velcro", &rows);
        assert_eq!(out.len(), 1);
        assert!(out[0].summary.contains("1 recorded as missed"));
        assert!(out[0].detail.contains("not proof a dose was missed"));
    }

    #[test]
    fn adherence_with_no_gaps_returns_nothing() {
        let rows = vec![AdherencePoint {
            id: 1,
            prescription_id: 9,
            prescription_name: "Apelka".into(),
            scheduled_for: "2026-11-04".into(),
            status: "given".into(),
        }];
        assert!(adherence_gaps(1, 7, "Velcro", &rows).is_empty());
    }

    #[test]
    fn lab_deltas_require_matching_test_and_unit() {
        let labs = vec![
            LabPoint {
                report_id: 2,
                test_name: "Creatinine".into(),
                value_numeric: 2.2,
                unit: Some("mg/dL".into()),
                test_date: Some("2026-10-01".into()),
                imported_at: t(5),
            },
            LabPoint {
                report_id: 1,
                test_name: "Creatinine".into(),
                value_numeric: 2.0,
                unit: Some("mg/dL".into()),
                test_date: Some("2026-09-01".into()),
                imported_at: t(40),
            },
            LabPoint {
                report_id: 3,
                test_name: "Creatinine".into(),
                value_numeric: 180.0,
                unit: Some("umol/L".into()),
                test_date: Some("2026-10-01".into()),
                imported_at: t(5),
            },
        ];
        let out = lab_deltas(1, 7, "Velcro", &labs);
        assert_eq!(out.len(), 1);
        assert!(out[0].summary.contains("Creatinine"));
        assert!(out[0].detail.contains("not a clinical interpretation"));
        let _ = Utc.timestamp_opt(0, 0);
    }
}
