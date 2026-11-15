//! Correcting stored blood tests without losing anything.
//!
//! Every operation here follows the same rules:
//! - A preview comes first. Nothing changes without `confirm: true`.
//! - Results are matched by analyte and unit, never by position.
//! - A printed value that differs from what is stored is a *conflict*: it is
//!   shown, never applied by reprocessing. Fixing it is an explicit edit.
//! - Nothing is deleted except a duplicate report the owner chose to merge
//!   away, and that report is copied into the audit trail first.
//! - Every applied change writes an audit event holding the before and
//!   after values, so a correction can be traced and reversed.

use crate::{
    db,
    domain::{LabReport, LabResult},
    labvalue,
    ocr::ParsedLabResult,
};
use anyhow::{Context, anyhow};
use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serde::Serialize;
use serde_json::{Value, json};

/// Load one report with all its results, household-scoped.
pub async fn get_report(
    db: &DatabaseConnection,
    household_id: i64,
    report_id: i64,
) -> anyhow::Result<Option<LabReport>> {
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT pet_id FROM lab_reports WHERE household_id=? AND id=?",
            [household_id.into(), report_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let pet_id: i64 = row.try_get("", "pet_id")?;
    Ok(db::list_lab_reports_unbounded(db, household_id, pet_id)
        .await?
        .into_iter()
        .find(|r| r.id == report_id))
}

/// Same analyte and same unit system ⇒ same measurement within a report.
fn match_key(name: &str, unit: Option<&str>) -> String {
    let unit = unit
        .map(crate::analytes::normalize_unit)
        .unwrap_or_default();
    let standard = crate::analytes::standardize(name, Some(&unit), None);
    // `standardize` folds recognised units into the analyte key; keep the
    // printed unit system so "%" and "/µL" eosinophils never match.
    format!(
        "{}|{unit}",
        standard.key.split('|').next().unwrap_or_default()
    )
}

fn printed(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '↑' | '↓' | '^' | '●' | '*'))
        .collect()
}

fn same_option(a: Option<&str>, b: Option<&str>) -> bool {
    a.map(str::trim).filter(|s| !s.is_empty()) == b.map(str::trim).filter(|s| !s.is_empty())
}

