use crate::{
    auth,
    domain::{
        ClinicalTimeline, DEFAULT_HOUSEHOLD_ID, HealthEvent, KnowledgeArticle, MedicationAdherence,
        MedicationAdministration, MedicationPlan, MedicationPrescription, Pet, ProposedEvent,
        ShareGrant, SymptomObservation, TemporalLink, TimelineEntry, UserAccount,
        event_presentation, lab_effective_at,
    },
};
use anyhow::{Context, anyhow};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rand::{Rng, distr::Alphanumeric};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult,
    Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, time::Duration as StdDuration};

pub async fn connect(url: &str) -> anyhow::Result<DatabaseConnection> {
    create_parent(url)?;
    let mut options = ConnectOptions::new(url.to_owned());
    options
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(StdDuration::from_secs(10))
        .acquire_timeout(StdDuration::from_secs(10))
        .sqlx_logging(false);
    let database = Database::connect(options).await?;
    for pragma in [
        "PRAGMA journal_mode=WAL",
        "PRAGMA synchronous=FULL",
        "PRAGMA foreign_keys=ON",
        "PRAGMA busy_timeout=5000",
    ] {
        database.execute(stmt(pragma)).await?;
    }
    Ok(database)
}

fn create_parent(url: &str) -> anyhow::Result<()> {
    let path = url
        .strip_prefix("sqlite://")
        .and_then(|value| value.split('?').next())
        .unwrap_or(url);
    if path == ":memory:" || path.is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }
    Ok(())
}

pub async fn migrate(db: &DatabaseConnection) -> anyhow::Result<()> {
    db.execute(stmt(
        r#"
        CREATE TABLE IF NOT EXISTS households (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            username TEXT NOT NULL COLLATE NOCASE UNIQUE,
            email TEXT COLLATE NOCASE UNIQUE,
            display_name TEXT NOT NULL,
            password_hash TEXT NOT NULL,
            role TEXT NOT NULL DEFAULT 'owner' CHECK(role IN ('owner','member')),
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id)
        );
        CREATE INDEX IF NOT EXISTS idx_users_household ON users(household_id, id);
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            revoked_at TEXT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_token_active
            ON sessions(token_hash, expires_at, revoked_at);
        CREATE TABLE IF NOT EXISTS api_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            household_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            last_used_at TEXT,
            revoked_at TEXT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
            FOREIGN KEY (household_id) REFERENCES households(id)
        );
        CREATE INDEX IF NOT EXISTS idx_api_tokens_hash
            ON api_tokens(token_hash, revoked_at);
        CREATE TABLE IF NOT EXISTS oauth_states (
            state_hash TEXT PRIMARY KEY,
            next TEXT,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS pets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            name TEXT NOT NULL COLLATE NOCASE,
            species TEXT NOT NULL,
            breed TEXT,
            date_of_birth TEXT,
            microchip_number TEXT,
            weight_kg REAL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            UNIQUE (household_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_pets_household ON pets(household_id, name);
        CREATE TABLE IF NOT EXISTS health_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            concept TEXT NOT NULL,
            summary TEXT NOT NULL,
            raw_input TEXT NOT NULL DEFAULT '',
            details TEXT,
            occurred_at TEXT NOT NULL,
            recorded_at TEXT NOT NULL,
            temporal_precision TEXT NOT NULL DEFAULT 'exact',
            source TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','undone')),
            undone_at TEXT,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_events_tenant_pet_time
            ON health_events(household_id, pet_id, occurred_at DESC);
        CREATE INDEX IF NOT EXISTS idx_events_tenant_concept
            ON health_events(household_id, concept, occurred_at DESC);
        CREATE TABLE IF NOT EXISTS symptom_observations (
            event_id INTEGER PRIMARY KEY,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            episode_id TEXT NOT NULL,
            symptom TEXT NOT NULL,
            occurrence_count INTEGER,
            amount TEXT,
            contents TEXT,
            meal_relation TEXT,
            water_status TEXT,
            appetite_status TEXT,
            energy_status TEXT,
            pain_status TEXT,
            note TEXT,
            FOREIGN KEY (event_id) REFERENCES health_events(id) ON DELETE CASCADE,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_symptoms_tenant_pet_episode
            ON symptom_observations(household_id, pet_id, symptom, episode_id);
        CREATE TABLE IF NOT EXISTS medication_administrations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            active_ingredient TEXT,
            dose_value REAL,
            dose_unit TEXT,
            route TEXT,
            scheduled_at TEXT,
            administered_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'given',
            raw_input TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_meds_tenant_pet_time
            ON medication_administrations(household_id, pet_id, administered_at DESC);
        CREATE TABLE IF NOT EXISTS medication_prescriptions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            active_ingredient TEXT,
            concentration_value REAL,
            concentration_unit TEXT,
            dose_value REAL,
            dose_unit TEXT,
            frequency TEXT,
            route TEXT,
            instructions TEXT,
            started_on TEXT,
            ended_on TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            raw_input TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_prescriptions_tenant_pet_status
            ON medication_prescriptions(household_id, pet_id, status, created_at DESC);
        CREATE TABLE IF NOT EXISTS medication_plan_confirmations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            token_hash TEXT NOT NULL,
            prescription_id INTEGER NOT NULL,
            event_id INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            FOREIGN KEY (prescription_id) REFERENCES medication_prescriptions(id),
            FOREIGN KEY (event_id) REFERENCES health_events(id),
            UNIQUE (household_id, token_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_plan_confirmations_tenant_pet
            ON medication_plan_confirmations(household_id, pet_id, created_at DESC);
        CREATE TABLE IF NOT EXISTS medication_stop_confirmations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            token_hash TEXT NOT NULL,
            event_id INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            FOREIGN KEY (event_id) REFERENCES health_events(id),
            UNIQUE (household_id, token_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_stop_confirmations_tenant_pet
            ON medication_stop_confirmations(household_id, pet_id, created_at DESC);
        CREATE TABLE IF NOT EXISTS medication_adherence (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            prescription_id INTEGER NOT NULL,
            scheduled_for TEXT NOT NULL,
            expected_dose_value REAL,
            expected_dose_unit TEXT,
            actual_dose_value REAL,
            actual_dose_unit TEXT,
            status TEXT NOT NULL,
            reason TEXT,
            raw_input TEXT,
            recorded_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            FOREIGN KEY (prescription_id) REFERENCES medication_prescriptions(id)
        );
        CREATE INDEX IF NOT EXISTS idx_adherence_tenant_pet_date
            ON medication_adherence(household_id, pet_id, scheduled_for DESC);
        CREATE TABLE IF NOT EXISTS audit_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            record_type TEXT NOT NULL,
            record_id INTEGER NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audit_tenant_time
            ON audit_events(household_id, created_at DESC);
        CREATE TABLE IF NOT EXISTS knowledge_articles (
            concept TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            summary TEXT NOT NULL,
            monitoring TEXT NOT NULL,
            urgent_signs TEXT NOT NULL,
            source_url TEXT
        );
        CREATE TABLE IF NOT EXISTS share_grants (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            label TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            permission TEXT NOT NULL DEFAULT 'read',
            expires_at TEXT NOT NULL,
            revoked_at TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_shares_tenant_pet
            ON share_grants(household_id, pet_id, expires_at DESC);
        CREATE TABLE IF NOT EXISTS weight_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            weight_kg REAL NOT NULL,
            measured_at TEXT NOT NULL,
            note TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_weights_tenant_pet_time
            ON weight_entries(household_id, pet_id, measured_at DESC);
        CREATE TABLE IF NOT EXISTS pet_passports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL REFERENCES households(id),
            pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
            label TEXT,
            filename TEXT NOT NULL,
            mime_type TEXT NOT NULL,
            storage_path TEXT NOT NULL UNIQUE,
            uploaded_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_passports_household_pet
            ON pet_passports(household_id, pet_id, uploaded_at DESC);
        CREATE TABLE IF NOT EXISTS pet_photos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            storage_name TEXT NOT NULL,
            original_filename TEXT NOT NULL,
            mime_type TEXT NOT NULL,
            captured_at TEXT NOT NULL,
            uploaded_at TEXT NOT NULL,
            left_eye_x REAL,
            left_eye_y REAL,
            right_eye_x REAL,
            right_eye_y REAL,
            alignment_status TEXT NOT NULL DEFAULT 'pending',
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            UNIQUE (household_id, storage_name)
        );
        CREATE INDEX IF NOT EXISTS idx_pet_photos_tenant_pet_time
            ON pet_photos(household_id, pet_id, captured_at ASC);
        CREATE TABLE IF NOT EXISTS lab_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            source_filename TEXT NOT NULL,
            document_hash TEXT NOT NULL,
            raw_text TEXT NOT NULL,
            test_date TEXT,
            imported_at TEXT NOT NULL,
            parse_status TEXT NOT NULL DEFAULT 'needs_review',
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            UNIQUE (household_id, document_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_lab_reports_tenant_pet
            ON lab_reports(household_id, pet_id, test_date DESC);
        CREATE TABLE IF NOT EXISTS lab_results (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            report_id INTEGER NOT NULL,
            test_name TEXT NOT NULL,
            value_text TEXT NOT NULL,
            value_numeric REAL,
            unit TEXT,
            reference_range TEXT,
            flag TEXT,
            FOREIGN KEY (report_id) REFERENCES lab_reports(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_lab_results_report ON lab_results(report_id, test_name);
        CREATE TABLE IF NOT EXISTS oauth_clients (
            client_id TEXT PRIMARY KEY,
            client_name TEXT NOT NULL,
            redirect_uris TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS oauth_codes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            code_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            redirect_uri TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_codes_active ON oauth_codes(code_hash, expires_at, used_at);
        CREATE TABLE IF NOT EXISTS oauth_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            access_token_hash TEXT NOT NULL UNIQUE,
            refresh_token_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            access_expires_at TEXT NOT NULL,
            refresh_expires_at TEXT NOT NULL,
            revoked_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_access_active ON oauth_tokens(access_token_hash, access_expires_at, revoked_at);
        CREATE INDEX IF NOT EXISTS idx_oauth_refresh_active ON oauth_tokens(refresh_token_hash, refresh_expires_at, revoked_at);
        CREATE TABLE IF NOT EXISTS oauth_device_codes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            device_code_hash TEXT NOT NULL UNIQUE,
            user_code_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            user_id INTEGER,
            approved_at TEXT,
            consumed_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_device_active
            ON oauth_device_codes(device_code_hash, client_id, expires_at, consumed_at);
        CREATE INDEX IF NOT EXISTS idx_oauth_user_code_active
            ON oauth_device_codes(user_code_hash, expires_at, consumed_at);
        "#,))
    .await?;
    db.execute(stmt(crate::insights::MIGRATION_SQL)).await?;
    db.execute(stmt(crate::clinical_cases::MIGRATION_SQL))
        .await?;
    db.execute(stmt(
        r#"
        CREATE TABLE IF NOT EXISTS household_subscriptions (
            household_id INTEGER PRIMARY KEY,
            plan TEXT NOT NULL DEFAULT 'free' CHECK(plan IN ('free','plus')),
            stripe_customer_id TEXT,
            stripe_subscription_id TEXT,
            status TEXT NOT NULL DEFAULT 'none',
            current_period_end TEXT,
            cancel_at_period_end INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id)
        );
        CREATE TABLE IF NOT EXISTS stripe_events (
            event_id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            received_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS mcp_tool_calls (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            ok INTEGER NOT NULL,
            error_code INTEGER,
            duration_ms INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mcp_tool_calls_time ON mcp_tool_calls(created_at DESC);
        CREATE TABLE IF NOT EXISTS household_invites (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL REFERENCES households(id),
            invited_by INTEGER NOT NULL REFERENCES users(id),
            email TEXT NOT NULL COLLATE NOCASE,
            token_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            accepted_at TEXT,
            revoked_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_household_invites_pending
            ON household_invites(household_id, email, expires_at);
        CREATE TABLE IF NOT EXISTS login_codes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL COLLATE NOCASE,
            challenge_hash TEXT NOT NULL UNIQUE,
            code_hash TEXT NOT NULL,
            next TEXT,
            client_ip TEXT,
            attempts INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            consumed_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_login_codes_email ON login_codes(email, created_at);
        CREATE INDEX IF NOT EXISTS idx_login_codes_ip ON login_codes(client_ip, created_at);
        "#,
    ))
    .await?;
    ensure_google_sub_column(db).await?;
    ensure_column(db, "lab_reports", "original_file", "TEXT").await?;
    ensure_column(db, "pets", "microchip_number", "TEXT").await?;
    db.execute(stmt(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_pets_household_microchip ON pets(household_id, microchip_number) WHERE microchip_number IS NOT NULL",
    ))
    .await?;

    let now = Utc::now().to_rfc3339();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT OR IGNORE INTO households(id, name, created_at) VALUES (?, ?, ?)",
        [
            DEFAULT_HOUSEHOLD_ID.into(),
            "My household".into(),
            now.into(),
        ],
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT OR IGNORE INTO knowledge_articles
           (concept,title,summary,monitoring,urgent_signs,source_url)
           VALUES (?,?,?,?,?,?)"#,
        [
            "vomiting".into(),
            "Vomiting".into(),
            "A useful record includes frequency, appearance, food or medication changes, and whether your pet can keep water down.".into(),
            "Record repeat episodes, appetite, drinking, energy, stool, and any suspected material eaten.".into(),
            "Repeated vomiting, blood, severe lethargy, pain, breathing difficulty, collapse, or inability to keep water down warrants prompt veterinary advice.".into(),
            "https://www.vet.cornell.edu/departments-centers-and-institutes/riney-canine-health-center/canine-health-information/vomiting".into(),
        ],
    )).await?;
    Ok(())
}

pub async fn bootstrap_owner(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let exists = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM users WHERE username=? COLLATE NOCASE",
            [username.into()],
        ))
        .await?
        .is_some();
    if exists {
        return Ok(());
    }
    let password_hash = auth::hash_password(password.to_owned()).await?;
    let now = Utc::now().to_rfc3339();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO users(household_id,username,email,display_name,password_hash,role,created_at)
           VALUES(?,?,?,?,?,'owner',?)"#,
        [
            DEFAULT_HOUSEHOLD_ID.into(),
            username.into(),
            Option::<String>::None.into(),
            "Owner".into(),
            password_hash.into(),
            now.into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn create_account(
    db: &DatabaseConnection,
    email: &str,
    display_name: &str,
    password_hash: &str,
) -> anyhow::Result<UserAccount> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let household_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO households(name,created_at) VALUES(?,?) RETURNING id",
            [
                format!("{display_name}'s household").into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("household insert returned no id"))?;
    let household_id: i64 = household_row.try_get("", "id")?;
    let user_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO users(household_id,username,email,display_name,password_hash,role,created_at)
               VALUES(?,?,?,?,?,'owner',?) RETURNING id"#,
            [
                household_id.into(),
                email.into(),
                email.into(),
                display_name.into(),
                password_hash.into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("user insert returned no id"))?;
    let id: i64 = user_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        &format!("user:{id}"),
        "account.created",
        "user",
        id,
        "Owner account created",
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(UserAccount {
        id,
        household_id,
        username: email.into(),
        email: Some(email.into()),
        display_name: display_name.into(),
        initials: initials(display_name),
    })
}

pub async fn user_for_login(
    db: &DatabaseConnection,
    identifier: &str,
) -> anyhow::Result<Option<(UserAccount, String)>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT id,household_id,username,email,display_name,password_hash FROM users
           WHERE username=? COLLATE NOCASE OR email=? COLLATE NOCASE LIMIT 1"#,
        [identifier.into(), identifier.into()],
    ))
    .await?
    .map(user_with_password_from_row)
    .transpose()
}

pub async fn create_session(db: &DatabaseConnection, user_id: i64) -> anyhow::Result<String> {
    let token = auth::new_session_token();
    let token_hash = auth::token_hash(&token);
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO sessions(user_id,token_hash,created_at,expires_at) VALUES(?,?,?,?)",
        [
            user_id.into(),
            token_hash.into(),
            now.to_rfc3339().into(),
            (now + Duration::days(auth::SESSION_DAYS))
                .to_rfc3339()
                .into(),
        ],
    ))
    .await?;
    Ok(token)
}

pub async fn resolve_session(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<UserAccount>> {
    let token_hash = auth::token_hash(token);
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT u.id,u.household_id,u.username,u.email,u.display_name
           FROM sessions s JOIN users u ON u.id=s.user_id
           WHERE s.token_hash=? AND s.revoked_at IS NULL AND s.expires_at>?"#,
        [token_hash.into(), Utc::now().to_rfc3339().into()],
    ))
    .await?
    .map(user_from_row)
    .transpose()
}

pub async fn revoke_session(db: &DatabaseConnection, token: &str) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE sessions SET revoked_at=? WHERE token_hash=? AND revoked_at IS NULL",
        [
            Utc::now().to_rfc3339().into(),
            auth::token_hash(token).into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Adds `table.column` if missing. SQLite lacks `ADD COLUMN IF NOT EXISTS`.
/// Only called with compile-time identifiers, never user input.
async fn ensure_column(
    db: &DatabaseConnection,
    table: &'static str,
    column: &'static str,
    ddl_type: &'static str,
) -> anyhow::Result<()> {
    let cols = db
        .query_all(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA table_info({table})"),
        ))
        .await?;
    let has = cols.iter().any(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == column)
    });
    if !has {
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            format!("ALTER TABLE {table} ADD COLUMN {column} {ddl_type}"),
        ))
        .await?;
    }
    Ok(())
}

