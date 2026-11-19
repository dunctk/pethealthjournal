use crate::{
    AppState, clinical_cases, db,
    domain::{ProposedEvent, UserAccount},
};
use axum::{
    Json,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

pub async fn endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RpcRequest>,
) -> impl IntoResponse {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"jsonrpc":"2.0", "id": id, "error":{"code":-32600,"message":"jsonrpc must be 2.0."}})),
        ).into_response();
    }
    let response = match authenticate(&state, &headers).await {
        Ok(user) => {
            if request.id.is_none() {
                return StatusCode::NO_CONTENT.into_response();
            }
            let origin = state
                .config
                .app_base_url
                .clone()
                .map(|base| base.trim_end_matches('/').to_owned())
                .unwrap_or_else(|| public_origin(&state, &headers));
            handle(&state, &user, request, &origin).await
        }
        Err((status, message)) => {
            return (
                status,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({
                    "jsonrpc":"2.0", "id": id, "error": {"code": -32001, "message": message}
                })),
            )
                .into_response();
        }
    };
    (StatusCode::OK, Json(response)).into_response()
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<UserAccount, (StatusCode, &'static str)> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let token = bearer.or_else(|| cookie_token(state, headers));
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "Sign in is required."));
    };
    if let Some(user) = db::resolve_oauth_access_token(&state.db, token)
        .await
        .ok()
        .flatten()
    {
        return Ok(user);
    }
    // Personal MCP tokens (phj_...): long-lived revocable credentials a
    // member mints on their account page for their own AI assistant.
    if let Some(user) = db::resolve_api_token(&state.db, token).await.ok().flatten() {
        return Ok(user);
    }
    db::resolve_session(&state.db, token)
        .await
        .ok()
        .flatten()
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "The session is missing, expired, or revoked.",
        ))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

pub async fn register_client(
    State(state): State<AppState>,
    Json(request): Json<RegisterRequest>,
) -> Result<Json<Value>, Response> {
    if request.redirect_uris.is_empty() || request.redirect_uris.len() > 10 {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "redirect_uris is required.",
        ));
    }
    if request.redirect_uris.iter().any(|uri| {
        uri.is_empty()
            || uri.contains('#')
            || uri
                .parse::<Uri>()
                .ok()
                .is_none_or(|parsed| parsed.scheme().is_none() || parsed.authority().is_none())
    }) {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Each redirect URI must be absolute and must not contain a fragment.",
        ));
    }
    let client_id = format!("pethealth_{}", crate::auth::new_session_token());
    let client_name = request.client_name.unwrap_or_else(|| "MCP client".into());
    db::create_oauth_client(&state.db, &client_id, &client_name, &request.redirect_uris)
        .await
        .map_err(internal_response)?;
    Ok(Json(json!({
        "client_id":client_id,
        "client_name":client_name,
        "redirect_uris":request.redirect_uris,
        "grant_types":["authorization_code","refresh_token"],
        "response_types":["code"],
        "token_endpoint_auth_method":"none"
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationForm {
    pub client_id: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: Option<String>,
}

pub async fn start_device_authorization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeviceAuthorizationForm>,
) -> Result<Json<Value>, Response> {
    validate_device_client(&state, &form).await?;
    let device_code = crate::auth::new_session_token();
    let user_code = new_user_code();
    db::create_oauth_device_code(
        &state.db,
        &crate::auth::token_hash(&device_code),
        &crate::auth::token_hash(&normalize_user_code(&user_code)),
        &form.client_id,
        &form.code_challenge,
    )
    .await
    .map_err(internal_response)?;
    let origin = public_origin(&state, &headers);
    Ok(Json(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": format!("{origin}/oauth/device"),
        "verification_uri_complete": format!("{origin}/oauth/device?user_code={}", urlencoding::encode(&user_code)),
        "expires_in": 600,
        "interval": 5,
        "scope": form.scope.unwrap_or_else(|| "pethealth".into())
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct DevicePageQuery {
    pub user_code: Option<String>,
}

pub async fn device_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DevicePageQuery>,
) -> Result<Response, Response> {
    let Some(raw_code) = query.user_code.as_deref() else {
        return Ok(Html(device_code_entry_html(None)).into_response());
    };
    let user_code = normalize_user_code(raw_code);
    if user_code.len() != 10
        || !db::oauth_device_user_code_exists(&state.db, &crate::auth::token_hash(&user_code))
            .await
            .map_err(internal_response)?
    {
        return Ok(Html(device_code_entry_html(Some(
            "That code is invalid or expired.",
        )))
        .into_response());
    }
    if cookie_user(&state, &headers).await.is_none() {
        let next = format!("/oauth/device?user_code={}", urlencoding::encode(raw_code));
        return Ok(
            Redirect::to(&format!("/login?next={}", urlencoding::encode(&next))).into_response(),
        );
    }
    Ok(Html(device_consent_html(&user_code)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct DeviceVerifyForm {
    pub user_code: String,
}

pub async fn verify_device_code(Form(form): Form<DeviceVerifyForm>) -> Result<Redirect, Response> {
    let user_code = normalize_user_code(&form.user_code);
    if user_code.len() != 10 {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Enter the code shown by the MCP client.",
        ));
    }
    Ok(Redirect::to(&format!(
        "/oauth/device?user_code={}",
        urlencoding::encode(&form.user_code)
    )))
}

#[derive(Debug, Deserialize)]
pub struct DeviceApproveForm {
    pub user_code: String,
}

pub async fn approve_device_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeviceApproveForm>,
) -> Result<Response, Response> {
    let user = cookie_user(&state, &headers)
        .await
        .ok_or_else(|| oauth_error(StatusCode::UNAUTHORIZED, "Sign in is required."))?;
    let user_code = normalize_user_code(&form.user_code);
    let approved =
        db::approve_oauth_device_code(&state.db, &crate::auth::token_hash(&user_code), user.id)
            .await
            .map_err(internal_response)?;
    if !approved {
        return Ok(Html(device_code_entry_html(Some(
            "That code is invalid, expired, or already approved.",
        )))
        .into_response());
    }
    Ok(Html("<!doctype html><html lang=\"en\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Pet Health connected</title><link rel=\"stylesheet\" href=\"/static/app.css\"><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connected.</h1><p>The MCP client can finish signing in now. You can close this window.</p></section></main></body></html>").into_response())
}

async fn validate_device_client(
    state: &AppState,
    form: &DeviceAuthorizationForm,
) -> Result<(), Response> {
    if form.code_challenge_method != "S256" || form.code_challenge.len() < 43 {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Use S256 PKCE."));
    }
    if db::oauth_client_redirects(&state.db, &form.client_id)
        .await
        .map_err(internal_response)?
        .is_none()
    {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Unknown client_id."));
    }
    Ok(())
}

fn new_user_code() -> String {
    let raw = crate::auth::new_session_token().to_ascii_uppercase();
    format!("{}-{}", &raw[..5], &raw[5..10])
}

fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

fn device_code_entry_html(error: Option<&str>) -> String {
    let message = error
        .map(|value| {
            format!(
                "<div class=\"auth-error\" role=\"alert\">{}</div>",
                escape(value)
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect an MCP client</h1><p>Enter the one-time code shown in your terminal.</p>{message}<form method=\"post\" action=\"/oauth/device/verify\" class=\"auth-form\"><label>Connection code<input required autofocus name=\"user_code\" autocomplete=\"one-time-code\" autocapitalize=\"characters\" placeholder=\"ABCDE-FGHIJ\"></label><button class=\"button primary\" type=\"submit\">Continue</button></form></section></main></body></html>"
    )
}

fn device_consent_html(user_code: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect this client?</h1><p>The MCP client will be able to read and update the signed-in household through Pet Health.</p><form method=\"post\" action=\"/oauth/device/approve\" class=\"auth-form\"><input type=\"hidden\" name=\"user_code\" value=\"{}\"><button class=\"button primary\" type=\"submit\">Allow access</button></form><p class=\"auth-switch\"><a href=\"/\">Cancel</a></p></section></main></body></html>",
        escape(user_code)
    )
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: Option<String>,
}

pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Response, Response> {
    validate_authorize_request(&state, &query).await?;
    let user = cookie_user(&state, &headers).await;
    let Some(_) = user else {
        let next = format!(
            "/oauth/authorize?response_type={}&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256{}",
            urlencoding::encode(&query.response_type),
            urlencoding::encode(&query.client_id),
            urlencoding::encode(&query.redirect_uri),
            urlencoding::encode(&query.code_challenge),
            query
                .state
                .as_deref()
                .map(|state| format!("&state={}", urlencoding::encode(state)))
                .unwrap_or_default()
        );
        return Ok(
            Redirect::to(&format!("/login?next={}", urlencoding::encode(&next))).into_response(),
        );
    };
    let state_field = query.state.clone().unwrap_or_default();
    let csp = consent_csp(&query.redirect_uri);
    let mut response = Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connect Pet Health</title><link rel=\"stylesheet\" href=\"/static/app.css\"></head><body class=\"auth-page\"><main class=\"auth-shell\"><section class=\"auth-panel\"><div class=\"eyebrow\">PET HEALTH / MCP</div><h1>Connect your pet history?</h1><p>This agent will be able to read and update the signed-in household through Pet Health.</p><form method=\"post\" action=\"/oauth/authorize/approve\" class=\"auth-form\"><input type=\"hidden\" name=\"client_id\" value=\"{}\"><input type=\"hidden\" name=\"redirect_uri\" value=\"{}\"><input type=\"hidden\" name=\"code_challenge\" value=\"{}\"><input type=\"hidden\" name=\"state\" value=\"{}\"><button class=\"button primary\" type=\"submit\">Allow access</button></form><p class=\"auth-switch\"><a href=\"/\">Cancel</a></p></section></main></body></html>",
        escape(&query.client_id), escape(&query.redirect_uri), escape(&query.code_challenge), escape(&state_field)
    )).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&csp) {
        response
            .headers_mut()
            .insert("content-security-policy", value);
    }
    Ok(response)
}

/// The consent form POSTs to us and we 303 to the client's redirect_uri.
/// Browsers apply `form-action` to that redirect, so the registered
/// redirect origin must be allowed or the approval silently goes nowhere.
fn consent_csp(redirect_uri: &str) -> String {
    let redirect_origin = reqwest::Url::parse(redirect_uri)
        .ok()
        .filter(|url| matches!(url.scheme(), "https" | "http"))
        .map(|url| url.origin().ascii_serialization())
        .filter(|origin| origin != "null")
        .map(|origin| format!(" {origin}"))
        .unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'{redirect_origin}"
    )
}

pub async fn approve_authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Result<Response, Response> {
    let query = AuthorizeQuery {
        response_type: "code".into(),
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        code_challenge: form.code_challenge.clone(),
        code_challenge_method: "S256".into(),
        state: (!form.state.is_empty()).then_some(form.state.clone()),
    };
    validate_authorize_request(&state, &query).await?;
    let user = cookie_user(&state, &headers)
        .await
        .ok_or_else(|| oauth_error(StatusCode::UNAUTHORIZED, "Sign in is required."))?;
    let code = crate::auth::new_session_token();
    db::create_oauth_code(
        &state.db,
        &crate::auth::token_hash(&code),
        &form.client_id,
        user.id,
        &form.redirect_uri,
        &form.code_challenge,
    )
    .await
    .map_err(internal_response)?;
    let mut location = format!(
        "{}{}code={}",
        form.redirect_uri,
        if form.redirect_uri.contains('?') {
            '&'
        } else {
            '?'
        },
        urlencoding::encode(&code)
    );
    if !form.state.is_empty() {
        location.push_str(&format!("&state={}", urlencoding::encode(&form.state)));
    }
    Ok(Redirect::to(&location).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ApproveForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    device_code: Option<String>,
}

pub async fn token(
    State(state): State<AppState>,
    Form(form): Form<TokenForm>,
) -> Result<Json<Value>, Response> {
    let tokens =
        match form.grant_type.as_str() {
            "authorization_code" => {
                let code = form
                    .code
                    .as_deref()
                    .ok_or_else(|| oauth_error(StatusCode::BAD_REQUEST, "code is required."))?;
                let redirect_uri = form.redirect_uri.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "redirect_uri is required.")
                })?;
                let verifier = form.code_verifier.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "code_verifier is required.")
                })?;
                let Some((user_id, challenge)) = db::redeem_oauth_code(
                    &state.db,
                    &crate::auth::token_hash(code),
                    &form.client_id,
                    redirect_uri,
                )
                .await
                .map_err(internal_response)?
                else {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "The authorization code is invalid or expired.",
                    ));
                };
                let digest = Sha256::digest(verifier.as_bytes());
                let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                if computed.as_bytes().ct_eq(challenge.as_bytes()).unwrap_u8() != 1 {
                    return Err(oauth_error(
                        StatusCode::BAD_REQUEST,
                        "PKCE verification failed.",
                    ));
                }
                db::create_oauth_tokens(&state.db, &form.client_id, user_id)
                    .await
                    .map_err(internal_response)?
            }
            "refresh_token" => {
                let refresh = form.refresh_token.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "refresh_token is required.")
                })?;
                db::refresh_oauth_tokens(&state.db, refresh, &form.client_id)
                    .await
                    .map_err(internal_response)?
                    .ok_or_else(|| {
                        oauth_error(
                            StatusCode::BAD_REQUEST,
                            "The refresh token is invalid or expired.",
                        )
                    })?
            }
            "urn:ietf:params:oauth:grant-type:device_code" => {
                let device_code = form.device_code.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "device_code is required.")
                })?;
                let verifier = form.code_verifier.as_deref().ok_or_else(|| {
                    oauth_error(StatusCode::BAD_REQUEST, "code_verifier is required.")
                })?;
                let Some((Some(user_id), challenge)) = db::oauth_device_code_state(
                    &state.db,
                    &crate::auth::token_hash(device_code),
                    &form.client_id,
                )
                .await
                .map_err(internal_response)?
                else {
                    if db::oauth_device_code_state(
                        &state.db,
                        &crate::auth::token_hash(device_code),
                        &form.client_id,
                    )
                    .await
                    .map_err(internal_response)?
                    .is_some()
                    {
                        return Err(oauth_error_code(
                            StatusCode::BAD_REQUEST,
                            "authorization_pending",
                            "Approve the connection in a browser first.",
                        ));
                    }
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "The device code is invalid or expired.",
                    ));
                };
                let digest = Sha256::digest(verifier.as_bytes());
                let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                if computed.as_bytes().ct_eq(challenge.as_bytes()).unwrap_u8() != 1 {
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        "PKCE verification failed.",
                    ));
                }
                if !db::consume_oauth_device_code(
                    &state.db,
                    &crate::auth::token_hash(device_code),
                    &form.client_id,
                    user_id,
                )
                .await
                .map_err(internal_response)?
                {
                    return Err(oauth_error_code(
                        StatusCode::BAD_REQUEST,
                        "authorization_pending",
                        "Approve the connection in a browser first.",
                    ));
                }
                db::create_oauth_tokens(&state.db, &form.client_id, user_id)
                    .await
                    .map_err(internal_response)?
            }
            _ => {
                return Err(oauth_error(
                    StatusCode::BAD_REQUEST,
                    "Unsupported grant_type.",
                ));
            }
        };
    Ok(Json(
        json!({"token_type":"Bearer","access_token":tokens.0,"refresh_token":tokens.1,"expires_in":tokens.2,"scope":"pethealth"}),
    ))
}

