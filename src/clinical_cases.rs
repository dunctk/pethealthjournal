//! Owner-led clinical investigations. Structured context for a vet, never a diagnosis.
use crate::db;
use anyhow::{Result, anyhow};
use chrono::{NaiveDate, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, QueryResult, Statement, TransactionTrait,
};
use serde::Serialize;

pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS clinical_cases (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 household_id INTEGER NOT NULL REFERENCES households(id),
 pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
 title TEXT NOT NULL,
 presenting_concern TEXT NOT NULL,
 status TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open','monitoring','resolved')),
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cases_household_pet ON clinical_cases(household_id,pet_id,updated_at DESC);
CREATE TABLE IF NOT EXISTS clinical_case_updates (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 household_id INTEGER NOT NULL REFERENCES households(id),
 pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
 case_id INTEGER NOT NULL REFERENCES clinical_cases(id) ON DELETE CASCADE,
 kind TEXT NOT NULL CHECK(kind IN ('observation','treatment','response','vet_finding','test','question')),
 original_wording TEXT NOT NULL,
 observed_on TEXT,
 source TEXT NOT NULL CHECK(source IN ('owner_reported','owner_relayed_vet')),
 linked_event_id INTEGER REFERENCES health_events(id) ON DELETE SET NULL,
 created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_case_updates ON clinical_case_updates(household_id,case_id,created_at DESC);
CREATE TABLE IF NOT EXISTS clinical_case_hypotheses (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 household_id INTEGER NOT NULL REFERENCES households(id),
 pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
 case_id INTEGER NOT NULL REFERENCES clinical_cases(id) ON DELETE CASCADE,
 label TEXT NOT NULL,
 status TEXT NOT NULL DEFAULT 'unreviewed' CHECK(status IN ('unreviewed','under_review','less_likely','ruled_out','vet_supported')),
 rationale TEXT,
 reported_by TEXT NOT NULL CHECK(reported_by IN ('owner_question','owner_relayed_vet')),
 created_at TEXT NOT NULL,
 updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_case_hypotheses ON clinical_case_hypotheses(household_id,case_id,created_at DESC);
"#;

#[derive(Clone, Debug, Serialize)]
pub struct ClinicalCase {
    pub id: i64,
    pub pet_id: i64,
    pub title: String,
    pub presenting_concern: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CaseUpdate {
    pub id: i64,
    pub case_id: i64,
    pub kind: String,
    pub original_wording: String,
    pub observed_on: Option<String>,
    pub source: String,
    pub linked_event_id: Option<i64>,
    pub created_at: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CaseHypothesis {
    pub id: i64,
    pub case_id: i64,
    pub label: String,
    pub status: String,
    pub rationale: Option<String>,
    pub reported_by: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CaseDetail {
    pub investigation: ClinicalCase,
    pub updates: Vec<CaseUpdate>,
    pub hypotheses: Vec<CaseHypothesis>,
}

pub fn bounded(value: &str, limit: usize, name: &str) -> Result<String> {
    let clean = value.trim();
    if clean.is_empty() || clean.chars().count() > limit {
        return Err(anyhow!("{name} must contain 1–{limit} characters"));
    }
    Ok(clean.to_owned())
}
pub fn date_or_none(value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow!("observed_on must be YYYY-MM-DD"))?;
    if date > Utc::now().date_naive() {
        return Err(anyhow!("observation date cannot be in the future"));
    }
    Ok(Some(date.to_string()))
}
fn one_of<'a>(value: &'a str, allowed: &[&str], name: &str) -> Result<&'a str> {
    if !allowed.contains(&value) {
        return Err(anyhow!("invalid {name}"));
    }
    Ok(value)
}
fn case_from_row(row: QueryResult) -> Result<ClinicalCase> {
    Ok(ClinicalCase {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        title: row.try_get("", "title")?,
        presenting_concern: row.try_get("", "presenting_concern")?,
        status: row.try_get("", "status")?,
        created_at: row.try_get("", "created_at")?,
        updated_at: row.try_get("", "updated_at")?,
    })
}
fn update_from_row(row: QueryResult) -> Result<CaseUpdate> {
    Ok(CaseUpdate {
        id: row.try_get("", "id")?,
        case_id: row.try_get("", "case_id")?,
        kind: row.try_get("", "kind")?,
        original_wording: row.try_get("", "original_wording")?,
        observed_on: row.try_get("", "observed_on")?,
        source: row.try_get("", "source")?,
        linked_event_id: row.try_get("", "linked_event_id")?,
        created_at: row.try_get("", "created_at")?,
    })
}
fn hypothesis_from_row(row: QueryResult) -> Result<CaseHypothesis> {
    Ok(CaseHypothesis {
        id: row.try_get("", "id")?,
        case_id: row.try_get("", "case_id")?,
        label: row.try_get("", "label")?,
        status: row.try_get("", "status")?,
        rationale: row.try_get("", "rationale")?,
        reported_by: row.try_get("", "reported_by")?,
        created_at: row.try_get("", "created_at")?,
        updated_at: row.try_get("", "updated_at")?,
    })
}
async fn audit(
    tx: &sea_orm::DatabaseTransaction,
    household: i64,
    actor: &str,
    action: &str,
    table: &str,
    id: i64,
    detail: &str,
    at: &str,
) -> Result<()> {
    tx.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,?,?,?,?,?)",
        [household.into(),actor.into(),action.into(),table.into(),id.into(),detail.into(),at.into()],
    )).await?;
    Ok(())
}
async fn touched(
    tx: &sea_orm::DatabaseTransaction,
    household: i64,
    case_id: i64,
    at: &str,
) -> Result<()> {
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE clinical_cases SET updated_at=? WHERE household_id=? AND id=?",
        [at.into(), household.into(), case_id.into()],
    ))
    .await?;
    Ok(())
}
pub async fn list(
    db: &DatabaseConnection,
    household: i64,
    pet_id: i64,
) -> Result<Vec<ClinicalCase>> {
    let rows=db.query_all(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT id,pet_id,title,presenting_concern,status,created_at,updated_at FROM clinical_cases WHERE household_id=? AND pet_id=? ORDER BY updated_at DESC LIMIT 50",
        [household.into(),pet_id.into()],
    )).await?;
    rows.into_iter().map(case_from_row).collect()
}
pub async fn get(
    db: &DatabaseConnection,
    household: i64,
    case_id: i64,
) -> Result<Option<CaseDetail>> {
    let row=db.query_one(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT id,pet_id,title,presenting_concern,status,created_at,updated_at FROM clinical_cases WHERE household_id=? AND id=?",
        [household.into(),case_id.into()],
    )).await?;
    let Some(row) = row else { return Ok(None) };
    let investigation = case_from_row(row)?;
    let updates=db.query_all(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT id,case_id,kind,original_wording,observed_on,source,linked_event_id,created_at FROM clinical_case_updates WHERE household_id=? AND pet_id=? AND case_id=? ORDER BY created_at ASC,id ASC LIMIT 500",
        [household.into(),investigation.pet_id.into(),case_id.into()],
    )).await?.into_iter().map(update_from_row).collect::<Result<Vec<_>>>()?;
    let hypotheses=db.query_all(Statement::from_sql_and_values(DbBackend::Sqlite,
        "SELECT id,case_id,label,status,rationale,reported_by,created_at,updated_at FROM clinical_case_hypotheses WHERE household_id=? AND pet_id=? AND case_id=? ORDER BY created_at ASC,id ASC LIMIT 100",
        [household.into(),investigation.pet_id.into(),case_id.into()],
    )).await?.into_iter().map(hypothesis_from_row).collect::<Result<Vec<_>>>()?;
    Ok(Some(CaseDetail {
        investigation,
        updates,
        hypotheses,
    }))
}
pub async fn create(
    db: &DatabaseConnection,
    household: i64,
    pet_id: i64,
    actor: &str,
    title: &str,
    concern: &str,
) -> Result<i64> {
    let title = bounded(title, 120, "title")?;
    let concern = bounded(concern, 2000, "presenting_concern")?;
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row=tx.query_one(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO clinical_cases(household_id,pet_id,title,presenting_concern,created_at,updated_at) SELECT ?,id,?,?,?,? FROM pets WHERE household_id=? AND id=? RETURNING id",
        [household.into(),title.clone().into(),concern.clone().into(),now.clone().into(),now.clone().into(),household.into(),pet_id.into()],
    )).await?.ok_or_else(|| anyhow!("pet not found in household"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &tx,
        household,
        actor,
        "clinical_case.created",
        "clinical_case",
        id,
        &concern,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
#[allow(clippy::too_many_arguments)]
pub async fn add_update(
    db: &DatabaseConnection,
    household: i64,
    case_id: i64,
    actor: &str,
    kind: &str,
    wording: &str,
    observed_on: Option<&str>,
    source: &str,
    linked_event_id: Option<i64>,
) -> Result<i64> {
    let detail = get(db, household, case_id)
        .await?
        .ok_or_else(|| anyhow!("investigation not found"))?;
    one_of(
        kind,
        &[
            "observation",
            "treatment",
            "response",
            "vet_finding",
            "test",
            "question",
        ],
        "kind",
    )?;
    one_of(source, &["owner_reported", "owner_relayed_vet"], "source")?;
    let wording = bounded(wording, 3000, "original_wording")?;
    let date = date_or_none(observed_on)?;
    if let Some(event_id) = linked_event_id {
        if db::get_event(db, household, detail.investigation.pet_id, event_id)
            .await?
            .is_none()
        {
            return Err(anyhow!(
                "linked event must be an active event belonging to this pet"
            ));
        }
    }
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row=tx.query_one(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO clinical_case_updates(household_id,pet_id,case_id,kind,original_wording,observed_on,source,linked_event_id,created_at) VALUES(?,?,?,?,?,?,?,?,?) RETURNING id",
        [household.into(),detail.investigation.pet_id.into(),case_id.into(),kind.into(),wording.clone().into(),date.into(),source.into(),linked_event_id.into(),now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("update insert failed"))?;
    let id: i64 = row.try_get("", "id")?;
    touched(&tx, household, case_id, &now).await?;
    audit(
        &tx,
        household,
        actor,
        "clinical_case.update_added",
        "clinical_case_update",
        id,
        &wording,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn add_hypothesis(
    db: &DatabaseConnection,
    household: i64,
    case_id: i64,
    actor: &str,
    label: &str,
    rationale: Option<&str>,
    reported_by: &str,
) -> Result<i64> {
    let detail = get(db, household, case_id)
        .await?
        .ok_or_else(|| anyhow!("investigation not found"))?;
    one_of(
        reported_by,
        &["owner_question", "owner_relayed_vet"],
        "reported_by",
    )?;
    let label = bounded(label, 200, "hypothesis")?;
    let rationale = rationale
        .filter(|v| !v.trim().is_empty())
        .map(|v| bounded(v, 2000, "rationale"))
        .transpose()?;
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row=tx.query_one(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO clinical_case_hypotheses(household_id,pet_id,case_id,label,rationale,reported_by,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?) RETURNING id",
        [household.into(),detail.investigation.pet_id.into(),case_id.into(),label.clone().into(),rationale.into(),reported_by.into(),now.clone().into(),now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("hypothesis insert failed"))?;
    let id: i64 = row.try_get("", "id")?;
    touched(&tx, household, case_id, &now).await?;
    audit(
        &tx,
        household,
        actor,
        "clinical_case.hypothesis_added",
        "clinical_case_hypothesis",
        id,
        &label,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn set_hypothesis_status(
    db: &DatabaseConnection,
    household: i64,
    case_id: i64,
    hypothesis_id: i64,
    actor: &str,
    status: &str,
    rationale: &str,
    reported_by: &str,
) -> Result<bool> {
    one_of(
        status,
        &[
            "unreviewed",
            "under_review",
            "less_likely",
            "ruled_out",
            "vet_supported",
        ],
        "status",
    )?;
    one_of(
        reported_by,
        &["owner_question", "owner_relayed_vet"],
        "reported_by",
    )?;
    if ["ruled_out", "vet_supported"].contains(&status) && reported_by != "owner_relayed_vet" {
        return Err(anyhow!(
            "only record ruled_out or vet_supported when relaying an explicit veterinary assessment"
        ));
    }
    let rationale = bounded(rationale, 2000, "rationale")?;
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let changed=tx.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "UPDATE clinical_case_hypotheses SET status=?,rationale=?,reported_by=?,updated_at=? WHERE household_id=? AND case_id=? AND id=?",
        [status.into(),rationale.clone().into(),reported_by.into(),now.clone().into(),household.into(),case_id.into(),hypothesis_id.into()],
    )).await?.rows_affected()==1;
    if changed {
        touched(&tx, household, case_id, &now).await?;
        audit(
            &tx,
            household,
            actor,
            "clinical_case.hypothesis_reviewed",
            "clinical_case_hypothesis",
            hypothesis_id,
            &rationale,
            &now,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}
pub async fn set_status(
    db: &DatabaseConnection,
    household: i64,
    case_id: i64,
    actor: &str,
    status: &str,
) -> Result<bool> {
    one_of(status, &["open", "monitoring", "resolved"], "status")?;
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let changed = tx
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE clinical_cases SET status=?,updated_at=? WHERE household_id=? AND id=?",
            [
                status.into(),
                now.clone().into(),
                household.into(),
                case_id.into(),
            ],
        ))
        .await?
        .rows_affected()
        == 1;
    if changed {
        audit(
            &tx,
            household,
            actor,
            "clinical_case.status_changed",
            "clinical_case",
            case_id,
            status,
            &now,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsupported_status_and_future_dates() {
        assert!(one_of("diagnosed", &["unreviewed", "under_review"], "status").is_err());
        assert!(date_or_none(Some("not-a-date")).is_err());
        assert!(bounded("   ", 120, "title").is_err());
    }
}

// Web forms deliberately use the same repository functions as MCP; all routes
// are mounted inside web::router's authenticated + same-origin protected router.
use crate::{
    AppState,
    domain::{HealthEvent, LabReport, MedicationPrescription, Pet, UserAccount},
};
use askama::Template;
use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, Redirect},
};
use serde::Deserialize;
type WebError = (StatusCode, String);
fn web_err(error: impl std::fmt::Display) -> WebError {
    (StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
}
fn missing() -> WebError {
    (StatusCode::NOT_FOUND, "Investigation not found".to_owned())
}
fn rendered(template: &impl Template) -> Result<Html<String>, WebError> {
    template
        .render()
        .map(Html)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
#[derive(Deserialize)]
pub struct PetCaseQuery {
    pet_id: i64,
}
#[derive(Deserialize)]
pub struct NewCaseForm {
    pet_id: i64,
    title: String,
    presenting_concern: String,
}
#[derive(Deserialize)]
pub struct NewUpdateForm {
    kind: String,
    original_wording: String,
    observed_on: Option<String>,
    source: String,
    linked_event_id: Option<i64>,
}
#[derive(Deserialize)]
pub struct NewHypothesisForm {
    label: String,
    rationale: Option<String>,
    reported_by: String,
}
#[derive(Deserialize)]
pub struct HypothesisStatusForm {
    status: String,
    rationale: String,
    reported_by: String,
}
#[derive(Deserialize)]
pub struct CaseStatusForm {
    status: String,
}
#[derive(Template)]
#[template(path = "clinical_cases.html")]
struct CasesPage {
    pet: Pet,
    investigations: Vec<ClinicalCase>,
}
#[derive(Template)]
#[template(path = "clinical_case.html")]
struct DetailPage {
    pet: Pet,
    detail: CaseDetail,
}
#[derive(Template)]
#[template(path = "clinical_case_brief.html")]
struct BriefPage {
    pet: Pet,
    detail: CaseDetail,
    prescriptions: Vec<MedicationPrescription>,
    events: Vec<HealthEvent>,
    labs: Vec<LabReport>,
}
pub async fn list_page(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PetCaseQuery>,
) -> Result<Html<String>, WebError> {
    let pet = db::get_pet(&state.db, user.household_id, query.pet_id)
        .await
        .map_err(web_err)?
        .ok_or_else(missing)?;
    let investigations = list(&state.db, user.household_id, pet.id)
        .await
        .map_err(web_err)?;
    rendered(&CasesPage {
        pet,
        investigations,
    })
}
pub async fn create_form(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<NewCaseForm>,
) -> Result<Redirect, WebError> {
    let id = create(
        &state.db,
        user.household_id,
        form.pet_id,
        &user.audit_actor(),
        &form.title,
        &form.presenting_concern,
    )
    .await
    .map_err(web_err)?;
    Ok(Redirect::to(&format!("/clinical-cases/{id}")))
}
async fn load_for_user(
    state: &AppState,
    user: &UserAccount,
    id: i64,
) -> Result<(Pet, CaseDetail), WebError> {
    let detail = get(&state.db, user.household_id, id)
        .await
        .map_err(web_err)?
        .ok_or_else(missing)?;
    let pet = db::get_pet(&state.db, user.household_id, detail.investigation.pet_id)
        .await
        .map_err(web_err)?
        .ok_or_else(missing)?;
    Ok((pet, detail))
}
pub async fn detail_page(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
) -> Result<Html<String>, WebError> {
    let (pet, detail) = load_for_user(&state, &user, id).await?;
    rendered(&DetailPage { pet, detail })
}
pub async fn add_update_form(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
    Form(form): Form<NewUpdateForm>,
) -> Result<Redirect, WebError> {
    add_update(
        &state.db,
        user.household_id,
        id,
        &user.audit_actor(),
        &form.kind,
        &form.original_wording,
        form.observed_on.as_deref(),
        &form.source,
        form.linked_event_id,
    )
    .await
    .map_err(web_err)?;
    Ok(Redirect::to(&format!("/clinical-cases/{id}")))
}
pub async fn add_hypothesis_form(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
    Form(form): Form<NewHypothesisForm>,
) -> Result<Redirect, WebError> {
    add_hypothesis(
        &state.db,
        user.household_id,
        id,
        &user.audit_actor(),
        &form.label,
        form.rationale.as_deref(),
        &form.reported_by,
    )
    .await
    .map_err(web_err)?;
    Ok(Redirect::to(&format!("/clinical-cases/{id}")))
}
pub async fn hypothesis_status_form(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path((id, hypothesis_id)): Path<(i64, i64)>,
    Form(form): Form<HypothesisStatusForm>,
) -> Result<Redirect, WebError> {
    if !set_hypothesis_status(
        &state.db,
        user.household_id,
        id,
        hypothesis_id,
        &user.audit_actor(),
        &form.status,
        &form.rationale,
        &form.reported_by,
    )
    .await
    .map_err(web_err)?
    {
        return Err(missing());
    }
    Ok(Redirect::to(&format!("/clinical-cases/{id}")))
}
pub async fn case_status_form(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
    Form(form): Form<CaseStatusForm>,
) -> Result<Redirect, WebError> {
    if !set_status(
        &state.db,
        user.household_id,
        id,
        &user.audit_actor(),
        &form.status,
    )
    .await
    .map_err(web_err)?
    {
        return Err(missing());
    }
    Ok(Redirect::to(&format!("/clinical-cases/{id}")))
}
pub async fn brief_page(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
) -> Result<Html<String>, WebError> {
    let (pet, detail) = load_for_user(&state, &user, id).await?;
    // Context is clearly separated from investigation-specific updates.
    let prescriptions = db::list_prescriptions(&state.db, user.household_id, pet.id, 30)
        .await
        .map_err(web_err)?;
    let events = db::list_events(&state.db, user.household_id, Some(pet.id), 40)
        .await
        .map_err(web_err)?;
    let labs = db::list_lab_reports(&state.db, user.household_id, pet.id)
        .await
        .map_err(web_err)?;
    rendered(&BriefPage {
        pet,
        detail,
        prescriptions,
        events,
        labs,
    })
}