fn numbers_equal(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        (None, None) => true,
        _ => false,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Proposed {
    pub test_name: String,
    pub value_text: String,
    pub value_numeric: Option<f64>,
    pub parse_status: String,
    pub unit: Option<String>,
    pub reference_range: Option<String>,
    pub flag: Option<String>,
}

impl From<&ParsedLabResult> for Proposed {
    fn from(r: &ParsedLabResult) -> Self {
        Self {
            test_name: r.test_name.clone(),
            value_text: r.value_text.clone(),
            value_numeric: r.value_numeric,
            parse_status: r.parse_status.clone(),
            unit: r.unit.clone(),
            reference_range: r.reference_range.clone(),
            flag: r.flag.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Correction {
    pub result_id: i64,
    pub before: LabResult,
    pub after: Proposed,
    pub fields: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    pub result_id: Option<i64>,
    pub reason: String,
    pub stored: Option<LabResult>,
    pub source: Option<Proposed>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ResultPlan {
    pub missing: Vec<Proposed>,
    pub corrections: Vec<Correction>,
    pub conflicts: Vec<Conflict>,
    pub unchanged: Vec<i64>,
}

/// Compare stored results with proposed ones. `stored_not_in_source`
/// decides whether a stored result absent from the proposal is a conflict
/// (reprocessing: the source should contain everything) or simply left
/// alone (an owner's partial edit).
/// A stored row as today's parser would have written it, for matching.
/// Older imports kept the unit inside the value ("90 10^3/ul") and markdown
/// around names ("**Plaquetas grandes**").
struct StoredView {
    name: String,
    value: String,
    unit: Option<String>,
}

fn stored_view(s: &LabResult) -> StoredView {
    let name = crate::labparse::clean_stored_name(&s.test_name);
    if s.unit.is_none()
        && let Some((value, _, unit)) = crate::labparse::split_stored_value(&s.value_text)
    {
        return StoredView { name, value, unit };
    }
    StoredView {
        name,
        value: s.value_text.clone(),
        unit: s.unit.clone(),
    }
}

fn plan_results(
    stored: &[LabResult],
    proposed: &[Proposed],
    stored_not_in_source_is_conflict: bool,
) -> ResultPlan {
    let mut plan = ResultPlan::default();
    let mut used = vec![false; stored.len()];
    let views: Vec<StoredView> = stored.iter().map(stored_view).collect();
    for p in proposed {
        let key = match_key(&p.test_name, p.unit.as_deref());
        let candidates: Vec<usize> = views
            .iter()
            .enumerate()
            .filter(|(i, v)| !used[*i] && match_key(&v.name, v.unit.as_deref()) == key)
            .map(|(i, _)| i)
            .collect();
        // Prefer the stored row with the same printed value.
        let chosen = candidates
            .iter()
            .copied()
            .find(|&i| printed(&views[i].value) == printed(&p.value_text))
            .or_else(|| candidates.first().copied());
        let Some(i) = chosen else {
            plan.missing.push(p.clone());
            continue;
        };
        used[i] = true;
        let s = &stored[i];
        let view = &views[i];
        if printed(&view.value) != printed(&p.value_text) {
            plan.conflicts.push(Conflict {
                result_id: Some(s.id),
                reason: format!(
                    "Stored value \"{}\" differs from the printed value \"{}\".",
                    s.value_text, p.value_text
                ),
                stored: Some(s.clone()),
                source: Some(p.clone()),
            });
            continue;
        }
        let mut fields = Vec::new();
        if s.test_name != p.test_name {
            fields.push("test_name");
        }
        if s.value_text != p.value_text {
            // Same printed value, now stored without the unit or marker.
            fields.push("value_text");
        }
        if !numbers_equal(s.value_numeric, p.value_numeric) {
            fields.push("value_numeric");
        }
        if s.parse_status.as_deref() != Some(p.parse_status.as_str()) {
            fields.push("parse_status");
        }
        if !same_option(s.unit.as_deref(), p.unit.as_deref()) && p.unit.is_some() {
            fields.push("unit");
        }
        if !same_option(s.reference_range.as_deref(), p.reference_range.as_deref())
            && p.reference_range.is_some()
        {
            fields.push("reference_range");
        }
        if !same_option(s.flag.as_deref(), p.flag.as_deref()) && p.flag.is_some() {
            fields.push("flag");
        }
        if fields.is_empty() {
            plan.unchanged.push(s.id);
        } else {
            plan.corrections.push(Correction {
                result_id: s.id,
                before: s.clone(),
                after: p.clone(),
                fields,
            });
        }
    }
    if stored_not_in_source_is_conflict {
        for (i, s) in stored.iter().enumerate() {
            if !used[i] {
                plan.conflicts.push(Conflict {
                    result_id: Some(s.id),
                    reason: "Stored result was not found in the report text; kept as is.".into(),
                    stored: Some(s.clone()),
                    source: None,
                });
            }
        }
    }
    plan
}

/// Insert, then update, inside one transaction; returns the new result ids.
async fn apply_results<C: ConnectionTrait>(
    tx: &C,
    report_id: i64,
    missing: &[Proposed],
    corrections: &[Correction],
) -> anyhow::Result<Vec<i64>> {
    let mut added = Vec::new();
    for p in missing {
        let row = tx
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO lab_results(report_id,test_name,value_text,value_numeric,unit,reference_range,flag,parse_status) VALUES(?,?,?,?,?,?,?,?) RETURNING id",
                [
                    report_id.into(),
                    p.test_name.clone().into(),
                    p.value_text.clone().into(),
                    p.value_numeric.into(),
                    p.unit.clone().into(),
                    p.reference_range.clone().into(),
                    p.flag.clone().into(),
                    p.parse_status.clone().into(),
                ],
            ))
            .await?
            .ok_or_else(|| anyhow!("result insert returned no id"))?;
        added.push(row.try_get("", "id")?);
    }
    for c in corrections {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_results SET test_name=?,value_text=?,value_numeric=?,unit=?,reference_range=?,flag=?,parse_status=? WHERE id=? AND report_id=?",
            [
                c.after.test_name.clone().into(),
                c.after.value_text.clone().into(),
                c.after.value_numeric.into(),
                c.after.unit.clone().or(c.before.unit.clone()).into(),
                c.after
                    .reference_range
                    .clone()
                    .or(c.before.reference_range.clone())
                    .into(),
                c.after.flag.clone().or(c.before.flag.clone()).into(),
                c.after.parse_status.clone().into(),
                c.result_id.into(),
                report_id.into(),
            ],
        ))
        .await?;
    }
    Ok(added)
}

async fn audit<C: ConnectionTrait>(
    tx: &C,
    household_id: i64,
    actor: &str,
    action: &str,
    report_id: i64,
    detail: &Value,
) -> anyhow::Result<()> {
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,?,?,?,?,?)",
        [
            household_id.into(),
            actor.into(),
            action.into(),
            "lab_report".into(),
            report_id.into(),
            detail.to_string().into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- reprocess

#[derive(Debug, Serialize)]
pub struct ReprocessPreview {
    pub report_id: i64,
    pub dry_run: bool,
    pub applied: bool,
    pub number_format: labvalue::NumberLocale,
    pub results_in_source: usize,
    pub repeated_rows_ignored: usize,
    pub stored_before: usize,
    pub test_date: DateChange,
    #[serde(flatten)]
    pub plan: ResultPlan,
    pub added_result_ids: Vec<i64>,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct DateChange {
    pub stored: Option<String>,
    pub found_in_source: Option<String>,
    /// "fill" (stored is empty), "same", "conflict" (left unchanged) or "none".
    pub action: &'static str,
}

/// Re-read a report from its stored OCR text with the current parser.
pub async fn reprocess(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    report_id: i64,
    apply: bool,
) -> anyhow::Result<Result<ReprocessPreview, String>> {
    let Some(report) = get_report(db, household_id, report_id).await? else {
        return Ok(Err(
            "That blood-test report is not in this household.".into()
        ));
    };
    if report.parse_status == "assistant_transcribed" {
        return Ok(Err("This report was transcribed by an assistant, so there is no stored OCR text to re-read. Read the original report again and use upsert_blood_test_results to add or correct values.".into()));
    }
    if report.raw_text.trim().is_empty() {
        return Ok(Err("This report has no stored text to re-read.".into()));
    }
    let parsed = crate::labparse::parse_report(&report.raw_text);
    let proposed: Vec<Proposed> = parsed.results.iter().map(Proposed::from).collect();
    let plan = plan_results(&report.results, &proposed, true);
    let test_date = match (&report.test_date, &parsed.test_date) {
        (None, Some(found)) => DateChange {
            stored: None,
            found_in_source: Some(found.clone()),
            action: "fill",
        },
        (Some(s), Some(f)) if s == f => DateChange {
            stored: Some(s.clone()),
            found_in_source: Some(f.clone()),
            action: "same",
        },
        (Some(s), Some(f)) => DateChange {
            stored: Some(s.clone()),
            found_in_source: Some(f.clone()),
            action: "conflict",
        },
        (s, None) => DateChange {
            stored: s.clone(),
            found_in_source: None,
            action: "none",
        },
    };
    let mut preview = ReprocessPreview {
        report_id,
        dry_run: !apply,
        applied: false,
        number_format: parsed.locale,
        results_in_source: proposed.len(),
        repeated_rows_ignored: parsed.duplicates_removed,
        stored_before: report.results.len(),
        test_date,
        plan,
        added_result_ids: Vec::new(),
        message: String::new(),
    };
    let nothing_to_do = preview.plan.missing.is_empty()
        && preview.plan.corrections.is_empty()
        && preview.test_date.action != "fill";
    if !apply || nothing_to_do {
        preview.message = if nothing_to_do {
            "Nothing to change: the stored results already match the report.".into()
        } else {
            format!(
                "Preview only. {} missing result(s) would be added, {} corrected and the date {}. {} conflict(s) are listed and would not be changed. Show this to the owner; call again with dry_run:false and confirm:true to apply.",
                preview.plan.missing.len(),
                preview.plan.corrections.len(),
                if preview.test_date.action == "fill" {
                    "filled in"
                } else {
                    "left as is"
                },
                preview.plan.conflicts.len()
            )
        };
        return Ok(Ok(preview));
    }
    let tx = db.begin().await?;
    let added = apply_results(
        &tx,
        report_id,
        &preview.plan.missing,
        &preview.plan.corrections,
    )
    .await?;
    if preview.test_date.action == "fill" {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_reports SET test_date=? WHERE household_id=? AND id=? AND test_date IS NULL",
            [
                preview.test_date.found_in_source.clone().into(),
                household_id.into(),
                report_id.into(),
            ],
        ))
        .await?;
    }
    audit(
        &tx,
        household_id,
        actor,
        "lab_report.reprocessed",
        report_id,
        &json!({
            "added": added.iter().zip(&preview.plan.missing).map(|(id, p)| json!({"result_id": id, "result": p})).collect::<Vec<_>>(),
            "corrected": preview.plan.corrections,
            "test_date": {"before": preview.test_date.stored, "after": preview.test_date.found_in_source, "action": preview.test_date.action},
            "parser": "labparse v2",
        }),
    )
    .await?;
    tx.commit().await?;
    preview.applied = true;
    preview.added_result_ids = added;
    preview.message = format!(
        "Applied: {} result(s) added, {} corrected{}. {} conflict(s) were left unchanged.",
        preview.plan.missing.len(),
        preview.plan.corrections.len(),
        if preview.test_date.action == "fill" {
            ", date filled in"
        } else {
            ""
        },
        preview.plan.conflicts.len()
    );
    Ok(Ok(preview))
}

// ------------------------------------------------------- metadata and upsert

pub async fn update_metadata(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    report_id: i64,
    test_date: Option<String>,
    label: Option<String>,
    apply: bool,
) -> anyhow::Result<Result<Value, String>> {
    let Some(report) = get_report(db, household_id, report_id).await? else {
        return Ok(Err(
            "That blood-test report is not in this household.".into()
        ));
    };
    if test_date.is_none() && label.is_none() {
        return Ok(Err("Give test_date and/or label to change.".into()));
    }
    let mut changes = serde_json::Map::new();
    if let Some(date) = &test_date
        && report.test_date.as_deref() != Some(date.as_str())
    {
        changes.insert(
            "test_date".into(),
            json!({"before": report.test_date, "after": date}),
        );
    }
    if let Some(label) = &label
        && report.source_filename != *label
    {
        changes.insert(
            "label".into(),
            json!({"before": report.source_filename, "after": label}),
        );
    }
    if changes.is_empty() {
        return Ok(Ok(
            json!({"report_id": report_id, "applied": false, "changes": {}, "message": "Nothing to change."}),
        ));
    }
    if !apply {
        return Ok(Ok(
            json!({"report_id": report_id, "applied": false, "dry_run": true, "changes": changes,
            "message": "Preview only. Confirm with the owner, then call again with confirm:true."}),
        ));
    }
    let tx = db.begin().await?;
    if changes.contains_key("test_date") {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_reports SET test_date=? WHERE household_id=? AND id=?",
            [
                test_date.clone().into(),
                household_id.into(),
                report_id.into(),
            ],
        ))
        .await?;
    }
    if changes.contains_key("label") {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_reports SET source_filename=? WHERE household_id=? AND id=?",
            [label.clone().into(), household_id.into(), report_id.into()],
        ))
        .await?;
    }
    audit(
        &tx,
        household_id,
        actor,
        "lab_report.metadata_updated",
        report_id,
        &Value::Object(changes.clone()),
    )
    .await?;
    tx.commit().await?;
    Ok(Ok(
        json!({"report_id": report_id, "applied": true, "changes": changes}),
    ))
}