pub async fn protected_resource_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Value> {
    let origin = public_origin(&state, &headers);
    Json(
        json!({"resource":format!("{origin}/mcp"),"authorization_servers":[origin],"scopes_supported":["pethealth"],"bearer_methods_supported":["header"]}),
    )
}

pub async fn authorization_server_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Value> {
    let origin = public_origin(&state, &headers);
    Json(
        json!({"issuer":origin,"authorization_endpoint":format!("{origin}/oauth/authorize"),"device_authorization_endpoint":format!("{origin}/oauth/device"),"token_endpoint":format!("{origin}/oauth/token"),"registration_endpoint":format!("{origin}/oauth/register"),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token","urn:ietf:params:oauth:grant-type:device_code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"scopes_supported":["pethealth"]}),
    )
}

async fn validate_authorize_request(
    state: &AppState,
    query: &AuthorizeQuery,
) -> Result<(), Response> {
    if query.response_type != "code"
        || query.code_challenge_method != "S256"
        || query.code_challenge.len() < 43
    {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "Use response_type=code with S256 PKCE.",
        ));
    }
    let Some(redirects) = db::oauth_client_redirects(&state.db, &query.client_id)
        .await
        .map_err(internal_response)?
    else {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "Unknown client_id."));
    };
    if !redirects.iter().any(|uri| uri == &query.redirect_uri) {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "redirect_uri is not registered.",
        ));
    }
    Ok(())
}

async fn cookie_user(state: &AppState, headers: &HeaderMap) -> Option<UserAccount> {
    let token = cookie_token(state, headers)?;
    db::resolve_session(&state.db, token).await.ok().flatten()
}

fn public_origin(state: &AppState, headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or(if state.config.production {
            "https"
        } else {
            "http"
        });
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost:3000");
    format!("{scheme}://{host}")
}

fn oauth_error(status: StatusCode, message: &str) -> Response {
    oauth_error_code(status, "invalid_request", message)
}
fn oauth_error_code(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":code,"error_description":message})),
    )
        .into_response()
}
fn internal_response(error: impl std::fmt::Display) -> Response {
    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
}
fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cookie_token<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    let name = if state.config.production {
        "__Host-pethealth_session"
    } else {
        "pethealth_session"
    };
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// MCP protocol versions this server speaks, newest first. `initialize`
/// negotiates: when the client asks for a version we know, we answer in it;
/// otherwise we answer with the newest and the client decides whether it
/// can proceed. A single hard-coded version breaks clients that pin a
/// different protocol date (notably ChatGPT and Claude releases).
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Pure protocol-version negotiation, extracted for tests: answer in the
/// client's version when we speak it, else the newest we support.
fn negotiate_protocol_version(asked: &str) -> &'static str {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|version| *version == &asked)
        .copied()
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
}

async fn handle(
    state: &AppState,
    user: &UserAccount,
    request: RpcRequest,
    origin: &str,
) -> RpcResponse {
    let id = request.id.clone();
    let result = match request.method.as_str() {
        "initialize" => {
            let asked = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Answer in the client's version when we speak it, else the
            // newest we support. The client decides whether to proceed.
            let negotiated = negotiate_protocol_version(asked).to_owned();
            Ok(json!({
                "protocolVersion": negotiated,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"pethealth","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Pet Health Journal is a longitudinal pet health record. You (the assistant) interpret the owner's words and call structured tools; the server timestamps and preserves the original wording. Prefer record_observation or a typed record_* tool over record_health_event to avoid a second model call. Never diagnose, prescribe, or promise emergency monitoring. Findings from get_health_insights describe measured change only. First run: if list_pets returns no pets, the owner is new; ask for their pet's name and species (breed and birth date are optional), then call add_pet before recording anything."
            }))
        }
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let tool = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .chars()
                .take(64)
                .collect::<String>();
            let started = std::time::Instant::now();
            let result = call_tool(state, user, request.params, origin).await;
            let elapsed = started.elapsed().as_millis() as i64;
            let error_code = result.as_ref().err().map(|(code, _)| *code);
            tracing::info!(
                target: "pethealth::mcp",
                tool = %tool,
                ok = error_code.is_none(),
                error_code,
                duration_ms = elapsed,
                household_id = user.household_id,
                "mcp tool call"
            );
            if let Err(error) =
                db::log_tool_call(&state.db, user.household_id, &tool, error_code, elapsed).await
            {
                tracing::warn!(%error, "failed to log mcp tool call");
            }
            result
        }
        _ => Err((-32601, format!("Unknown method: {}", request.method))),
    };
    match result {
        Ok(result) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        },
        Err((code, message)) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message }),
        },
    }
}