/// Records one MCP tool call for observability: tool name, outcome and
/// latency only. Arguments and results are never stored (health data).
pub async fn log_tool_call(
    db: &DatabaseConnection,
    household_id: i64,
    tool: &str,
    error_code: Option<i32>,
    duration_ms: i64,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO mcp_tool_calls(household_id,tool,ok,error_code,duration_ms,created_at) VALUES(?,?,?,?,?,?)",
        [
            household_id.into(),
            tool.into(),
            (error_code.is_none() as i32).into(),
            error_code.into(),
            duration_ms.into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Attach the stored original report file (relative path under the
/// household's blood-test directory) to a lab report.
pub async fn set_lab_report_original(
    db: &DatabaseConnection,
    household_id: i64,
    report_id: i64,
    relative_path: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE lab_reports SET original_file=? WHERE household_id=? AND id=?",
        [relative_path.into(), household_id.into(), report_id.into()],
    ))
    .await?;
    Ok(())
}

/// All lab report ids in a household (for existence checks).
pub async fn list_lab_report_ids(
    db: &DatabaseConnection,
    household_id: i64,
) -> anyhow::Result<Vec<i64>> {
    db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id FROM lab_reports WHERE household_id=?",
        [household_id.into()],
    ))
    .await?
    .into_iter()
    .map(|row| row.try_get::<i64>("", "id").map_err(Into::into))
    .collect()
}

/// The stored original file for one lab report, household-scoped.
pub async fn lab_report_original(
    db: &DatabaseConnection,
    household_id: i64,
    report_id: i64,
) -> anyhow::Result<Option<String>> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT original_file FROM lab_reports WHERE household_id=? AND id=? AND original_file IS NOT NULL",
            [household_id.into(), report_id.into()],
        ))
        .await?
        .map(|row| row.try_get::<String>("", "original_file"))
        .transpose()?)
}

/// Idempotent `users.google_sub` column for Google OAuth linking. SQLite
/// has no `ADD COLUMN IF NOT EXISTS`, so inspect first; safe to run on
/// every boot and on databases created before this column existed.
async fn ensure_google_sub_column(db: &DatabaseConnection) -> anyhow::Result<()> {
    let cols = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "PRAGMA table_info(users)",
            [],
        ))
        .await?;
    let has = cols.iter().any(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|name| name == "google_sub")
    });
    if !has {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "ALTER TABLE users ADD COLUMN google_sub TEXT",
            [],
        ))
        .await?;
    }
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_google_sub ON users(google_sub)",
        [],
    ))
    .await?;
    Ok(())
}

/// Personal MCP tokens: long-lived revocable bearer credentials a household
/// member mints for their own AI assistant. Stored as SHA-256 hashes only;
/// the plaintext is shown once at creation.
pub async fn create_api_token(
    db: &DatabaseConnection,
    user_id: i64,
    household_id: i64,
    name: &str,
) -> anyhow::Result<(i64, String)> {
    use rand::{Rng, distr::Alphanumeric};
    let secret: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let token = format!("phj_{secret}");
    let now = Utc::now().to_rfc3339();
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO api_tokens(user_id,household_id,name,token_hash,created_at) VALUES(?,?,?,?,?) RETURNING id",
            [
                user_id.into(),
                household_id.into(),
                name.into(),
                auth::token_hash(&token).into(),
                now.into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("api token insert returned no id"))?;
    Ok((row.try_get("", "id")?, token))
}

pub async fn resolve_api_token(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<UserAccount>> {
    if !token.starts_with("phj_") {
        return Ok(None);
    }
    let user = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT u.id,u.household_id,u.username,u.email,u.display_name
           FROM api_tokens t JOIN users u ON u.id=t.user_id
           WHERE t.token_hash=? AND t.revoked_at IS NULL"#,
            [auth::token_hash(token).into()],
        ))
        .await?
        .map(user_from_row)
        .transpose()?;
    if user.is_some() {
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE api_tokens SET last_used_at=? WHERE token_hash=?",
            [
                Utc::now().to_rfc3339().into(),
                auth::token_hash(token).into(),
            ],
        ))
        .await?;
    }
    Ok(user)
}

pub async fn list_api_tokens(
    db: &DatabaseConnection,
    user_id: i64,
) -> anyhow::Result<Vec<crate::domain::ApiTokenInfo>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,name,created_at,last_used_at FROM api_tokens WHERE user_id=? AND revoked_at IS NULL ORDER BY created_at DESC",
            [user_id.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(crate::domain::ApiTokenInfo {
                id: row.try_get("", "id")?,
                name: row.try_get("", "name")?,
                created_at: row.try_get("", "created_at")?,
                last_used_at: row.try_get("", "last_used_at")?,
            })
        })
        .collect()
}

pub async fn revoke_api_token(
    db: &DatabaseConnection,
    household_id: i64,
    user_id: i64,
    id: i64,
) -> anyhow::Result<bool> {
    let res = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE api_tokens SET revoked_at=? WHERE id=? AND user_id=? AND household_id=? AND revoked_at IS NULL",
            [
                Utc::now().to_rfc3339().into(),
                id.into(),
                user_id.into(),
                household_id.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Short-lived OAuth `state` values (Google login). Single-use: consuming
/// deletes the row, so replays fail closed.
pub async fn create_oauth_state(
    db: &DatabaseConnection,
    state: &str,
    next: Option<&str>,
    ttl_minutes: i64,
) -> anyhow::Result<()> {
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_states(state_hash,next,created_at,expires_at) VALUES(?,?,?,?)",
        [
            auth::token_hash(state).into(),
            next.into(),
            now.to_rfc3339().into(),
            (now + Duration::minutes(ttl_minutes)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn consume_oauth_state(
    db: &DatabaseConnection,
    state: &str,
) -> anyhow::Result<Option<String>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM oauth_states WHERE state_hash=? AND expires_at>? RETURNING next",
            [
                auth::token_hash(state).into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
    match row {
        None => Ok(None),
        Some(row) => Ok(row.try_get("", "next")?),
    }
}

/// Wrong guesses allowed per emailed code before it stops working.
pub const LOGIN_CODE_MAX_ATTEMPTS: i64 = 5;

/// How many codes were issued recently: for this email, for this client IP,
/// and in total, plus when the newest code for this email was created.
pub struct LoginCodeVolume {
    pub for_email: i64,
    pub for_ip: i64,
    pub total: i64,
    pub latest_for_email: Option<DateTime<Utc>>,
}

pub async fn login_code_volume(
    db: &DatabaseConnection,
    email: &str,
    client_ip: Option<&str>,
    since: DateTime<Utc>,
) -> anyhow::Result<LoginCodeVolume> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT
                 COALESCE(SUM(CASE WHEN email=? COLLATE NOCASE THEN 1 ELSE 0 END),0) AS for_email,
                 COALESCE(SUM(CASE WHEN client_ip IS NOT NULL AND client_ip=? THEN 1 ELSE 0 END),0) AS for_ip,
                 COUNT(*) AS total,
                 MAX(CASE WHEN email=? COLLATE NOCASE THEN created_at END) AS latest_for_email
               FROM login_codes WHERE created_at>?"#,
            [
                email.into(),
                client_ip.unwrap_or("").into(),
                email.into(),
                since.to_rfc3339().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("login code volume query returned no row"))?;
    let latest: Option<String> = row.try_get("", "latest_for_email")?;
    Ok(LoginCodeVolume {
        for_email: row.try_get("", "for_email")?,
        for_ip: row.try_get("", "for_ip")?,
        total: row.try_get("", "total")?,
        latest_for_email: latest
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc)),
    })
}

fn login_code_hash(challenge: &str, code: &str) -> String {
    auth::token_hash(&format!("{challenge}:{code}"))
}

/// Store a new emailed code. Earlier unused codes for the same email stop
/// working, so only the most recent email is valid. Rows older than a day
/// are purged here, which keeps the table small without a background job.
pub async fn create_login_code(
    db: &DatabaseConnection,
    email: &str,
    challenge: &str,
    code: &str,
    next: Option<&str>,
    client_ip: Option<&str>,
    ttl_minutes: i64,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let transaction = db.begin().await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM login_codes WHERE created_at<?",
            [(now - Duration::days(1)).to_rfc3339().into()],
        ))
        .await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE login_codes SET consumed_at=? WHERE email=? COLLATE NOCASE AND consumed_at IS NULL",
            [now.to_rfc3339().into(), email.into()],
        ))
        .await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO login_codes(email,challenge_hash,code_hash,next,client_ip,created_at,expires_at)
               VALUES(?,?,?,?,?,?,?)"#,
            [
                email.into(),
                auth::token_hash(challenge).into(),
                login_code_hash(challenge, code).into(),
                next.into(),
                client_ip.into(),
                now.to_rfc3339().into(),
                (now + Duration::minutes(ttl_minutes)).to_rfc3339().into(),
            ],
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginCodeCheck {
    Valid {
        email: String,
        next: Option<String>,
    },
    Wrong {
        attempts_left: i64,
    },
    /// Expired, already used, replaced by a newer code, out of attempts,
    /// or an unknown challenge.
    Unusable,
}

/// Check a code against its challenge. A correct code is consumed so it
/// works exactly once; a wrong one uses up an attempt.
pub async fn check_login_code(
    db: &DatabaseConnection,
    challenge: &str,
    code: &str,
) -> anyhow::Result<LoginCodeCheck> {
    use subtle::ConstantTimeEq;
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT id,email,code_hash,next,attempts FROM login_codes
               WHERE challenge_hash=? AND consumed_at IS NULL AND expires_at>?"#,
            [auth::token_hash(challenge).into(), now.clone().into()],
        ))
        .await?
    else {
        return Ok(LoginCodeCheck::Unusable);
    };
    let id: i64 = row.try_get("", "id")?;
    let attempts: i64 = row.try_get("", "attempts")?;
    if attempts >= LOGIN_CODE_MAX_ATTEMPTS {
        return Ok(LoginCodeCheck::Unusable);
    }
    let stored: String = row.try_get("", "code_hash")?;
    let offered = login_code_hash(challenge, code);
    if bool::from(stored.as_bytes().ct_eq(offered.as_bytes())) {
        transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE login_codes SET consumed_at=? WHERE id=?",
                [now.into(), id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(LoginCodeCheck::Valid {
            email: row.try_get("", "email")?,
            next: row.try_get("", "next")?,
        });
    }
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE login_codes SET attempts=attempts+1 WHERE id=?",
            [id.into()],
        ))
        .await?;
    transaction.commit().await?;
    let attempts_left = LOGIN_CODE_MAX_ATTEMPTS - attempts - 1;
    if attempts_left <= 0 {
        Ok(LoginCodeCheck::Unusable)
    } else {
        Ok(LoginCodeCheck::Wrong { attempts_left })
    }
}

/// Sign-in by a verified email: the existing account with that email, or a
/// new household. New accounts get an unusable random password; they sign
/// in with emailed codes (or Google) instead.
pub async fn find_or_create_email_user(
    db: &DatabaseConnection,
    email: &str,
) -> anyhow::Result<(UserAccount, bool)> {
    if let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,household_id,username,email,display_name FROM users WHERE email=? COLLATE NOCASE OR username=? COLLATE NOCASE LIMIT 1",
            [email.into(), email.into()],
        ))
        .await?
    {
        return Ok((user_from_row(row)?, false));
    }
    let random_password: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let password_hash = auth::hash_password(random_password).await?;
    let display_name = email.split('@').next().unwrap_or("Pet owner");
    Ok((
        create_account(db, email, display_name, &password_hash).await?,
        true,
    ))
}

/// Link-or-create for Google OAuth: existing `google_sub` wins; else a
/// Google-verified email matching an existing account links the sub; else a
/// new household is created. Callers must only pass verified emails.
pub async fn find_or_create_google_user(
    db: &DatabaseConnection,
    google_sub: &str,
    email: &str,
    display_name: &str,
) -> anyhow::Result<UserAccount> {
    if let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,household_id,username,email,display_name FROM users WHERE google_sub=?",
            [google_sub.into()],
        ))
        .await?
    {
        return user_from_row(row);
    }
    if let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,household_id,username,email,display_name FROM users WHERE email=? COLLATE NOCASE",
            [email.into()],
        ))
        .await?
    {
        let user = user_from_row(row)?;
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE users SET google_sub=? WHERE id=?",
            [google_sub.into(), user.id.into()],
        ))
        .await?;
        return Ok(user);
    }
    use rand::{Rng, distr::Alphanumeric};
    let random_password: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let password_hash = auth::hash_password(random_password).await?;
    let user = create_account(db, email, display_name, &password_hash).await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE users SET google_sub=? WHERE id=?",
        [google_sub.into(), user.id.into()],
    ))
    .await?;
    Ok(user)
}