pub struct UpsertItem {
    pub result_id: Option<i64>,
    pub test_name: String,
    pub value_text: String,
    pub unit: Option<String>,
    pub reference_range: Option<String>,
    pub flag: Option<String>,
}

/// Add missing analytes or correct stored ones. Unlike reprocessing, an
/// owner-approved value change is applied: that is the point of this tool.
pub async fn upsert_results(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    report_id: i64,
    items: Vec<UpsertItem>,
    apply: bool,
) -> anyhow::Result<Result<Value, String>> {
    let Some(report) = get_report(db, household_id, report_id).await? else {
        return Ok(Err(
            "That blood-test report is not in this household.".into()
        ));
    };
    // Judge the decimal convention from the new values together with the
    // report's existing ones.
    let locale = labvalue::detect_locale(
        items
            .iter()
            .flat_map(|i| [Some(i.value_text.as_str()), i.reference_range.as_deref()])
            .chain(
                report
                    .results
                    .iter()
                    .flat_map(|r| [Some(r.value_text.as_str()), r.reference_range.as_deref()]),
            )
            .flatten(),
    );
    let mut missing = Vec::new();
    let mut corrections = Vec::new();
    let mut unchanged = Vec::new();
    for item in items {
        let number = labvalue::parse(&item.value_text, locale);
        let proposed = Proposed {
            test_name: item.test_name.clone(),
            value_text: item.value_text.clone(),
            value_numeric: number.value,
            parse_status: number.status.as_str().to_owned(),
            unit: item.unit.clone(),
            reference_range: item.reference_range.clone(),
            flag: item.flag.clone().or(number.arrow_flag.map(str::to_owned)),
        };
        let existing = match item.result_id {
            Some(id) => match report.results.iter().find(|r| r.id == id) {
                Some(r) => Some(r),
                None => {
                    return Ok(Err(format!(
                        "Result {id} is not part of report {report_id}."
                    )));
                }
            },
            None => {
                let key = match_key(&proposed.test_name, proposed.unit.as_deref());
                report
                    .results
                    .iter()
                    .find(|r| match_key(&r.test_name, r.unit.as_deref()) == key)
            }
        };
        match existing {
            None => missing.push(proposed),
            Some(before) => {
                let mut fields = Vec::new();
                if before.test_name != proposed.test_name {
                    fields.push("test_name");
                }
                if printed(&before.value_text) != printed(&proposed.value_text) {
                    fields.push("value_text");
                }
                if !numbers_equal(before.value_numeric, proposed.value_numeric) {
                    fields.push("value_numeric");
                }
                if proposed.unit.is_some()
                    && !same_option(before.unit.as_deref(), proposed.unit.as_deref())
                {
                    fields.push("unit");
                }
                if proposed.reference_range.is_some()
                    && !same_option(
                        before.reference_range.as_deref(),
                        proposed.reference_range.as_deref(),
                    )
                {
                    fields.push("reference_range");
                }
                if proposed.flag.is_some()
                    && !same_option(before.flag.as_deref(), proposed.flag.as_deref())
                {
                    fields.push("flag");
                }
                if fields.is_empty() {
                    unchanged.push(before.id);
                } else {
                    corrections.push(Correction {
                        result_id: before.id,
                        before: before.clone(),
                        after: proposed,
                        fields,
                    });
                }
            }
        }
    }
    let summary = json!({
        "report_id": report_id,
        "number_format": locale,
        "to_add": missing,
        "to_correct": corrections,
        "unchanged": unchanged,
    });
    if !apply || (missing.is_empty() && corrections.is_empty()) {
        let mut out = summary;
        out["applied"] = json!(false);
        out["message"] = json!(if missing.is_empty() && corrections.is_empty() {
            "Nothing to change."
        } else {
            "Preview only. Show the owner each addition and correction, then call again with confirm:true."
        });
        return Ok(Ok(out));
    }
    let tx = db.begin().await?;
    let added = apply_results(&tx, report_id, &missing, &corrections).await?;
    audit(
        &tx,
        household_id,
        actor,
        "lab_report.results_edited",
        report_id,
        &json!({
            "added": added.iter().zip(&missing).map(|(id, p)| json!({"result_id": id, "result": p})).collect::<Vec<_>>(),
            "corrected": corrections,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut out = summary;
    out["applied"] = json!(true);
    out["added_result_ids"] = json!(added);
    Ok(Ok(out))
}

// ----------------------------------------------------- duplicates and merge

#[derive(Debug, Serialize)]
pub struct DuplicateCandidate {
    pub report_ids: [i64; 2],
    pub test_dates: [Option<String>; 2],
    pub result_counts: [usize; 2],
    pub has_original: [bool; 2],
    pub shared_analytes: usize,
    pub same_values: usize,
    pub different_values: usize,
    /// The report to keep if they are merged: more results, then the one
    /// with the original file attached.
    pub suggested_keep: i64,
    pub reason: String,
}

fn value_of(r: &LabResult) -> Option<f64> {
    r.value_numeric
}

pub fn find_duplicates(reports: &[LabReport]) -> Vec<DuplicateCandidate> {
    let mut out = Vec::new();
    for (i, a) in reports.iter().enumerate() {
        for b in &reports[i + 1..] {
            let dates_compatible = match (&a.test_date, &b.test_date) {
                (Some(x), Some(y)) => x == y,
                _ => true,
            };
            if !dates_compatible {
                continue;
            }
            let (mut shared, mut same, mut different) = (0, 0, 0);
            let views_b: Vec<(StoredView, &LabResult)> =
                b.results.iter().map(|r| (stored_view(r), r)).collect();
            for ra in &a.results {
                let va = stored_view(ra);
                let key = match_key(&va.name, va.unit.as_deref());
                if let Some((vb, rb)) = views_b
                    .iter()
                    .find(|(vb, _)| match_key(&vb.name, vb.unit.as_deref()) == key)
                {
                    shared += 1;
                    if printed(&va.value) == printed(&vb.value)
                        || numbers_equal(value_of(ra), value_of(rb)) && value_of(ra).is_some()
                    {
                        same += 1;
                    } else {
                        different += 1;
                    }
                }
            }
            let smaller = a.results.len().min(b.results.len()).max(1);
            // Most of the smaller report's analytes appear in the other with
            // the same values.
            if shared * 2 < smaller || same * 10 < shared * 8 {
                continue;
            }
            let keep = if (b.results.len(), b.has_original) > (a.results.len(), a.has_original) {
                b.id
            } else {
                a.id
            };
            out.push(DuplicateCandidate {
                report_ids: [a.id, b.id],
                test_dates: [a.test_date.clone(), b.test_date.clone()],
                result_counts: [a.results.len(), b.results.len()],
                has_original: [a.has_original, b.has_original],
                shared_analytes: shared,
                same_values: same,
                different_values: different,
                suggested_keep: keep,
                reason: format!(
                    "{same} of {shared} shared analytes have identical values{}.",
                    if a.test_date == b.test_date {
                        " and the test dates match"
                    } else {
                        "; one report has no date"
                    }
                ),
            });
        }
    }
    out
}

/// Merge `remove` into `keep`: results only `remove` has are moved, its
/// original file is adopted if `keep` has none, then `remove` is deleted.
/// Values that disagree stay as `keep` has them and are listed.
pub async fn merge(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    keep_id: i64,
    remove_id: i64,
    apply: bool,
) -> anyhow::Result<Result<Value, String>> {
    if keep_id == remove_id {
        return Ok(Err(
            "keep_report_id and remove_report_id must differ.".into()
        ));
    }
    let (Some(keep), Some(remove)) = (
        get_report(db, household_id, keep_id).await?,
        get_report(db, household_id, remove_id).await?,
    ) else {
        return Ok(Err("Both reports must be in this household.".into()));
    };
    if keep.pet_id != remove.pet_id {
        return Ok(Err(
            "These reports belong to different pets; they are not duplicates.".into(),
        ));
    }
    let proposed: Vec<Proposed> = remove
        .results
        .iter()
        .map(|r| Proposed {
            test_name: r.test_name.clone(),
            value_text: r.value_text.clone(),
            value_numeric: r.value_numeric,
            parse_status: r.parse_status.clone().unwrap_or_else(|| "legacy".into()),
            unit: r.unit.clone(),
            reference_range: r.reference_range.clone(),
            flag: r.flag.clone(),
        })
        .collect();
    let plan = plan_results(&keep.results, &proposed, false);
    let remove_original = db::lab_report_original(db, household_id, remove_id).await?;
    let adopt_original = !keep.has_original && remove_original.is_some();
    let preview = json!({
        "keep_report_id": keep_id,
        "remove_report_id": remove_id,
        "results_to_move": plan.missing,
        "value_disagreements_kept_as_in_keep": plan.conflicts,
        "already_in_keep": plan.unchanged.len() + plan.corrections.len(),
        "adopt_original_file": adopt_original,
        "dates": [keep.test_date, remove.test_date],
    });
    if !apply {
        let mut out = preview;
        out["applied"] = json!(false);
        out["message"] = json!(format!(
            "Preview only. Report {remove_id} would be deleted after moving {} result(s) into report {keep_id}. A full copy of it is kept in the audit trail. Get the owner's explicit approval, then call again with confirm:true.",
            plan.missing.len()
        ));
        return Ok(Ok(out));
    }
    let snapshot = serde_json::to_value(&remove).context("snapshot")?;
    let tx = db.begin().await?;
    let moved = apply_results(&tx, keep_id, &plan.missing, &[]).await?;
    if adopt_original {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_reports SET original_file=(SELECT original_file FROM lab_reports WHERE household_id=? AND id=?) WHERE household_id=? AND id=? AND original_file IS NULL",
            [household_id.into(), remove_id.into(), household_id.into(), keep_id.into()],
        ))
        .await?;
    }
    if keep.test_date.is_none() && remove.test_date.is_some() {
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE lab_reports SET test_date=? WHERE household_id=? AND id=? AND test_date IS NULL",
            [remove.test_date.clone().into(), household_id.into(), keep_id.into()],
        ))
        .await?;
    }
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM lab_results WHERE report_id IN (SELECT id FROM lab_reports WHERE household_id=? AND id=?)",
        [household_id.into(), remove_id.into()],
    ))
    .await?;
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM lab_reports WHERE household_id=? AND id=?",
        [household_id.into(), remove_id.into()],
    ))
    .await?;
    audit(
        &tx,
        household_id,
        actor,
        "lab_report.merged",
        keep_id,
        &json!({
            "removed_report": snapshot,
            "removed_original_file": remove_original,
            "moved_result_ids": moved,
            "original_adopted": adopt_original,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut out = preview;
    out["applied"] = json!(true);
    out["moved_result_ids"] = json!(moved);
    Ok(Ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(id: i64, name: &str, value: &str, num: Option<f64>, unit: &str) -> LabResult {
        LabResult {
            id,
            test_name: name.into(),
            value_text: value.into(),
            value_numeric: num,
            unit: Some(unit.into()),
            reference_range: None,
            flag: None,
            parse_status: None,
        }
    }

    fn proposed(name: &str, value: &str, num: Option<f64>, unit: &str) -> Proposed {
        Proposed {
            test_name: name.into(),
            value_text: value.into(),
            value_numeric: num,
            parse_status: "validated".into(),
            unit: Some(unit.into()),
            reference_range: None,
            flag: None,
        }
    }

    #[test]
    fn legacy_rows_with_unit_inside_the_value_are_corrected_in_place() {
        let mut legacy = stored(7, "**Plaquetas grandes**", "90 10^3/ul", Some(90.0), "");
        legacy.unit = None;
        let mut caret = stored(8, "Creatinina", "^ 1,68 mg/dl", Some(1.68), "");
        caret.unit = None;
        let mut new_crea = proposed("Creatinina", "1,68", Some(1.68), "mg/dl");
        new_crea.flag = Some("high".into());
        let plan = plan_results(
            &[legacy, caret],
            &[
                proposed("Plaquetas grandes", "90", Some(90.0), "10^3/ul"),
                new_crea,
            ],
            true,
        );
        assert!(plan.missing.is_empty(), "{:?}", plan.missing);
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        assert_eq!(plan.corrections.len(), 2);
        let plt = &plan.corrections[0];
        assert_eq!(plt.result_id, 7);
        assert!(plt.fields.contains(&"test_name") && plt.fields.contains(&"unit"));
    }

    #[test]
    fn plan_separates_missing_corrections_conflicts_and_unchanged() {
        let existing = vec![
            stored(1, "EOSINÓFILOS", "6", Some(6.0), "%"),
            stored(2, "EOSINÓFILOS", "1.096,20", Some(1.096), "/µL"),
            stored(3, "HEMATOCRITO", "35,9", Some(35.9), "%"),
            stored(4, "MYSTERY", "1", Some(1.0), "U/L"),
        ];
        let mut unchanged_eos = proposed("EOSINÓFILOS", "6", Some(6.0), "%");
        unchanged_eos.parse_status = "validated".into();
        let mut existing = existing;
        existing[0].parse_status = Some("validated".into());
        existing[2].parse_status = Some("validated".into());
        let source = vec![
            unchanged_eos,
            proposed("EOSINÓFILOS", "1.096,20", Some(1096.2), "/µL"),
            proposed("HEMATOCRITO", "36,9", Some(36.9), "%"),
            proposed("UREA / SUERO", "63", Some(63.0), "mg/dL"),
        ];
        let plan = plan_results(&existing, &source, true);
        assert_eq!(plan.unchanged, vec![1]);
        assert_eq!(plan.corrections.len(), 1);
        assert_eq!(plan.corrections[0].result_id, 2);
        assert!(plan.corrections[0].fields.contains(&"value_numeric"));
        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].test_name, "UREA / SUERO");
        // A different printed value is never auto-corrected; an unknown
        // stored row is reported, not deleted.
        assert_eq!(plan.conflicts.len(), 2);
        assert_eq!(plan.conflicts[0].result_id, Some(3));
        assert_eq!(plan.conflicts[1].result_id, Some(4));
    }
}

#[cfg(test)]
mod real_database {
    /// Dry-run reprocessing against a COPY of a real database:
    /// `PHJ_DB_COPY=/path/copy.sqlite PHJ_HOUSEHOLD=2 cargo test real_database -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn preview_reprocessing_on_a_database_copy() {
        let (Ok(path), Ok(household)) =
            (std::env::var("PHJ_DB_COPY"), std::env::var("PHJ_HOUSEHOLD"))
        else {
            return;
        };
        let household: i64 = household.parse().unwrap();
        let db = crate::db::connect(&format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        crate::db::migrate(&db).await.unwrap();
        for id in 1..=20 {
            match super::reprocess(&db, household, "preview", id, false)
                .await
                .unwrap()
            {
                Ok(p) => println!(
                    "report {id}: stored {} -> source {} | add {} | correct {} | conflicts {} | date {} ({:?} -> {:?})\n  add: {}\n  correct: {}\n  conflicts: {}",
                    p.stored_before,
                    p.results_in_source,
                    p.plan.missing.len(),
                    p.plan.corrections.len(),
                    p.plan.conflicts.len(),
                    p.test_date.action,
                    p.test_date.stored,
                    p.test_date.found_in_source,
                    p.plan
                        .missing
                        .iter()
                        .map(|m| format!(
                            "{} {} {}",
                            m.test_name,
                            m.value_text,
                            m.unit.clone().unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    p.plan
                        .corrections
                        .iter()
                        .map(|c| format!(
                            "{} {:?}→{:?} [{}]",
                            c.before.test_name,
                            c.before.value_numeric,
                            c.after.value_numeric,
                            c.fields.join(",")
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    p.plan
                        .conflicts
                        .iter()
                        .map(|c| c.reason.clone()
                            + " "
                            + &c.stored
                                .as_ref()
                                .map(|s| s.test_name.clone())
                                .unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                Err(message) => println!("report {id}: {message}"),
            }
        }
        let reports = crate::db::list_lab_reports_unbounded(&db, household, 1)
            .await
            .unwrap();
        for pair in super::find_duplicates(&reports) {
            println!(
                "duplicate: {:?} keep {} — {}",
                pair.report_ids, pair.suggested_keep, pair.reason
            );
        }
    }
}
