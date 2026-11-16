//! Household membership through email invitations.
//!
//! An invitation is proof of access to the invited mailbox, not a password.
//! GET renders a confirmation page; only an explicit same-origin POST consumes
//! the single-use token. MCP authorization continues to use each user's OAuth.
use crate::{AppState, auth, db, domain::UserAccount, email, web};
use askama::Template;
use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, QueryResult, Statement, TransactionTrait,
};
use serde::Deserialize;

const INVITE_DAYS: i64 = 7;

#[derive(Debug)]
pub struct HouseholdMember {
    pub id: i64,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
}

#[derive(Debug)]
pub struct PendingInvite {
    pub id: i64,
    pub email: String,
    pub expires_at: String,
}

#[derive(Debug)]
pub struct InviteDetails {
    pub email: String,
    pub household_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptResult {
    Accepted(i64),
    Unusable,
    AlreadyRegistered,
}

fn sql(query: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, query, values)
}

pub async fn is_owner(db: &DatabaseConnection, user: &UserAccount) -> anyhow::Result<bool> {
    Ok(db
        .query_one(sql(
            "SELECT 1 FROM users WHERE id=? AND household_id=? AND role='owner'",
            vec![user.id.into(), user.household_id.into()],
        ))
        .await?
        .is_some())
}

pub async fn members(
    db: &DatabaseConnection,
    household_id: i64,
) -> anyhow::Result<Vec<HouseholdMember>> {
    db.query_all(sql(
        "SELECT id,display_name,email,role FROM users WHERE household_id=? ORDER BY role DESC,id",
        vec![household_id.into()],
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok(HouseholdMember {
            id: row.try_get("", "id")?,
            display_name: row.try_get("", "display_name")?,
            email: row.try_get("", "email")?,
            role: row.try_get("", "role")?,
        })
    })
    .collect()
}

pub async fn pending(
    db: &DatabaseConnection,
    household_id: i64,
) -> anyhow::Result<Vec<PendingInvite>> {
    db.query_all(sql(
        "SELECT id,email,expires_at FROM household_invites WHERE household_id=? AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at>? ORDER BY created_at DESC",
        vec![household_id.into(), Utc::now().to_rfc3339().into()],
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok(PendingInvite {
            id: row.try_get("", "id")?,
            email: row.try_get("", "email")?,
            expires_at: row.try_get("", "expires_at")?,
        })
    })
    .collect()
}

