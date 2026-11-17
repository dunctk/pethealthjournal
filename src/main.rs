#![recursion_limit = "256"]
mod agent;
mod analytes;
mod auth;
mod billing;
mod clinical_cases;
mod config;
mod db;
mod domain;
mod email;
mod entitlements;
mod insights;
#[cfg(test)]
mod invitation_tests;
mod invitations;
#[cfg(test)]
mod login_code_tests;
mod mcp;
#[cfg(test)]
mod mcp_e2e_tests;
mod ocr;
mod passports;
mod photos;
mod storage;
mod vet_share;
mod web;

use anyhow::Context;
use config::Config;
use std::sync::Arc;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub db: sea_orm::DatabaseConnection,
    pub config: Arc<Config>,
    pub agent: Arc<agent::CaptureAgent>,
    pub chat: Arc<agent::ChatAgent>,
    pub mailer: Arc<email::Mailer>,
    pub files: Arc<storage::FileStore>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pethealth=info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    let database = db::connect(&config.database_url).await?;
    db::migrate(&database).await?;
    db::bootstrap_owner(&database, &config.username, &config.password).await?;
    if let Some(command) = std::env::args().nth(1) {
        return run_operator_command(&database, &command).await;
    }
    let agent = Arc::new(agent::CaptureAgent::new(&config));
    let chat = Arc::new(agent::ChatAgent::new(&config));
    let mailer = Arc::new(email::Mailer::from_config(config.smtp.as_ref())?);
    let files = Arc::new(storage::FileStore::from_config(&config)?);
    let state = AppState {
        db: database,
        config: config.clone(),
        agent,
        chat,
        mailer,
        files,
    };

    let app = web::router(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http());
    let address = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    info!(
        address,
        database = %config.database_url,
        production = config.production,
        deployment_mode = ?config.deployment_mode,
        stripe_configured = config.stripe.is_some(),
        proactive_ai_requested = config.proactive_ai_requested,
        email_configured = config.smtp.is_some(),
        object_storage = config.object_store.is_some(),
        "pethealth ready"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Operator commands, run inside the container (`docker exec ... pethealth
/// <command>`), never over HTTP.
///
/// `set-login-password <email>`: create the account if needed and set its
/// password from the `NEW_PASSWORD` environment variable. For accounts that
/// cannot receive emailed codes, such as an app-store reviewer's.
async fn run_operator_command(
    database: &sea_orm::DatabaseConnection,
    command: &str,
) -> anyhow::Result<()> {
    match command {
        "set-login-password" => {
            let email = std::env::args()
                .nth(2)
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| value.contains('@') && !value.contains(char::is_whitespace))
                .context("usage: NEW_PASSWORD=... pethealth set-login-password <email>")?;
            let password = std::env::var("NEW_PASSWORD").unwrap_or_default();
            if password.chars().count() < 12 || password.chars().count() > 128 {
                anyhow::bail!("NEW_PASSWORD must be 12 to 128 characters");
            }
            let (user, created) = db::find_or_create_email_user(database, &email).await?;
            let hash = auth::hash_password(password).await?;
            db::update_password_and_revoke_sessions(database, &user, &hash).await?;
            println!(
                "{} account {} (household {}); password set, other sessions signed out",
                if created { "created" } else { "updated" },
                user.email.as_deref().unwrap_or(&email),
                user.household_id
            );
            Ok(())
        }
        other => anyhow::bail!("unknown command {other:?}; available: set-login-password"),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
