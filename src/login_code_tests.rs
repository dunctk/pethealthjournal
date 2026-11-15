//! Sign-in flows a ChatGPT connection depends on: a brand-new person must
//! come back to `/oauth/authorize` after signing up, whichever way they sign in.

use crate::{AppState, agent, db, email, web};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const NEXT: &str = "/oauth/authorize?response_type=code&client_id=chatgpt&redirect_uri=https%3A%2F%2Fchatgpt.com%2Fcb&code_challenge=abc&code_challenge_method=S256&state=xyz";

struct Site {
    app: Router,
    state: AppState,
    outbox: Arc<Mutex<Vec<email::OutgoingEmail>>>,
    _root: tempfile::TempDir,
}

impl Site {
    async fn new(with_email: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = crate::ocr::tests::test_config(root.path());
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

    async fn get(&self, path: &str) -> (StatusCode, Option<String>, String) {
        let response = self
            .app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        read(response).await
    }

    async fn post(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> (StatusCode, Option<String>, String) {
        let body = fields
            .iter()
            .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("x-forwarded-for", "203.0.113.7")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        read(response).await
    }

    fn last_code(&self) -> String {
        let outbox = self.outbox.lock().unwrap();
        let subject = &outbox.last().expect("an email was sent").subject;
        subject[..6].to_owned()
    }

    async fn request_code(&self, address: &str) -> String {
        let (status, _, page) = self
            .post("/login/code", &[("email", address), ("next", NEXT)])
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        challenge_in(&page)
    }
}

async fn read(response: axum::response::Response) -> (StatusCode, Option<String>, String) {
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned());
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        location,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn challenge_in(page: &str) -> String {
    let marker = "name=\"challenge\" value=\"";
    let start = page.find(marker).expect("code page carries a challenge") + marker.len();
    page[start..start + page[start..].find('"').unwrap()].to_owned()
}

#[tokio::test]
async fn new_person_signs_up_by_code_and_returns_to_authorization() {
    let site = Site::new(true).await;
    let (status, _, page) = site
        .get(&format!("/login?next={}", urlencoding::encode(NEXT)))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Email me a code"));
    assert!(
        !page.contains("name=\"password\""),
        "no password on the code page"
    );

    let challenge = site.request_code("New.Owner@Example.com").await;
    {
        let outbox = site.outbox.lock().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].to, "new.owner@example.com");
        assert!(outbox[0].text.contains(&outbox[0].subject[..6]));
    }
    let code = site.last_code();
    let (status, location, _) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "new.owner@example.com"),
                ("code", &format!("{} {}", &code[..3], &code[3..])),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some(NEXT),
        "back to the ChatGPT consent screen"
    );

    let (user, created) = db::find_or_create_email_user(&site.state.db, "new.owner@example.com")
        .await
        .unwrap();
    assert!(!created, "the code sign-in already created the account");
    assert_eq!(user.email.as_deref(), Some("new.owner@example.com"));

    // A code works once.
    let (status, _, page) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "new.owner@example.com"),
                ("code", &code),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("expired or was replaced"));
}

#[tokio::test]
async fn existing_password_account_can_sign_in_by_code() {
    let site = Site::new(true).await;
    let existing = db::create_account(&site.state.db, "owner@example.com", "Sam", "hash")
        .await
        .unwrap();
    let challenge = site.request_code("owner@example.com").await;
    let code = site.last_code();
    let (status, location, _) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "owner@example.com"),
                ("code", &code),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some(NEXT));
    let (user, created) = db::find_or_create_email_user(&site.state.db, "owner@example.com")
        .await
        .unwrap();
    assert!(!created);
    assert_eq!(
        user.household_id, existing.household_id,
        "same household, no duplicate"
    );
}

#[tokio::test]
async fn wrong_codes_run_out_and_newer_codes_replace_older_ones() {
    let site = Site::new(true).await;
    let challenge = site.request_code("guess@example.com").await;
    let code = site.last_code();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for tries_left in (1..db::LOGIN_CODE_MAX_ATTEMPTS).rev() {
        let (status, _, page) = site
            .post(
                "/login/code/verify",
                &[
                    ("challenge", &challenge),
                    ("email", "guess@example.com"),
                    ("code", wrong),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(page.contains(&format!("({tries_left} tr")), "{page}");
    }
    let (_, _, page) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "guess@example.com"),
                ("code", wrong),
            ],
        )
        .await;
    assert!(page.contains("expired or was replaced"));
    // Even the right code is dead once the attempts are used up.
    let (status, _, _) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "guess@example.com"),
                ("code", &code),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Replacement: store two codes directly (bypassing the resend wait).
    db::create_login_code(
        &site.state.db,
        "swap@example.com",
        "first-challenge",
        "123456",
        None,
        None,
        10,
    )
    .await
    .unwrap();
    db::create_login_code(
        &site.state.db,
        "swap@example.com",
        "second-challenge",
        "654321",
        None,
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        db::check_login_code(&site.state.db, "first-challenge", "123456")
            .await
            .unwrap(),
        db::LoginCodeCheck::Unusable
    );
    assert!(matches!(
        db::check_login_code(&site.state.db, "second-challenge", "654321")
            .await
            .unwrap(),
        db::LoginCodeCheck::Valid { .. }
    ));
}

#[tokio::test]
async fn immediate_resend_is_refused_and_keeps_the_current_code() {
    let site = Site::new(true).await;
    let challenge = site.request_code("busy@example.com").await;
    let (status, _, page) = site
        .post(
            "/login/code",
            &[
                ("email", "busy@example.com"),
                ("next", NEXT),
                ("challenge", &challenge),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(page.contains("Give it a moment"));
    assert_eq!(challenge_in(&page), challenge, "still on the same code");
    assert_eq!(site.outbox.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unsafe_next_is_ignored() {
    let site = Site::new(true).await;
    let (status, _, page) = site
        .post(
            "/login/code",
            &[
                ("email", "safe@example.com"),
                ("next", "https://evil.example/"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let challenge = challenge_in(&page);
    let code = site.last_code();
    let (_, location, _) = site
        .post(
            "/login/code/verify",
            &[
                ("challenge", &challenge),
                ("email", "safe@example.com"),
                ("code", &code),
            ],
        )
        .await;
    assert_eq!(location.as_deref(), Some("/app"));
}

#[tokio::test]
async fn without_email_password_signup_also_returns_to_authorization() {
    let site = Site::new(false).await;
    let encoded = urlencoding::encode(NEXT).into_owned();
    let (status, _, page) = site.get(&format!("/login?next={encoded}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("name=\"password\""));
    assert!(
        page.contains(&format!("href=\"/register?next={encoded}\"")),
        "Create an account keeps the authorization"
    );
    let (status, location, _) = site
        .post(
            "/register",
            &[
                ("display_name", "Robin"),
                ("email", "robin@example.com"),
                ("password", "a long enough password"),
                ("next", NEXT),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some(NEXT));

    // Code routes are inert without email.
    let (status, location, _) = site
        .post("/login/code", &[("email", "x@example.com")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/login/password"));
}

#[tokio::test]
async fn with_email_register_redirects_to_code_sign_in() {
    let site = Site::new(true).await;
    let encoded = urlencoding::encode(NEXT).into_owned();
    let (status, location, _) = site.get(&format!("/register?next={encoded}")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location, Some(format!("/login?next={encoded}")));
    // Password sign-in stays reachable for existing accounts.
    let (status, _, page) = site.get(&format!("/login/password?next={encoded}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("name=\"password\""));
    assert!(page.contains("Email me a sign-in code instead"));
}