pub async fn create_oauth_client(
    db: &DatabaseConnection,
    client_id: &str,
    client_name: &str,
    redirect_uris: &[String],
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_clients(client_id,client_name,redirect_uris,created_at) VALUES(?,?,?,?)",
        [
            client_id.into(),
            client_name.into(),
            serde_json::to_string(redirect_uris)?.into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn oauth_client_redirects(
    db: &DatabaseConnection,
    client_id: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT redirect_uris FROM oauth_clients WHERE client_id=?",
            [client_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(
        &row.try_get::<String>("", "redirect_uris")?,
    )?))
}

pub async fn create_oauth_code(
    db: &DatabaseConnection,
    code_hash: &str,
    client_id: &str,
    user_id: i64,
    redirect_uri: &str,
    code_challenge: &str,
) -> anyhow::Result<()> {
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_codes(code_hash,client_id,user_id,redirect_uri,code_challenge,expires_at) VALUES(?,?,?,?,?,?)",
        [
            code_hash.into(),
            client_id.into(),
            user_id.into(),
            redirect_uri.into(),
            code_challenge.into(),
            (now + Duration::minutes(5)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn redeem_oauth_code(
    db: &DatabaseConnection,
    code_hash: &str,
    client_id: &str,
    redirect_uri: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    let transaction = db.begin().await?;
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,user_id,redirect_uri,code_challenge FROM oauth_codes WHERE code_hash=? AND client_id=? AND expires_at>? AND used_at IS NULL",
            [
                code_hash.into(),
                client_id.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?
    else {
        return Ok(None);
    };
    let stored_redirect: String = row.try_get("", "redirect_uri")?;
    if stored_redirect != redirect_uri {
        return Ok(None);
    }
    let id: i64 = row.try_get("", "id")?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_codes SET used_at=? WHERE id=? AND used_at IS NULL",
            [Utc::now().to_rfc3339().into(), id.into()],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Ok(None);
    }
    transaction.commit().await?;
    Ok(Some((
        row.try_get("", "user_id")?,
        row.try_get("", "code_challenge")?,
    )))
}

pub async fn create_oauth_tokens(
    db: &DatabaseConnection,
    client_id: &str,
    user_id: i64,
) -> anyhow::Result<(String, String, i64)> {
    let access = auth::new_session_token();
    let refresh = auth::new_session_token();
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_tokens(access_token_hash,refresh_token_hash,client_id,user_id,access_expires_at,refresh_expires_at) VALUES(?,?,?,?,?,?)",
        [
            auth::token_hash(&access).into(),
            auth::token_hash(&refresh).into(),
            client_id.into(),
            user_id.into(),
            (now + Duration::hours(1)).to_rfc3339().into(),
            (now + Duration::days(30)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok((access, refresh, 3600))
}

pub async fn resolve_oauth_access_token(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<UserAccount>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT u.id,u.household_id,u.username,u.email,u.display_name FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.access_token_hash=? AND t.access_expires_at>? AND t.revoked_at IS NULL",
        [
            auth::token_hash(token).into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?
    .map(user_from_row)
    .transpose()
}

pub async fn refresh_oauth_tokens(
    db: &DatabaseConnection,
    refresh_token: &str,
    client_id: &str,
) -> anyhow::Result<Option<(String, String, i64)>> {
    let hash = auth::token_hash(refresh_token);
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT user_id FROM oauth_tokens WHERE refresh_token_hash=? AND client_id=? AND refresh_expires_at>? AND revoked_at IS NULL",
            [hash.clone().into(), client_id.into(), Utc::now().to_rfc3339().into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE oauth_tokens SET revoked_at=? WHERE refresh_token_hash=?",
        [Utc::now().to_rfc3339().into(), hash.into()],
    ))
    .await?;
    Ok(Some(
        create_oauth_tokens(db, client_id, row.try_get("", "user_id")?).await?,
    ))
}

pub async fn create_oauth_device_code(
    db: &DatabaseConnection,
    device_code_hash: &str,
    user_code_hash: &str,
    client_id: &str,
    code_challenge: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_device_codes(device_code_hash,user_code_hash,client_id,code_challenge,expires_at) VALUES(?,?,?,?,?)",
        [
            device_code_hash.into(),
            user_code_hash.into(),
            client_id.into(),
            code_challenge.into(),
            (Utc::now() + Duration::minutes(10)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn oauth_device_code_state(
    db: &DatabaseConnection,
    device_code_hash: &str,
    client_id: &str,
) -> anyhow::Result<Option<(Option<i64>, String)>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT user_id,code_challenge FROM oauth_device_codes WHERE device_code_hash=? AND client_id=? AND expires_at>? AND consumed_at IS NULL",
        [
            device_code_hash.into(),
            client_id.into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?
    .map(|row| {
        Ok((
            row.try_get("", "user_id")?,
            row.try_get("", "code_challenge")?,
        ))
    })
    .transpose()
}

pub async fn oauth_device_user_code_exists(
    db: &DatabaseConnection,
    user_code_hash: &str,
) -> anyhow::Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM oauth_device_codes WHERE user_code_hash=? AND expires_at>? AND consumed_at IS NULL",
            [user_code_hash.into(), Utc::now().to_rfc3339().into()],
        ))
        .await?
        .is_some())
}

pub async fn approve_oauth_device_code(
    db: &DatabaseConnection,
    user_code_hash: &str,
    user_id: i64,
) -> anyhow::Result<bool> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_device_codes SET user_id=?,approved_at=? WHERE user_code_hash=? AND expires_at>? AND approved_at IS NULL AND consumed_at IS NULL",
            [
                user_id.into(),
                Utc::now().to_rfc3339().into(),
                user_code_hash.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn consume_oauth_device_code(
    db: &DatabaseConnection,
    device_code_hash: &str,
    client_id: &str,
    user_id: i64,
) -> anyhow::Result<bool> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_device_codes SET consumed_at=? WHERE device_code_hash=? AND client_id=? AND user_id=? AND expires_at>? AND consumed_at IS NULL",
            [
                Utc::now().to_rfc3339().into(),
                device_code_hash.into(),
                client_id.into(),
                user_id.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn update_password_and_revoke_sessions(
    db: &DatabaseConnection,
    user: &UserAccount,
    password_hash: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE users SET password_hash=? WHERE id=? AND household_id=?",
            [
                password_hash.into(),
                user.id.into(),
                user.household_id.into(),
            ],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!("account was not found in its household"));
    }
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE sessions SET revoked_at=? WHERE user_id=? AND revoked_at IS NULL",
            [now.clone().into(), user.id.into()],
        ))
        .await?;
    audit(
        &transaction,
        user.household_id,
        &user.audit_actor(),
        "account.password_changed",
        "user",
        user.id,
        "Password changed and sessions revoked",
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn list_pets(db: &DatabaseConnection, household_id: i64) -> anyhow::Result<Vec<Pet>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,microchip_number,weight_kg FROM pets WHERE household_id=? ORDER BY name",
        [household_id.into()],
    )).await?;
    rows.into_iter().map(pet_from_row).collect()
}

pub async fn get_pet(
    db: &DatabaseConnection,
    household_id: i64,
    id: i64,
) -> anyhow::Result<Option<Pet>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,microchip_number,weight_kg FROM pets WHERE household_id=? AND id=?",
        [household_id.into(), id.into()],
    )).await?.map(pet_from_row).transpose()
}

pub async fn find_pet_by_name(
    db: &DatabaseConnection,
    household_id: i64,
    name: &str,
) -> anyhow::Result<Option<Pet>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,microchip_number,weight_kg FROM pets WHERE household_id=? AND name=? COLLATE NOCASE",
        [household_id.into(), name.into()],
    )).await?.map(pet_from_row).transpose()
}

pub async fn create_weight(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    weight_kg: f64,
    measured_at: &str,
    note: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO weight_entries(household_id,pet_id,weight_kg,measured_at,note,created_at) VALUES(?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), weight_kg.into(), measured_at.into(), note.into(), now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("weight insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "weight.created",
        "weight_entry",
        id,
        &format!("{weight_kg:.2} kg"),
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn list_weights(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::WeightEntry>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,pet_id,weight_kg,measured_at,note FROM weight_entries WHERE household_id=? AND pet_id=? ORDER BY measured_at DESC LIMIT 100",
        [household_id.into(), pet_id.into()],
    )).await?;
    rows.into_iter().map(weight_from_row).collect()
}

/// Owner-confirmed edit of a pet's profile fields. `None` leaves a field
/// unchanged; `Some("")` clears an optional field. Returns false if the pet is
/// not in the household. Name uniqueness is enforced by the schema.
#[allow(clippy::too_many_arguments)]
pub async fn update_pet(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    name: Option<&str>,
    species: Option<&str>,
    breed: Option<&str>,
    date_of_birth: Option<&str>,
    microchip_number: Option<&str>,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE pets SET
                 name=COALESCE(?,name),
                 species=COALESCE(?,species),
                 breed=CASE WHEN ? IS NULL THEN breed ELSE NULLIF(?,'') END,
                 date_of_birth=CASE WHEN ? IS NULL THEN date_of_birth ELSE NULLIF(?,'') END,
                 microchip_number=CASE WHEN ? IS NULL THEN microchip_number ELSE NULLIF(?,'') END,
                 updated_at=?
               WHERE household_id=? AND id=?"#,
            [
                name.into(),
                species.into(),
                breed.into(),
                breed.into(),
                date_of_birth.into(),
                date_of_birth.into(),
                microchip_number.into(),
                microchip_number.into(),
                now.clone().into(),
                household_id.into(),
                pet_id.into(),
            ],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Ok(false);
    }
    let detail = serde_json::json!({
        "name": name, "species": species, "breed": breed, "date_of_birth": date_of_birth,
        "microchip_changed": microchip_number.is_some()
    })
    .to_string();
    audit(
        &transaction,
        household_id,
        actor,
        "pet.updated",
        "pet",
        pet_id,
        &detail,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(true)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DeletedPet {
    pub pet_id: i64,
    pub name: String,
    /// Rows removed per table, for the confirmation message and audit.
    pub removed: std::collections::BTreeMap<String, u64>,
    /// Stored blood-test source filenames, so the caller can remove the files
    /// and they are not re-imported for another pet.
    #[serde(skip)]
    pub lab_source_files: Vec<String>,
}

/// Permanently deletes one pet and every household-scoped record about it in
/// a single transaction, with an audit row. Returns `None` if not found.
pub async fn delete_pet(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
) -> anyhow::Result<Option<DeletedPet>> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let Some(pet_row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT name FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let name: String = pet_row.try_get("", "name")?;
    let lab_source_files = transaction
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT source_filename AS f FROM lab_reports WHERE household_id=? AND pet_id=? UNION ALL SELECT original_file AS f FROM lab_reports WHERE household_id=? AND pet_id=? AND original_file IS NOT NULL UNION ALL SELECT storage_path AS f FROM pet_passports WHERE household_id=? AND pet_id=?",
            [household_id.into(), pet_id.into(), household_id.into(), pet_id.into(), household_id.into(), pet_id.into()],
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "f"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut removed = std::collections::BTreeMap::new();
    // Children before parents so foreign keys hold even with enforcement on.
    let steps: [(&str, &str); 14] = [
        (
            "clinical_cases",
            "DELETE FROM clinical_cases WHERE household_id=? AND pet_id=?",
        ),
        (
            "pet_passports",
            "DELETE FROM pet_passports WHERE household_id=? AND pet_id=?",
        ),
        (
            "lab_results",
            "DELETE FROM lab_results WHERE report_id IN (SELECT id FROM lab_reports WHERE household_id=? AND pet_id=?)",
        ),
        (
            "lab_reports",
            "DELETE FROM lab_reports WHERE household_id=? AND pet_id=?",
        ),
        (
            "medication_plan_confirmations",
            "DELETE FROM medication_plan_confirmations WHERE household_id=? AND pet_id=?",
        ),
        (
            "medication_stop_confirmations",
            "DELETE FROM medication_stop_confirmations WHERE household_id=? AND pet_id=?",
        ),
        (
            "medication_adherence",
            "DELETE FROM medication_adherence WHERE household_id=? AND pet_id=?",
        ),
        (
            "medication_prescriptions",
            "DELETE FROM medication_prescriptions WHERE household_id=? AND pet_id=?",
        ),
        (
            "medication_administrations",
            "DELETE FROM medication_administrations WHERE household_id=? AND pet_id=?",
        ),
        (
            "symptom_observations",
            "DELETE FROM symptom_observations WHERE household_id=? AND pet_id=?",
        ),
        (
            "health_events",
            "DELETE FROM health_events WHERE household_id=? AND pet_id=?",
        ),
        (
            "weight_entries",
            "DELETE FROM weight_entries WHERE household_id=? AND pet_id=?",
        ),
        (
            "pet_photos",
            "DELETE FROM pet_photos WHERE household_id=? AND pet_id=?",
        ),
        (
            "share_grants",
            "DELETE FROM share_grants WHERE household_id=? AND pet_id=?",
        ),
    ];
    for (table, sql) in steps {
        let affected = transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql,
                [household_id.into(), pet_id.into()],
            ))
            .await?
            .rows_affected();
        if affected > 0 {
            removed.insert(table.to_owned(), affected);
        }
    }
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet_id.into()],
        ))
        .await?;
    let detail = serde_json::json!({"name": name, "removed": removed}).to_string();
    audit(
        &transaction,
        household_id,
        actor,
        "pet.deleted",
        "pet",
        pet_id,
        &detail,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(Some(DeletedPet {
        pet_id,
        name,
        removed,
        lab_source_files,
    }))
}

/// Deletes one mistaken weight entry, household-scoped; the audit row keeps
/// a snapshot. Returns that snapshot, or `None` if not found.
pub async fn delete_weight(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    weight_id: i64,
) -> anyhow::Result<Option<serde_json::Value>> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT json_object('id',id,'pet_id',pet_id,'weight_kg',weight_kg,'measured_at',measured_at,'note',note) AS snapshot FROM weight_entries WHERE household_id=? AND id=?",
            [household_id.into(), weight_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let snapshot: String = row.try_get("", "snapshot")?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM weight_entries WHERE household_id=? AND id=?",
            [household_id.into(), weight_id.into()],
        ))
        .await?;
    audit(
        &transaction,
        household_id,
        actor,
        "weight.deleted",
        "weight_entry",
        weight_id,
        &snapshot,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(Some(serde_json::from_str(&snapshot)?))
}

pub async fn create_pet_passport(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    label: Option<&str>,
    filename: &str,
    mime_type: &str,
    storage_path: &str,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO pet_passports(household_id,pet_id,label,filename,mime_type,storage_path,uploaded_at) SELECT ?,?,?,?,?,?,? FROM pets WHERE id=? AND household_id=? RETURNING id",
            [
                household_id.into(),
                pet_id.into(),
                label.into(),
                filename.into(),
                mime_type.into(),
                storage_path.into(),
                now.clone().into(),
                pet_id.into(),
                household_id.into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("pet does not belong to household"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &tx,
        household_id,
        actor,
        "passport.created",
        "pet_passport",
        id,
        "Private passport document uploaded",
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

fn pet_passport_from_row(row: QueryResult) -> anyhow::Result<crate::domain::PetPassport> {
    Ok(crate::domain::PetPassport {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        label: row.try_get("", "label")?,
        filename: row.try_get("", "filename")?,
        mime_type: row.try_get("", "mime_type")?,
        storage_path: row.try_get("", "storage_path")?,
        uploaded_at: row.try_get("", "uploaded_at")?,
    })
}

pub async fn list_pet_passports(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::PetPassport>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,household_id,pet_id,label,filename,mime_type,storage_path,uploaded_at FROM pet_passports WHERE household_id=? AND pet_id=? ORDER BY uploaded_at DESC,id DESC LIMIT 100",
            [household_id.into(), pet_id.into()],
        ))
        .await?;
    rows.into_iter().map(pet_passport_from_row).collect()
}

pub async fn get_pet_passport(
    db: &DatabaseConnection,
    household_id: i64,
    passport_id: i64,
) -> anyhow::Result<Option<crate::domain::PetPassport>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,household_id,pet_id,label,filename,mime_type,storage_path,uploaded_at FROM pet_passports WHERE household_id=? AND id=?",
        [household_id.into(), passport_id.into()],
    ))
    .await?
    .map(pet_passport_from_row)
    .transpose()
}

pub async fn delete_pet_passport(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    passport_id: i64,
) -> anyhow::Result<Option<crate::domain::PetPassport>> {
    let tx = db.begin().await?;
    let found = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,household_id,pet_id,label,filename,mime_type,storage_path,uploaded_at FROM pet_passports WHERE household_id=? AND id=?",
            [household_id.into(), passport_id.into()],
        ))
        .await?
        .map(pet_passport_from_row)
        .transpose()?;
    let Some(passport) = found else {
        return Ok(None);
    };
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM pet_passports WHERE household_id=? AND id=?",
        [household_id.into(), passport_id.into()],
    ))
    .await?;
    audit(
        &tx,
        household_id,
        actor,
        "passport.deleted",
        "pet_passport",
        passport_id,
        "Private passport document deleted",
        &Utc::now().to_rfc3339(),
    )
    .await?;
    tx.commit().await?;
    Ok(Some(passport))
}

pub async fn create_pet_photo(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    storage_name: &str,
    original_filename: &str,
    mime_type: &str,
    captured_at: &str,
) -> anyhow::Result<i64> {
    let uploaded_at = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO pet_photos(household_id,pet_id,storage_name,original_filename,mime_type,captured_at,uploaded_at,alignment_status) VALUES(?,?,?,?,?,?,?,'pending') RETURNING id",
        [
            household_id.into(),
            pet_id.into(),
            storage_name.into(),
            original_filename.into(),
            mime_type.into(),
            captured_at.into(),
            uploaded_at.clone().into(),
        ],
    )).await?.ok_or_else(|| anyhow!("photo insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "photo.created",
        "pet_photo",
        id,
        original_filename,
        &uploaded_at,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn list_pet_photos(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::PetPhoto>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,household_id,pet_id,storage_name,original_filename,mime_type,captured_at,uploaded_at,left_eye_x,left_eye_y,right_eye_x,right_eye_y,alignment_status FROM pet_photos WHERE household_id=? AND pet_id=? ORDER BY captured_at ASC LIMIT 365",
        [household_id.into(), pet_id.into()],
    )).await?;
    rows.into_iter().map(photo_from_row).collect()
}

pub async fn get_pet_photo(
    db: &DatabaseConnection,
    household_id: i64,
    photo_id: i64,
) -> anyhow::Result<Option<crate::domain::PetPhoto>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,household_id,pet_id,storage_name,original_filename,mime_type,captured_at,uploaded_at,left_eye_x,left_eye_y,right_eye_x,right_eye_y,alignment_status FROM pet_photos WHERE household_id=? AND id=?",
        [household_id.into(), photo_id.into()],
    )).await?.map(photo_from_row).transpose()
}

#[allow(clippy::too_many_arguments)]
pub async fn update_photo_alignment(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    photo_id: i64,
    left_eye_x: Option<f64>,
    left_eye_y: Option<f64>,
    right_eye_x: Option<f64>,
    right_eye_y: Option<f64>,
    status: &str,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE pet_photos SET left_eye_x=?,left_eye_y=?,right_eye_x=?,right_eye_y=?,alignment_status=? WHERE household_id=? AND id=?",
        [
            left_eye_x.into(),
            left_eye_y.into(),
            right_eye_x.into(),
            right_eye_y.into(),
            status.into(),
            household_id.into(),
            photo_id.into(),
        ],
    )).await?;
    if result.rows_affected() == 0 {
        transaction.rollback().await?;
        return Ok(false);
    }
    audit(
        &transaction,
        household_id,
        actor,
        "photo.alignment.updated",
        "pet_photo",
        photo_id,
        status,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(true)
}

pub async fn report_hash_exists(
    db: &DatabaseConnection,
    household_id: i64,
    document_hash: &str,
) -> anyhow::Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM lab_reports WHERE household_id=? AND document_hash=?",
            [household_id.into(), document_hash.into()],
        ))
        .await?
        .is_some())
}

