//! Stripe billing for the managed hosted service (test mode first).
//!
//! Boundaries:
//! - All Stripe calls run server-side with the operator-held secret key.
//! - Webhooks are signature-verified (HMAC-SHA256) and idempotent via the
//!   `stripe_events` table; out-of-order delivery is safe.
//! - Subscription state is persisted per household in
//!   `household_subscriptions`; the app enforces entitlements from that row,
//!   never from client-supplied claims or a Checkout redirect alone.
//! - Self-hosted installs (`DEPLOYMENT_MODE != managed`) bypass everything
//!   here: [`checkout_available`] returns false and no key is required.

use crate::{config::Config, entitlements::Plan};
use anyhow::{Context, anyhow};
use hmac::{Hmac, Mac};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// True when the server may offer Stripe checkout/portal links.
pub fn checkout_available(config: &Config) -> bool {
    config.deployment_mode == crate::config::DeploymentMode::Managed && config.stripe_configured()
}

/// Verify a Stripe webhook signature (`Stripe-Signature` header).
/// Returns the event id + type when valid. Never logs the secret.
pub fn verify_webhook_signature(
    payload: &[u8],
    header: &str,
    secret: &str,
    tolerance_secs: i64,
) -> anyhow::Result<(String, String)> {
    // Header form: t=1492774577,v1=<hex>[,v0=...]
    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<String> = Vec::new();
    for part in header.split(',') {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("malformed Stripe-Signature header"))?;
        match k.trim() {
            "t" => timestamp = v.trim().parse().ok(),
            "v1" => signatures.push(v.trim().to_owned()),
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or_else(|| anyhow!("missing webhook timestamp"))?;
    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > tolerance_secs {
        anyhow::bail!("webhook timestamp outside tolerance");
    }
    let signed = format!("{timestamp}.{}", String::from_utf8_lossy(payload));
    let mut ok = false;
    for sig in &signatures {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|_| anyhow!("invalid webhook secret"))?;
        mac.update(signed.as_bytes());
        let expected = hex_decode(sig)?;
        if mac.verify_slice(&expected).is_ok() {
            ok = true;
            break;
        }
    }
    if !ok {
        anyhow::bail!("webhook signature mismatch");
    }
    let event: serde_json::Value =
        serde_json::from_slice(payload).context("webhook payload is not JSON")?;
    let id = event
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("webhook event has no id"))?
        .to_owned();
    let kind = event
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_owned();
    Ok((id, kind))
}

fn hex_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        anyhow::bail!("bad hex signature");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("bad hex signature")))
        .collect()
}

/// Idempotency guard: true when this event id was already processed.
pub async fn already_processed<C: ConnectionTrait>(db: &C, event_id: &str) -> anyhow::Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT event_id FROM stripe_events WHERE event_id=?",
            [event_id.into()],
        ))
        .await?
        .is_some())
}

/// Record an event id after handling it.
pub async fn mark_processed<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    event_type: &str,
    now_rfc3339: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT OR IGNORE INTO stripe_events(event_id,event_type,received_at) VALUES(?,?,?)",
        [event_id.into(), event_type.into(), now_rfc3339.into()],
    ))
    .await?;
    Ok(())
}

/// Apply a verified Stripe event to the household mapped by customer id.
/// Called after signature verification + idempotency check. Handles
/// checkout completion, subscription updates, cancellations and payment
/// failures. Unknown customers are logged, never granted access.
pub async fn apply_event<C: ConnectionTrait>(
    db: &C,
    event_type: &str,
    event: &serde_json::Value,
    now_rfc3339: &str,
) -> anyhow::Result<&'static str> {
    let data = event.get("data").and_then(|d| d.get("object"));
    match event_type {
        "checkout.session.completed" => {
            let obj = data.unwrap_or(&serde_json::Value::Null);
            let customer = obj.get("customer").and_then(|v| v.as_str());
            let subscription = obj.get("subscription").and_then(|v| v.as_str());
            let household = household_for_customer(db, customer).await?;
            let Some(household_id) = household else {
                return Ok("no_household_for_customer");
            };
            crate::entitlements::set_subscription(
                db,
                household_id,
                Plan::Plus,
                "active",
                customer,
                subscription,
                None,
                false,
                now_rfc3339,
            )
            .await?;
            Ok("plus_granted")
        }
        "customer.subscription.updated" | "customer.subscription.created" => {
            let obj = data.unwrap_or(&serde_json::Value::Null);
            let customer = obj.get("customer").and_then(|v| v.as_str());
            let sub_id = obj.get("id").and_then(|v| v.as_str());
            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("active");
            let cancel_at_end = obj
                .get("cancel_at_period_end")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let period_end = obj
                .get("current_period_end")
                .and_then(|v| v.as_i64())
                .map(|ts| {
                    chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default()
                });
            let Some(household_id) = household_for_customer(db, customer).await? else {
                return Ok("no_household_for_customer");
            };
            let plan = if matches!(status, "active" | "trialing") {
                Plan::Plus
            } else {
                Plan::Free
            };
            crate::entitlements::set_subscription(
                db,
                household_id,
                plan,
                status,
                customer,
                sub_id,
                period_end.as_deref(),
                cancel_at_end,
                now_rfc3339,
            )
            .await?;
            Ok("subscription_synced")
        }
        "customer.subscription.deleted" => {
            let obj = data.unwrap_or(&serde_json::Value::Null);
            let customer = obj.get("customer").and_then(|v| v.as_str());
            let sub_id = obj.get("id").and_then(|v| v.as_str());
            let Some(household_id) = household_for_customer(db, customer).await? else {
                return Ok("no_household_for_customer");
            };
            // Downgrade: keep history/export, revoke paid processing only.
            crate::entitlements::set_subscription(
                db,
                household_id,
                Plan::Free,
                "canceled",
                customer,
                sub_id,
                None,
                false,
                now_rfc3339,
            )
            .await?;
            Ok("downgraded")
        }
        "invoice.payment_failed" => {
            let obj = data.unwrap_or(&serde_json::Value::Null);
            let customer = obj.get("customer").and_then(|v| v.as_str());
            let Some(household_id) = household_for_customer(db, customer).await? else {
                return Ok("no_household_for_customer");
            };
            crate::entitlements::set_subscription(
                db,
                household_id,
                Plan::Free,
                "past_due",
                customer,
                None,
                None,
                false,
                now_rfc3339,
            )
            .await?;
            Ok("past_due")
        }
        _ => Ok("ignored"),
    }
}