fn tool_list() -> Value {
    json!({"tools":[
        {"name":"list_pets","description":"List the pets in the signed-in household.","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}},
        {"name":"list_pet_passports","description":"List saved passport scans for one pet. Download URLs require a signed-in member of the household; no OCR or travel-validity assessment.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"add_pet_passport","description":"Save one passport PDF or photographed page. Use passport_file for a ChatGPT attachment OR content_base64 for API clients with raw bytes; never both. Confirm the correct pet before saving.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"label":{"type":"string","maxLength":100},"filename":{"type":"string","maxLength":120},"passport_file":{"type":"object","properties":{"download_url":{"type":"string"},"file_id":{"type":"string"}}},"content_base64":{"type":"string","description":"Base64-encoded PDF/JPEG/PNG/WebP bytes (API clients only)."}},"required":["pet_id"]},"_meta":{"openai/fileParams":["passport_file"]}},
        {"name":"delete_pet_passport","description":"Permanently remove a stored passport scan and its file, only after explicit approval and confirm:true.","inputSchema":{"type":"object","properties":{"passport_id":{"type":"integer"},"confirm":{"type":"boolean"}},"required":["passport_id","confirm"]},"annotations":{"destructiveHint":true}},
        {"name":"get_pet_timeline","description":"Read active health events for one pet, newest first. The owner's original wording is preserved in raw_input. Prefer narrow date ranges and small limits over bulk history.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":25},"offset":{"type":"integer","minimum":0,"default":0},"since":{"type":"string","description":"Earliest date to include, YYYY-MM-DD."},"until":{"type":"string","description":"Latest date to include, YYYY-MM-DD."},"category":{"type":"string","enum":["symptom","observation","medication","measurement","vet_visit"],"description":"Filter by event type."}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"list_clinical_cases","description":"List persistent clinical investigations for one pet. These organize unresolved problems, not diagnoses.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_clinical_case","description":"Read the owner's dated notes, vet-reported findings, treatment responses, uncertainties and differential questions in one investigation. Vet attribution is owner-relayed unless directly verified.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"}},"required":["case_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"open_clinical_case","description":"Start an unresolved clinical investigation for a pet using the owner's original presenting concern. Do not supply an AI diagnosis.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"title":{"type":"string","maxLength":120},"presenting_concern":{"type":"string","maxLength":2000}},"required":["pet_id","title","presenting_concern"]}},
        {"name":"add_clinical_case_update","description":"Append a dated owner observation, medication course, response, vet-reported finding, test or question. Use exact wording, never invent dates or claim independent vet authentication. Optionally link an existing active health event from the same pet.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"},"kind":{"type":"string","enum":["observation","treatment","response","vet_finding","test","question"]},"original_wording":{"type":"string","maxLength":3000},"observed_on":{"type":"string","description":"Known observation date YYYY-MM-DD. Omit rather than guessing."},"source":{"type":"string","enum":["owner_reported","owner_relayed_vet"],"default":"owner_reported"},"linked_event_id":{"type":"integer"}},"required":["case_id","kind","original_wording"]}},
        {"name":"add_clinical_hypothesis","description":"Store an explicitly UNREVIEWED possible explanation or question for the veterinarian. Do not present it as a diagnosis. reported_by says where the idea came from.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"},"label":{"type":"string","maxLength":200},"rationale":{"type":"string","maxLength":2000},"reported_by":{"type":"string","enum":["owner_question","owner_relayed_vet"],"default":"owner_question"}},"required":["case_id","label"]}},
        {"name":"review_clinical_hypothesis","description":"Update hypothesis review status only with owner's explicit approval. 'ruled_out' or 'vet_supported' require the owner to relay a specific veterinary assessment; this is NOT independently verified.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"},"hypothesis_id":{"type":"integer"},"status":{"type":"string","enum":["unreviewed","under_review","less_likely","ruled_out","vet_supported"]},"rationale":{"type":"string","maxLength":2000},"reported_by":{"type":"string","enum":["owner_question","owner_relayed_vet"]},"confirm":{"type":"boolean"}},"required":["case_id","hypothesis_id","status","rationale","reported_by","confirm"]},"annotations":{"destructiveHint":true}},
        {"name":"set_clinical_case_status","description":"Mark a clinical investigation open, monitoring or resolved, only with owner's explicit approval. Do not equate 'resolved' with a diagnosis.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"},"status":{"type":"string","enum":["open","monitoring","resolved"]},"confirm":{"type":"boolean"}},"required":["case_id","status","confirm"]},"annotations":{"destructiveHint":true}},
        {"name":"get_vet_case_brief","description":"Get a structured owner-reported investigation brief with prescription context, recent events and lab reports, clearly separated from possible explanations. The owner can print the same brief in the web app.","inputSchema":{"type":"object","properties":{"case_id":{"type":"integer"}},"required":["case_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_clinical_timeline","description":"Read a pet's symptoms, medications, and neutral time links between them. This is context for a vet, not a diagnosis.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_medication_plan","description":"Read a pet's current prescriptions and dose-adherence history. This is a record, not a dosing recommendation.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"review_medication_plan","description":"Read-only medication reconciliation. Compare owner-confirmed current medicine names against structured active prescriptions and propose stale active rows or missing medicines for review. Never stop or prescribe medications automatically. Owner approval is required for any subsequent update_prescription call.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"reported_current_medicines":{"type":"array","items":{"type":"string","minLength":1,"maxLength":120},"maxItems":30,"description":"Medicine names the owner explicitly reports as current, including any ongoing short courses. An empty list means the owner explicitly reports no current medicines."}},"required":["pet_id","reported_current_medicines"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_health_context","description":"Read practical care context for a health concept. This is not a diagnosis.","inputSchema":{"type":"object","properties":{"concept":{"type":"string"}},"required":["concept"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_weight_history","description":"Read weight measurements over time for one pet, newest first. Use since/until (YYYY-MM-DD) to bound the range.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":50},"since":{"type":"string","description":"Earliest date to include, YYYY-MM-DD."},"until":{"type":"string","description":"Latest date to include, YYYY-MM-DD."}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_blood_test_history","description":"Read blood-test values for one pet, newest first. Each result keeps the printed name/value/unit; recognised analytes also include a 'standard' object (common name and value converted to one unit) so results from different labs can be compared. Original reports remain stored for review. Filter by date with since/until (YYYY-MM-DD).","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":50,"default":25},"since":{"type":"string","description":"Earliest test date to include, YYYY-MM-DD."},"until":{"type":"string","description":"Latest test date to include, YYYY-MM-DD."}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"get_health_insights","description":"Read deterministic, LLM-free health findings for one pet (weight trends, symptom frequency, adherence gaps, lab deltas), newest first. Each finding cites its source records. These describe measured changes only, never a diagnosis.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":50,"default":25},"include_dismissed":{"type":"boolean","default":false}},"required":["pet_id"]},"annotations":{"readOnlyHint":true}},
        {"name":"add_pet","description":"Add a pet to the signed-in household.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"species":{"type":"string"},"breed":{"type":"string"},"weight_kg":{"type":"number"}},"required":["name","species"]}},
        {"name":"record_weight","description":"Save a weight measurement for a pet.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"weight_kg":{"type":"number"},"measured_at":{"type":"string","description":"YYYY-MM-DD"},"note":{"type":"string"}},"required":["pet_id","weight_kg","measured_at"]}},
        {"name":"record_symptom_event","description":"Record a structured symptom using the user's original wording. The server timestamps it now or by relative minutes_ago; absolute timestamps are not accepted from the model.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"message":{"type":"string"},"symptom":{"type":"string","enum":["vomiting","diarrhea","reduced_appetite","wheezing","nasal_discharge","nasal_congestion","eye_discharge","coughing","labored_breathing"]},"minutes_ago":{"type":"integer","minimum":0,"maximum":525600},"occurrence_count":{"type":"integer","minimum":1,"maximum":100},"amount":{"type":"string"},"contents":{"type":"string"},"meal_relation":{"type":"string"},"water_status":{"type":"string"},"appetite_status":{"type":"string"},"energy_status":{"type":"string"},"pain_status":{"type":"string"},"note":{"type":"string"}},"required":["pet_id","message","symptom"]}},
        {"name":"record_medication","description":"Record a medication exposure. The server timestamps administration now or by relative minutes_ago; include the exact product, active ingredient, strength, dose, route, and whether it was given, missed, extra, or vomited back.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"name":{"type":"string"},"active_ingredient":{"type":"string"},"dose_value":{"type":"number"},"dose_unit":{"type":"string"},"route":{"type":"string"},"minutes_ago":{"type":"integer","minimum":0,"maximum":525600},"status":{"type":"string","enum":["given","missed","extra","vomited_back"]},"message":{"type":"string"}},"required":["pet_id","name"]}},
        {"name":"record_prescription","description":"Save a pet's prescription plan (strength, dose, frequency, instructions). The owner's wording is preserved. CONFIRMATION: if the pet already has an ACTIVE prescription with the same name and different dosing, the call is rejected unless confirm:true is passed after the owner explicitly approves the change.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"name":{"type":"string"},"active_ingredient":{"type":"string"},"concentration_value":{"type":"number"},"concentration_unit":{"type":"string"},"dose_value":{"type":"number"},"dose_unit":{"type":"string"},"frequency":{"type":"string"},"route":{"type":"string"},"instructions":{"type":"string"},"started_days_ago":{"type":"integer","minimum":0,"maximum":36500},"status":{"type":"string","enum":["active","paused","stopped"]},"message":{"type":"string"},"confirm":{"type":"boolean","description":"Set true only after the owner explicitly confirms replacing a differing active prescription.","default":false}},"required":["pet_id","name"]},"annotations":{"destructiveHint":true}},
        {"name":"update_prescription","description":"Change an EXISTING prescription: stop it, pause it, resume a paused one, or change its dose/frequency. Use this instead of record_prescription when the owner says the vet stopped, paused, or adjusted a medication. Get prescription_id from get_medication_plan. History is kept: a dose change stops the old regimen and creates a new active one. Restate the change to the owner and get their explicit approval before calling with confirm:true.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"prescription_id":{"type":"integer"},"action":{"type":"string","enum":["stop","pause","resume","change_dose"]},"effective_days_ago":{"type":"integer","minimum":0,"maximum":3650,"description":"Only specify a verified effective date as days ago (0 = today). Omit when the date is unknown: never substitute the date of recording."},"dose_value":{"type":"number","exclusiveMinimum":0},"dose_unit":{"type":"string","maxLength":40},"frequency":{"type":"string","maxLength":120},"concentration_value":{"type":"number","exclusiveMinimum":0},"concentration_unit":{"type":"string","maxLength":40},"instructions":{"type":"string","maxLength":1000},"message":{"type":"string","minLength":1,"maxLength":1000,"description":"The owner's exact words; preserved on the timeline."},"confirm":{"type":"boolean","description":"Must be true, only after the owner explicitly approved this change."}},"required":["pet_id","prescription_id","action","message","confirm"]},"annotations":{"destructiveHint":true}},
        {"name":"undo_medication_entry","description":"Delete one mistaken medication record: a dose logged with record_medication (entry_type 'dose', id = medication_id) or an adherence record from record_medication_adherence (entry_type 'adherence', id = adherence_id). Prescriptions are changed with update_prescription instead. Only call when the owner asks to remove that entry.","inputSchema":{"type":"object","properties":{"entry_type":{"type":"string","enum":["dose","adherence"]},"id":{"type":"integer"}},"required":["entry_type","id"]},"annotations":{"destructiveHint":true}},
        {"name":"record_medication_adherence","description":"Record whether a scheduled prescription dose was given, partial, missed, or unknown. Use days_ago for a past calendar day; the server chooses the date.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"prescription_id":{"type":"integer"},"days_ago":{"type":"integer","minimum":0,"maximum":36500},"actual_dose_value":{"type":"number"},"actual_dose_unit":{"type":"string"},"status":{"type":"string","enum":["given","partial","missed","unknown"]},"reason":{"type":"string"},"message":{"type":"string"}},"required":["pet_id","prescription_id","status"]}},
        {"name":"record_blood_test","description":"Save blood-test (lab) results for a pet, e.g. from a photo or PDF of a vet lab report the owner shared with you. You read the report yourself; no server-side OCR runs. WORKFLOW: 1) call list_pets to get pet_id; 2) transcribe EVERY analyte exactly as printed (name, value, unit, reference range, H/L flag) plus the report's collection/test date; 3) show the owner the extracted table and ask them to confirm or correct it; 4) only then call this tool with confirm:true. Do not convert units, round, infer missing values or interpret results. If the photo is unreadable or the date is missing, ask the owner instead of guessing.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"test_date":{"type":"string","description":"Date the sample was collected/tested as printed on the report, YYYY-MM-DD. Not today's date unless the report says so."},"lab_name":{"type":"string","maxLength":120,"description":"Clinic or laboratory name, if printed."},"panel":{"type":"string","maxLength":120,"description":"Panel/report title if printed, e.g. 'Biochemistry', 'CBC', 'SDMA'."},"transcription":{"type":"string","maxLength":20000,"description":"Verbatim text of the report as you read it, kept as the source record."},"results":{"type":"array","minItems":1,"maxItems":200,"items":{"type":"object","properties":{"test_name":{"type":"string","maxLength":120,"description":"Analyte exactly as printed, e.g. 'CREA', 'ALT', 'SDMA'."},"value":{"type":"string","maxLength":60,"description":"Value exactly as printed, e.g. '1.8', '<0.1', '>500', 'Positive'."},"unit":{"type":"string","maxLength":40},"reference_range":{"type":"string","maxLength":80,"description":"As printed, e.g. '0.8 - 2.4'."},"flag":{"type":"string","enum":["high","low","normal","abnormal"],"description":"Only if the report marks it (H/L/*). Omit otherwise."}},"required":["test_name","value"]}},"confirm":{"type":"boolean","description":"Must be true, and only after the owner reviewed and approved the extracted values."},"report_file":{"type":"object","description":"The photo or PDF of the report the owner attached, so the original is kept with the record.","properties":{"download_url":{"type":"string"},"file_id":{"type":"string"}}}},"required":["pet_id","test_date","results","confirm"]},"_meta":{"openai/fileParams":["report_file"]}},
        {"name":"update_pet","description":"Edit a pet's profile, including its microchip number. Only specified fields change; use an empty string to clear breed, date_of_birth, or microchip_number. A microchip number is a 9–20 digit string to preserve leading zeros. Confirm changes with the owner first.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"name":{"type":"string","maxLength":80},"species":{"type":"string","maxLength":60},"breed":{"type":"string","maxLength":120},"date_of_birth":{"type":"string","description":"YYYY-MM-DD, or empty to clear."},"microchip_number":{"type":"string","maxLength":20,"description":"9–20 digits. Pass as text to preserve leading zeroes, or an empty string to clear."}},"required":["pet_id"]}},
        {"name":"delete_pet","description":"PERMANENTLY delete a pet and ALL of its records (timeline, weights, medications, blood tests, photos, passport scans, vet share links). Cannot be undone. Only when the owner explicitly asks; state what will be lost and get approval. confirm_name must exactly equal the pet's name.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"confirm_name":{"type":"string","description":"The pet's exact name, as typed confirmation."},"confirm":{"type":"boolean"}},"required":["pet_id","confirm_name","confirm"]},"annotations":{"destructiveHint":true}},
        {"name":"delete_weight","description":"Delete one mistaken weight entry. Get weight ids from get_weight_history. Only when the owner asks.","inputSchema":{"type":"object","properties":{"weight_id":{"type":"integer"}},"required":["weight_id"]},"annotations":{"destructiveHint":true}},
        {"name":"create_vet_share","description":"Create a private, read-only link to one pet's health record for a vet or clinic. The link expires (default 14 days, max 90) and can be revoked. Returns the URL once; give it to the owner to send. Confirm the recipient and duration with the owner first.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"label":{"type":"string","maxLength":120,"description":"Who it is for, e.g. 'Dr Smith, Riverside Vets'."},"days":{"type":"integer","minimum":1,"maximum":90,"default":14},"confirm":{"type":"boolean"}},"required":["pet_id","label","confirm"]}},
        {"name":"list_vet_shares","description":"List vet share links for the household with their status (active, expired, revoked). URLs are not shown again after creation.","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}},
        {"name":"revoke_vet_share","description":"Revoke a vet share link immediately, by share id from list_vet_shares.","inputSchema":{"type":"object","properties":{"share_id":{"type":"integer"}},"required":["share_id"]},"annotations":{"destructiveHint":true}},
        {"name":"attach_blood_test_original","description":"Attach the original photo or PDF the owner shared to an already saved blood-test report (e.g. one saved without its file). Get report_id from get_blood_test_history.","inputSchema":{"type":"object","properties":{"report_id":{"type":"integer"},"report_file":{"type":"object","description":"The attached report file.","properties":{"download_url":{"type":"string"},"file_id":{"type":"string"}}}},"required":["report_id","report_file"]},"_meta":{"openai/fileParams":["report_file"]}},
        {"name":"delete_blood_test","description":"Delete one saved blood-test report and all its values, e.g. to correct a mistaken transcription. Only call after the owner explicitly asks.","inputSchema":{"type":"object","properties":{"report_id":{"type":"integer"}},"required":["report_id"]},"annotations":{"destructiveHint":true}},
        {"name":"upload_blood_test","description":"NOT for files attached in chat. Only for API clients that hold raw file bytes: stores one PDF or image as base64 and imports it with server-side OCR. If the owner attached a report in the conversation, read it yourself and use record_blood_test with report_file instead.","inputSchema":{"type":"object","properties":{"filename":{"type":"string"},"content_base64":{"type":"string","description":"The PDF or image bytes encoded as base64."}},"required":["filename","content_base64"]}},
        {"name":"import_blood_tests","description":"OCR new PDF and image files from this household's private blood-test folder using Mistral OCR 4.","inputSchema":{"type":"object","properties":{} }},
        {"name":"record_observation","description":"PREFERRED neutral write path: save a general note using the owner's exact words (1-1000 chars). No model is called; the server timestamps it now or by relative minutes_ago. Use this when you (the assistant) have already interpreted the user's request.","inputSchema":{"type":"object","properties":{"pet_id":{"type":"integer"},"message":{"type":"string","description":"The owner's exact words; preserved verbatim.","minLength":1,"maxLength":1000},"minutes_ago":{"type":"integer","minimum":0,"maximum":525600},"summary":{"type":"string","description":"Short label, max ~280 chars. Defaults to 'Observation recorded'."},"note":{"type":"string","description":"Optional extra detail."}},"required":["pet_id","message"]}},
        {"name":"record_health_event","description":"Legacy free-text capture. May invoke an additional server-side LLM even when the MCP client is ChatGPT or Claude. Prefer specific structured record_* tools or record_observation to avoid duplicate inference.","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
        {"name":"undo_health_event","description":"Undo an active health event in the signed-in household.","inputSchema":{"type":"object","properties":{"event_id":{"type":"integer"}},"required":["event_id"]}}
    ]})
}

async fn call_tool(
    state: &AppState,
    user: &UserAccount,
    params: Value,
    origin: &str,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Tool name is required.".into()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = match name {
        "list_pets" => serde_json::to_value(
            db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?,
        )
        .map_err(internal)?,
        "list_pet_passports" => {
            let pet_id = integer(&args, "pet_id")?;
            db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let documents = db::list_pet_passports(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?;
            let items: Vec<Value> = documents
                .iter()
                .map(|doc| {
                    json!({
                        "passport_id": doc.id,
                        "pet_id": doc.pet_id,
                        "label": doc.label,
                        "filename": doc.filename,
                        "mime_type": doc.mime_type,
                        "uploaded_at": doc.uploaded_at,
                        "download_url": format!("{origin}/passports/{}/file", doc.id),
                        "authentication_required": true
                    })
                })
                .collect();
            json!({"items": items})
        }
        "add_pet_passport" => {
            let pet_id = integer(&args, "pet_id")?;
            db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let label =
                bounded_text(args.get("label"), "label", 100).map_err(|error| (-32602, error))?;
            let filename = bounded_text(args.get("filename"), "filename", 120)
                .map_err(|error| (-32602, error))?;
            let attached_url = args
                .get("passport_file")
                .and_then(|file| file.get("download_url"))
                .and_then(Value::as_str);
            let base64_content = args.get("content_base64").and_then(Value::as_str);
            if attached_url.is_some() == base64_content.is_some() {
                return Err((
                    -32602,
                    "Supply either an attached passport_file or content_base64, but not both."
                        .into(),
                ));
            }
            let bytes = if let Some(url) = attached_url {
                let (bytes, _) = crate::ocr::fetch_client_file(url).await.map_err(|error| {
                    (
                        -32602,
                        format!("Could not access attached passport file: {error}"),
                    )
                })?;
                bytes
            } else {
                let content =
                    base64_content.ok_or((-32602, "Missing passport file.".to_owned()))?;
                if content.len() > crate::passports::MAX_PASSPORT_BYTES * 4 / 3 + 16 {
                    return Err((-32602, "Passport upload is too large (15 MB max).".into()));
                }
                base64::engine::general_purpose::STANDARD
                    .decode(content)
                    .map_err(|_| (-32602, "content_base64 is invalid.".into()))?
            };
            let (storage_path, mime, extension) =
                crate::passports::store(&state.files, user.household_id, pet_id, &bytes)
                    .await
                    .map_err(|error| (-32602, error.to_string()))?;
            let filename =
                crate::passports::safe_filename(filename.as_deref().unwrap_or(""), extension);
            let saved = db::create_pet_passport(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet_id,
                label.as_deref(),
                &filename,
                mime,
                &storage_path,
            )
            .await;
            let passport_id = match saved {
                Ok(id) => id,
                Err(error) => {
                    crate::passports::remove(&state.files, user.household_id, &storage_path).await;
                    return Err(internal(error));
                }
            };
            json!({
                "passport_id": passport_id,
                "pet_id": pet_id,
                "filename": filename,
                "mime_type": mime,
                "download_url": format!("{origin}/passports/{passport_id}/file"),
                "authentication_required": true,
                "message": "Private passport scan saved. This does not establish travel eligibility."
            })
        }
        "delete_pet_passport" => {
            let passport_id = integer(&args, "passport_id")?;
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err((
                    -32602,
                    "Ask the owner to confirm permanent removal, then retry with confirm:true."
                        .into(),
                ));
            }
            let document = db::delete_pet_passport(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                passport_id,
            )
            .await
            .map_err(internal)?
            .ok_or((
                -32602,
                "That passport document is not in this household.".into(),
            ))?;
            crate::passports::remove(&state.files, user.household_id, &document.storage_path).await;
            json!({"deleted":true,"passport_id":passport_id,"pet_id":document.pet_id})
        }
        "get_pet_timeline" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(25)
                .clamp(1, 100);
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let since = opt_date(&args, "since")?;
            let until = opt_date(&args, "until")?;
            let category = args.get("category").and_then(Value::as_str);
            if let Some(c) = category
                && !matches!(
                    c,
                    "symptom" | "observation" | "medication" | "measurement" | "vet_visit"
                )
            {
                return Err((-32602, "category is not supported.".into()));
            }
            // Fetch one bounded page plus the offset window, then filter in
            // memory. Pet timelines are small; this keeps date/category
            // filtering correct without a new SQL cursor.
            let mut events = db::list_events(
                &state.db,
                user.household_id,
                Some(pet_id),
                (limit + offset).clamp(1, 200),
            )
            .await
            .map_err(internal)?;
            if let Some(since) = since {
                events.retain(|e| e.occurred_at.date_naive() >= since);
            }
            if let Some(until) = until {
                events.retain(|e| e.occurred_at.date_naive() <= until);
            }
            if let Some(c) = category {
                events.retain(|e| e.event_type == c);
            }
            let events: Vec<_> = events
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect();
            serde_json::to_value(events).map_err(internal)?
        }
        "list_clinical_cases" => {
            let pet_id = integer(&args, "pet_id")?;
            db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "Pet not in this household.".into()))?;
            json!({"investigations":clinical_cases::list(&state.db,user.household_id,pet_id).await.map_err(internal)?})
        }
        "get_clinical_case" => {
            let id = integer(&args, "case_id")?;
            serde_json::to_value(
                clinical_cases::get(&state.db, user.household_id, id)
                    .await
                    .map_err(internal)?
                    .ok_or((-32602, "Investigation not found.".into()))?,
            )
            .map_err(internal)?
        }
        "open_clinical_case" => {
            let id = clinical_cases::create(
                &state.db,
                user.household_id,
                integer(&args, "pet_id")?,
                &user.audit_actor(),
                &string(&args, "title")?,
                &string(&args, "presenting_concern")?,
            )
            .await
            .map_err(|e| (-32602, e.to_string()))?;
            json!({"case_id":id,"message":"Investigation opened. This is not a diagnosis."})
        }
        "add_clinical_case_update" => {
            let id = clinical_cases::add_update(
                &state.db,
                user.household_id,
                integer(&args, "case_id")?,
                &user.audit_actor(),
                &string(&args, "kind")?,
                &string(&args, "original_wording")?,
                string_opt(&args, "observed_on"),
                args.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("owner_reported"),
                args.get("linked_event_id").and_then(Value::as_i64),
            )
            .await
            .map_err(|e| (-32602, e.to_string()))?;
            json!({"case_update_id":id,"message":"Case evidence added without interpreting its cause."})
        }
        "add_clinical_hypothesis" => {
            let id = clinical_cases::add_hypothesis(
                &state.db,
                user.household_id,
                integer(&args, "case_id")?,
                &user.audit_actor(),
                &string(&args, "label")?,
                string_opt(&args, "rationale"),
                args.get("reported_by")
                    .and_then(Value::as_str)
                    .unwrap_or("owner_question"),
            )
            .await
            .map_err(|e| (-32602, e.to_string()))?;
            json!({"hypothesis_id":id,"status":"unreviewed","message":"Question recorded for veterinary review, not diagnosed."})
        }
        "review_clinical_hypothesis" => {
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err((
                    -32602,
                    "Explicit owner approval (confirm:true) is required.".into(),
                ));
            }
            let changed = clinical_cases::set_hypothesis_status(
                &state.db,
                user.household_id,
                integer(&args, "case_id")?,
                integer(&args, "hypothesis_id")?,
                &user.audit_actor(),
                &string(&args, "status")?,
                &string(&args, "rationale")?,
                &string(&args, "reported_by")?,
            )
            .await
            .map_err(|e| (-32602, e.to_string()))?;
            if !changed {
                return Err((-32602, "Hypothesis not found.".into()));
            }
            json!({"updated":true,"note":"Vet assessment, if present, is owner-relayed and not independently verified."})
        }
        "set_clinical_case_status" => {
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err((
                    -32602,
                    "Explicit owner approval (confirm:true) is required.".into(),
                ));
            }
            let changed = clinical_cases::set_status(
                &state.db,
                user.household_id,
                integer(&args, "case_id")?,
                &user.audit_actor(),
                &string(&args, "status")?,
            )
            .await
            .map_err(|e| (-32602, e.to_string()))?;
            if !changed {
                return Err((-32602, "Investigation not found.".into()));
            }
            json!({"updated":true})
        }
        "get_vet_case_brief" => {
            let id = integer(&args, "case_id")?;
            let detail = clinical_cases::get(&state.db, user.household_id, id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "Investigation not found.".into()))?;
            let pet_id = detail.investigation.pet_id;
            json!({
                "case":detail,
                "prescriptions":db::list_prescriptions(&state.db,user.household_id,pet_id,30).await.map_err(internal)?,
                "recent_events_may_be_unrelated":db::list_events(&state.db,user.household_id,Some(pet_id),40).await.map_err(internal)?,
                "labs":db::list_lab_reports(&state.db,user.household_id,pet_id).await.map_err(internal)?,
                "disclaimer":"Owner-maintained record. Possible explanations are unconfirmed; veterinary assessments are owner-relayed. No dosing recommendations."
            })
        }
        "get_clinical_timeline" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            serde_json::to_value(
                db::clinical_timeline(
                    &state.db,
                    user.household_id,
                    pet_id,
                    args.get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(50)
                        .clamp(1, 100),
                )
                .await
                .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "get_medication_plan" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            serde_json::to_value(
                db::medication_plan(
                    &state.db,
                    user.household_id,
                    pet_id,
                    args.get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(50)
                        .clamp(1, 100),
                )
                .await
                .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "review_medication_plan" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let names = args
                .get("reported_current_medicines")
                .and_then(Value::as_array)
                .ok_or((-32602, "reported_current_medicines must be an array of medicine names.".into()))?;
            if names.len() > 30 {
                return Err((-32602, "At most 30 reported medicine names are supported.".into()));
            }
            let mut current_names = Vec::<String>::new();
            for item in names {
                let name = item
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.chars().count() <= 120)
                    .ok_or((-32602, "Each medicine name must contain 1–120 characters.".into()))?;
                if !current_names.iter().any(|known| known.eq_ignore_ascii_case(name)) {
                    current_names.push(name.to_owned());
                }
            }
            let plan = db::medication_plan(&state.db, user.household_id, pet_id, 100)
                .await
                .map_err(internal)?;
            let active: Vec<_> = plan.prescriptions.iter().filter(|p| p.status == "active").collect();
            let candidate_stops: Vec<_> = active
                .iter()
                .filter(|prescription| {
                    !current_names.iter().any(|name| name.eq_ignore_ascii_case(&prescription.name))
                })
                .map(|p| json!({"prescription_id":p.id,"name":p.name,"current_status":p.status}))
                .collect();
            let reported_not_in_active: Vec<_> = current_names.iter()
                .filter(|name| !active.iter().any(|p| p.name.eq_ignore_ascii_case(name)))
                .cloned()
                .collect();
            json!({
                "owner_reported_current_medicines": current_names,
                "structured_active_prescriptions": active,
                "possible_stale_active_prescriptions": candidate_stops,
                "reported_medicines_missing_from_active_plan": reported_not_in_active,
                "needs_owner_review": !candidate_stops.is_empty() || !reported_not_in_active.is_empty(),
                "caution": "Read-only comparison of exact medicine names. Confirm synonyms, dose and timing with the owner/vet before changing anything. No medication was modified."
            })
        }
        "get_health_context" => {
            let concept = string(&args, "concept")?;
            serde_json::to_value(
                db::get_knowledge(&state.db, &concept)
                    .await
                    .map_err(internal)?,
            )
            .map_err(internal)?
        }
        "get_weight_history" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let since = opt_date(&args, "since")?;
            let until = opt_date(&args, "until")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 100) as usize;
            let mut weights = db::list_weights(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?;
            if let Some(since) = since {
                weights.retain(|w| w.measured_at.date_naive() >= since);
            }
            if let Some(until) = until {
                weights.retain(|w| w.measured_at.date_naive() <= until);
            }
            weights.truncate(limit);
            serde_json::to_value(weights).map_err(internal)?
        }
        "get_blood_test_history" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let since = opt_date(&args, "since")?;
            let until = opt_date(&args, "until")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(25)
                .clamp(1, 50) as usize;
            let mut reports = db::list_lab_reports(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?;
            if since.is_some() || until.is_some() {
                reports.retain(|r| {
                    let Some(d) = r
                        .test_date
                        .as_deref()
                        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                    else {
                        return false;
                    };
                    since.is_none_or(|s| d >= s) && until.is_none_or(|u| d <= u)
                });
            }
            reports.truncate(limit);
            let mut value = serde_json::to_value(&reports).map_err(internal)?;
            // Add a standardized view next to each printed result so clients
            // can line up the same analyte across labs and unit systems.
            if let Some(items) = value.as_array_mut() {
                for (report_json, report) in items.iter_mut().zip(&reports) {
                    let Some(results) =
                        report_json.get_mut("results").and_then(Value::as_array_mut)
                    else {
                        continue;
                    };
                    for (result_json, result) in results.iter_mut().zip(&report.results) {
                        let standard = crate::analytes::standardize(
                            &result.test_name,
                            result.unit.as_deref(),
                            result.value_numeric,
                        );
                        if standard.recognized {
                            result_json["standard"] = json!({
                                "analyte": standard.key,
                                "label": standard.label,
                                "value": standard.value,
                                "unit": standard.unit,
                            });
                        }
                    }
                }
            }
            value
        }
        "get_health_insights" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(25)
                .clamp(1, 50);
            let include_dismissed = args
                .get("include_dismissed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let findings = recompute_insights(state, user.household_id, &pet)
                .await
                .map_err(internal)?;
            crate::insights::save_findings(&state.db, &findings)
                .await
                .map_err(internal)?;
            let stored = crate::insights::list_findings(
                &state.db,
                user.household_id,
                pet_id,
                include_dismissed,
                limit,
            )
            .await
            .map_err(internal)?;
            serde_json::to_value(stored).map_err(internal)?
        }
        "add_pet" => {
            let name = string(&args, "name")?;
            let species = string(&args, "species")?;
            let id = db::create_pet(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &name,
                &species,
                args.get("breed").and_then(Value::as_str),
                args.get("weight_kg").and_then(Value::as_f64),
            )
            .await
            .map_err(internal)?;
            json!({"pet_id":id,"message":"Pet added."})
        }
        "record_weight" => {
            let pet_id = integer(&args, "pet_id")?;
            if db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .is_none()
            {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let weight = args
                .get("weight_kg")
                .and_then(Value::as_f64)
                .ok_or((-32602, "weight_kg is required.".into()))?;
            if !(0.01..=500.0).contains(&weight) {
                return Err((-32602, "weight_kg must be between 0.01 and 500.".into()));
            }
            let date = string(&args, "measured_at")?;
            let measured_at = if date.len() == 10 {
                format!("{date}T12:00:00Z")
            } else {
                date
            };
            let id = db::create_weight(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet_id,
                weight,
                &measured_at,
                args.get("note").and_then(Value::as_str),
            )
            .await
            .map_err(internal)?;
            json!({"weight_id":id,"message":"Weight saved."})
        }
        "record_symptom_event" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let message = string(&args, "message")?;
            let symptom = args
                .get("symptom")
                .and_then(Value::as_str)
                .unwrap_or("vomiting");
            if !matches!(
                symptom,
                "vomiting"
                    | "diarrhea"
                    | "reduced_appetite"
                    | "wheezing"
                    | "nasal_discharge"
                    | "nasal_congestion"
                    | "eye_discharge"
                    | "coughing"
                    | "labored_breathing"
            ) {
                return Err((-32602, "symptom is not supported.".into()));
            }
            let minutes_ago = args
                .get("minutes_ago")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 525_600);
            let event_id = db::create_symptom_event(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &message,
                Utc::now() - Duration::minutes(minutes_ago),
                symptom,
                args.get("occurrence_count")
                    .and_then(Value::as_i64)
                    .filter(|value| (1..=100).contains(value)),
                string_opt(&args, "amount"),
                string_opt(&args, "contents"),
                string_opt(&args, "meal_relation"),
                string_opt(&args, "water_status"),
                string_opt(&args, "appetite_status"),
                string_opt(&args, "energy_status"),
                string_opt(&args, "pain_status"),
                string_opt(&args, "note"),
                "mcp",
            )
            .await
            .map_err(internal)?;
            json!({"event_id":event_id,"pet":pet.name,"message":"Structured symptom event recorded. The original wording is preserved."})
        }
        "record_medication" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let name = string(&args, "name")?;
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("given");
            if !matches!(status, "given" | "missed" | "extra" | "vomited_back") {
                return Err((-32602, "status is not supported.".into()));
            }
            let minutes_ago = args
                .get("minutes_ago")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 525_600);
            let medication_id = db::create_medication_administration(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &name,
                string_opt(&args, "active_ingredient"),
                args.get("dose_value").and_then(Value::as_f64),
                string_opt(&args, "dose_unit"),
                string_opt(&args, "route"),
                Utc::now() - Duration::minutes(minutes_ago),
                None,
                status,
                string_opt(&args, "message"),
            )
            .await
            .map_err(internal)?;
            json!({"medication_id":medication_id,"pet":pet.name,"message":"Medication exposure recorded."})
        }
        "record_prescription" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let name = string(&args, "name")?;
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("active");
            if !matches!(status, "active" | "paused" | "stopped") {
                return Err((-32602, "status is not supported.".into()));
            }
            let started_on = args
                .get("started_days_ago")
                .and_then(Value::as_i64)
                .map(|days| {
                    (Utc::now().date_naive() - Duration::days(days.clamp(0, 36_500))).to_string()
                });
            // Consequential-change gate: replacing a differing active
            // prescription requires explicit owner confirmation. The web flow
            // stages these for human review; the MCP path enforces the same
            // rule via confirm:true so an assistant cannot silently rewrite
            // dosing. Same-name + same-dose re-saves stay frictionless.
            if status == "active"
                && !args
                    .get("confirm")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                let existing = db::list_active_prescriptions_by_name(
                    &state.db,
                    user.household_id,
                    pet.id,
                    &name,
                )
                .await
                .map_err(internal)?;
                if let Some(current) = existing.first() {
                    let dose_differs = args.get("dose_value").and_then(Value::as_f64)
                        != current.dose_value
                        || string_opt(&args, "dose_unit") != current.dose_unit.as_deref()
                        || string_opt(&args, "frequency") != current.frequency.as_deref();
                    if dose_differs {
                        return Err((
                            -32602,
                            format!(
                                "{} already has an active '{}' prescription ({} {} {}). Re-send with confirm:true only after the owner explicitly approves replacing it.",
                                pet.name,
                                current.name,
                                current
                                    .dose_value
                                    .map(|d| d.to_string())
                                    .as_deref()
                                    .unwrap_or("?"),
                                current.dose_unit.as_deref().unwrap_or("?"),
                                current.frequency.as_deref().unwrap_or("?"),
                            ),
                        ));
                    }
                }
            }
            let prescription_id = db::create_medication_prescription(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &name,
                string_opt(&args, "active_ingredient"),
                args.get("concentration_value").and_then(Value::as_f64),
                string_opt(&args, "concentration_unit"),
                args.get("dose_value").and_then(Value::as_f64),
                string_opt(&args, "dose_unit"),
                string_opt(&args, "frequency"),
                string_opt(&args, "route"),
                string_opt(&args, "instructions"),
                started_on.as_deref(),
                status,
                string_opt(&args, "message"),
            )
            .await
            .map_err(internal)?;
            json!({"prescription_id":prescription_id,"pet":pet.name,"message":"Prescription plan recorded. The original wording is preserved."})
        }
        "record_medication_adherence" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let prescription_id = integer(&args, "prescription_id")?;
            let prescription =
                db::get_prescription(&state.db, user.household_id, pet.id, prescription_id)
                    .await
                    .map_err(internal)?
                    .ok_or((
                        -32602,
                        "That prescription is not in this household for this pet.".into(),
                    ))?;
            let status = string(&args, "status")?;
            if !matches!(status.as_str(), "given" | "partial" | "missed" | "unknown") {
                return Err((-32602, "status is not supported.".into()));
            }
            let days_ago = args
                .get("days_ago")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 36_500);
            let scheduled_for = (Utc::now().date_naive() - Duration::days(days_ago)).to_string();
            let adherence_id = db::create_medication_adherence(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &prescription,
                &scheduled_for,
                args.get("actual_dose_value").and_then(Value::as_f64),
                string_opt(&args, "actual_dose_unit"),
                &status,
                string_opt(&args, "reason"),
                string_opt(&args, "message"),
            )
            .await
            .map_err(internal)?;
            json!({"adherence_id":adherence_id,"prescription_id":prescription.id,"pet":pet.name,"scheduled_for":scheduled_for,"message":"Medication adherence recorded."})
        }
        "upload_blood_test" => {
            let filename = string(&args, "filename")?;
            let content = string(&args, "content_base64")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|_| (-32602, "content_base64 is invalid.".into()))?;
            crate::ocr::store_upload(&state.config, user.household_id, &filename, &bytes)
                .await
                .map_err(internal)?;
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let imported = crate::ocr::import_directory(
                &state.config,
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pets,
            )
            .await
            .map_err(internal)?;
            serde_json::to_value(imported.iter().map(|item| json!({"filename":item.filename,"report_id":item.report_id,"message":item.message})).collect::<Vec<_>>()).map_err(internal)?
        }
        "update_pet" => {
            let pet_id = integer(&args, "pet_id")?;
            let name = bounded_text(args.get("name"), "name", 80).map_err(|m| (-32602, m))?;
            let species =
                bounded_text(args.get("species"), "species", 60).map_err(|m| (-32602, m))?;
            // Empty string clears these; absent leaves them unchanged.
            let breed = match args.get("breed") {
                Some(Value::String(text)) if text.trim().is_empty() => Some(String::new()),
                other => bounded_text(other, "breed", 120).map_err(|m| (-32602, m))?,
            };
            let date_of_birth = match args.get("date_of_birth") {
                Some(Value::String(text)) if text.trim().is_empty() => Some(String::new()),
                other => {
                    let text = bounded_text(other, "date_of_birth", 10).map_err(|m| (-32602, m))?;
                    if let Some(text) = &text {
                        let date =
                            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| {
                                (-32602, "date_of_birth must be YYYY-MM-DD.".to_owned())
                            })?;
                        if date > Utc::now().date_naive() {
                            return Err((-32602, "date_of_birth cannot be in the future.".into()));
                        }
                    }
                    text
                }
            };
            let microchip_number = match args.get("microchip_number") {
                Some(Value::String(value)) if value.trim().is_empty() => Some(String::new()),
                Some(Value::String(value)) => {
                    let number = value.trim();
                    if !crate::domain::valid_microchip_number(number) {
                        return Err((-32602, "microchip_number must be 9 to 20 digits.".into()));
                    }
                    Some(number.to_owned())
                }
                Some(_) => return Err((-32602, "microchip_number must be a string.".into())),
                None => None,
            };
            if name.is_none()
                && species.is_none()
                && breed.is_none()
                && date_of_birth.is_none()
                && microchip_number.is_none()
            {
                return Err((-32602, "Give at least one field to change.".into()));
            }
            let updated = db::update_pet(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet_id,
                name.as_deref(),
                species.as_deref(),
                breed.as_deref(),
                date_of_birth.as_deref(),
                microchip_number.as_deref(),
            )
            .await
            .map_err(|error| {
                if error
                    .to_string()
                    .contains("pets.household_id, pets.microchip_number")
                {
                    (
                        -32602,
                        "Another pet in this household already uses that microchip number."
                            .to_owned(),
                    )
                } else if error.to_string().contains("UNIQUE") {
                    (
                        -32602,
                        "Another pet in this household already has that name.".to_owned(),
                    )
                } else {
                    internal(error)
                }
            })?;
            if !updated {
                return Err((-32602, "That pet is not in this household.".into()));
            }
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?;
            json!({"updated": true, "pet": pet})
        }
        "delete_pet" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let typed = string(&args, "confirm_name")?;
            if args.get("confirm").and_then(Value::as_bool) != Some(true)
                || typed.trim() != pet.name
            {
                return Err((
                    -32602,
                    format!(
                        "Deleting {} permanently removes all of its records. Get the owner's explicit approval, then call with confirm:true and confirm_name exactly \"{}\".",
                        pet.name, pet.name
                    ),
                ));
            }
            let deleted = db::delete_pet(&state.db, user.household_id, &user.audit_actor(), pet.id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            // Remove stored files after the records are gone: photos, and lab
            // source files so the folder importer cannot re-import them.
            state
                .files
                .delete_pet_photos(user.household_id, pet.id)
                .await;
            for file in &deleted.lab_source_files {
                state
                    .files
                    .delete(&crate::storage::FileKey::Report {
                        household_id: user.household_id,
                        relative: file,
                    })
                    .await;
            }
            json!({"deleted": true, "pet_id": deleted.pet_id, "name": deleted.name, "removed": deleted.removed})
        }
        "delete_weight" => {
            let weight_id = integer(&args, "weight_id")?;
            let removed =
                db::delete_weight(&state.db, user.household_id, &user.audit_actor(), weight_id)
                    .await
                    .map_err(internal)?
                    .ok_or((-32602, "That weight entry is not in this household.".into()))?;
            json!({"deleted": true, "removed": removed})
        }
        "create_vet_share" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            if args.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err((
                    -32602,
                    "Confirm who the link is for and how long it should last with the owner, then call again with confirm:true.".into(),
                ));
            }
            let label = bounded_text(args.get("label"), "label", 120)
                .map_err(|m| (-32602, m))?
                .ok_or((
                    -32602,
                    "label (who the link is for) is required.".to_owned(),
                ))?;
            let days = args.get("days").and_then(Value::as_i64).unwrap_or(14);
            if !(1..=90).contains(&days) {
                return Err((-32602, "days must be 1 to 90.".into()));
            }
            let share = db::create_share(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet.id,
                &label,
                days,
            )
            .await
            .map_err(internal)?;
            let token = share.token.clone().unwrap_or_default();
            json!({
                "share_id": share.id,
                "pet": share.pet_name,
                "label": share.label,
                "url": format!("{origin}/share/{token}"),
                "expires_at": share.expires_at,
                "note": "Read-only. This URL is shown only once. Revoke any time with revoke_vet_share."
            })
        }
        "list_vet_shares" => {
            let shares = db::list_shares(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            json!({"shares": shares.into_iter().map(|share| json!({
                "share_id": share.id,
                "pet": share.pet_name,
                "label": share.label,
                "status": share.status,
                "expires_at": share.expires_at,
                "revoked_at": share.revoked_at,
            })).collect::<Vec<_>>()})
        }
        "revoke_vet_share" => {
            let share_id = integer(&args, "share_id")?;
            let revoked =
                db::revoke_share(&state.db, user.household_id, &user.audit_actor(), share_id)
                    .await
                    .map_err(internal)?;
            if !revoked {
                return Err((
                    -32602,
                    "No active share with that id in this household (it may already be revoked)."
                        .into(),
                ));
            }
            json!({"revoked": true, "share_id": share_id})
        }
        "update_prescription" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let prescription_id = integer(&args, "prescription_id")?;
            let current =
                db::get_prescription(&state.db, user.household_id, pet.id, prescription_id)
                    .await
                    .map_err(internal)?
                    .ok_or((
                        -32602,
                        "That prescription is not on this pet's plan.".into(),
                    ))?;
            let request =
                parse_prescription_change(&args, &current.status, Utc::now().date_naive())
                    .map_err(|m| (-32602, m))?;
            let update = db::update_prescription(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                prescription_id,
                &current.status,
                &request.change,
                &request.message,
            )
            .await
            .map_err(internal)?
            .ok_or((
                -32602,
                "The prescription changed meanwhile. Call get_medication_plan and try again."
                    .into(),
            ))?;
            json!({
                "pet": pet.name,
                "medication": current.name,
                "action": request.action,
                "previous_prescription_id": update.previous_prescription_id,
                "current_prescription_id": update.current_prescription_id,
                "previous_status": update.previous_status,
                "new_status": update.new_status,
                "timeline_event_id": update.event_id,
                "note": "Recorded as the owner reported it. This is a record, not a dosing recommendation."
            })
        }
        "undo_medication_entry" => {
            let entry_type = string(&args, "entry_type")?;
            if !matches!(entry_type.as_str(), "dose" | "adherence") {
                return Err((-32602, "entry_type must be 'dose' or 'adherence'.".into()));
            }
            let id = integer(&args, "id")?;
            let removed = db::delete_medication_entry(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &entry_type,
                id,
            )
            .await
            .map_err(internal)?
            .ok_or((
                -32602,
                "That medication entry is not in this household.".into(),
            ))?;
            json!({"deleted": true, "entry_type": entry_type, "removed": removed})
        }
        "record_blood_test" => {
            let input =
                parse_blood_test(&args, Utc::now().date_naive()).map_err(|m| (-32602, m))?;
            let pet = db::get_pet(&state.db, user.household_id, input.pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let hash = input.content_hash();
            if db::report_hash_exists(&state.db, user.household_id, &hash)
                .await
                .map_err(internal)?
            {
                return Err((
                    -32602,
                    "These exact results are already saved for this pet and date. Use get_blood_test_history to view them.".into(),
                ));
            }
            let report_id = db::record_lab_report(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                pet.id,
                &input.source_label(),
                &hash,
                &input.raw_text(),
                &input.test_date,
                &input.results,
            )
            .await
            .map_err(internal)?;
            // Keep the original report when the client attached it. Saving
            // the values never depends on this succeeding.
            let original = match args
                .get("report_file")
                .and_then(|file| file.get("download_url"))
                .and_then(Value::as_str)
            {
                None => json!({"attached": false, "reason": "no file was provided"}),
                Some(url) => {
                    match attach_original(state, user.household_id, report_id, url).await {
                        Ok(()) => json!({"attached": true}),
                        Err(error) => {
                            tracing::warn!(%error, report_id, "could not attach original blood-test file");
                            json!({"attached": false, "reason": error.to_string()})
                        }
                    }
                }
            };
            let same_date: Vec<i64> = db::list_lab_reports(&state.db, user.household_id, pet.id)
                .await
                .map_err(internal)?
                .into_iter()
                .filter(|r| {
                    r.id != report_id && r.test_date.as_deref() == Some(input.test_date.as_str())
                })
                .map(|r| r.id)
                .collect();
            json!({
                "report_id": report_id,
                "original_report": original,
                "other_reports_same_date": same_date,
                "duplicate_hint": if same_date.is_empty() { Value::Null } else {
                    json!("This pet already had a report for this date. Tell the owner; if it is the same report, they may want delete_blood_test on the older one. Do not delete without their explicit request.")
                },
                "pet_id": pet.id,
                "pet_name": pet.name,
                "test_date": input.test_date,
                "saved_results": input.results.len(),
                "results": input.results.iter().map(|r| json!({
                    "test_name": r.test_name,
                    "value": r.value_text,
                    "value_numeric": r.value_numeric,
                    "unit": r.unit,
                    "reference_range": r.reference_range,
                    "flag": r.flag,
                })).collect::<Vec<_>>(),
                "note": "Saved as transcribed and confirmed by the owner. These are records, not a diagnosis."
            })
        }
        "attach_blood_test_original" => {
            let report_id = integer(&args, "report_id")?;
            let exists = db::list_lab_report_ids(&state.db, user.household_id)
                .await
                .map_err(internal)?
                .contains(&report_id);
            if !exists {
                return Err((
                    -32602,
                    "That blood-test report is not in this household.".into(),
                ));
            }
            let url = args
                .get("report_file")
                .and_then(|file| file.get("download_url"))
                .and_then(Value::as_str)
                .ok_or((-32602, "report_file with a download_url is required; attach the report file to the conversation.".to_owned()))?;
            attach_original(state, user.household_id, report_id, url)
                .await
                .map_err(|error| (-32602, format!("Could not attach the original: {error}")))?;
            json!({"attached": true, "report_id": report_id})
        }
        "delete_blood_test" => {
            let report_id = integer(&args, "report_id")?;
            let deleted =
                db::delete_lab_report(&state.db, user.household_id, &user.audit_actor(), report_id)
                    .await
                    .map_err(internal)?;
            if !deleted {
                return Err((
                    -32602,
                    "That blood-test report is not in this household.".into(),
                ));
            }
            json!({"deleted": true, "report_id": report_id})
        }
        "import_blood_tests" => {
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let imported = crate::ocr::import_directory(
                &state.config,
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pets,
            )
            .await
            .map_err(internal)?;
            serde_json::to_value(imported.iter().map(|item| json!({"filename":item.filename,"report_id":item.report_id,"message":item.message})).collect::<Vec<_>>()).map_err(internal)?
        }
        "record_observation" => {
            let pet_id = integer(&args, "pet_id")?;
            let pet = db::get_pet(&state.db, user.household_id, pet_id)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            // Neutral path: no model call. The assistant has already
            // interpreted the request; the message is stored verbatim.
            // Length-bounded like any other owner-supplied text field.
            let message = string(&args, "message")?;
            if message.chars().count() > 1000 {
                return Err((-32602, "message must be 1 to 1000 characters.".into()));
            }
            let minutes_ago = args
                .get("minutes_ago")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 525_600);
            let event_id = db::create_observation(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &message,
                Utc::now() - Duration::minutes(minutes_ago),
                args.get("summary").and_then(Value::as_str),
                args.get("note").and_then(Value::as_str),
                "mcp",
            )
            .await
            .map_err(internal)?;
            json!({"event_id":event_id,"pet":pet.name,"message":"Observation recorded without model inference. The original wording is preserved."})
        }
        "record_health_event" => {
            let message = string(&args, "message")?;
            let pets = db::list_pets(&state.db, user.household_id)
                .await
                .map_err(internal)?;
            let names = pets.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
            let proposal: ProposedEvent = state
                .agent
                .propose(&message, &names)
                .await
                .map_err(|e| (-32602, e.to_string()))?;
            let pet = db::find_pet_by_name(&state.db, user.household_id, &proposal.pet_name)
                .await
                .map_err(internal)?
                .ok_or((-32602, "That pet is not in this household.".into()))?;
            let event_id = db::create_health_event(
                &state.db,
                user.household_id,
                &user.audit_actor(),
                &pet,
                &proposal,
                &message,
                state.agent.occurred_at(&proposal, Utc::now()),
                "mcp",
            )
            .await
            .map_err(internal)?;
            json!({"event_id":event_id,"pet":pet.name,"summary":proposal.summary,"message":"Health event recorded."})
        }
        "undo_health_event" => {
            json!({"undone":db::undo_event(&state.db, user.household_id, &user.audit_actor(), integer(&args, "event_id")?).await.map_err(internal)?})
        }
        _ => return Err((-32601, format!("Unknown tool: {name}"))),
    };
    tool_result(value)
}