pub async fn create_lab_report(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    source_filename: &str,
    document_hash: &str,
    raw_text: &str,
    test_date: Option<&str>,
    results: &[crate::ocr::ParsedLabResult],
) -> anyhow::Result<i64> {
    let parse_status = if results.is_empty() {
        "needs_review"
    } else {
        "parsed"
    };
    insert_lab_report(
        db,
        household_id,
        actor,
        pet_id,
        source_filename,
        document_hash,
        raw_text,
        test_date,
        results,
        parse_status,
        "lab_report.imported",
    )
    .await
}

/// Save blood-test values that an MCP client (e.g. ChatGPT reading a photo)
/// transcribed and the owner confirmed. No server-side OCR or model call.
#[allow(clippy::too_many_arguments)]
pub async fn record_lab_report(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    source_label: &str,
    document_hash: &str,
    raw_text: &str,
    test_date: &str,
    results: &[crate::ocr::ParsedLabResult],
) -> anyhow::Result<i64> {
    insert_lab_report(
        db,
        household_id,
        actor,
        pet_id,
        source_label,
        document_hash,
        raw_text,
        Some(test_date),
        results,
        "assistant_transcribed",
        "lab_report.recorded",
    )
    .await
}

/// Remove one lab report and its values, household-scoped and audited.
pub async fn delete_lab_report(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    report_id: i64,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let exists = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT source_filename FROM lab_reports WHERE household_id=? AND id=?",
            [household_id.into(), report_id.into()],
        ))
        .await?;
    let Some(row) = exists else {
        return Ok(false);
    };
    let label: String = row.try_get("", "source_filename")?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM lab_results WHERE report_id IN (SELECT id FROM lab_reports WHERE household_id=? AND id=?)",
            [household_id.into(), report_id.into()],
        ))
        .await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM lab_reports WHERE household_id=? AND id=?",
            [household_id.into(), report_id.into()],
        ))
        .await?;
    audit(
        &transaction,
        household_id,
        actor,
        "lab_report.deleted",
        "lab_report",
        report_id,
        &label,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn insert_lab_report(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    source_filename: &str,
    document_hash: &str,
    raw_text: &str,
    test_date: Option<&str>,
    results: &[crate::ocr::ParsedLabResult],
    parse_status: &str,
    audit_action: &str,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO lab_reports(household_id,pet_id,source_filename,document_hash,raw_text,test_date,imported_at,parse_status) VALUES(?,?,?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), source_filename.into(), document_hash.into(), raw_text.into(), test_date.into(), now.clone().into(), parse_status.into()],
    )).await?.ok_or_else(|| anyhow!("lab report insert returned no id"))?;
    let report_id: i64 = row.try_get("", "id")?;
    for result in results {
        transaction.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO lab_results(report_id,test_name,value_text,value_numeric,unit,reference_range,flag) VALUES(?,?,?,?,?,?,?)",
            [report_id.into(), result.test_name.clone().into(), result.value_text.clone().into(), result.value_numeric.into(), result.unit.clone().into(), result.reference_range.clone().into(), result.flag.clone().into()],
        )).await?;
    }
    audit(
        &transaction,
        household_id,
        actor,
        audit_action,
        "lab_report",
        report_id,
        source_filename,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(report_id)
}

pub async fn list_lab_reports(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::LabReport>> {
    let reports = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT r.id,r.pet_id,p.name,r.source_filename,r.raw_text,r.test_date,r.imported_at,r.parse_status,(r.original_file IS NOT NULL) AS has_original FROM lab_reports r JOIN pets p ON p.id=r.pet_id WHERE r.household_id=? AND r.pet_id=? ORDER BY COALESCE(r.test_date,r.imported_at) DESC LIMIT 50",
        [household_id.into(), pet_id.into()],
    )).await?;
    let mut output = Vec::with_capacity(reports.len());
    for row in reports {
        let report_id: i64 = row.try_get("", "id")?;
        let result_rows = db.query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,test_name,value_text,value_numeric,unit,reference_range,flag FROM lab_results WHERE report_id=? ORDER BY id",
            [report_id.into()],
        )).await?;
        let results = result_rows
            .into_iter()
            .map(|result| {
                Ok(crate::domain::LabResult {
                    id: result.try_get("", "id")?,
                    test_name: result.try_get("", "test_name")?,
                    value_text: result.try_get("", "value_text")?,
                    value_numeric: result.try_get("", "value_numeric")?,
                    unit: result.try_get("", "unit")?,
                    reference_range: result.try_get("", "reference_range")?,
                    flag: result.try_get("", "flag")?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        output.push(crate::domain::LabReport {
            id: report_id,
            pet_id: row.try_get("", "pet_id")?,
            pet_name: row.try_get("", "name")?,
            source_filename: row.try_get("", "source_filename")?,
            raw_text: row.try_get("", "raw_text")?,
            test_date: row.try_get("", "test_date")?,
            imported_at: row
                .try_get::<String>("", "imported_at")?
                .parse()
                .context("invalid lab timestamp")?,
            parse_status: row.try_get("", "parse_status")?,
            has_original: row.try_get::<i64>("", "has_original")? != 0,
            results,
        });
    }
    Ok(output)
}

pub async fn create_pet(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    name: &str,
    species: &str,
    breed: Option<&str>,
    weight_kg: Option<f64>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO pets(household_id,name,species,breed,weight_kg,created_at,updated_at) VALUES(?,?,?,?,?,?,?) RETURNING id",
        [household_id.into(), name.into(), species.into(), breed.into(), weight_kg.into(), now.clone().into(), now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("pet insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "pet.created",
        "pet",
        id,
        name,
        &now,
    )
    .await?;
    if let Some(weight_kg) = weight_kg {
        let weight_row = transaction
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO weight_entries(household_id,pet_id,weight_kg,measured_at,note,created_at) VALUES(?,?,?,?,?,?) RETURNING id",
                [
                    household_id.into(),
                    id.into(),
                    weight_kg.into(),
                    now.clone().into(),
                    "Initial weight".into(),
                    now.clone().into(),
                ],
            ))
            .await?
            .ok_or_else(|| anyhow!("initial weight insert returned no id"))?;
        let weight_id: i64 = weight_row.try_get("", "id")?;
        audit(
            &transaction,
            household_id,
            actor,
            "weight.created",
            "weight_entry",
            weight_id,
            &format!("{weight_kg:.2} kg"),
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(id)
}

pub async fn create_health_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    proposal: &ProposedEvent,
    raw_input: &str,
    occurred_at: DateTime<Utc>,
    source: &str,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO health_events
           (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
           VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), proposal.event_type.clone().into(),
            proposal.concept.clone().into(), proposal.summary.clone().into(), raw_input.into(),
            proposal.details.clone().into(), occurred_at.to_rfc3339().into(), now.clone().into(),
            (if proposal.minutes_ago.is_some() { "inferred_relative" } else { "inferred_now" }).into(), source.into(),
        ],
    )).await?.ok_or_else(|| anyhow!("event insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        id,
        raw_input,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

/// Store a neutral free-text observation with no model involvement.
///
/// This is the preferred no-LLM write path for MCP clients that have already
/// interpreted the user's words (ChatGPT/Claude): the caller's `message` is
/// preserved verbatim as `raw_input`, the server chooses the timestamp, and
/// no inference call is made. Contrast with `record_health_event` (MCP) /
/// `CaptureAgent` (web), which may invoke a backend model.
#[allow(clippy::too_many_arguments)]
pub async fn create_observation(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    raw_input: &str,
    occurred_at: DateTime<Utc>,
    summary: Option<&str>,
    details: Option<&str>,
    source: &str,
) -> anyhow::Result<i64> {
    let summary = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Observation recorded");
    let summary: String = summary.chars().take(280).collect();
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO health_events
           (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
           VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), "observation".into(), "general_observation".into(),
            summary.into(), raw_input.into(), details.map(str::to_owned).into(),
            occurred_at.to_rfc3339().into(), now.clone().into(), "exact".into(), source.into(),
        ],
    )).await?.ok_or_else(|| anyhow!("observation insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        id,
        raw_input,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_symptom_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    raw_input: &str,
    occurred_at: DateTime<Utc>,
    symptom: &str,
    occurrence_count: Option<i64>,
    amount: Option<&str>,
    contents: Option<&str>,
    meal_relation: Option<&str>,
    water_status: Option<&str>,
    appetite_status: Option<&str>,
    energy_status: Option<&str>,
    pain_status: Option<&str>,
    note: Option<&str>,
    source: &str,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO health_events
           (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
           VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), "symptom".into(), symptom.into(),
            symptom_summary(symptom).into(), raw_input.into(), note.map(str::to_owned).into(),
            occurred_at.to_rfc3339().into(), now.clone().into(), "exact".into(), source.into(),
        ],
    )).await?.ok_or_else(|| anyhow!("symptom event insert returned no id"))?;
    let event_id: i64 = row.try_get("", "id")?;
    let episode_id = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT s.episode_id FROM symptom_observations s
           JOIN health_events e ON e.id=s.event_id
           WHERE s.household_id=? AND s.pet_id=? AND s.symptom=? AND e.status='active'
             AND e.occurred_at>=? ORDER BY e.occurred_at DESC LIMIT 1"#,
            [
                household_id.into(),
                pet.id.into(),
                symptom.into(),
                (occurred_at - Duration::hours(6)).to_rfc3339().into(),
            ],
        ))
        .await?
        .map(|existing| existing.try_get("", "episode_id"))
        .transpose()?
        .unwrap_or_else(|| format!("episode-{event_id}"));
    transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO symptom_observations
           (event_id,household_id,pet_id,episode_id,symptom,occurrence_count,amount,contents,meal_relation,water_status,appetite_status,energy_status,pain_status,note)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        [
            event_id.into(), household_id.into(), pet.id.into(), episode_id.into(), symptom.into(),
            occurrence_count.into(), amount.into(), contents.into(), meal_relation.into(),
            water_status.into(), appetite_status.into(), energy_status.into(), pain_status.into(),
            note.into(),
        ],
    )).await?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        event_id,
        raw_input,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(event_id)
}

fn symptom_summary(symptom: &str) -> &str {
    match symptom {
        "vomiting" => "Vomited",
        "diarrhea" => "Had diarrhea",
        "reduced_appetite" => "Reduced appetite",
        "wheezing" => "Wheezing",
        "nasal_discharge" => "Nasal discharge",
        "eye_discharge" => "Eye discharge",
        "nasal_congestion" => "Nasal congestion",
        "coughing" => "Coughing",
        "labored_breathing" => "Breathing difficulty",
        _ => "Symptom recorded",
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create_medication_administration(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    name: &str,
    active_ingredient: Option<&str>,
    dose_value: Option<f64>,
    dose_unit: Option<&str>,
    route: Option<&str>,
    administered_at: DateTime<Utc>,
    scheduled_at: Option<DateTime<Utc>>,
    status: &str,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO medication_administrations
           (household_id,pet_id,name,active_ingredient,dose_value,dose_unit,route,scheduled_at,administered_at,status,raw_input,created_at)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), name.into(), active_ingredient.into(),
            dose_value.into(), dose_unit.into(), route.into(), scheduled_at.map(|at| at.to_rfc3339()).into(),
            administered_at.to_rfc3339().into(), status.into(), raw_input.into(), now.clone().into(),
        ],
    )).await?.ok_or_else(|| anyhow!("medication insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "medication.created",
        "medication_administration",
        id,
        name,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_medication_prescription(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    name: &str,
    active_ingredient: Option<&str>,
    concentration_value: Option<f64>,
    concentration_unit: Option<&str>,
    dose_value: Option<f64>,
    dose_unit: Option<&str>,
    frequency: Option<&str>,
    route: Option<&str>,
    instructions: Option<&str>,
    started_on: Option<&str>,
    status: &str,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_prescriptions
               (household_id,pet_id,name,active_ingredient,concentration_value,concentration_unit,dose_value,dose_unit,frequency,route,instructions,started_on,status,raw_input,created_at,updated_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(), pet.id.into(), name.into(), active_ingredient.into(),
                concentration_value.into(), concentration_unit.into(), dose_value.into(),
                dose_unit.into(), frequency.into(), route.into(), instructions.into(),
                started_on.into(), status.into(), raw_input.into(), now.clone().into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("prescription insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "medication.prescription.created",
        "medication_prescription",
        id,
        raw_input.unwrap_or(name),
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

#[derive(Clone, Debug)]
pub struct MedicationPlanConfirmation {
    pub replaced_prescriptions: u64,
    pub already_applied: bool,
}

/// Applies a human-confirmed medication regimen change atomically. Matching
/// active prescriptions are retained as stopped history, the new regimen is
/// created as active, and a timeline event preserves the owner's wording.
/// The household-scoped token makes a double-click or request replay idempotent.
#[allow(clippy::too_many_arguments)]
pub async fn confirm_medication_plan_change(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    confirmation_token_hash: &str,
    medication_name: &str,
    dose_value: f64,
    dose_unit: &str,
    frequency: &str,
    reason: Option<&str>,
    raw_input: &str,
) -> anyhow::Result<MedicationPlanConfirmation> {
    let now = Utc::now();
    let now_text = now.to_rfc3339();
    let today = now.date_naive().to_string();
    let transaction = db.begin().await?;

    let pet_exists = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet.id.into()],
        ))
        .await?
        .is_some();
    if !pet_exists {
        return Err(anyhow!("pet is outside the household scope"));
    }

    if let Some(existing) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT pet_id FROM medication_plan_confirmations
               WHERE household_id=? AND token_hash=?"#,
            [household_id.into(), confirmation_token_hash.into()],
        ))
        .await?
    {
        let confirmed_pet_id: i64 = existing.try_get("", "pet_id")?;
        if confirmed_pet_id != pet.id {
            return Err(anyhow!("confirmation token belongs to another pet"));
        }
        transaction.commit().await?;
        return Ok(MedicationPlanConfirmation {
            replaced_prescriptions: 0,
            already_applied: true,
        });
    }

    let matching = transaction
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT id,active_ingredient,concentration_value,concentration_unit,route
               FROM medication_prescriptions
               WHERE household_id=? AND pet_id=? AND status='active' AND name=? COLLATE NOCASE
               ORDER BY created_at DESC"#,
            [household_id.into(), pet.id.into(), medication_name.into()],
        ))
        .await?;
    let carried_active_ingredient = matching
        .first()
        .map(|row| row.try_get::<Option<String>>("", "active_ingredient"))
        .transpose()?
        .flatten();
    let carried_concentration_value = matching
        .first()
        .map(|row| row.try_get::<Option<f64>>("", "concentration_value"))
        .transpose()?
        .flatten();
    let carried_concentration_unit = matching
        .first()
        .map(|row| row.try_get::<Option<String>>("", "concentration_unit"))
        .transpose()?
        .flatten();
    let carried_route = matching
        .first()
        .map(|row| row.try_get::<Option<String>>("", "route"))
        .transpose()?
        .flatten();
    let matching_ids = matching
        .iter()
        .map(|row| row.try_get::<i64>("", "id"))
        .collect::<Result<Vec<_>, _>>()?;

    let replaced = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE medication_prescriptions
               SET status='stopped',ended_on=?,updated_at=?
               WHERE household_id=? AND pet_id=? AND status='active' AND name=? COLLATE NOCASE"#,
            [
                today.clone().into(),
                now_text.clone().into(),
                household_id.into(),
                pet.id.into(),
                medication_name.into(),
            ],
        ))
        .await?
        .rows_affected();
    for id in matching_ids {
        audit(
            &transaction,
            household_id,
            actor,
            "medication.prescription.stopped",
            "medication_prescription",
            id,
            "Replaced by a confirmed medication-plan change",
            &now_text,
        )
        .await?;
    }

    let prescription_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_prescriptions
               (household_id,pet_id,name,active_ingredient,concentration_value,concentration_unit,dose_value,dose_unit,frequency,route,instructions,started_on,status,raw_input,created_at,updated_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(),
                pet.id.into(),
                medication_name.into(),
                carried_active_ingredient.into(),
                carried_concentration_value.into(),
                carried_concentration_unit.into(),
                dose_value.into(),
                dose_unit.into(),
                frequency.into(),
                carried_route.into(),
                Option::<String>::None.into(),
                today.into(),
                "active".into(),
                raw_input.into(),
                now_text.clone().into(),
                now_text.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("confirmed prescription insert returned no id"))?;
    let prescription_id: i64 = prescription_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "medication.prescription.created",
        "medication_prescription",
        prescription_id,
        raw_input,
        &now_text,
    )
    .await?;

    let summary = format!("{medication_name}: {dose_value} {dose_unit} {frequency}");
    let details = reason
        .map(|value| format!("Confirmed medication plan change. Reason/context: {value}"))
        .unwrap_or_else(|| "Confirmed medication plan change.".to_owned());
    let event_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO health_events
               (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
               VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(),
                pet.id.into(),
                "medication".into(),
                "medication_plan_change".into(),
                summary.into(),
                raw_input.into(),
                details.into(),
                now_text.clone().into(),
                now_text.clone().into(),
                "confirmed_now".into(),
                "owner_agent_confirmed".into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("confirmed medication event insert returned no id"))?;
    let event_id: i64 = event_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        event_id,
        raw_input,
        &now_text,
    )
    .await?;

    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_plan_confirmations
               (household_id,pet_id,token_hash,prescription_id,event_id,created_at)
               VALUES(?,?,?,?,?,?)"#,
            [
                household_id.into(),
                pet.id.into(),
                confirmation_token_hash.into(),
                prescription_id.into(),
                event_id.into(),
                now_text.into(),
            ],
        ))
        .await?;
    transaction.commit().await?;
    Ok(MedicationPlanConfirmation {
        replaced_prescriptions: replaced,
        already_applied: false,
    })
}

