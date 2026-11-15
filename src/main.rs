#![recursion_limit = "256"]
mod agent;
mod analytes;
mod auth;
mod billing;
mod config;
mod db;
mod domain;
mod email;
mod entitlements;
mod insights;
#[cfg(test)]
mod login_code_tests;
mod mcp;
#[cfg(test)]
mod mcp_e2e_tests;
mod ocr;
mod photos;
mod storage;
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
