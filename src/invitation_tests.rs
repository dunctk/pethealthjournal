//! End-to-end regression tests for household invitation onboarding.
use crate::{AppState, agent, auth, db, email, invitations, web};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

struct Site {
    app: Router,
    state: AppState,
    outbox: Arc<Mutex<Vec<email::OutgoingEmail>>>,
    _root: tempfile::TempDir,
}

impl Site {
    async fn new(with_email: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::ocr::tests::test_config(root.path());
        config.app_base_url = Some("https://pets.example".into());
        let database = db::connect("sqlite::memory:").await.unwrap();
        db::migrate(&database).await.unwrap();
        let outbox = Arc::new(Mutex::new(Vec::new()));
        let mailer = if with_email {
            email::Mailer::Capture(outbox.clone())
        } else {
            email::Mailer::Disabled
        };
        let state = AppState {
            db: database,
            agent: Arc::new(agent::CaptureAgent::new(&config)),
            chat: Arc::new(agent::ChatAgent::new(&config)),
            mailer: Arc::new(mailer),
            files: Arc::new(crate::storage::FileStore::from_config(&config).unwrap()),
            config: Arc::new(config),
        };
        Self {
            app: web::router(state.clone()),
            state,
            outbox,
            _root: root,
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> (StatusCode, Option<String>, Option<String>, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_owned());
        let cookie = response.headers().get(header::SET_COOKIE).map(|v| v.to_str().unwrap().to_owned());
        let body = String::from_utf8(to_bytes(response.into_body(), 1_000_000).await.unwrap().to_vec()).unwrap();
        (status, location, cookie, body)
    }

    async fn owner(&self) -> (crate::domain::UserAccount, String) {
        let owner = db::create_account(&self.state.db, "owner@example.com", "Owner", "hash")
            .await
            .unwrap();
        let session = db::create_session(&self.state.db, owner.id).await.unwrap();
        (owner, format!("pethealth_session={session}"))
    }

    fn invite_token(&self) -> String {
        let sent = self.outbox.lock().unwrap();
        let mail = sent.last().unwrap();
        let prefix = "https://pets.example/invite/";
        let start = mail.text.find(prefix).unwrap() + prefix.len();
        mail.text[start..].split_whitespace().next().unwrap().to_owned()
    }

    async fn invite(&self, cookie: &str, email: &str) -> String {
        let body = format!("email={}", urlencoding::encode(email));
        let (status, location, _, page) = self.request(
            "POST", "/account/household/invite", Some(cookie), &body,
        ).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        assert_eq!(location.as_deref(), Some("/account/household?sent=1"));
        self.invite_token()
    }
}

#[tokio::test]
async fn invitation_preview_is_inert_and_acceptance_creates_shared_member_session() {
    let site = Site::new(true).await;
    let (owner, owner_cookie) = site.owner().await;
    let token = site.invite(&owner_cookie, "MEMBER@EXAMPLE.COM").await;
    let (_, _, _, html) = site.request("GET", &format!("/invite/{token}"), None, "").await;
    assert!(html.contains("Accept invitation and sign in"));
    let (_, _, _, html) = site.request("GET", &format!("/invite/{token}"), None, "").await;
    assert!(html.contains("Accept invitation and sign in"), "scanners must not consume GET");

    let (status, location, cookie, _) = site.request(
        "POST", &format!("/invite/{token}/accept"), None, "",
    ).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/app"));
    let session_cookie = cookie.expect("automatic login sets a session");
    assert!(session_cookie.contains("HttpOnly"));
    assert!(session_cookie.contains("SameSite=Lax"));
    let member_cookie = session_cookie.split(';').next().unwrap();
    let (status, _, _, _) = site.request("GET", "/app", Some(member_cookie), "").await;
    assert_eq!(status, StatusCode::OK);
    let (member, _) = db::user_for_login(&site.state.db, "member@example.com")
        .await.unwrap().expect("member exists");
    assert_eq!(member.household_id, owner.household_id);
    assert!(!invitations::is_owner(&site.state.db, &member).await.unwrap());
    assert_eq!(invitations::members(&site.state.db, owner.household_id).await.unwrap().len(), 2);

    let (_, _, _, html) = site.request("GET", &format!("/invite/{token}"), None, "").await;
    assert!(html.contains("Invitation unavailable"));
    let (status, _, cookie, _) = site.request("POST", &format!("/invite/{token}/accept"), None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(cookie.is_none(), "single-use invitation never creates another session");
}

#[tokio::test]
async fn owner_can_revoke_pending_invite_and_remove_member_without_deleting_household() {
    let site = Site::new(true).await;
    let (owner, owner_cookie) = site.owner().await;
    let token = site.invite(&owner_cookie, "member@example.com").await;
    let invitations = invitations::pending(&site.state.db, owner.household_id).await.unwrap();
    assert_eq!(invitations.len(), 1);
    let invite_id = invitations[0].id;
    let (status, _, _, _) = site.request(
        "POST", &format!("/account/household/invites/{invite_id}/revoke"),
        Some(&owner_cookie), "",
    ).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, _, html) = site.request("GET", &format!("/invite/{token}"), None, "").await;
    assert!(html.contains("Invitation unavailable"));

    let token = site.invite(&owner_cookie, "member@example.com").await;
    let (_, _, cookie, _) = site.request("POST", &format!("/invite/{token}/accept"), None, "").await;
    let cookie = cookie.unwrap();
    let member_cookie = cookie.split(';').next().unwrap().to_owned();
    let (member, _) = db::user_for_login(&site.state.db, "member@example.com")
        .await.unwrap().unwrap();
    let (status, _, _, _) = site.request(
        "POST", &format!("/account/household/members/{member_id}/remove", member_id=owner.id),
        Some(&member_cookie), "",
    ).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member may not remove owners");
    let (status, _, _, _) = site.request(
        "POST", &format!("/account/household/members/{member_id}/remove", member_id=member.id),
        Some(&owner_cookie), "",
    ).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _, _) = site.request("GET", "/app", Some(&member_cookie), "").await;
    assert_eq!(status, StatusCode::SEE_OTHER, "removed member session revoked");
    assert_eq!(invitations::members(&site.state.db, owner.household_id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn owner_cannot_invite_existing_account_or_send_without_smtp() {
    let site = Site::new(true).await;
    let (_, cookie) = site.owner().await;
    let (status, _, _, page) = site.request(
        "POST", "/account/household/invite", Some(&cookie), "email=owner%40example.com",
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("already has an account"));
    assert_eq!(site.outbox.lock().unwrap().len(), 0);

    let site = Site::new(false).await;
    let (_, cookie) = site.owner().await;
    let (status, _, _, page) = site.request(
        "POST", "/account/household/invite", Some(&cookie), "email=member%40example.com",
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Configure SMTP"));
}

#[tokio::test]
async fn invite_acceptance_does_not_consume_link_when_existing_user_claims_email() {
    let site = Site::new(true).await;
    let (_, cookie) = site.owner().await;
    let token = site.invite(&cookie, "member@example.com").await;
    let existing = db::create_account(&site.state.db, "member@example.com", "Other", "hash")
        .await.unwrap();
    let (status, _, session, html) = site.request("POST", &format!("/invite/{token}/accept"), None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(session.is_none());
    assert!(html.contains("already has an account"));
    let (user, _) = db::user_for_login(&site.state.db, "member@example.com").await.unwrap().unwrap();
    assert_eq!(user.household_id, existing.household_id);
    assert!(invitations::lookup_invite(&site.state.db, &token).await.unwrap().is_some());
}

#[test]
fn invitation_tokens_are_random_and_hashed_for_storage() {
    let a = auth::new_action_token();
    let b = auth::new_action_token();
    assert_eq!(a.len(), 48);
    assert_ne!(a, b);
    assert_ne!(auth::token_hash(&a), a);
}
