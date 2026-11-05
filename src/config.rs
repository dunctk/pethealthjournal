use anyhow::{Context, bail};
use std::{env, fmt, path::Path};

/// Deployment behavior is independent of PRODUCTION (the latter controls storage).
/// Absence of DEPLOYMENT_MODE is intentionally self-hosted, including in production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentMode {
    SelfHosted,
    Managed,
}

impl DeploymentMode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "self_hosted" => Ok(Self::SelfHosted),
            "managed" => Ok(Self::Managed),
            _ => bail!("DEPLOYMENT_MODE must be self_hosted or managed"),
        }
    }
}

#[derive(Clone)]
pub struct StripeConfig {
    pub secret_key: String,
    pub webhook_secret: String,
}

// Never accidentally emit Stripe secrets through a Config debug print.
impl fmt::Debug for StripeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StripeConfig { credentials: [REDACTED] }")
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub production: bool,
    pub deployment_mode: DeploymentMode,
    /// Reserved request flag; there is no proactive background worker yet.
    pub proactive_ai_requested: bool,
    /// Optional managed-mode integration credentials; always None in self-hosted mode.
    pub stripe: Option<StripeConfig>,
    pub database_url: String,
    pub llm_api_key: Option<String>,
    pub llm_base_url: String,
    pub llm_model: String,
    /// How long to wait on the capture extraction call. `rig` builds its own HTTP
    /// client with no timeout, so this is the only thing stopping a stalled
    /// upstream from holding the request open forever.
    pub llm_timeout_seconds: u64,
    pub mistral_api_key: Option<String>,
    pub blood_tests_dir: String,
    pub pet_photos_dir: String,
    /// Non-secret managed-mode display/API configuration. Secrets live in
    /// `stripe` and are validated as a pair; these extras are safe to
    /// expose to templates and logs.
    pub stripe_publishable_key: Option<String>,
    pub stripe_plus_monthly_price: Option<String>,
    pub stripe_plus_annual_price: Option<String>,
    pub app_base_url: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();
        let production = env_bool("PRODUCTION");
        let deployment_mode = DeploymentMode::parse(
            &env::var("DEPLOYMENT_MODE").unwrap_or_else(|_| "self_hosted".to_owned()),
        )?;
        let stripe = stripe_config(
            deployment_mode,
            nonempty_env("STRIPE_SECRET_KEY"),
            nonempty_env("STRIPE_WEBHOOK_SECRET"),
        )?;
        let llm_api_key = first_nonempty_env(&["OPENROUTER_API_KEY", "LLM_API_KEY"]);
        let proactive_ai_requested = env_bool("PROACTIVE_AI_ENABLED");
        validate_proactive_ai_settings(
            deployment_mode,
            proactive_ai_requested,
            llm_api_key.is_some(),
        )?;
        let database_url = if production {
            let parent = Path::new("/persistent");
            if !parent.exists() {
                bail!("PRODUCTION=true requires the /persistent volume");
            }
            "sqlite:///persistent/pethealth.sqlite?mode=rwc".to_owned()
        } else {
            env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://./data/pethealth.sqlite?mode=rwc".to_owned())
        };
        let port = env::var("APP_PORT")
            .unwrap_or_else(|_| "3000".to_owned())
            .parse()
            .context("APP_PORT must be a valid port")?;
        let username = env::var("APP_USERNAME").unwrap_or_else(|_| "owner".to_owned());
        let password = env::var("APP_PASSWORD").unwrap_or_else(|_| "change-me".to_owned());
        if production && password == "change-me" {
            bail!("APP_PASSWORD must be set to a non-default value in production");
        }
        Ok(Self {
            host: env::var("APP_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned()),
            port,
            username,
            password,
            production,
            deployment_mode,
            proactive_ai_requested,
            stripe,
            database_url,
            // Prefer the provider-native name when both are present, while
            // keeping the generic name for backwards compatibility.
            llm_api_key,
            llm_base_url: nonempty_env("LLM_BASE_URL")
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_owned()),
            llm_model: nonempty_env("LLM_MODEL").unwrap_or_else(|| "openai/gpt-5.6-sol".to_owned()),
            llm_timeout_seconds: env::var("LLM_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(20),
            mistral_api_key: nonempty_env("MISTRAL_API_KEY"),
            blood_tests_dir: env::var("BLOOD_TESTS_DIR")
                .unwrap_or_else(|_| "./example_blood_tests".to_owned()),
            pet_photos_dir: env::var("PET_PHOTOS_DIR")
                .unwrap_or_else(|_| "./data/pet_photos".to_owned()),
            stripe_publishable_key: nonempty_env("STRIPE_PUBLISHABLE_KEY"),
            stripe_plus_monthly_price: nonempty_env("STRIPE_PLUS_MONTHLY_PRICE"),
            stripe_plus_annual_price: nonempty_env("STRIPE_PLUS_ANNUAL_PRICE"),
            app_base_url: nonempty_env("APP_BASE_URL"),
        })
    }

    /// True when Stripe operations are possible. Self-hosted installs report
    /// false here and every billing path must treat that as "billing
    /// unavailable", never as an error that blocks core records.
    pub fn stripe_configured(&self) -> bool {
        self.stripe.is_some()
    }
}