#[derive(Clone, Debug)]
pub struct MedicationStopConfirmation {
    pub stopped_prescriptions: u64,
    pub already_applied: bool,
}

/// Applies a human-confirmed medication stop atomically. No replacement
/// prescription is created: non-drug care remains context on the timeline.
/// The owner-supplied day is stored with day precision, while `recorded_at`
/// and all audit timestamps remain server-owned.
#[allow(clippy::too_many_arguments)]
pub async fn confirm_medication_plan_stop(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    confirmation_token_hash: &str,
    medication_name: &str,
    replacement: &str,
    occurred_on: NaiveDate,
    raw_input: &str,
) -> anyhow::Result<MedicationStopConfirmation> {
    let now = Utc::now();
    if occurred_on > now.date_naive() {
        return Err(anyhow!("medication stop date cannot be in the future"));
    }
    let now_text = now.to_rfc3339();
    let occurred_at = occurred_on
        .and_hms_opt(12, 0, 0)
        .ok_or_else(|| anyhow!("invalid medication stop date"))?
        .and_utc()
        .to_rfc3339();
    let ended_on = occurred_on.to_string();
    let transaction = db.begin().await?;

    let pet_exists = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet.id.into()],
        ))
        .await?
        .is_some();
    if !pet_exists {
        return Err(anyhow!("pet is outside the household scope"));
    }

    if let Some(existing) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT pet_id FROM medication_stop_confirmations
               WHERE household_id=? AND token_hash=?"#,
            [household_id.into(), confirmation_token_hash.into()],
        ))
        .await?
    {
        let confirmed_pet_id: i64 = existing.try_get("", "pet_id")?;
        if confirmed_pet_id != pet.id {
            return Err(anyhow!("confirmation token belongs to another pet"));
        }
        transaction.commit().await?;
        return Ok(MedicationStopConfirmation {
            stopped_prescriptions: 0,
            already_applied: true,
        });
    }

    let matching_ids = transaction
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT id FROM medication_prescriptions
               WHERE household_id=? AND pet_id=? AND status='active' AND name=? COLLATE NOCASE"#,
            [household_id.into(), pet.id.into(), medication_name.into()],
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<i64>("", "id"))
        .collect::<Result<Vec<_>, _>>()?;
    let stopped = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE medication_prescriptions
               SET status='stopped',ended_on=?,updated_at=?
               WHERE household_id=? AND pet_id=? AND status='active' AND name=? COLLATE NOCASE"#,
            [
                ended_on.clone().into(),
                now_text.clone().into(),
                household_id.into(),
                pet.id.into(),
                medication_name.into(),
            ],
        ))
        .await?
        .rows_affected();
    for id in matching_ids {
        audit(
            &transaction,
            household_id,
            actor,
            "medication.prescription.stopped",
            "medication_prescription",
            id,
            raw_input,
            &now_text,
        )
        .await?;
    }

    let summary = format!("Stopped {medication_name}; switched to {replacement}");
    let details = format!("Confirmed medication stop. Replacement/context: {replacement}.");
    let event_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO health_events
               (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
               VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(),
                pet.id.into(),
                "medication".into(),
                "medication_plan_stop".into(),
                summary.into(),
                raw_input.into(),
                details.into(),
                occurred_at.into(),
                now_text.clone().into(),
                "day".into(),
                "owner_agent_confirmed".into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("confirmed medication stop event insert returned no id"))?;
    let event_id: i64 = event_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        event_id,
        raw_input,
        &now_text,
    )
    .await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_stop_confirmations
               (household_id,pet_id,token_hash,event_id,created_at)
               VALUES(?,?,?,?,?)"#,
            [
                household_id.into(),
                pet.id.into(),
                confirmation_token_hash.into(),
                event_id.into(),
                now_text.into(),
            ],
        ))
        .await?;
    transaction.commit().await?;
    Ok(MedicationStopConfirmation {
        stopped_prescriptions: stopped,
        already_applied: false,
    })
}

/// An owner-confirmed change to one existing prescription, requested by an
/// MCP client. Known dates are server-computed from relative day offsets.
/// None means the actual effective date was not supplied, not that it was today.
#[derive(Clone, Debug)]
pub enum PrescriptionChange {
    Stop {
        ended_on: Option<NaiveDate>,
    },
    Pause,
    Resume,
    ChangeDose {
        dose_value: Option<f64>,
        dose_unit: Option<String>,
        frequency: Option<String>,
        concentration_value: Option<f64>,
        concentration_unit: Option<String>,
        instructions: Option<String>,
        effective_on: Option<NaiveDate>,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct PrescriptionUpdate {
    pub previous_prescription_id: i64,
    pub current_prescription_id: i64,
    pub previous_status: String,
    pub new_status: String,
    pub event_id: i64,
}

/// Applies one owner-confirmed prescription change atomically and keeps
/// history: a dose change stops the old row and creates a new active row that
/// carries the unchanged fields. The update is guarded on the status the
/// caller observed, so a concurrent change yields `None` instead of a
/// silent overwrite. A timeline event preserves the owner's wording.
pub async fn update_prescription(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    prescription_id: i64,
    expected_status: &str,
    change: &PrescriptionChange,
    raw_input: &str,
) -> anyhow::Result<Option<PrescriptionUpdate>> {
    let now = Utc::now();
    let now_text = now.to_rfc3339();
    let transaction = db.begin().await?;
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
               JOIN pets p ON p.id=m.pet_id
               WHERE m.household_id=? AND m.pet_id=? AND m.id=? AND m.status=?"#,
            [
                household_id.into(),
                pet.id.into(),
                prescription_id.into(),
                expected_status.into(),
            ],
        ))
        .await?
    else {
        return Ok(None);
    };
    let current = prescription_from_row(row)?;
    let (new_status, ended_on, concept, summary, occurred_on) = match change {
        PrescriptionChange::Stop { ended_on } => (
            "stopped",
            ended_on.map(|day| day.to_string()),
            "medication_plan_stop",
            format!("Stopped {}", current.name),
            *ended_on,
        ),
        PrescriptionChange::Pause => (
            "paused",
            None,
            "medication_plan_pause",
            format!("Paused {}", current.name),
            Some(now.date_naive()),
        ),
        PrescriptionChange::Resume => (
            "active",
            None,
            "medication_plan_resume",
            format!("Resumed {}", current.name),
            Some(now.date_naive()),
        ),
        PrescriptionChange::ChangeDose { effective_on, .. } => (
            "stopped",
            effective_on.map(|day| day.to_string()),
            "medication_plan_change",
            String::new(),
            *effective_on,
        ),
    };
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE medication_prescriptions SET status=?,ended_on=COALESCE(?,ended_on),updated_at=?
               WHERE household_id=? AND id=?"#,
            [
                new_status.into(),
                ended_on.into(),
                now_text.clone().into(),
                household_id.into(),
                prescription_id.into(),
            ],
        ))
        .await?;
    let audit_action = match change {
        PrescriptionChange::Stop { .. } | PrescriptionChange::ChangeDose { .. } => {
            "medication.prescription.stopped"
        }
        PrescriptionChange::Pause => "medication.prescription.paused",
        PrescriptionChange::Resume => "medication.prescription.resumed",
    };
    audit(
        &transaction,
        household_id,
        actor,
        audit_action,
        "medication_prescription",
        prescription_id,
        raw_input,
        &now_text,
    )
    .await?;

    let mut current_id = prescription_id;
    let mut summary = summary;
    let mut result_status = new_status.to_owned();
    if let PrescriptionChange::ChangeDose {
        dose_value,
        dose_unit,
        frequency,
        concentration_value,
        concentration_unit,
        instructions,
        effective_on,
    } = change
    {
        let dose_value = dose_value.or(current.dose_value);
        let dose_unit = dose_unit.clone().or(current.dose_unit.clone());
        let frequency = frequency.clone().or(current.frequency.clone());
        let new_row = transaction
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                r#"INSERT INTO medication_prescriptions
                   (household_id,pet_id,name,active_ingredient,concentration_value,concentration_unit,dose_value,dose_unit,frequency,route,instructions,started_on,status,raw_input,created_at,updated_at)
                   VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
                [
                    household_id.into(),
                    pet.id.into(),
                    current.name.clone().into(),
                    current.active_ingredient.clone().into(),
                    concentration_value.or(current.concentration_value).into(),
                    concentration_unit
                        .clone()
                        .or(current.concentration_unit.clone())
                        .into(),
                    dose_value.into(),
                    dose_unit.clone().into(),
                    frequency.clone().into(),
                    current.route.clone().into(),
                    instructions.clone().or(current.instructions.clone()).into(),
                    effective_on.map(|day| day.to_string()).into(),
                    "active".into(),
                    raw_input.into(),
                    now_text.clone().into(),
                    now_text.clone().into(),
                ],
            ))
            .await?
            .ok_or_else(|| anyhow!("prescription insert returned no id"))?;
        current_id = new_row.try_get("", "id")?;
        audit(
            &transaction,
            household_id,
            actor,
            "medication.prescription.created",
            "medication_prescription",
            current_id,
            raw_input,
            &now_text,
        )
        .await?;
        let dose = [dose_value.map(|v| v.to_string()), dose_unit, frequency]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        summary = format!("Changed {} to {dose}", current.name);
        result_status = "active".into();
    }

    // A record made today does not prove that an older treatment stopped
    // today. For unknown onset dates, preserve the recorded timestamp but
    // label its temporal precision so timelines/vet exports do not invent one.
    let (occurred_at, temporal_precision, event_details) = match occurred_on {
        None => (
            now_text.clone(),
            "recorded_only",
            "Owner-confirmed prescription update; actual effective date unknown.",
        ),
        Some(day) if day == now.date_naive() => (
            now_text.clone(),
            "confirmed_now",
            "Owner-confirmed prescription update via MCP.",
        ),
        Some(day) => (
            day.and_hms_opt(12, 0, 0)
                .ok_or_else(|| anyhow!("invalid date"))?
                .and_utc()
                .to_rfc3339(),
            "day",
            "Owner-confirmed prescription update via MCP.",
        ),
    };
    let event_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO health_events
               (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
               VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(),
                pet.id.into(),
                "medication".into(),
                concept.into(),
                summary.into(),
                raw_input.into(),
                event_details.into(),
                occurred_at.into(),
                now_text.clone().into(),
                temporal_precision.into(),
                "mcp".into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("prescription update event insert returned no id"))?;
    let event_id: i64 = event_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        event_id,
        raw_input,
        &now_text,
    )
    .await?;
    transaction.commit().await?;
    Ok(Some(PrescriptionUpdate {
        previous_prescription_id: prescription_id,
        current_prescription_id: current_id,
        previous_status: current.status,
        new_status: result_status,
        event_id,
    }))
}

/// Deletes one mistaken medication dose (`medication_administrations`) or
/// adherence record, household-scoped. The audit row keeps a JSON snapshot of
/// what was removed. Returns that snapshot, or `None` if not found.
pub async fn delete_medication_entry(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    entry_type: &str,
    entry_id: i64,
) -> anyhow::Result<Option<serde_json::Value>> {
    let (table, record_type, action) = match entry_type {
        "dose" => (
            "medication_administrations",
            "medication_administration",
            "medication.deleted",
        ),
        "adherence" => (
            "medication_adherence",
            "medication_adherence",
            "medication.adherence.deleted",
        ),
        _ => return Err(anyhow!("unsupported medication entry type")),
    };
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let snapshot_sql = match entry_type {
        "dose" => {
            "SELECT json_object('id',id,'pet_id',pet_id,'name',name,'dose_value',dose_value,'dose_unit',dose_unit,'status',status,'administered_at',administered_at,'raw_input',raw_input) AS snapshot FROM medication_administrations WHERE household_id=? AND id=?"
        }
        _ => {
            "SELECT json_object('id',id,'pet_id',pet_id,'prescription_id',prescription_id,'scheduled_for',scheduled_for,'status',status,'actual_dose_value',actual_dose_value,'actual_dose_unit',actual_dose_unit,'reason',reason,'raw_input',raw_input) AS snapshot FROM medication_adherence WHERE household_id=? AND id=?"
        }
    };
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            snapshot_sql,
            [household_id.into(), entry_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let snapshot: String = row.try_get("", "snapshot")?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            &format!("DELETE FROM {table} WHERE household_id=? AND id=?"),
            [household_id.into(), entry_id.into()],
        ))
        .await?;
    audit(
        &transaction,
        household_id,
        actor,
        action,
        record_type,
        entry_id,
        &snapshot,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(Some(serde_json::from_str(&snapshot)?))
}

pub async fn create_medication_adherence(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    prescription: &MedicationPrescription,
    scheduled_for: &str,
    actual_dose_value: Option<f64>,
    actual_dose_unit: Option<&str>,
    status: &str,
    reason: Option<&str>,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_adherence
               (household_id,pet_id,prescription_id,scheduled_for,expected_dose_value,expected_dose_unit,actual_dose_value,actual_dose_unit,status,reason,raw_input,recorded_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(), pet.id.into(), prescription.id.into(), scheduled_for.into(),
                prescription.dose_value.into(), prescription.dose_unit.clone().into(),
                actual_dose_value.into(), actual_dose_unit.into(), status.into(), reason.into(),
                raw_input.into(), now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("adherence insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        db,
        household_id,
        actor,
        "medication.adherence.recorded",
        "medication_adherence",
        id,
        raw_input.unwrap_or(reason.unwrap_or("")),
        &now,
    )
    .await?;
    Ok(id)
}

pub async fn list_medications(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationAdministration>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_administrations m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=?
           ORDER BY m.administered_at DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(medication_from_row).collect()
}

pub async fn list_prescriptions(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationPrescription>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=?
           ORDER BY CASE WHEN m.status='active' THEN 0 ELSE 1 END, m.created_at DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(prescription_from_row).collect()
}

pub async fn get_prescription(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    prescription_id: i64,
) -> anyhow::Result<Option<MedicationPrescription>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=? AND m.id=?"#,
            [household_id.into(), pet_id.into(), prescription_id.into()],
        ))
        .await?;
    row.map(prescription_from_row).transpose()
}

/// Active same-name prescriptions for one pet, used by the MCP
/// consequential-change gate: replacing a differing active regimen requires
/// explicit owner confirmation (`confirm:true`). Household + pet scoped.
pub async fn list_active_prescriptions_by_name(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    name: &str,
) -> anyhow::Result<Vec<MedicationPrescription>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
           JOIN pets p ON p.id=m.pet_id
           WHERE m.household_id=? AND m.pet_id=? AND m.status='active' AND m.name=? COLLATE NOCASE
           ORDER BY m.created_at DESC"#,
            [household_id.into(), pet_id.into(), name.into()],
        ))
        .await?;
    rows.into_iter().map(prescription_from_row).collect()
}

pub async fn list_adherence(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationAdherence>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT a.*,p.name AS pet_name,m.name AS prescription_name FROM medication_adherence a
           JOIN pets p ON p.id=a.pet_id JOIN medication_prescriptions m ON m.id=a.prescription_id
           WHERE a.household_id=? AND a.pet_id=?
           ORDER BY a.scheduled_for DESC, a.id DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(adherence_from_row).collect()
}