/// Creates/replaces the pending invitation for this address. Caller must be
/// an owner; the database checks this again inside the transaction.
pub async fn create_invite(
    db: &DatabaseConnection,
    owner: &UserAccount,
    email: &str,
    token: &str,
) -> anyhow::Result<()> {
    let tx = db.begin().await?;
    let allowed = tx
        .query_one(sql(
            "SELECT 1 FROM users WHERE id=? AND household_id=? AND role='owner'",
            vec![owner.id.into(), owner.household_id.into()],
        ))
        .await?
        .is_some();
    anyhow::ensure!(allowed, "Only a household owner can invite members.");
    let existing = tx
        .query_one(sql(
            "SELECT 1 FROM users WHERE email=? COLLATE NOCASE OR username=? COLLATE NOCASE",
            vec![email.into(), email.into()],
        ))
        .await?
        .is_some();
    anyhow::ensure!(
        !existing,
        "That email already has an account. Existing accounts cannot be transferred by invitation."
    );
    let now = Utc::now();
    tx.execute(sql(
        "UPDATE household_invites SET revoked_at=? WHERE household_id=? AND email=? COLLATE NOCASE AND accepted_at IS NULL AND revoked_at IS NULL",
        vec![now.to_rfc3339().into(), owner.household_id.into(), email.into()],
    ))
    .await?;
    tx.execute(sql(
        "INSERT INTO household_invites(household_id,invited_by,email,token_hash,created_at,expires_at) VALUES(?,?,?,?,?,?)",
        vec![
            owner.household_id.into(),
            owner.id.into(),
            email.into(),
            auth::token_hash(token).into(),
            now.to_rfc3339().into(),
            (now + Duration::days(INVITE_DAYS)).to_rfc3339().into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn lookup_invite(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<InviteDetails>> {
    // No side effects here: email link previewers and scanners perform GETs.
    let row = db
        .query_one(sql(
            "SELECT i.email,h.name FROM household_invites i JOIN households h ON h.id=i.household_id WHERE i.token_hash=? AND i.accepted_at IS NULL AND i.revoked_at IS NULL AND i.expires_at>?",
            vec![auth::token_hash(token).into(), Utc::now().to_rfc3339().into()],
        ))
        .await?;
    row.map(|row| {
        Ok(InviteDetails {
            email: row.try_get("", "email")?,
            household_name: row.try_get("", "name")?,
        })
    })
    .transpose()
}

pub async fn accept_invite(db: &DatabaseConnection, token: &str) -> anyhow::Result<AcceptResult> {
    // The temporary password cannot be inferred from the invitation URL.
    // Email-code login or Google OAuth can be used on later visits.
    let password_hash = auth::hash_password(auth::new_session_token()).await?;
    let tx = db.begin().await?;
    let now = Utc::now().to_rfc3339();
    let row = tx
        .query_one(sql(
            "UPDATE household_invites SET accepted_at=? WHERE token_hash=? AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at>? RETURNING id,household_id,email",
            vec![now.clone().into(), auth::token_hash(token).into(), now.clone().into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(AcceptResult::Unusable);
    };
    let invite_id: i64 = row.try_get("", "id")?;
    let household_id: i64 = row.try_get("", "household_id")?;
    let email: String = row.try_get("", "email")?;
    if tx
        .query_one(sql(
            "SELECT id FROM users WHERE email=? COLLATE NOCASE OR username=? COLLATE NOCASE",
            vec![email.clone().into(), email.clone().into()],
        ))
        .await?
        .is_some()
    {
        // Roll back consumption. A previously registered user must never be
        // silently moved from one household to another.
        return Ok(AcceptResult::AlreadyRegistered);
    }
    let display_name = "Household member";
    let user = tx
        .query_one(sql(
            "INSERT INTO users(household_id,username,email,display_name,password_hash,role,created_at) VALUES(?,?,?,?,?,'member',?) RETURNING id",
            vec![
                household_id.into(),
                email.clone().into(),
                email.into(),
                display_name.into(),
                password_hash.into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("member insertion returned no id"))?;
    let user_id: i64 = user.try_get("", "id")?;
    tx.execute(sql(
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?, 'household.member_joined','user',?,'Member joined by email invitation',?)",
        vec![
            household_id.into(),
            format!("user:{user_id}").into(),
            user_id.into(),
            now.into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    let _ = invite_id;
    Ok(AcceptResult::Accepted(user_id))
}

pub async fn revoke_invite(
    db: &DatabaseConnection,
    owner: &UserAccount,
    invite_id: i64,
) -> anyhow::Result<bool> {
    if !is_owner(db, owner).await? {
        return Ok(false);
    }
    let res = db.execute(sql(
        "UPDATE household_invites SET revoked_at=? WHERE id=? AND household_id=? AND accepted_at IS NULL AND revoked_at IS NULL",
        vec![Utc::now().to_rfc3339().into(), invite_id.into(), owner.household_id.into()],
    ))
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn revoke_invite_token(
    db: &DatabaseConnection,
    owner: &UserAccount,
    token: &str,
) -> anyhow::Result<()> {
    if is_owner(db, owner).await? {
        db.execute(sql(
            "UPDATE household_invites SET revoked_at=? WHERE household_id=? AND token_hash=? AND accepted_at IS NULL",
            vec![Utc::now().to_rfc3339().into(), owner.household_id.into(), auth::token_hash(token).into()],
        ))
        .await?;
    }
    Ok(())
}

pub async fn remove_member(
    db: &DatabaseConnection,
    owner: &UserAccount,
    member_id: i64,
) -> anyhow::Result<bool> {
    let tx = db.begin().await?;
    let allowed = tx
        .query_one(sql(
            "SELECT 1 FROM users WHERE id=? AND household_id=? AND role='owner'",
            vec![owner.id.into(), owner.household_id.into()],
        ))
        .await?
        .is_some();
    if !allowed {
        return Ok(false);
    }
    // Deleting a member revokes their sessions, API tokens, OAuth tokens,
    // codes and device grants via foreign keys, never the shared pet data.
    let res = tx
        .execute(sql(
            "DELETE FROM users WHERE id=? AND household_id=? AND role='member'",
            vec![member_id.into(), owner.household_id.into()],
        ))
        .await?;
    if res.rows_affected() > 0 {
        tx.execute(sql(
            "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,'household.member_removed','user',?,'Member access revoked',?)",
            vec![
                owner.household_id.into(),
                owner.audit_actor().into(),
                member_id.into(),
                Utc::now().to_rfc3339().into(),
            ],
        )).await?;
    }
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

#[derive(Template)]
#[template(path = "household.html")]
struct HouseholdTemplate {
    is_owner: bool,
    members: Vec<HouseholdMember>,
    invites: Vec<PendingInvite>,
    notice: Option<String>,
    error: Option<String>,
    email_enabled: bool,
}

#[derive(Template)]
#[template(path = "invite.html")]
struct InviteTemplate {
    token: String,
    invite: Option<InviteDetails>,
    error: Option<String>,
}

fn render_page<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Could not display page").into_response(),
    }
}

async fn household_response(
    state: &AppState,
    user: &UserAccount,
    notice: Option<String>,
    error: Option<String>,
) -> Response {
    let page = async {
        Ok::<_, anyhow::Error>(HouseholdTemplate {
            is_owner: is_owner(&state.db, user).await?,
            members: members(&state.db, user.household_id).await?,
            invites: pending(&state.db, user.household_id).await?,
            notice,
            error,
            email_enabled: state.mailer.enabled(),
        })
    }
    .await;
    match page {
        Ok(page) => render_page(&page),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load household",
        )
            .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    sent: Option<String>,
}

pub async fn household_page(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Query(query): Query<PageQuery>,
) -> Response {
    household_response(
        &state,
        &user,
        query.sent.map(|_| "Invitation emailed.".to_owned()),
        None,
    )
    .await
}

#[derive(Deserialize)]
pub struct InviteForm {
    email: String,
}

fn valid_email(input: &str) -> Option<String> {
    let email = input.trim().to_ascii_lowercase();
    let mut parts = email.split('@');
    let local = parts.next()?;
    let domain = parts.next()?;
    (email.len() <= 254
        && !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && parts.next().is_none()
        && !email.chars().any(char::is_whitespace))
    .then_some(email)
}

pub async fn send_invite(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Form(form): Form<InviteForm>,
) -> Response {
    if !is_owner(&state.db, &user).await.unwrap_or(false) {
        return (StatusCode::FORBIDDEN, "Only owners can invite members").into_response();
    }
    let Some(email) = valid_email(&form.email) else {
        return household_response(
            &state,
            &user,
            None,
            Some("Enter a valid email address.".into()),
        )
        .await;
    };
    if !state.mailer.enabled() {
        return household_response(
            &state,
            &user,
            None,
            Some("Configure SMTP to email invitations.".into()),
        )
        .await;
    }
    // A fixed configured origin prevents Host-header poisoning of emailed
    // authentication links. Production invitations require HTTPS.
    let Some(base) = state.config.app_base_url.as_deref() else {
        return household_response(
            &state,
            &user,
            None,
            Some("Set APP_BASE_URL to send invitations.".into()),
        )
        .await;
    };
    let base = base.trim_end_matches('/');
    if reqwest::Url::parse(base).ok().is_none_or(|url| {
        url.host_str().is_none()
            || !matches!(url.scheme(), "https" | "http")
            || (state.config.production && url.scheme() != "https")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
    }) {
        return household_response(
            &state,
            &user,
            None,
            Some("APP_BASE_URL must be the site's origin (HTTPS in production).".into()),
        )
        .await;
    }
    let token = auth::new_action_token();
    if let Err(error) = create_invite(&state.db, &user, &email, &token).await {
        return household_response(&state, &user, None, Some(error.to_string())).await;
    }
    let link = format!("{base}/invite/{token}");
    if let Err(error) = state
        .mailer
        .send(email::household_invite_email(&email, &link))
        .await
    {
        tracing::warn!(%error, "household invitation email delivery failed");
        let _ = revoke_invite_token(&state.db, &user, &token).await;
        return household_response(
            &state,
            &user,
            None,
            Some("The invitation could not be emailed. Please retry.".into()),
        )
        .await;
    }
    Redirect::to("/account/household?sent=1").into_response()
}

pub async fn cancel_invite(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
) -> Response {
    match revoke_invite(&state.db, &user, id).await {
        Ok(true) => Redirect::to("/account/household").into_response(),
        Ok(false) => (StatusCode::FORBIDDEN, "Invite not found or not permitted").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to revoke invitation",
        )
            .into_response(),
    }
}

pub async fn remove_household_member(
    State(state): State<AppState>,
    Extension(user): Extension<UserAccount>,
    Path(id): Path<i64>,
) -> Response {
    match remove_member(&state.db, &user, id).await {
        Ok(true) => Redirect::to("/account/household").into_response(),
        Ok(false) => (StatusCode::FORBIDDEN, "Member not found or not permitted").into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Unable to remove member").into_response(),
    }
}

pub async fn invitation_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let invite = if token.len() == 48 && token.bytes().all(|value| value.is_ascii_alphanumeric()) {
        lookup_invite(&state.db, &token).await.ok().flatten()
    } else {
        None
    };
    render_page(&InviteTemplate {
        token,
        invite,
        error: None,
    })
}

pub async fn accept_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !web::invite_form_same_origin(&state, &headers) {
        return (
            StatusCode::FORBIDDEN,
            "Cross-origin invitation acceptance rejected",
        )
            .into_response();
    }
    if token.len() != 48 || !token.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return render_page(&InviteTemplate {
            token,
            invite: None,
            error: None,
        });
    }
    match accept_invite(&state.db, &token).await {
        Ok(AcceptResult::Accepted(user_id)) => match db::create_session(&state.db, user_id).await {
            Ok(session) => web::invite_session_redirect(&state, &session),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed. Use email login.").into_response(),
        },
        Ok(AcceptResult::AlreadyRegistered) => render_page(&InviteTemplate {
            token,
            invite: lookup_invite(&state.db, &token).await.ok().flatten(),
            error: Some("This address already has an account. Contact the household owner; existing accounts are never moved automatically.".into()),
        }),
        Ok(AcceptResult::Unusable) => render_page(&InviteTemplate { token, invite: None, error: None }),
        Err(error) => {
            tracing::error!(%error, "invitation acceptance failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Could not accept invitation.").into_response()
        }
    }
}