/// MCP requires `structuredContent` to be a JSON object; clients such as
/// ChatGPT reject arrays or scalars. Non-object tool output is wrapped as
/// `{"items": ...}` (arrays) or `{"result": ...}` (anything else).
fn tool_result(value: Value) -> Result<Value, (i32, String)> {
    let structured = match value {
        Value::Object(_) => value,
        Value::Array(_) => json!({ "items": value }),
        other => json!({ "result": other }),
    };
    Ok(
        json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&structured).map_err(internal)?}],"structuredContent":structured,"isError":false}),
    )
}

#[derive(Debug)]
struct PrescriptionChangeRequest {
    action: String,
    message: String,
    change: db::PrescriptionChange,
}

/// Validates an update_prescription call against the prescription's current
/// status. Dates are computed from day offsets; the model never supplies one.
fn parse_prescription_change(
    args: &Value,
    current_status: &str,
    today: chrono::NaiveDate,
) -> Result<PrescriptionChangeRequest, String> {
    if args.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err("Restate the change to the owner, get explicit approval, then call again with confirm:true.".into());
    }
    let message = bounded_text(args.get("message"), "message", 1000)?
        .ok_or("message (the owner's words) is required.")?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("action is required.")?
        .to_owned();
    // Omitting the date means unknown, not today. Callers may explicitly
    // send 0 only when the owner confirms the change took effect today.
    let days_ago = match args.get("effective_days_ago").filter(|v| !v.is_null()) {
        Some(value) => Some(value.as_i64().ok_or("effective_days_ago must be an integer.")?),
        None => None,
    };
    if days_ago.is_some_and(|days| !(0..=3650).contains(&days)) {
        return Err("effective_days_ago must be 0 to 3650.".into());
    }
    let effective_on = days_ago.map(|days| today - Duration::days(days));
    let positive = |key: &str| -> Result<Option<f64>, String> {
        match args.get(key).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(Some)
                .ok_or(format!("{key} must be a positive number.")),
        }
    };
    let change = match (action.as_str(), current_status) {
        ("stop", "active" | "paused") => db::PrescriptionChange::Stop {
            ended_on: effective_on,
        },
        ("pause", "active") => db::PrescriptionChange::Pause,
        ("resume", "paused") => db::PrescriptionChange::Resume,
        ("change_dose", "active") => {
            let change = db::PrescriptionChange::ChangeDose {
                dose_value: positive("dose_value")?,
                dose_unit: bounded_text(args.get("dose_unit"), "dose_unit", 40)?,
                frequency: bounded_text(args.get("frequency"), "frequency", 120)?,
                concentration_value: positive("concentration_value")?,
                concentration_unit: bounded_text(
                    args.get("concentration_unit"),
                    "concentration_unit",
                    40,
                )?,
                instructions: bounded_text(args.get("instructions"), "instructions", 1000)?,
                effective_on,
            };
            if let db::PrescriptionChange::ChangeDose {
                dose_value: None,
                dose_unit: None,
                frequency: None,
                concentration_value: None,
                concentration_unit: None,
                instructions: None,
                ..
            } = change
            {
                return Err("change_dose needs at least one new value (dose, unit, frequency, concentration or instructions).".into());
            }
            change
        }
        ("stop" | "pause" | "resume" | "change_dose", status) => {
            return Err(format!("Cannot {action} a prescription that is {status}."));
        }
        _ => return Err("action must be stop, pause, resume or change_dose.".into()),
    };
    Ok(PrescriptionChangeRequest {
        action,
        message,
        change,
    })
}