/// Map a Stripe customer id to the household that started checkout.
/// The mapping row is written at checkout-session creation time.
async fn household_for_customer<C: ConnectionTrait>(
    db: &C,
    customer: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    let Some(customer) = customer else {
        return Ok(None);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT household_id FROM household_subscriptions WHERE stripe_customer_id=?",
            [customer.into()],
        ))
        .await?;
    match row {
        Some(r) => Ok(Some(r.try_get("", "household_id")?)),
        None => Ok(None),
    }
}

/// Record the pending checkout mapping (household -> customer) before
/// redirecting to Stripe. Keeps the webhook able to find the household.
pub async fn remember_checkout_customer<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    customer_id: &str,
    now_rfc3339: &str,
) -> anyhow::Result<()> {
    let plan = crate::entitlements::household_plan(db, household_id)
        .await
        .unwrap_or(Plan::Free);
    crate::entitlements::set_subscription(
        db,
        household_id,
        plan,
        "incomplete",
        Some(customer_id),
        None,
        None,
        false,
        now_rfc3339,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::DeploymentMode, db};

    use crate::config::StripeConfig;

    fn test_config(managed: bool, key: bool) -> Config {
        Config {
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            production: false,
            database_url: String::new(),
            llm_api_key: None,
            llm_base_url: String::new(),
            llm_model: String::new(),
            llm_timeout_seconds: 20,
            mistral_api_key: None,
            blood_tests_dir: String::new(),
            pet_photos_dir: String::new(),
            deployment_mode: if managed {
                DeploymentMode::Managed
            } else {
                DeploymentMode::SelfHosted
            },
            proactive_ai_requested: false,
            stripe: if managed && key {
                Some(StripeConfig {
                    secret_key: "sk_test_x".into(),
                    webhook_secret: "whsec_x".into(),
                })
            } else {
                None
            },
            stripe_publishable_key: None,
            stripe_plus_monthly_price: None,
            stripe_plus_annual_price: None,
            app_base_url: None,
            google_client_id: None,
            google_client_secret: None,
        }
    }

    #[test]
    fn self_hosted_never_offers_checkout() {
        assert!(!checkout_available(&test_config(false, true)));
        assert!(!checkout_available(&test_config(true, false)));
        assert!(checkout_available(&test_config(true, true)));
    }

    #[test]
    fn webhook_rejects_bad_signature() {
        let payload = br#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        // Current timestamp so the tolerance check passes and the HMAC
        // comparison is what fails; a wrong-key signature must not verify.
        let now = chrono::Utc::now().timestamp();
        let header = format!("t={now},v1=deadbeef");
        let err = verify_webhook_signature(payload, &header, "whsec_x", 300).unwrap_err();
        assert!(err.to_string().contains("mismatch"));
    }

    #[test]
    fn webhook_rejects_stale_timestamp() {
        let payload = br#"{"id":"evt_1","type":"x"}"#;
        let err = verify_webhook_signature(payload, "t=1,v1=aa", "whsec_x", 60).unwrap_err();
        assert!(err.to_string().contains("tolerance"));
    }

    #[tokio::test]
    async fn subscription_lifecycle_is_idempotent() {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::migrate(&db).await.unwrap();
        remember_checkout_customer(&db, 1, "cus_1", "2026-01-01T00:00:00Z")
            .await
            .unwrap();
        let event = serde_json::json!({
            "id": "evt_1",
            "type": "checkout.session.completed",
            "data": {"object": {"customer": "cus_1", "subscription": "sub_1"}}
        });
        assert!(!already_processed(&db, "evt_1").await.unwrap());
        let outcome = apply_event(
            &db,
            "checkout.session.completed",
            &event,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(outcome, "plus_granted");
        mark_processed(
            &db,
            "evt_1",
            "checkout.session.completed",
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert!(already_processed(&db, "evt_1").await.unwrap());
        assert_eq!(
            crate::entitlements::household_plan(&db, 1).await.unwrap(),
            Plan::Plus
        );
        // Cancellation downgrades but keeps the row (history intact).
        let cancel = serde_json::json!({
            "id": "evt_2",
            "type": "customer.subscription.deleted",
            "data": {"object": {"id": "sub_1", "customer": "cus_1"}}
        });
        apply_event(
            &db,
            "customer.subscription.deleted",
            &cancel,
            "2026-02-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(
            crate::entitlements::household_plan(&db, 1).await.unwrap(),
            Plan::Free
        );
    }
}
