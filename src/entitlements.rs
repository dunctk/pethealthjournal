//! Server-side household entitlements for the managed hosted service.
//!
//! Design rules (see `pethealthjournal-business/BUSINESS_MODEL.md`):
//! - `DEPLOYMENT_MODE=self_hosted` (the default) bypasses every commercial
//!   check: no Stripe, no licensing server, core records always available.
//! - In `managed` mode, paid features require a **verified per-household**
//!   Plus entitlement persisted from trusted Stripe events. A global env var
//!   never proves a household paid.
//! - Downgrade/cancellation removes paid capabilities only; recorded history
//!   and exports always remain accessible.

use crate::config::DeploymentMode;
use sea_orm::{ConnectionTrait, DbBackend, Statement};

/// Household plan as persisted in `household_subscriptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Plan {
    #[default]
    Free,
    Plus,
}

impl Plan {
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Plus => "plus",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "plus" => Plan::Plus,
            _ => Plan::Free,
        }
    }
}

/// Read the persisted plan for one household. Missing rows mean free.
pub async fn household_plan<C: ConnectionTrait>(db: &C, household_id: i64) -> anyhow::Result<Plan> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT plan, status FROM household_subscriptions WHERE household_id=?",
            [household_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(Plan::Free);
    };
    let plan: String = row.try_get("", "plan")?;
    let status: String = row.try_get("", "status")?;
    // Only an active or trialing subscription counts. Past-due, canceled,
    // incomplete, or unpaid rows fall back to free without deleting history.
    if plan == "plus" && matches!(status.as_str(), "active" | "trialing") {
        Ok(Plan::Plus)
    } else {
        Ok(Plan::Free)
    }
}

/// Upsert the subscription row for a household. Called only from verified
/// Stripe webhook handling or server-side reconciliation — never from
/// client-supplied state.
#[allow(clippy::too_many_arguments)]
pub async fn set_subscription<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    plan: Plan,
    status: &str,
    stripe_customer_id: Option<&str>,
    stripe_subscription_id: Option<&str>,
    current_period_end: Option<&str>,
    cancel_at_period_end: bool,
    now_rfc3339: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO household_subscriptions
           (household_id,plan,stripe_customer_id,stripe_subscription_id,status,current_period_end,cancel_at_period_end,updated_at)
           VALUES(?,?,?,?,?,?,?,?)
           ON CONFLICT(household_id) DO UPDATE SET
             plan=excluded.plan, stripe_customer_id=excluded.stripe_customer_id,
             stripe_subscription_id=excluded.stripe_subscription_id, status=excluded.status,
             current_period_end=excluded.current_period_end,
             cancel_at_period_end=excluded.cancel_at_period_end, updated_at=excluded.updated_at"#,
        [
            household_id.into(),
            plan.as_str().into(),
            stripe_customer_id.into(),
            stripe_subscription_id.into(),
            status.into(),
            current_period_end.into(),
            (i64::from(cancel_at_period_end)).into(),
            now_rfc3339.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Is proactive LLM analysis allowed for this household right now?
///
/// Fail-closed: every condition must hold, otherwise `false` with a
/// machine-readable reason for logging (never shown medical content).
/// - managed deployment AND `PROACTIVE_AI_ENABLED` guard on
/// - verified Plus entitlement for THIS household
/// - explicit opt-in flag (stored per household; see `proactive_opt_in`)
/// - an LLM provider key available (self-hosted BYOK counts when the
///   deployment is self-hosted; managed requires operator configuration)
pub struct ProactiveDecision {
    pub allowed: bool,
    pub reason: &'static str,
}

pub async fn can_run_proactive_ai<C: ConnectionTrait>(
    db: &C,
    mode: DeploymentMode,
    proactive_enabled: bool,
    household_id: i64,
    opt_in: bool,
    provider_key_present: bool,
) -> ProactiveDecision {
    if mode != DeploymentMode::Managed {
        // Self-hosted proactive runs are governed by the BYOK path in
        // billing::proactive_self_hosted_allowed, not here; the managed
        // gate never authorizes anything on a self-hosted install.
        return ProactiveDecision {
            allowed: false,
            reason: "self_hosted_uses_byok_path",
        };
    }
    if !proactive_enabled {
        return ProactiveDecision {
            allowed: false,
            reason: "proactive_guard_disabled",
        };
    }
    if !opt_in {
        return ProactiveDecision {
            allowed: false,
            reason: "no_opt_in",
        };
    }
    if !provider_key_present {
        return ProactiveDecision {
            allowed: false,
            reason: "no_provider_key",
        };
    }
    let plan = household_plan(db, household_id).await.unwrap_or(Plan::Free);
    if plan != Plan::Plus {
        return ProactiveDecision {
            allowed: false,
            reason: "free_tier",
        };
    }
    ProactiveDecision {
        allowed: true,
        reason: "ok",
    }
}

/// Whether a managed household may access a paid-gated capability.
/// Self-hosted always returns true (no commercial gate exists there).
pub async fn plus_or_self_hosted<C: ConnectionTrait>(
    db: &C,
    mode: DeploymentMode,
    household_id: i64,
) -> bool {
    if mode != DeploymentMode::Managed {
        return true;
    }
    matches!(
        household_plan(db, household_id).await.unwrap_or(Plan::Free),
        Plan::Plus
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn mem_db() -> sea_orm::DatabaseConnection {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::migrate(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn missing_row_is_free() {
        let db = mem_db().await;
        assert_eq!(household_plan(&db, 1).await.unwrap(), Plan::Free);
    }

    #[tokio::test]
    async fn active_plus_counts_canceled_does_not() {
        let db = mem_db().await;
        set_subscription(
            &db,
            1,
            Plan::Plus,
            "active",
            Some("cus_1"),
            Some("sub_1"),
            None,
            false,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(household_plan(&db, 1).await.unwrap(), Plan::Plus);
        set_subscription(
            &db,
            1,
            Plan::Free,
            "canceled",
            Some("cus_1"),
            Some("sub_1"),
            None,
            false,
            "2026-02-01T00:00:00Z",
        )
        .await
        .unwrap();
        assert_eq!(household_plan(&db, 1).await.unwrap(), Plan::Free);
    }

    #[tokio::test]
    async fn free_tier_never_runs_proactive() {
        let db = mem_db().await;
        let d = can_run_proactive_ai(&db, DeploymentMode::Managed, true, 1, true, true).await;
        assert!(!d.allowed);
        assert_eq!(d.reason, "free_tier");
    }

    #[tokio::test]
    async fn proactive_fails_closed_without_opt_in_or_guard() {
        let db = mem_db().await;
        set_subscription(
            &db,
            1,
            Plan::Plus,
            "active",
            None,
            None,
            None,
            false,
            "2026-01-01T00:00:00Z",
        )
        .await
        .unwrap();
        let d = can_run_proactive_ai(&db, DeploymentMode::Managed, true, 1, false, true).await;
        assert!(!d.allowed);
        let d = can_run_proactive_ai(&db, DeploymentMode::Managed, false, 1, true, true).await;
        assert!(!d.allowed);
    }

    #[tokio::test]
    async fn self_hosted_bypasses_commercial_gate() {
        let db = mem_db().await;
        assert!(plus_or_self_hosted(&db, DeploymentMode::SelfHosted, 1).await);
    }
}