async fn attach_original(
    state: &AppState,
    household_id: i64,
    report_id: i64,
    download_url: &str,
) -> anyhow::Result<()> {
    let (bytes, extension) = crate::ocr::fetch_client_file(download_url).await?;
    let relative =
        crate::ocr::store_original(&state.files, household_id, report_id, &bytes, extension)
            .await?;
    db::set_lab_report_original(&state.db, household_id, report_id, &relative).await
}

/// A validated blood-test transcription supplied by an MCP client.
#[derive(Debug)]
struct BloodTestInput {
    pet_id: i64,
    test_date: String,
    lab_name: Option<String>,
    panel: Option<String>,
    transcription: Option<String>,
    results: Vec<crate::ocr::ParsedLabResult>,
}

impl BloodTestInput {
    fn source_label(&self) -> String {
        let what = self.panel.as_deref().unwrap_or("Blood test");
        match &self.lab_name {
            Some(lab) => format!("{what} – {lab} ({}) via assistant", self.test_date),
            None => format!("{what} ({}) via assistant", self.test_date),
        }
    }

    /// Source evidence stored with the report: the assistant's verbatim
    /// transcription when given, otherwise a plain listing of the values.
    fn raw_text(&self) -> String {
        if let Some(text) = &self.transcription {
            return text.clone();
        }
        let mut lines = vec![format!("Test date: {}", self.test_date)];
        lines.extend(self.lab_name.iter().map(|lab| format!("Lab: {lab}")));
        lines.extend(self.panel.iter().map(|panel| format!("Panel: {panel}")));
        for r in &self.results {
            let mut line = format!("{}: {}", r.test_name, r.value_text);
            if let Some(unit) = &r.unit {
                line.push_str(&format!(" {unit}"));
            }
            if let Some(range) = &r.reference_range {
                line.push_str(&format!(" (ref {range})"));
            }
            if let Some(flag) = &r.flag {
                line.push_str(&format!(" [{flag}]"));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// Dedupes identical re-submissions for the same pet and date.
    fn content_hash(&self) -> String {
        let canonical = json!({
            "source": "assistant",
            "pet_id": self.pet_id,
            "test_date": self.test_date,
            "results": self.results.iter().map(|r| json!([
                r.test_name.to_lowercase(), r.value_text, r.unit, r.reference_range
            ])).collect::<Vec<_>>(),
        });
        Sha256::digest(canonical.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn bounded_text(value: Option<&Value>, field: &str, max: usize) -> Result<Option<String>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return Err(format!("{field} must be text.")),
    };
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > max {
        return Err(format!("{field} must be at most {max} characters."));
    }
    Ok(Some(text))
}

/// Numeric value only when the printed value is a plain number
/// ("1.8", "1,8", "1 234"). Qualified values like "<0.1" stay text-only.
fn plain_number(value: &str) -> Option<f64> {
    let cleaned: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let cleaned = if cleaned.matches(',').count() == 1 && !cleaned.contains('.') {
        cleaned.replace(',', ".")
    } else {
        cleaned.replace(',', "")
    };
    cleaned.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn parse_blood_test(args: &Value, today: chrono::NaiveDate) -> Result<BloodTestInput, String> {
    if args.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err("Show the owner the extracted values and ask them to confirm first, then call again with confirm:true.".into());
    }
    let pet_id = args
        .get("pet_id")
        .and_then(Value::as_i64)
        .ok_or("pet_id is required.")?;
    let date_text = args
        .get("test_date")
        .and_then(Value::as_str)
        .ok_or("test_date is required (YYYY-MM-DD, as printed on the report).")?;
    let date = chrono::NaiveDate::parse_from_str(date_text.trim(), "%Y-%m-%d")
        .map_err(|_| "test_date must be YYYY-MM-DD.".to_owned())?;
    if date > today {
        return Err("test_date is in the future. Check the report's date format (day/month vs month/day) with the owner.".into());
    }
    if date < chrono::NaiveDate::from_ymd_opt(1980, 1, 1).expect("valid date") {
        return Err("test_date looks wrong; check it with the owner.".into());
    }
    let items = args
        .get("results")
        .and_then(Value::as_array)
        .ok_or("results must be a list of analytes.")?;
    if items.is_empty() || items.len() > 200 {
        return Err("results must contain 1 to 200 analytes.".into());
    }
    let mut results = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let at = |field: &str| format!("results[{index}].{field}");
        let test_name = bounded_text(item.get("test_name"), &at("test_name"), 120)?
            .ok_or_else(|| format!("{} is required.", at("test_name")))?;
        let value_text = bounded_text(item.get("value"), &at("value"), 60)?
            .ok_or_else(|| format!("{} is required.", at("value")))?;
        let flag = bounded_text(item.get("flag"), &at("flag"), 20)?.map(|flag| flag.to_lowercase());
        if let Some(flag) = &flag
            && !matches!(flag.as_str(), "high" | "low" | "normal" | "abnormal")
        {
            return Err(format!(
                "{} must be high, low, normal or abnormal.",
                at("flag")
            ));
        }
        results.push(crate::ocr::ParsedLabResult {
            value_numeric: plain_number(&value_text),
            unit: bounded_text(item.get("unit"), &at("unit"), 40)?,
            reference_range: bounded_text(item.get("reference_range"), &at("reference_range"), 80)?,
            test_name,
            value_text,
            flag,
        });
    }
    Ok(BloodTestInput {
        pet_id,
        test_date: date.format("%Y-%m-%d").to_string(),
        lab_name: bounded_text(args.get("lab_name"), "lab_name", 120)?,
        panel: bounded_text(args.get("panel"), "panel", 120)?,
        transcription: bounded_text(args.get("transcription"), "transcription", 20_000)?,
        results,
    })
}

fn integer(args: &Value, key: &str) -> Result<i64, (i32, String)> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or((-32602, format!("{key} must be an integer.")))
}
fn string(args: &Value, key: &str) -> Result<String, (i32, String)> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .ok_or((-32602, format!("{key} is required.")))
}
fn string_opt<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
fn internal(error: impl std::fmt::Display) -> (i32, String) {
    (-32603, error.to_string())
}

/// Optional `YYYY-MM-DD` argument. Rejects malformed dates loudly rather
/// than silently ignoring the bound.
fn opt_date(args: &Value, key: &str) -> Result<Option<chrono::NaiveDate>, (i32, String)> {
    match args.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(raw) => chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
            .map(Some)
            .map_err(|_| (-32602, format!("{key} must be YYYY-MM-DD."))),
    }
}