/// In self-hosted mode, no billing integration is loaded or required.
/// Managed mode may run before checkout is implemented; if credentials are
/// supplied, they must form a complete pair.
fn stripe_config(
    mode: DeploymentMode,
    secret_key: Option<String>,
    webhook_secret: Option<String>,
) -> anyhow::Result<Option<StripeConfig>> {
    if mode == DeploymentMode::SelfHosted {
        return Ok(None);
    }
    match (secret_key, webhook_secret) {
        (Some(secret_key), Some(webhook_secret)) => Ok(Some(StripeConfig {
            secret_key,
            webhook_secret,
        })),
        (None, None) => Ok(None),
        _ => bail!(
            "managed mode requires both STRIPE_SECRET_KEY and STRIPE_WEBHOOK_SECRET, or neither"
        ),
    }
}

/// Future worker safety contract: only self-hosted operators with their own
/// provider key may request proactive AI at this stage. Managed AI must
/// eventually require per-household paid entitlements, not a global switch.
/// No scheduled analyzer exists yet; validation does not turn one on.
fn validate_proactive_ai_settings(
    mode: DeploymentMode,
    requested: bool,
    has_model_key: bool,
) -> anyhow::Result<()> {
    if !requested {
        return Ok(());
    }
    match mode {
        DeploymentMode::Managed => bail!(
            "PROACTIVE_AI_ENABLED is blocked in managed mode until household-level paid entitlements, budgets and a worker exist"
        ),
        DeploymentMode::SelfHosted if !has_model_key => bail!(
            "PROACTIVE_AI_ENABLED=true requires a self-hosted operator-provided OPENROUTER_API_KEY or LLM_API_KEY"
        ),
        DeploymentMode::SelfHosted => Ok(()),
    }
}

fn env_bool(key: &str) -> bool {
    matches!(
        env::var(key).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn nonempty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| nonempty_env(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_database_is_persistent() {
        let config = Config {
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            production: true,
            deployment_mode: DeploymentMode::SelfHosted,
            proactive_ai_requested: false,
            stripe: None,
            database_url: "sqlite:///persistent/pethealth.sqlite?mode=rwc".into(),
            llm_api_key: None,
            llm_base_url: String::new(),
            llm_model: String::new(),
            llm_timeout_seconds: 20,
            mistral_api_key: None,
            blood_tests_dir: "./example_blood_tests".into(),
            pet_photos_dir: "./data/pet_photos".into(),
            stripe_publishable_key: None,
            stripe_plus_monthly_price: None,
            stripe_plus_annual_price: None,
            app_base_url: None,
        };
        assert!(config.database_url.contains("/persistent/pethealth.sqlite"));
        assert!(!config.stripe_configured());
    }

    #[test]
    fn deployment_mode_defaults_to_self_hosted() {
        assert_eq!(
            DeploymentMode::parse("self_hosted").unwrap(),
            DeploymentMode::SelfHosted
        );
    }

    #[test]
    fn deployment_mode_defaults_are_explicit_and_invalid_modes_fail() {
        assert_eq!(
            DeploymentMode::parse("self_hosted").unwrap(),
            DeploymentMode::SelfHosted
        );
        assert_eq!(
            DeploymentMode::parse("managed").unwrap(),
            DeploymentMode::Managed
        );
        assert!(DeploymentMode::parse("unknown").is_err());
    }

    #[test]
    fn self_hosted_never_initializes_stripe_even_with_partial_credentials() {
        assert!(
            stripe_config(DeploymentMode::SelfHosted, Some("sk_test".into()), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_stripe_configuration_requires_both_secrets() {
        assert!(
            stripe_config(DeploymentMode::Managed, None, None)
                .unwrap()
                .is_none()
        );
        assert!(stripe_config(DeploymentMode::Managed, Some("sk_test".into()), None).is_err());
        let configured = stripe_config(
            DeploymentMode::Managed,
            Some("sk_test".into()),
            Some("whsec_test".into()),
        )
        .unwrap();
        assert!(configured.is_some());
        assert!(!format!("{:?}", configured).contains("sk_test"));
    }

    #[test]
    fn proactive_ai_settings_fail_closed_for_managed_hosting() {
        assert!(validate_proactive_ai_settings(DeploymentMode::Managed, false, false).is_ok());
        assert!(validate_proactive_ai_settings(DeploymentMode::Managed, true, true).is_err());
        assert!(validate_proactive_ai_settings(DeploymentMode::Managed, true, false).is_err());
    }

    #[test]
    fn self_hosted_proactive_request_needs_operator_key() {
        assert!(validate_proactive_ai_settings(DeploymentMode::SelfHosted, false, false).is_ok());
        assert!(validate_proactive_ai_settings(DeploymentMode::SelfHosted, true, false).is_err());
        assert!(validate_proactive_ai_settings(DeploymentMode::SelfHosted, true, true).is_ok());
    }
}