pub async fn medication_plan(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<MedicationPlan> {
    Ok(MedicationPlan {
        prescriptions: list_prescriptions(db, household_id, pet_id, limit).await?,
        adherence: list_adherence(db, household_id, pet_id, limit).await?,
    })
}

pub async fn clinical_timeline(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<ClinicalTimeline> {
    let events = list_events(db, household_id, Some(pet_id), limit).await?;
    let medications = list_medications(db, household_id, pet_id, limit).await?;
    let plan = medication_plan(db, household_id, pet_id, limit).await?;
    let temporal_links = events
        .iter()
        .filter(|event| event.symptom.is_some())
        .flat_map(|event| {
            medications.iter().filter_map(move |medication| {
                let minutes = (event.occurred_at - medication.administered_at).num_minutes();
                (0..=4_320).contains(&minutes).then_some(TemporalLink {
                    event_id: event.id,
                    medication_id: medication.id,
                    minutes_after_medication: minutes,
                })
            })
        })
        .collect();
    Ok(ClinicalTimeline {
        events,
        medications,
        prescriptions: plan.prescriptions,
        adherence: plan.adherence,
        temporal_links,
    })
}

pub async fn undo_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    event_id: i64,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE health_events SET status='undone', undone_at=? WHERE household_id=? AND id=? AND status='active'",
        [now.clone().into(), household_id.into(), event_id.into()],
    )).await?;
    if result.rows_affected() == 1 {
        audit(
            &transaction,
            household_id,
            actor,
            "event.undone",
            "health_event",
            event_id,
            "Undo from timeline",
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Changes only the short, human-facing label shown for an event. The original
/// wording and all structured fields remain untouched so correcting an AI label
/// never rewrites the underlying health record.
pub async fn update_event_summary(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    actor: &str,
    event_id: i64,
    summary: &str,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE health_events SET summary=? WHERE household_id=? AND pet_id=? AND id=? AND status='active'",
            [
                summary.into(),
                household_id.into(),
                pet_id.into(),
                event_id.into(),
            ],
        ))
        .await?;
    if result.rows_affected() == 1 {
        audit(
            &transaction,
            household_id,
            actor,
            "event.summary.updated",
            "health_event",
            event_id,
            "Timeline label manually edited",
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn list_events(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: Option<i64>,
    limit: u64,
) -> anyhow::Result<Vec<HealthEvent>> {
    let (sql, values) = if let Some(pet_id) = pet_id {
        (
            r#"SELECT e.*,p.name AS pet_name,
                s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
                s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
                s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
                s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
            FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
            WHERE e.household_id=? AND e.pet_id=? AND e.status='active' ORDER BY e.occurred_at DESC LIMIT ?"#,
            vec![household_id.into(), pet_id.into(), limit.into()],
        )
    } else {
        (
            r#"SELECT e.*,p.name AS pet_name,
                s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
                s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
                s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
                s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
            FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
            WHERE e.household_id=? AND e.status='active' ORDER BY e.occurred_at DESC LIMIT ?"#,
            vec![household_id.into(), limit.into()],
        )
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(event_from_row).collect()
}

/// A single `health_events` row, scoped by household *and* pet — used by the
/// Phase 4 knowledge drawer (`UI_REDESIGN_PLAN.md` §4A) to resolve the event a
/// timeline click refers to. Household- and pet-scoping here matter for the
/// same reason as everywhere else (`AGENTS.md`): the event id in the drawer's
/// query string is never trusted on its own. Only `status='active'` events are
/// returned, matching `list_timeline`'s event arm, so an undone event's old
/// drawer link 404s instead of showing stale detail.
pub async fn get_event(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    event_id: i64,
) -> anyhow::Result<Option<HealthEvent>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT e.*,p.name AS pet_name,
                s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
                s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
                s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
                s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
            FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
            WHERE e.household_id=? AND e.pet_id=? AND e.id=? AND e.status='active'"#,
            [household_id.into(), pet_id.into(), event_id.into()],
        ))
        .await?;
    row.map(event_from_row).transpose()
}

pub async fn count_related(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    concept: &str,
) -> anyhow::Result<u64> {
    let row = db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM health_events WHERE household_id=? AND pet_id=? AND concept=? AND status='active'",
        [household_id.into(), pet_id.into(), concept.into()],
    )).await?.ok_or_else(|| anyhow!("count returned no row"))?;
    let count: i64 = row.try_get("", "count")?;
    Ok(count as u64)
}

/// The merged, paginated timeline behind the Timeline tab (`UI_REDESIGN_PLAN.md`
/// §4 Phase 2). Reads a bounded page from each of the four source tables — never
/// writes to `health_events` on their behalf — and merges them in memory rather
/// than backfilling or a wide SQL `UNION`, so `count_related` and the MCP tools
/// (`clinical_timeline`, `medication_plan`) are untouched and cannot double-count.
///
/// `before` is a keyset cursor: `(occurred_at, id)` of the last entry already
/// shown. Every arm is scoped by `household_id` *and* `pet_id` — the easiest
/// place to leak another household's rows is forgetting that on one arm of a
/// union (`UI_REDESIGN_PLAN.md` §6) — and `health_events` additionally keeps the
/// existing `status='active'` filter so undone events never resurface here.
///
/// Correctness of the merge relies on a standard federated top-k bound: fetching
/// the top `limit` rows (by `(sort_key, id)` descending, keyset-filtered by
/// `before`) from each source is sufficient to guarantee the *merged* top `limit`
/// is correct, because the union's Nth-ranked item can only ever come from among
/// the first N items of whichever single source it belongs to. `lab_reports` is
/// the one exception: `list_lab_reports` doesn't take a cursor (it's capped at 50
/// reports per pet, an existing limit), so lab rows are filtered against `before`
/// in Rust after fetching. That is fine at this app's realistic per-pet report
/// volume, but means a pet with more than 50 lab reports could see older ones
/// vanish from pagination — worth revisiting if that ever stops being true.
pub async fn list_timeline(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    before: Option<(DateTime<Utc>, i64)>,
    limit: u64,
) -> anyhow::Result<Vec<TimelineEntry>> {
    let events = timeline_events_page(db, household_id, pet_id, before, limit).await?;
    let weights = timeline_weights_page(db, household_id, pet_id, before, limit).await?;
    let doses = timeline_doses_page(db, household_id, pet_id, before, limit).await?;
    let labs = list_lab_reports(db, household_id, pet_id).await?;

    let mut merged: Vec<TimelineEntry> =
        Vec::with_capacity(events.len() + weights.len() + doses.len() + labs.len());
    merged.extend(events.into_iter().map(TimelineEntry::Event));
    merged.extend(weights.into_iter().map(TimelineEntry::Weight));
    merged.extend(doses.into_iter().map(TimelineEntry::Dose));
    merged.extend(
        labs.into_iter()
            .filter(|report| {
                before.is_none_or(|cursor| (lab_effective_at(report), report.id) < cursor)
            })
            .map(TimelineEntry::Lab),
    );

    merged.sort_by(|a, b| b.sort_key().cmp(&a.sort_key()));
    merged.truncate(limit as usize);
    Ok(merged)
}

/// A page of `health_events`, matching `list_events`'s column set and `status`
/// filter but with a keyset cursor on `(occurred_at, id)`. Kept private and
/// separate from `list_events` so that function's signature and behaviour (used
/// by the MCP tools) stay byte-for-byte unchanged.
///
/// The `datetime(...)` wrapping on both sides of every timestamp comparison
/// normalises across the two RFC3339 spellings this codebase's own timestamps
/// can carry — `+00:00` (everything written via `DateTime::to_rfc3339`) and `Z`
/// (the bare-date branch in `web::create_weight`) — which otherwise compare
/// unequal as raw strings for the same instant and would let a tied row silently
/// vanish between pages instead of being included exactly once.
async fn timeline_events_page(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    before: Option<(DateTime<Utc>, i64)>,
    limit: u64,
) -> anyhow::Result<Vec<HealthEvent>> {
    let base = r#"SELECT e.*,p.name AS pet_name,
        s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
        s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
        s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
        s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
        FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
        WHERE e.household_id=? AND e.pet_id=? AND e.status='active'"#;
    let (sql, values) = match before {
        Some((at, id)) => (
            format!(
                "{base} AND (datetime(e.occurred_at)<datetime(?) OR (datetime(e.occurred_at)=datetime(?) AND e.id<?)) \
                 ORDER BY datetime(e.occurred_at) DESC, e.id DESC LIMIT ?"
            ),
            vec![
                household_id.into(),
                pet_id.into(),
                at.to_rfc3339().into(),
                at.to_rfc3339().into(),
                id.into(),
                limit.into(),
            ],
        ),
        None => (
            format!("{base} ORDER BY datetime(e.occurred_at) DESC, e.id DESC LIMIT ?"),
            vec![household_id.into(), pet_id.into(), limit.into()],
        ),
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(event_from_row).collect()
}

/// A page of `weight_entries` with a keyset cursor. See `timeline_events_page`
/// for why timestamps are compared through `datetime(...)`.
async fn timeline_weights_page(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    before: Option<(DateTime<Utc>, i64)>,
    limit: u64,
) -> anyhow::Result<Vec<crate::domain::WeightEntry>> {
    let base = "SELECT id,pet_id,weight_kg,measured_at,note FROM weight_entries WHERE household_id=? AND pet_id=?";
    let (sql, values) = match before {
        Some((at, id)) => (
            format!(
                "{base} AND (datetime(measured_at)<datetime(?) OR (datetime(measured_at)=datetime(?) AND id<?)) \
                 ORDER BY datetime(measured_at) DESC, id DESC LIMIT ?"
            ),
            vec![
                household_id.into(),
                pet_id.into(),
                at.to_rfc3339().into(),
                at.to_rfc3339().into(),
                id.into(),
                limit.into(),
            ],
        ),
        None => (
            format!("{base} ORDER BY datetime(measured_at) DESC, id DESC LIMIT ?"),
            vec![household_id.into(), pet_id.into(), limit.into()],
        ),
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(weight_from_row).collect()
}

/// A page of `medication_administrations` with a keyset cursor. See
/// `timeline_events_page` for why timestamps are compared through `datetime(...)`.
async fn timeline_doses_page(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    before: Option<(DateTime<Utc>, i64)>,
    limit: u64,
) -> anyhow::Result<Vec<MedicationAdministration>> {
    let base = "SELECT m.*,p.name AS pet_name FROM medication_administrations m \
        JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=?";
    let (sql, values) = match before {
        Some((at, id)) => (
            format!(
                "{base} AND (datetime(m.administered_at)<datetime(?) OR (datetime(m.administered_at)=datetime(?) AND m.id<?)) \
                 ORDER BY datetime(m.administered_at) DESC, m.id DESC LIMIT ?"
            ),
            vec![
                household_id.into(),
                pet_id.into(),
                at.to_rfc3339().into(),
                at.to_rfc3339().into(),
                id.into(),
                limit.into(),
            ],
        ),
        None => (
            format!("{base} ORDER BY datetime(m.administered_at) DESC, m.id DESC LIMIT ?"),
            vec![household_id.into(), pet_id.into(), limit.into()],
        ),
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(medication_from_row).collect()
}

pub async fn get_knowledge(
    db: &DatabaseConnection,
    concept: &str,
) -> anyhow::Result<Option<KnowledgeArticle>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT concept,title,summary,monitoring,urgent_signs,source_url FROM knowledge_articles WHERE concept=?",
        [concept.into()],
    )).await?.map(|row| Ok(KnowledgeArticle {
        concept: row.try_get("", "concept")?, title: row.try_get("", "title")?,
        summary: row.try_get("", "summary")?, monitoring: row.try_get("", "monitoring")?,
        urgent_signs: row.try_get("", "urgent_signs")?, source_url: row.try_get("", "source_url")?,
    })).transpose()
}

pub async fn create_share(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    label: &str,
    days: i64,
) -> anyhow::Result<ShareGrant> {
    let token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let hash = token_hash(&token);
    let now = Utc::now();
    let expires = now + Duration::days(days.clamp(1, 90));
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO share_grants(household_id,pet_id,label,token_hash,expires_at,created_at) VALUES(?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), label.into(), hash.into(), expires.to_rfc3339().into(), now.to_rfc3339().into()],
    )).await?.ok_or_else(|| anyhow!("share insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "share.created",
        "share_grant",
        id,
        label,
        &now.to_rfc3339(),
    )
    .await?;
    transaction.commit().await?;
    let pet = get_pet(db, household_id, pet_id)
        .await?
        .ok_or_else(|| anyhow!("pet not found"))?;
    Ok(ShareGrant {
        id,
        household_id,
        pet_id,
        pet_name: pet.name,
        label: label.into(),
        token: Some(token),
        expires_at: expires,
        revoked_at: None,
        status: "active".into(),
    })
}

pub async fn list_shares(
    db: &DatabaseConnection,
    household_id: i64,
) -> anyhow::Result<Vec<ShareGrant>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT s.id,s.household_id,s.pet_id,p.name AS pet_name,s.label,s.expires_at,s.revoked_at
            FROM share_grants s JOIN pets p ON p.id=s.pet_id WHERE s.household_id=? ORDER BY s.created_at DESC"#,
        [household_id.into()],
    )).await?;
    rows.iter().map(share_from_row).collect()
}

pub async fn revoke_share(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    id: i64,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE share_grants SET revoked_at=? WHERE household_id=? AND id=? AND revoked_at IS NULL",
        [now.clone().into(), household_id.into(), id.into()],
    )).await?;
    if result.rows_affected() == 1 {
        audit(
            &transaction,
            household_id,
            actor,
            "share.revoked",
            "share_grant",
            id,
            "Revoked by owner",
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn resolve_share(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<(ShareGrant, Pet)>> {
    let hash = token_hash(token);
    let Some(row) = db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT s.id,s.pet_id,p.name AS pet_name,s.label,s.expires_at,s.revoked_at,s.household_id,
                  p.species,p.breed,p.date_of_birth,p.weight_kg
            FROM share_grants s JOIN pets p ON p.id=s.pet_id
            WHERE s.token_hash=? AND s.revoked_at IS NULL AND s.expires_at>?"#,
        [hash.into(), Utc::now().to_rfc3339().into()],
    )).await? else { return Ok(None); };
    let grant = share_from_row(&row)?;
    let pet = Pet {
        id: row.try_get("", "pet_id")?,
        name: row.try_get("", "pet_name")?,
        species: row.try_get("", "species")?,
        breed: row.try_get("", "breed")?,
        date_of_birth: row.try_get("", "date_of_birth")?,
        microchip_number: None, // Vet shares intentionally omit household-only identifiers.
        weight_kg: row.try_get("", "weight_kg")?,
        initials: initials(&row.try_get::<String>("", "pet_name")?),
    };
    Ok(Some((grant, pet)))
}

async fn audit<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    actor: &str,
    action: &str,
    record_type: &str,
    record_id: i64,
    detail: &str,
    created_at: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,?,?,?,?,?)",
        [household_id.into(), actor.into(), action.into(), record_type.into(), record_id.into(), detail.into(), created_at.into()],
    )).await?;
    Ok(())
}

fn user_from_row(row: QueryResult) -> anyhow::Result<UserAccount> {
    let display_name: String = row.try_get("", "display_name")?;
    Ok(UserAccount {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        username: row.try_get("", "username")?,
        email: row.try_get("", "email")?,
        initials: initials(&display_name),
        display_name,
    })
}

fn user_with_password_from_row(row: QueryResult) -> anyhow::Result<(UserAccount, String)> {
    let password_hash = row.try_get("", "password_hash")?;
    Ok((user_from_row(row)?, password_hash))
}

fn pet_from_row(row: QueryResult) -> anyhow::Result<Pet> {
    let name: String = row.try_get("", "name")?;
    Ok(Pet {
        id: row.try_get("", "id")?,
        initials: initials(&name),
        name,
        species: row.try_get("", "species")?,
        breed: row.try_get("", "breed")?,
        date_of_birth: row.try_get("", "date_of_birth")?,
        microchip_number: row.try_get("", "microchip_number")?,
        weight_kg: row.try_get("", "weight_kg")?,
    })
}

fn event_from_row(row: QueryResult) -> anyhow::Result<HealthEvent> {
    let event_type: String = row.try_get("", "event_type")?;
    let concept: String = row.try_get("", "concept")?;
    let occurred_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "occurred_at")?)?
        .with_timezone(&Utc);
    let recorded_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
        .with_timezone(&Utc);
    let (icon, tone) = event_presentation(&event_type, &concept);
    let symptom = match row.try_get::<Option<String>>("", "symptom_episode_id")? {
        Some(episode_id) => Some(SymptomObservation {
            event_id: row.try_get("", "id")?,
            episode_id,
            symptom: row.try_get("", "symptom_kind")?,
            occurrence_count: row.try_get("", "symptom_occurrence_count")?,
            amount: row.try_get("", "symptom_amount")?,
            contents: row.try_get("", "symptom_contents")?,
            meal_relation: row.try_get("", "symptom_meal_relation")?,
            water_status: row.try_get("", "symptom_water_status")?,
            appetite_status: row.try_get("", "symptom_appetite_status")?,
            energy_status: row.try_get("", "symptom_energy_status")?,
            pain_status: row.try_get("", "symptom_pain_status")?,
            note: row.try_get("", "symptom_note")?,
        }),
        None => None,
    };
    Ok(HealthEvent {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        event_type,
        concept,
        summary: row.try_get("", "summary")?,
        raw_input: row.try_get("", "raw_input")?,
        details: row.try_get("", "details")?,
        occurred_at,
        occurred_label: relative_time(occurred_at),
        recorded_at,
        source: row.try_get("", "source")?,
        status: row.try_get("", "status")?,
        icon,
        tone,
        symptom,
    })
}