/// Recompute deterministic insights for one pet from current rows.
/// Pure reads + arithmetic: no model, no network, no diagnosis.
async fn recompute_insights(
    state: &AppState,
    household_id: i64,
    pet: &crate::domain::Pet,
) -> anyhow::Result<Vec<crate::insights::HealthFinding>> {
    use crate::insights::{AdherencePoint, LabPoint, SymptomPoint, WeightPoint};
    let now = Utc::now();
    let weights = db::list_weights(&state.db, household_id, pet.id).await?;
    let weight_points: Vec<WeightPoint> = weights
        .into_iter()
        .take(200)
        .map(|w| WeightPoint {
            id: w.id,
            weight_kg: w.weight_kg,
            measured_at: w.measured_at,
        })
        .collect();
    let events = db::list_events(&state.db, household_id, Some(pet.id), 200).await?;
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
    let rows = db::list_adherence(&state.db, household_id, pet.id, 100).await?;
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
    let reports = db::list_lab_reports(&state.db, household_id, pet.id).await?;
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
    Ok(crate::insights::compute_insights(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn blood_args() -> Value {
        json!({
            "pet_id": 1,
            "test_date": "2026-11-02",
            "lab_name": "Vetlab",
            "results": [
                {"test_name": "CREA", "value": "1,8", "unit": "mg/dL", "reference_range": "0.8 - 2.4"},
                {"test_name": "SDMA", "value": "<5", "unit": "ug/dL", "flag": "Low"},
                {"test_name": "ALT", "value": 92, "flag": "high"}
            ],
            "confirm": true
        })
    }

    fn day(text: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn blood_test_input_keeps_printed_values_and_parses_plain_numbers() {
        let input = parse_blood_test(&blood_args(), day("2026-11-15")).unwrap();
        assert_eq!(input.results.len(), 3);
        assert_eq!(input.results[0].value_text, "1,8");
        assert_eq!(input.results[0].value_numeric, Some(1.8));
        assert_eq!(input.results[1].value_text, "<5");
        assert_eq!(input.results[1].value_numeric, None);
        assert_eq!(input.results[1].flag.as_deref(), Some("low"));
        assert_eq!(input.results[2].value_numeric, Some(92.0));
        assert!(input.raw_text().contains("CREA: 1,8 mg/dL (ref 0.8 - 2.4)"));
        assert_eq!(
            input.content_hash(),
            parse_blood_test(&blood_args(), day("2026-11-15"))
                .unwrap()
                .content_hash()
        );
    }

    #[test]
    fn blood_test_input_rejects_unconfirmed_future_and_bad_flags() {
        let mut args = blood_args();
        args["confirm"] = json!(false);
        assert!(
            parse_blood_test(&args, day("2026-11-15"))
                .unwrap_err()
                .contains("confirm")
        );
        let mut args = blood_args();
        args["test_date"] = json!("2026-12-11");
        assert!(
            parse_blood_test(&args, day("2026-11-15"))
                .unwrap_err()
                .contains("future")
        );
        let mut args = blood_args();
        args["test_date"] = json!("11/02/2026");
        assert!(parse_blood_test(&args, day("2026-11-15")).is_err());
        let mut args = blood_args();
        args["results"][0]["flag"] = json!("very high");
        assert!(parse_blood_test(&args, day("2026-11-15")).is_err());
        let mut args = blood_args();
        args["results"] = json!([]);
        assert!(parse_blood_test(&args, day("2026-11-15")).is_err());
    }

    #[test]
    fn prescription_change_enforces_confirmation_and_valid_transitions() {
        let today = day("2026-11-15");
        let base = json!({"pet_id":1,"prescription_id":2,"action":"stop","message":"Vet stopped it","confirm":true,"effective_days_ago":3});
        let ok = parse_prescription_change(&base, "active", today).unwrap();
        assert!(
            matches!(ok.change, db::PrescriptionChange::Stop { ended_on } if ended_on == day("2026-11-12"))
        );
        let mut unconfirmed = base.clone();
        unconfirmed["confirm"] = json!(false);
        assert!(parse_prescription_change(&unconfirmed, "active", today).is_err());
        assert!(
            parse_prescription_change(&base, "stopped", today)
                .unwrap_err()
                .contains("stopped")
        );
        let resume = json!({"pet_id":1,"prescription_id":2,"action":"resume","message":"Back on it","confirm":true});
        assert!(parse_prescription_change(&resume, "active", today).is_err());
        assert!(parse_prescription_change(&resume, "paused", today).is_ok());
        let empty_change = json!({"pet_id":1,"prescription_id":2,"action":"change_dose","message":"Change","confirm":true});
        assert!(parse_prescription_change(&empty_change, "active", today).is_err());
        let bad_dose = json!({"pet_id":1,"prescription_id":2,"action":"change_dose","message":"Change","confirm":true,"dose_value":-1});
        assert!(parse_prescription_change(&bad_dose, "active", today).is_err());
        let far = json!({"pet_id":1,"prescription_id":2,"action":"stop","message":"x","confirm":true,"effective_days_ago":5000});
        assert!(parse_prescription_change(&far, "active", today).is_err());
        // An omitted date is unknown, never silently the recording date.
        let unknown = json!({"pet_id":1,"prescription_id":2,"action":"stop","message":"It was stopped earlier; exact date unknown","confirm":true});
        let parsed = parse_prescription_change(&unknown, "active", today).unwrap();
        assert!(matches!(parsed.change, db::PrescriptionChange::Stop { ended_on: None }));
        let unknown_change = json!({"pet_id":1,"prescription_id":2,"action":"change_dose","dose_value":0.3,"message":"Dose is now 0.3 ml but we do not know when it started","confirm":true});
        let parsed = parse_prescription_change(&unknown_change, "active", today).unwrap();
        assert!(matches!(parsed.change, db::PrescriptionChange::ChangeDose { effective_on: None, .. }));
        let invalid = json!({"pet_id":1,"prescription_id":2,"action":"stop","message":"Stopped","confirm":true,"effective_days_ago":"yesterday"});
        assert!(parse_prescription_change(&invalid, "active", today).is_err());
    }

    #[test]
    fn structured_content_is_always_an_object() {
        let wrapped = tool_result(json!([{"id":1,"name":"Velcro"}])).unwrap();
        assert_eq!(wrapped["structuredContent"]["items"][0]["name"], "Velcro");
        let scalar = tool_result(json!(true)).unwrap();
        assert_eq!(scalar["structuredContent"], json!({"result": true}));
        let object = tool_result(json!({"undone": true})).unwrap();
        assert_eq!(object["structuredContent"], json!({"undone": true}));
    }

    #[test]
    fn consent_csp_allows_registered_redirect_origin() {
        let csp = consent_csp("https://chatgpt.com/connector_platform_oauth_redirect");
        assert!(csp.ends_with("form-action 'self' https://chatgpt.com"));
        assert!(consent_csp("javascript:alert(1)").ends_with("form-action 'self'"));
    }

    #[test]
    fn negotiates_known_versions_and_falls_back_to_newest() {
        assert_eq!(negotiate_protocol_version("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate_protocol_version("2024-11-05"), "2024-11-05");
        assert_eq!(negotiate_protocol_version("2025-11-25"), "2025-11-25");
        assert_eq!(negotiate_protocol_version("1999-01-01"), "2025-11-25");
        assert_eq!(negotiate_protocol_version(""), "2025-11-25");
    }

    #[test]
    fn tool_list_keeps_directory_required_tools() {
        let list = tool_list();
        let tools = list
            .get("tools")
            .and_then(Value::as_array)
            .expect("tool list has tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        for required in [
            "list_pets",
            "get_pet_timeline",
            "get_health_insights",
            "record_observation",
            "record_weight",
            "record_blood_test",
            "delete_blood_test",
            "update_prescription",
            "undo_medication_entry",
            "update_pet",
            "delete_pet",
            "delete_weight",
            "attach_blood_test_original",
            "create_vet_share",
            "list_vet_shares",
            "revoke_vet_share",
        ] {
            assert!(names.contains(&required), "missing tool: {required}");
        }
    }
}