fn weight_from_row(row: QueryResult) -> anyhow::Result<crate::domain::WeightEntry> {
    Ok(crate::domain::WeightEntry {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        weight_kg: row.try_get("", "weight_kg")?,
        measured_at: row
            .try_get::<String>("", "measured_at")?
            .parse()
            .context("invalid weight timestamp")?,
        note: row.try_get("", "note")?,
    })
}

fn photo_from_row(row: QueryResult) -> anyhow::Result<crate::domain::PetPhoto> {
    Ok(crate::domain::PetPhoto {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        storage_name: row.try_get("", "storage_name")?,
        original_filename: row.try_get("", "original_filename")?,
        mime_type: row.try_get("", "mime_type")?,
        captured_at: row
            .try_get::<String>("", "captured_at")?
            .parse()
            .context("invalid photo capture timestamp")?,
        uploaded_at: row
            .try_get::<String>("", "uploaded_at")?
            .parse()
            .context("invalid photo upload timestamp")?,
        left_eye_x: row.try_get("", "left_eye_x")?,
        left_eye_y: row.try_get("", "left_eye_y")?,
        right_eye_x: row.try_get("", "right_eye_x")?,
        right_eye_y: row.try_get("", "right_eye_y")?,
        alignment_status: row.try_get("", "alignment_status")?,
    })
}

fn medication_from_row(row: QueryResult) -> anyhow::Result<MedicationAdministration> {
    let parse_at =
        |value: String| DateTime::parse_from_rfc3339(&value).map(|at| at.with_timezone(&Utc));
    let administered_at = parse_at(row.try_get("", "administered_at")?)?;
    let scheduled_at = row
        .try_get::<Option<String>>("", "scheduled_at")?
        .map(parse_at)
        .transpose()?;
    Ok(MedicationAdministration {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        name: row.try_get("", "name")?,
        active_ingredient: row.try_get("", "active_ingredient")?,
        dose_value: row.try_get("", "dose_value")?,
        dose_unit: row.try_get("", "dose_unit")?,
        route: row.try_get("", "route")?,
        administered_at,
        scheduled_at,
        status: row.try_get("", "status")?,
        raw_input: row.try_get("", "raw_input")?,
    })
}

fn prescription_from_row(row: QueryResult) -> anyhow::Result<MedicationPrescription> {
    Ok(MedicationPrescription {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        name: row.try_get("", "name")?,
        active_ingredient: row.try_get("", "active_ingredient")?,
        concentration_value: row.try_get("", "concentration_value")?,
        concentration_unit: row.try_get("", "concentration_unit")?,
        dose_value: row.try_get("", "dose_value")?,
        dose_unit: row.try_get("", "dose_unit")?,
        frequency: row.try_get("", "frequency")?,
        route: row.try_get("", "route")?,
        instructions: row.try_get("", "instructions")?,
        started_on: row.try_get("", "started_on")?,
        ended_on: row.try_get("", "ended_on")?,
        status: row.try_get("", "status")?,
        raw_input: row.try_get("", "raw_input")?,
    })
}

fn adherence_from_row(row: QueryResult) -> anyhow::Result<MedicationAdherence> {
    let recorded_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
        .with_timezone(&Utc);
    Ok(MedicationAdherence {
        id: row.try_get("", "id")?,
        prescription_id: row.try_get("", "prescription_id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        prescription_name: row.try_get("", "prescription_name")?,
        scheduled_for: row.try_get("", "scheduled_for")?,
        expected_dose_value: row.try_get("", "expected_dose_value")?,
        expected_dose_unit: row.try_get("", "expected_dose_unit")?,
        actual_dose_value: row.try_get("", "actual_dose_value")?,
        actual_dose_unit: row.try_get("", "actual_dose_unit")?,
        status: row.try_get("", "status")?,
        reason: row.try_get("", "reason")?,
        raw_input: row.try_get("", "raw_input")?,
        recorded_at,
    })
}

fn share_from_row(row: &QueryResult) -> anyhow::Result<ShareGrant> {
    let expires_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "expires_at")?)?
        .with_timezone(&Utc);
    let revoked_raw: Option<String> = row.try_get("", "revoked_at")?;
    let revoked_at = revoked_raw
        .map(|value| DateTime::parse_from_rfc3339(&value).map(|v| v.with_timezone(&Utc)))
        .transpose()?;
    let status = if revoked_at.is_some() {
        "revoked"
    } else if expires_at < Utc::now() {
        "expired"
    } else {
        "active"
    };
    Ok(ShareGrant {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        label: row.try_get("", "label")?,
        token: None,
        expires_at,
        revoked_at,
        status: status.into(),
    })
}

fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}
fn relative_time(at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => at.format("%d %b, %H:%M UTC").to_string(),
    }
}
fn stmt(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> DatabaseConnection {
        let db = connect("sqlite::memory:").await.unwrap();
        migrate(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn updates_prescription_keeping_history_and_undoes_entries_in_scope() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Velcro", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let original = create_medication_prescription(
            &db,
            1,
            "user:1",
            &pet,
            "Semintra",
            Some("telmisartan"),
            Some(4.0),
            Some("mg/ml"),
            Some(0.35),
            Some("ml"),
            Some("once daily"),
            Some("oral"),
            None,
            Some("2026-10-01"),
            "active",
            Some("On Semintra"),
        )
        .await
        .unwrap();
        let today = Utc::now().date_naive();

        // Pause then resume, guarded on the observed status.
        let paused = update_prescription(
            &db,
            1,
            "user:1",
            &pet,
            original,
            "active",
            &PrescriptionChange::Pause,
            "Pausing",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(paused.new_status, "paused");
        assert!(
            update_prescription(
                &db,
                1,
                "user:1",
                &pet,
                original,
                "active",
                &PrescriptionChange::Pause,
                "again"
            )
            .await
            .unwrap()
            .is_none()
        );
        update_prescription(
            &db,
            1,
            "user:1",
            &pet,
            original,
            "paused",
            &PrescriptionChange::Resume,
            "Back on",
        )
        .await
        .unwrap()
        .unwrap();

        // Dose change: old row stopped, new active row carries unchanged fields.
        let changed = update_prescription(
            &db,
            1,
            "user:1",
            &pet,
            original,
            "active",
            &PrescriptionChange::ChangeDose {
                dose_value: Some(0.5),
                dose_unit: None,
                frequency: None,
                concentration_value: None,
                concentration_unit: None,
                instructions: None,
                effective_on: Some(today),
            },
            "Vet upped it to 0.5 ml",
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(changed.current_prescription_id, original);
        let old = get_prescription(&db, 1, pet_id, original)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.status, "stopped");
        assert_eq!(old.ended_on.as_deref(), Some(today.to_string().as_str()));
        let new = get_prescription(&db, 1, pet_id, changed.current_prescription_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(new.status, "active");
        assert_eq!(new.dose_value, Some(0.5));
        assert_eq!(new.dose_unit.as_deref(), Some("ml"));
        assert_eq!(new.frequency.as_deref(), Some("once daily"));
        assert_eq!(new.concentration_value, Some(4.0));
        assert_eq!(new.route.as_deref(), Some("oral"));

        // A confirmed historical stop with no reliable effective date must
        // leave ended_on NULL, and its event must be recorded_only.
        let unknown_stop = update_prescription(
            &db,
            1,
            "user:1",
            &pet,
            changed.current_prescription_id,
            "active",
            &PrescriptionChange::Stop { ended_on: None },
            "It stopped some time ago; exact date unknown",
        )
        .await
        .unwrap()
        .unwrap();
        let stopped = get_prescription(&db, 1, pet_id, unknown_stop.current_prescription_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stopped.status, "stopped");
        assert_eq!(stopped.ended_on, None);
        let event = get_event(&db, 1, pet_id, unknown_stop.event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.temporal_precision, "recorded_only");
        assert!(event.details.as_deref().unwrap_or("").contains("date unknown"));

        // Another household cannot touch it.
        assert!(
            update_prescription(
                &db,
                2,
                "user:2",
                &pet,
                changed.current_prescription_id,
                "active",
                &PrescriptionChange::Stop { ended_on: Some(today) },
                "x"
            )
            .await
            .unwrap()
            .is_none()
        );

        // Undo a dose and an adherence entry; other households cannot.
        let dose = create_medication_administration(
            &db,
            1,
            "user:1",
            &pet,
            "Cerenia",
            None,
            Some(1.0),
            Some("tablet"),
            None,
            Utc::now(),
            None,
            "given",
            Some("Gave Cerenia"),
        )
        .await
        .unwrap();
        let adherence = create_medication_adherence(
            &db,
            1,
            "user:1",
            &pet,
            &new,
            &today.to_string(),
            None,
            None,
            "given",
            None,
            Some("Given"),
        )
        .await
        .unwrap();
        assert!(
            delete_medication_entry(&db, 2, "user:2", "dose", dose)
                .await
                .unwrap()
                .is_none()
        );
        let removed = delete_medication_entry(&db, 1, "user:1", "dose", dose)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removed["name"], "Cerenia");
        assert!(
            delete_medication_entry(&db, 1, "user:1", "dose", dose)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            delete_medication_entry(&db, 1, "user:1", "adherence", adherence)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            delete_medication_entry(&db, 1, "user:1", "prescription", original)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn records_and_deletes_assistant_lab_report_within_household() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Velcro", "Cat", None, None)
            .await
            .unwrap();
        let results = vec![crate::ocr::ParsedLabResult {
            test_name: "CREA".into(),
            value_text: "1.8".into(),
            value_numeric: Some(1.8),
            unit: Some("mg/dL".into()),
            reference_range: Some("0.8 - 2.4".into()),
            flag: None,
        }];
        let report_id = record_lab_report(
            &db,
            1,
            "user:1",
            pet_id,
            "Blood test (2026-11-02) via assistant",
            "hash-1",
            "CREA: 1.8 mg/dL",
            "2026-11-02",
            &results,
        )
        .await
        .unwrap();
        let reports = list_lab_reports(&db, 1, pet_id).await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].parse_status, "assistant_transcribed");
        assert_eq!(reports[0].test_date.as_deref(), Some("2026-11-02"));
        assert_eq!(reports[0].results[0].unit.as_deref(), Some("mg/dL"));
        assert!(report_hash_exists(&db, 1, "hash-1").await.unwrap());
        assert!(
            !delete_lab_report(&db, 2, "user:2", report_id)
                .await
                .unwrap()
        );
        assert!(
            delete_lab_report(&db, 1, "user:1", report_id)
                .await
                .unwrap()
        );
        assert!(list_lab_reports(&db, 1, pet_id).await.unwrap().is_empty());
        assert!(!report_hash_exists(&db, 1, "hash-1").await.unwrap());
    }

    #[tokio::test]
    async fn creates_and_undoes_tenant_scoped_event() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let event_id = create_health_event(
            &db,
            1,
            "user:1",
            &pet,
            &ProposedEvent {
                pet_name: "Milo".into(),
                event_type: "symptom".into(),
                concept: "vomiting".into(),
                summary: "Vomited".into(),
                details: None,
                minutes_ago: None,
            },
            "Milo vomited just now",
            Utc::now(),
            "owner_agent",
        )
        .await
        .unwrap();
        assert_eq!(
            list_events(&db, 1, Some(pet_id), 20).await.unwrap().len(),
            1
        );
        assert!(!undo_event(&db, 99, "user:99", event_id).await.unwrap());
        assert!(undo_event(&db, 1, "user:1", event_id).await.unwrap());
        assert!(
            list_events(&db, 1, Some(pet_id), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pet_initial_weight_is_also_a_timeline_weight_record() {
        let db = test_db().await;
        let pet_id = create_pet(
            &db,
            1,
            "user:1",
            "Velcro",
            "Cat",
            Some("Domestic Shorthair"),
            Some(3.2),
        )
        .await
        .unwrap();

        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        assert_eq!(pet.weight_kg, Some(3.2));
        let weights = list_weights(&db, 1, pet_id).await.unwrap();
        assert_eq!(weights.len(), 1);
        assert_eq!(weights[0].weight_kg, 3.2);
        assert_eq!(weights[0].note.as_deref(), Some("Initial weight"));
    }

    #[tokio::test]
    async fn manually_edits_only_the_tenant_scoped_event_label() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Velcro", "Cat", None, None)
            .await
            .unwrap();
        let other_pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let raw_input = "Velcro vomited after breakfast";
        let event_id = create_health_event(
            &db,
            1,
            "user:1",
            &pet,
            &ProposedEvent {
                pet_name: "Velcro".into(),
                event_type: "symptom".into(),
                concept: "vomiting".into(),
                summary: "Vomited".into(),
                details: Some("After breakfast".into()),
                minutes_ago: None,
            },
            raw_input,
            Utc::now(),
            "owner_agent",
        )
        .await
        .unwrap();

        assert!(
            !update_event_summary(&db, 99, pet_id, "user:99", event_id, "Wrong household")
                .await
                .unwrap()
        );
        assert!(
            !update_event_summary(&db, 1, other_pet_id, "user:1", event_id, "Wrong pet")
                .await
                .unwrap()
        );
        assert!(
            update_event_summary(
                &db,
                1,
                pet_id,
                "user:1",
                event_id,
                "Vomited after breakfast",
            )
            .await
            .unwrap()
        );

        let event = get_event(&db, 1, pet_id, event_id).await.unwrap().unwrap();
        assert_eq!(event.summary, "Vomited after breakfast");
        assert_eq!(event.raw_input, raw_input);
        assert_eq!(event.event_type, "symptom");
        assert_eq!(event.concept, "vomiting");
        assert_eq!(event.details.as_deref(), Some("After breakfast"));

        let audit = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT action FROM audit_events WHERE household_id=? AND record_type='health_event' AND record_id=? AND action='event.summary.updated'",
                [1.into(), event_id.into()],
            ))
            .await
            .unwrap();
        assert!(audit.is_some());
    }

    #[tokio::test]
    async fn clinical_timeline_keeps_symptom_facts_and_links_recent_medication() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let medication_at = Utc::now() - Duration::minutes(30);
        let medication_id = create_medication_administration(
            &db,
            1,
            "user:1",
            &pet,
            "anti-nausea medicine",
            None,
            Some(8.0),
            Some("mg"),
            Some("oral"),
            medication_at,
            None,
            "given",
            Some("Gave an anti-nausea medicine before vomiting"),
        )
        .await
        .unwrap();
        let event_id = create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited again earlier",
            medication_at + Duration::minutes(30),
            "vomiting",
            Some(1),
            Some("small"),
            Some("food"),
            Some("after meal"),
            Some("drank"),
            Some("normal"),
            Some("normal"),
            Some("unknown"),
            Some("Owner recorded the episode"),
            "owner_form",
        )
        .await
        .unwrap();

        let timeline = clinical_timeline(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(timeline.events[0].id, event_id);
        assert_eq!(timeline.events[0].raw_input, "Milo vomited again earlier");
        assert_eq!(
            timeline.events[0]
                .symptom
                .as_ref()
                .unwrap()
                .contents
                .as_deref(),
            Some("food")
        );
        assert_eq!(timeline.medications[0].id, medication_id);
        assert_eq!(timeline.temporal_links.len(), 1);
        assert_eq!(timeline.temporal_links[0].minutes_after_medication, 30);
        assert!(
            clinical_timeline(&db, 2, pet_id, 20)
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn medication_plan_keeps_prescription_and_adherence_gaps_tenant_scoped() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let prescription_id = create_medication_prescription(
            &db,
            1,
            "user:1",
            &pet,
            "daily medicine",
            None,
            None,
            None,
            Some(1.0),
            Some("mg"),
            Some("once daily"),
            Some("by mouth"),
            Some("Split into two halves and give with treats"),
            None,
            "active",
            Some("Daily medicine; split into two halves and give with treats"),
        )
        .await
        .unwrap();
        let prescription = get_prescription(&db, 1, pet_id, prescription_id)
            .await
            .unwrap()
            .unwrap();
        let adherence_id = create_medication_adherence(
            &db,
            1,
            "user:1",
            &pet,
            &prescription,
            "2026-01-02",
            Some(0.5),
            Some("mg"),
            "partial",
            Some("Only half of the dose was eaten"),
            Some("Over the last few days, only half of the dose was eaten"),
        )
        .await
        .unwrap();
        let plan = medication_plan(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(plan.prescriptions[0].id, prescription_id);
        assert_eq!(plan.prescriptions[0].dose_value, Some(1.0));
        assert_eq!(plan.adherence[0].id, adherence_id);
        assert_eq!(plan.adherence[0].status, "partial");
        assert_eq!(plan.adherence[0].actual_dose_value, Some(0.5));
        assert!(
            medication_plan(&db, 2, pet_id, 20)
                .await
                .unwrap()
                .prescriptions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn confirmed_medication_change_is_scoped_audited_and_idempotent() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Velcro", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        create_medication_prescription(
            &db,
            1,
            "user:1",
            &pet,
            "Apelka",
            Some("thiamazole"),
            Some(5.0),
            Some("mg/mL"),
            Some(0.5),
            Some("mL"),
            Some("twice daily"),
            Some("by mouth"),
            None,
            Some("2026-01-01"),
            "active",
            Some("Previous Apelka plan"),
        )
        .await
        .unwrap();
        let raw = "deciding to change to just give 0.25ml Apelka 1x per day because she has been throwing up before";
        let first = confirm_medication_plan_change(
            &db,
            1,
            "user:1",
            &pet,
            "hashed-confirmation-token",
            "Apelka",
            0.25,
            "mL",
            "once daily",
            Some("she has been throwing up before"),
            raw,
        )
        .await
        .unwrap();
        assert!(!first.already_applied);
        assert_eq!(first.replaced_prescriptions, 1);

        let prescriptions = list_prescriptions(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(prescriptions.len(), 2);
        assert_eq!(prescriptions[0].status, "active");
        assert_eq!(prescriptions[0].dose_value, Some(0.25));
        assert_eq!(prescriptions[0].frequency.as_deref(), Some("once daily"));
        assert_eq!(
            prescriptions[0].active_ingredient.as_deref(),
            Some("thiamazole")
        );
        assert_eq!(prescriptions[0].concentration_value, Some(5.0));
        assert_eq!(prescriptions[1].status, "stopped");
        assert!(prescriptions[1].ended_on.is_some());

        let events = list_events(&db, 1, Some(pet_id), 20).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].concept, "medication_plan_change");
        assert_eq!(events[0].raw_input, raw);
        assert!(
            events[0]
                .details
                .as_deref()
                .unwrap()
                .contains("throwing up")
        );

        let replay = confirm_medication_plan_change(
            &db,
            1,
            "user:1",
            &pet,
            "hashed-confirmation-token",
            "Apelka",
            0.25,
            "mL",
            "once daily",
            Some("she has been throwing up before"),
            raw,
        )
        .await
        .unwrap();
        assert!(replay.already_applied);
        assert_eq!(
            list_prescriptions(&db, 1, pet_id, 20).await.unwrap().len(),
            2
        );
        assert_eq!(
            list_events(&db, 1, Some(pet_id), 20).await.unwrap().len(),
            1
        );

        assert!(
            confirm_medication_plan_change(
                &db,
                2,
                "user:2",
                &pet,
                "another-token",
                "Apelka",
                1.0,
                "mL",
                "daily",
                None,
                "out of scope",
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn confirmed_medication_stop_is_dated_scoped_and_idempotent() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Velcro", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        create_medication_prescription(
            &db,
            1,
            "user:1",
            &pet,
            "Apelka",
            Some("thiamazole"),
            Some(5.0),
            Some("mg/mL"),
            Some(0.25),
            Some("mL"),
            Some("once daily"),
            Some("by mouth"),
            None,
            Some("2026-01-01"),
            "active",
            Some("Previous Apelka plan"),
        )
        .await
        .unwrap();
        let raw = "we switched from alpeka to just thyroid food on 6th august";
        let occurred_on = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let first = confirm_medication_plan_stop(
            &db,
            1,
            "user:1",
            &pet,
            "hashed-stop-token",
            "Apelka",
            "thyroid food",
            occurred_on,
            raw,
        )
        .await
        .unwrap();
        assert_eq!(first.stopped_prescriptions, 1);
        assert!(!first.already_applied);

        let prescriptions = list_prescriptions(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(prescriptions[0].status, "stopped");
        assert_eq!(prescriptions[0].ended_on.as_deref(), Some("2026-08-06"));
        let events = list_events(&db, 1, Some(pet_id), 20).await.unwrap();
        assert_eq!(events[0].concept, "medication_plan_stop");
        assert_eq!(events[0].occurred_at.date_naive(), occurred_on);
        assert_eq!(events[0].raw_input, raw);

        let replay = confirm_medication_plan_stop(
            &db,
            1,
            "user:1",
            &pet,
            "hashed-stop-token",
            "Apelka",
            "thyroid food",
            occurred_on,
            raw,
        )
        .await
        .unwrap();
        assert!(replay.already_applied);
        assert_eq!(
            list_events(&db, 1, Some(pet_id), 20).await.unwrap().len(),
            1
        );

        assert!(
            confirm_medication_plan_stop(
                &db,
                2,
                "user:2",
                &pet,
                "another-stop-token",
                "Apelka",
                "thyroid food",
                occurred_on,
                "out of scope",
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn accounts_sessions_and_pets_are_household_scoped() {
        let db = test_db().await;
        let alice = create_account(&db, "alice@example.com", "Alice", "hash-a")
            .await
            .unwrap();
        let bob = create_account(&db, "bob@example.com", "Bob", "hash-b")
            .await
            .unwrap();
        let pet_id = create_pet(
            &db,
            alice.household_id,
            &alice.audit_actor(),
            "Milo",
            "Cat",
            None,
            None,
        )
        .await
        .unwrap();

        assert!(
            get_pet(&db, bob.household_id, pet_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(list_pets(&db, bob.household_id).await.unwrap().is_empty());

        let token = create_session(&db, alice.id).await.unwrap();
        let resolved = resolve_session(&db, &token).await.unwrap().unwrap();
        assert_eq!(resolved.id, alice.id);
        assert_eq!(resolved.household_id, alice.household_id);
        update_password_and_revoke_sessions(&db, &alice, "new-hash")
            .await
            .unwrap();
        assert!(resolve_session(&db, &token).await.unwrap().is_none());
        let (_, stored_hash) = user_for_login(&db, "alice@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_hash, "new-hash");

        let second_token = create_session(&db, alice.id).await.unwrap();
        revoke_session(&db, &second_token).await.unwrap();
        assert!(resolve_session(&db, &second_token).await.unwrap().is_none());
    }

    fn timeline_entry_id(entry: &TimelineEntry) -> i64 {
        match entry {
            TimelineEntry::Event(event) => event.id,
            TimelineEntry::Weight(weight) => weight.id,
            TimelineEntry::Dose(dose) => dose.id,
            TimelineEntry::Lab(lab) => lab.id,
        }
    }

    #[tokio::test]
    async fn list_timeline_merges_all_sources_in_time_order() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();

        // Four calendar days apart so the lab report's date-only (no time of
        // day) sort key can't collide with the others' full timestamps.
        let event_at = Utc::now() - Duration::days(4);
        let weight_at = Utc::now() - Duration::days(3);
        let dose_at = Utc::now() - Duration::days(2);
        let lab_date = (Utc::now() - Duration::days(1)).date_naive().to_string();

        let event_id = create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited",
            event_at,
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();
        let weight_id = create_weight(&db, 1, "user:1", pet_id, 4.8, &weight_at.to_rfc3339(), None)
            .await
            .unwrap();
        let dose_id = create_medication_administration(
            &db,
            1,
            "user:1",
            &pet,
            "Benazepril",
            None,
            Some(1.0),
            Some("mg"),
            Some("oral"),
            dose_at,
            None,
            "given",
            Some("Gave benazepril"),
        )
        .await
        .unwrap();
        let lab_id = create_lab_report(
            &db,
            1,
            "user:1",
            pet_id,
            "panel.pdf",
            "hash-1",
            "raw ocr text",
            Some(&lab_date),
            &[],
        )
        .await
        .unwrap();

        let entries = list_timeline(&db, 1, pet_id, None, 20).await.unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], TimelineEntry::Lab(lab) if lab.id == lab_id));
        assert!(matches!(&entries[1], TimelineEntry::Dose(dose) if dose.id == dose_id));
        assert!(matches!(&entries[2], TimelineEntry::Weight(weight) if weight.id == weight_id));
        assert!(matches!(&entries[3], TimelineEntry::Event(event) if event.id == event_id));
    }

    #[tokio::test]
    async fn list_timeline_never_leaks_another_households_rows() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited",
            Utc::now(),
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();
        create_weight(
            &db,
            1,
            "user:1",
            pet_id,
            4.8,
            &Utc::now().to_rfc3339(),
            None,
        )
        .await
        .unwrap();
        create_medication_administration(
            &db,
            1,
            "user:1",
            &pet,
            "Benazepril",
            None,
            Some(1.0),
            Some("mg"),
            Some("oral"),
            Utc::now(),
            None,
            "given",
            None,
        )
        .await
        .unwrap();
        create_lab_report(
            &db,
            1,
            "user:1",
            pet_id,
            "panel.pdf",
            "hash-1",
            "raw ocr text",
            Some("2026-01-01"),
            &[],
        )
        .await
        .unwrap();

        // A second, genuinely separate household (`households` has a foreign key
        // households(id), so this must be a real row, not a bare literal `2`).
        let other_household = create_account(&db, "other@example.com", "Other Owner", "hash")
            .await
            .unwrap();
        let other_household_id = other_household.household_id;
        let other_pet_id = create_pet(&db, other_household_id, "user:2", "Nala", "Dog", None, None)
            .await
            .unwrap();
        let other_pet = get_pet(&db, other_household_id, other_pet_id)
            .await
            .unwrap()
            .unwrap();
        create_symptom_event(
            &db,
            other_household_id,
            "user:2",
            &other_pet,
            "Nala scratched",
            Utc::now(),
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();
        create_weight(
            &db,
            other_household_id,
            "user:2",
            other_pet_id,
            9.0,
            &Utc::now().to_rfc3339(),
            None,
        )
        .await
        .unwrap();
        create_medication_administration(
            &db,
            other_household_id,
            "user:2",
            &other_pet,
            "Other medicine",
            None,
            Some(2.0),
            Some("mg"),
            Some("oral"),
            Utc::now(),
            None,
            "given",
            None,
        )
        .await
        .unwrap();
        create_lab_report(
            &db,
            other_household_id,
            "user:2",
            other_pet_id,
            "other.pdf",
            "hash-2",
            "raw ocr text",
            Some("2026-01-01"),
            &[],
        )
        .await
        .unwrap();

        assert_eq!(
            list_timeline(&db, 1, pet_id, None, 20).await.unwrap().len(),
            4
        );
        assert_eq!(
            list_timeline(&db, other_household_id, other_pet_id, None, 20)
                .await
                .unwrap()
                .len(),
            4
        );
        // Household 1 asking about the other household's pet id must come back
        // empty, not fall through to an unscoped match.
        assert!(
            list_timeline(&db, 1, other_pet_id, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_timeline(&db, other_household_id, pet_id, None, 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn get_event_is_scoped_to_household_and_pet() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited",
            Utc::now(),
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();
        let event_id = list_events(&db, 1, Some(pet_id), 1)
            .await
            .unwrap()
            .first()
            .unwrap()
            .id;

        assert!(get_event(&db, 1, pet_id, event_id).await.unwrap().is_some());

        let other_household =
            create_account(&db, "other-drawer@example.com", "Other Owner", "hash")
                .await
                .unwrap();
        let other_household_id = other_household.household_id;
        let other_pet_id = create_pet(&db, other_household_id, "user:2", "Nala", "Dog", None, None)
            .await
            .unwrap();
        // Wrong household entirely.
        assert!(
            get_event(&db, other_household_id, pet_id, event_id)
                .await
                .unwrap()
                .is_none()
        );
        // Right household, but a pet id that belongs to someone else's pet.
        assert!(
            get_event(&db, 1, other_pet_id, event_id)
                .await
                .unwrap()
                .is_none()
        );

        undo_event(&db, 1, "user:1", event_id).await.unwrap();
        // Undone events drop out of the drawer just like they drop out of the
        // timeline, so an old drawer link 404s instead of showing stale detail.
        assert!(get_event(&db, 1, pet_id, event_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_timeline_keyset_pagination_handles_ties_without_skip_or_duplicate() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let tied_at = Utc::now() - Duration::days(1);
        // Two weight entries sharing the exact same measured_at timestamp.
        let w1 = create_weight(&db, 1, "user:1", pet_id, 4.7, &tied_at.to_rfc3339(), None)
            .await
            .unwrap();
        let w2 = create_weight(&db, 1, "user:1", pet_id, 4.8, &tied_at.to_rfc3339(), None)
            .await
            .unwrap();
        let event_id = create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited",
            Utc::now(),
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();

        let page1 = list_timeline(&db, 1, pet_id, None, 2).await.unwrap();
        assert_eq!(page1.len(), 2);
        let cursor = page1.last().unwrap().sort_key();
        let page2 = list_timeline(&db, 1, pet_id, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);

        let mut seen_ids: Vec<i64> = page1
            .iter()
            .chain(page2.iter())
            .map(timeline_entry_id)
            .collect();
        seen_ids.sort_unstable();
        let mut expected = vec![event_id, w1, w2];
        expected.sort_unstable();
        assert_eq!(seen_ids, expected);

        // A third page beyond the data returns nothing, not a repeat.
        let cursor2 = page2.last().unwrap().sort_key();
        assert!(
            list_timeline(&db, 1, pet_id, Some(cursor2), 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn list_timeline_excludes_undone_events() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let event_id = create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited",
            Utc::now(),
            "vomiting",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "owner_form",
        )
        .await
        .unwrap();
        let weight_id = create_weight(
            &db,
            1,
            "user:1",
            pet_id,
            4.8,
            &Utc::now().to_rfc3339(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            list_timeline(&db, 1, pet_id, None, 20).await.unwrap().len(),
            2
        );
        assert!(undo_event(&db, 1, "user:1", event_id).await.unwrap());
        let entries = list_timeline(&db, 1, pet_id, None, 20).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], TimelineEntry::Weight(weight) if weight.id == weight_id));
    }

    async fn test_user(db: &DatabaseConnection) -> UserAccount {
        let hash = auth::hash_password("correct horse battery staple".into())
            .await
            .unwrap();
        create_account(db, "probe@example.com", "Probe", &hash)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn api_token_lifecycle_scoped_and_revocable() {
        let db = test_db().await;
        let user = test_user(&db).await;
        // Wrong prefix never resolves (falls through to other auth).
        assert!(
            resolve_api_token(&db, "not-a-token")
                .await
                .unwrap()
                .is_none()
        );
        let (id, token) = create_api_token(&db, user.id, user.household_id, "laptop")
            .await
            .unwrap();
        assert!(token.starts_with("phj_"));
        let resolved = resolve_api_token(&db, &token).await.unwrap().unwrap();
        assert_eq!(resolved.id, user.id);
        assert_eq!(resolved.household_id, user.household_id);
        let listed = list_api_tokens(&db, user.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "laptop");
        // Another household cannot revoke it.
        assert!(
            !revoke_api_token(&db, user.household_id + 999, user.id, id)
                .await
                .unwrap()
        );
        assert!(resolve_api_token(&db, &token).await.unwrap().is_some());
        // Owner revokes: gone from listings and resolution.
        assert!(
            revoke_api_token(&db, user.household_id, user.id, id)
                .await
                .unwrap()
        );
        assert!(list_api_tokens(&db, user.id).await.unwrap().is_empty());
        assert!(resolve_api_token(&db, &token).await.unwrap().is_none());
        // Double revoke is a no-op, not an error.
        assert!(
            !revoke_api_token(&db, user.household_id, user.id, id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn oauth_state_is_single_use_and_expires() {
        let db = test_db().await;
        create_oauth_state(&db, "good-state", Some("/app"), 10)
            .await
            .unwrap();
        assert_eq!(
            consume_oauth_state(&db, "good-state").await.unwrap(),
            Some("/app".to_owned())
        );
        // Replay fails: the row was deleted on first consume.
        assert!(
            consume_oauth_state(&db, "good-state")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            consume_oauth_state(&db, "unknown-state")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn google_link_or_create_prefers_sub_then_email() {
        let db = test_db().await;
        let first = find_or_create_google_user(&db, "sub-1", "g@example.com", "Gee")
            .await
            .unwrap();
        // Same sub, different email claim: same user (sub is authoritative).
        let same = find_or_create_google_user(&db, "sub-1", "other@example.com", "Gee")
            .await
            .unwrap();
        assert_eq!(first.id, same.id);
        // New sub with the verified email: links to the existing household.
        let linked = find_or_create_google_user(&db, "sub-2", "g@example.com", "Gee")
            .await
            .unwrap();
        assert_eq!(linked.household_id, first.household_id);
        // Unknown sub + unknown email: brand-new household.
        let fresh = find_or_create_google_user(&db, "sub-9", "new@example.com", "New")
            .await
            .unwrap();
        assert_ne!(fresh.household_id, first.household_id);
    }
}
