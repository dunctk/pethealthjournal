//! Comprehensive unit, integration, migration, isolation, and HTMX web tests
//! for the Food & Nutrition Diary.

use crate::{
    AppState, agent, db,
    nutrition::{
        self, FoodEntryFilter, FoodIntakePatch, FoodItemPatch, NewFoodIntakeItem, NewFoodItem,
        NewNutritionTarget, NutritionSnapshot, compute_entry_nutrition, summarize_day,
    },
    web,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use chrono::{Duration, NaiveDate, Utc};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use std::sync::Arc;
use tower::ServiceExt;

fn snap_renal_pouch() -> NutritionSnapshot {
    NutritionSnapshot {
        food_name: "Renal Chicken Loaf".into(),
        brand: Some("Acme Vet".into()),
        variant: Some("85g Pouch".into()),
        category: Some("prescription_diet".into()),
        completeness: Some("complete".into()),
        serving_size_value: Some(85.0),
        serving_size_unit: Some("g".into()),
        serving_label: Some("pouch".into()),
        nutrition_basis: Some("per_100g".into()),
        kcal_per_basis: Some(100.0),
        protein_g: Some(7.0),
        fat_g: Some(6.0),
        phosphorus_mg: Some(90.0),
        sodium_mg: Some(110.0),
        moisture_pct: Some(79.0),
        nutrition_confidence: "verified".into(),
        nutrition_source: Some("Manufacturer pouch label".into()),
    }
}

#[test]
fn unit_conversions_and_partial_servings_are_deterministic() {
    let renal = snap_renal_pouch();
    // Half an 85g pouch = 42.5g -> 42.5 kcal, 2.98g protein, 38.25mg phosphorus.
    let half = compute_entry_nutrition("consumed_partial", Some(0.5), Some("pouch"), false, &renal);
    assert_eq!(half.calculation_status, "computed");
    assert_eq!(half.kcal, Some(42.5));
    assert_eq!(half.protein_g, Some(2.98));
    assert_eq!(half.phosphorus_mg, Some(38.25));
    assert_eq!(half.sodium_mg, Some(46.75));

    // Direct grams & ounces on per_100g.
    let grams = compute_entry_nutrition("consumed_all", Some(25.0), Some("g"), true, &renal);
    assert_eq!(grams.calculation_status, "estimated"); // approximate quantity
    assert_eq!(grams.kcal, Some(25.0));

    let oz = compute_entry_nutrition("consumed_all", Some(1.0), Some("oz"), false, &renal);
    assert_eq!(oz.calculation_status, "computed");
    assert!((oz.kcal.unwrap() - 28.35).abs() < 0.02);

    // Liquid per_100ml with tsp (5 ml) and ml.
    let cream = NutritionSnapshot {
        food_name: "Heavy Cream".into(),
        nutrition_basis: Some("per_100ml".into()),
        kcal_per_basis: Some(340.0),
        fat_g: Some(36.0),
        nutrition_confidence: "estimated".into(),
        nutrition_source: Some("USDA FoodData Central".into()),
        ..Default::default()
    };
    let tsp = compute_entry_nutrition("consumed_all", Some(1.0), Some("tsp"), false, &cream);
    assert_eq!(tsp.calculation_status, "estimated");
    assert_eq!(tsp.kcal, Some(17.0));
    assert_eq!(tsp.fat_g, Some(1.8));

    // Per-serving treat (e.g. 1 Churu tube = 6 kcal).
    let treat = NutritionSnapshot {
        food_name: "Churu Tuna".into(),
        serving_label: Some("tube".into()),
        serving_size_value: Some(14.0),
        serving_size_unit: Some("g".into()),
        nutrition_basis: Some("per_serving".into()),
        kcal_per_basis: Some(6.0),
        protein_g: Some(1.0),
        nutrition_confidence: "verified".into(),
        nutrition_source: Some("Package label".into()),
        ..Default::default()
    };
    let one_tube = compute_entry_nutrition("consumed_all", Some(1.0), Some("tube"), false, &treat);
    assert_eq!(one_tube.calculation_status, "computed");
    assert_eq!(one_tube.kcal, Some(6.0));
    // Half weight in grams converts to 0.5 servings when serving_size_value=14g is explicit.
    let seven_g = compute_entry_nutrition("consumed_partial", Some(7.0), Some("g"), false, &treat);
    assert_eq!(seven_g.calculation_status, "computed");
    assert_eq!(seven_g.kcal, Some(3.0));
}

#[test]
fn incompatible_bases_and_dry_matter_without_moisture_are_never_silently_mixed() {
    let renal = snap_renal_pouch();
    // Volume on per_100g -> incompatible_basis
    let bad_vol = compute_entry_nutrition("consumed_all", Some(30.0), Some("ml"), false, &renal);
    assert_eq!(bad_vol.calculation_status, "incompatible_basis");
    assert_eq!(bad_vol.kcal, None);

    // Pouch without serving_size_value -> never assume universal pouch size!
    let mut no_size = snap_renal_pouch();
    no_size.serving_size_value = None;
    let unknown_pouch =
        compute_entry_nutrition("consumed_all", Some(1.0), Some("pouch"), false, &no_size);
    assert_eq!(unknown_pouch.calculation_status, "incompatible_basis");
    assert_eq!(unknown_pouch.kcal, None);

    // Dry-matter basis WITHOUT moisture_pct refuses as-fed grams!
    let dm_no_moisture = NutritionSnapshot {
        food_name: "Prescription Dry-Matter Label".into(),
        nutrition_basis: Some("dry_matter".into()),
        kcal_per_basis: Some(400.0),
        phosphorus_mg: Some(500.0),
        moisture_pct: None,
        nutrition_confidence: "verified".into(),
        ..Default::default()
    };
    let as_fed = compute_entry_nutrition(
        "consumed_all",
        Some(50.0),
        Some("g"),
        false,
        &dm_no_moisture,
    );
    assert_eq!(as_fed.calculation_status, "incompatible_basis");
    assert_eq!(as_fed.kcal, None);

    // Direct dry-matter grams works without moisture_pct.
    let direct_dm = compute_entry_nutrition(
        "consumed_all",
        Some(20.0),
        Some("g_dm"),
        false,
        &dm_no_moisture,
    );
    assert_eq!(direct_dm.calculation_status, "computed");
    assert_eq!(direct_dm.kcal, Some(80.0));
    assert_eq!(direct_dm.phosphorus_mg, Some(100.0));

    // Dry-matter WITH explicit moisture_pct (80% moisture -> 20% DM) converts transparently with note.
    let mut dm_with_moisture = dm_no_moisture;
    dm_with_moisture.moisture_pct = Some(80.0);
    let converted = compute_entry_nutrition(
        "consumed_all",
        Some(100.0),
        Some("g"),
        false,
        &dm_with_moisture,
    );
    assert_eq!(converted.calculation_status, "computed");
    assert_eq!(converted.kcal, Some(80.0));
    assert!(
        converted
            .calculation_note
            .as_deref()
            .unwrap_or("")
            .contains("80.0% moisture")
    );
}

#[test]
fn refusal_unknown_quantity_unknown_nutrition_and_not_offered_stay_distinct() {
    let renal = snap_renal_pouch();
    let refused = compute_entry_nutrition("refused", Some(0.0), Some("pouch"), false, &renal);
    assert_eq!(refused.calculation_status, "zero_refused");
    assert_eq!(refused.kcal, Some(0.0));

    let unknown_qty = compute_entry_nutrition("consumed_unknown_amount", None, None, false, &renal);
    assert_eq!(unknown_qty.calculation_status, "unknown_amount");
    assert_eq!(unknown_qty.kcal, None); // NEVER 0!

    let not_offered = compute_entry_nutrition("not_offered", None, None, false, &renal);
    assert_eq!(not_offered.calculation_status, "not_offered");
    assert_eq!(not_offered.kcal, None);

    let unknown_food = NutritionSnapshot {
        food_name: "Mystery homemade broth".into(),
        nutrition_confidence: "unknown".into(),
        ..Default::default()
    };
    let no_nutr =
        compute_entry_nutrition("consumed_all", Some(30.0), Some("ml"), false, &unknown_food);
    assert_eq!(no_nutr.calculation_status, "unknown_nutrition");
    assert_eq!(no_nutr.kcal, None);
}

#[tokio::test]
async fn atomic_meal_snapshots_idempotency_corrections_and_trends() {
    let database = db::connect("sqlite::memory:").await.unwrap();
    db::migrate(&database).await.unwrap();
    let user = db::create_account(&database, "owner@test.local", "Owner", "hash")
        .await
        .unwrap();
    let pet_id = db::create_pet(
        &database,
        user.household_id,
        &user.audit_actor(),
        "Milo",
        "Cat",
        Some("DSH"),
        Some(4.2),
    )
    .await
    .unwrap();

    // 1. Create reusable catalogue foods.
    let renal = nutrition::create_food(
        &database,
        user.household_id,
        &user.audit_actor(),
        NewFoodItem {
            name: "Renal Wet Pouch".into(),
            brand: Some("VetDiet".into()),
            category: "prescription_diet".into(),
            completeness: "complete".into(),
            serving_size_value: Some(85.0),
            serving_size_unit: Some("g".into()),
            serving_label: Some("pouch".into()),
            nutrition_basis: Some("per_100g".into()),
            kcal_per_basis: Some(100.0),
            protein_g: Some(7.0),
            fat_g: Some(6.0),
            phosphorus_mg: Some(90.0),
            sodium_mg: Some(100.0),
            nutrition_confidence: "verified".into(),
            nutrition_source: Some("Manufacturer label".into()),
            is_favorite: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let churu = nutrition::create_food(
        &database,
        user.household_id,
        &user.audit_actor(),
        NewFoodItem {
            name: "Churu".into(),
            category: "treat".into(),
            completeness: "complementary".into(),
            serving_label: Some("tube".into()),
            nutrition_basis: Some("per_serving".into()),
            kcal_per_basis: Some(6.0),
            nutrition_confidence: "verified".into(),
            nutrition_source: Some("Package label".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let today = Utc::now().date_naive();
    let d0 = today.format("%Y-%m-%d").to_string();
    let d1 = (today - Duration::days(1)).format("%Y-%m-%d").to_string();
    let d2 = (today - Duration::days(2)).format("%Y-%m-%d").to_string();

    // 2. Atomic multi-item meal with idempotency key.
    let meal = nutrition::record_meal_entries(
        &database,
        user.household_id,
        pet_id,
        &user.audit_actor(),
        "mcp",
        &d0,
        None,
        "date_only",
        Some("breakfast"),
        Some("retry-key-1"),
        vec![
            NewFoodIntakeItem {
                food_item_id: Some(renal.id),
                intake_status: "consumed_partial".into(),
                appetite: Some("normal".into()),
                offered_amount: Some(1.0),
                offered_unit: Some("pouch".into()),
                consumed_amount: Some(0.5),
                consumed_unit: Some("pouch".into()),
                original_wording: "Milo ate half a pouch of renal food and a Churu".into(),
                ..Default::default()
            },
            NewFoodIntakeItem {
                food_item_id: Some(churu.id),
                intake_status: "consumed_all".into(),
                appetite: Some("eager".into()),
                consumed_amount: Some(1.0),
                consumed_unit: Some("tube".into()),
                original_wording: "Milo ate half a pouch of renal food and a Churu".into(),
                ..Default::default()
            },
        ],
    )
    .await
    .unwrap();
    assert!(!meal.deduplicated);
    assert_eq!(meal.entries.len(), 2);
    assert_eq!(meal.entries[0].computed_kcal, Some(42.5));
    assert_eq!(meal.entries[1].computed_kcal, Some(6.0));
    assert_eq!(meal.entries[0].meal_group_id, meal.entries[1].meal_group_id);

    // Retry with same idempotency_key returns existing entries without duplicating!
    let retry = nutrition::record_meal_entries(
        &database,
        user.household_id,
        pet_id,
        &user.audit_actor(),
        "mcp",
        &d0,
        None,
        "date_only",
        Some("breakfast"),
        Some("retry-key-1"),
        vec![NewFoodIntakeItem {
            food_item_id: Some(churu.id),
            intake_status: "consumed_all".into(),
            consumed_amount: Some(1.0),
            consumed_unit: Some("tube".into()),
            original_wording: "duplicate retry".into(),
            ..Default::default()
        }],
    )
    .await
    .unwrap();
    assert!(retry.deduplicated);
    assert_eq!(retry.entries.len(), 2);
    assert_eq!(retry.entries[0].id, meal.entries[0].id);

    // 3. Updating catalogue food MUST preserve historical snapshot & computed kcal!
    let updated_renal = nutrition::update_food(
        &database,
        user.household_id,
        &user.audit_actor(),
        renal.id,
        FoodItemPatch {
            kcal_per_basis: Some(Some(200.0)),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(updated_renal.kcal_per_basis, Some(200.0));
    let old_entry = nutrition::get_food_entry(&database, user.household_id, meal.entries[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_entry.snapshot.kcal_per_basis, Some(100.0));
    assert_eq!(old_entry.computed_kcal, Some(42.5));

    // 4. Backdated day d1: refused breakfast + estimated chicken + unknown-nutrition snack -> partial_subtotal.
    nutrition::record_meal_entries(
        &database,
        user.household_id,
        pet_id,
        &user.audit_actor(),
        "mcp",
        &d1,
        None,
        "date_only",
        Some("breakfast"),
        None,
        vec![
            NewFoodIntakeItem {
                food_item_id: Some(renal.id),
                intake_status: "refused".into(),
                offered_amount: Some(1.0),
                offered_unit: Some("pouch".into()),
                original_wording: "Refused usual breakfast".into(),
                ..Default::default()
            },
            NewFoodIntakeItem {
                inline_snapshot: Some(NutritionSnapshot {
                    food_name: "Cooked chicken breast".into(),
                    category: Some("human_food".into()),
                    nutrition_basis: Some("per_100g".into()),
                    kcal_per_basis: Some(165.0),
                    protein_g: Some(31.0),
                    nutrition_confidence: "estimated".into(),
                    nutrition_source: Some("USDA generic cooked chicken".into()),
                    ..Default::default()
                }),
                intake_status: "consumed_all".into(),
                consumed_amount: Some(25.0),
                consumed_unit: Some("g".into()),
                is_approximate: true,
                original_wording: "Ate approximately 25g cooked chicken".into(),
                ..Default::default()
            },
            NewFoodIntakeItem {
                inline_snapshot: Some(NutritionSnapshot {
                    food_name: "Homemade fish broth".into(),
                    category: Some("custom_prep".into()),
                    nutrition_confidence: "unknown".into(),
                    ..Default::default()
                }),
                intake_status: "consumed_all".into(),
                consumed_amount: Some(20.0),
                consumed_unit: Some("ml".into()),
                original_wording: "Had 20ml homemade fish broth".into(),
                ..Default::default()
            },
        ],
    )
    .await
    .unwrap();

    // 5. Set owner/vet nutrition target and check daily summaries & rolling averages!
    let target = nutrition::set_nutrition_target(
        &database,
        user.household_id,
        pet_id,
        &user.audit_actor(),
        NewNutritionTarget {
            daily_kcal_min: Some(180.0),
            daily_kcal_max: Some(220.0),
            source: "owner_relayed_vet".into(),
            source_note: "Dr Patel 2026-11-20".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let all = nutrition::list_food_entries(
        &database,
        user.household_id,
        pet_id,
        FoodEntryFilter {
            limit: 100,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sum_d0 = summarize_day(&d0, &all.items, Some(&target));
    assert_eq!(sum_d0.kcal_completeness, "complete_for_logged_entries");
    assert_eq!(sum_d0.recorded_kcal, Some(48.5));
    assert_eq!(sum_d0.verified_kcal, 48.5);
    assert_eq!(sum_d0.estimated_kcal, 0.0);
    assert_eq!(
        sum_d0.target_comparison.as_ref().unwrap().status,
        "below_range"
    );

    let sum_d1 = summarize_day(&d1, &all.items, Some(&target));
    assert_eq!(sum_d1.kcal_completeness, "partial_subtotal");
    assert_eq!(sum_d1.refused_entries, 1);
    assert_eq!(sum_d1.missing_nutrition_entries, 1);
    assert_eq!(sum_d1.recorded_kcal, Some(41.25));
    assert_eq!(sum_d1.estimated_kcal, 41.25);
    assert_eq!(
        sum_d1.target_comparison.as_ref().unwrap().status,
        "incomplete_data"
    );

    // Unlogged day d2 is NEVER fasting/0!
    let sum_d2 = summarize_day(&d2, &all.items, Some(&target));
    assert_eq!(sum_d2.kcal_completeness, "no_entries");
    assert_eq!(sum_d2.recorded_kcal, None);

    // Rolling averages divide ONLY by logged computable days (2 days), never 7 or 30!
    let trend = nutrition::load_trend_report(
        &database,
        user.household_id,
        pet_id,
        today - Duration::days(6),
        today,
    )
    .await
    .unwrap();
    assert_eq!(trend.window_7d.days_with_entries, 2);
    assert_eq!(trend.window_7d.days_with_no_entries, 5);
    assert_eq!(trend.window_7d.days_with_complete_kcal, 1);
    assert_eq!(trend.window_7d.days_with_partial_kcal, 1);
    // (48.5 + 41.25) / 2 = 44.875 -> 44.9
    assert_eq!(trend.window_7d.avg_kcal_per_computable_day, Some(44.9));
    // Complete day only: 48.5
    assert_eq!(trend.window_7d.avg_kcal_per_complete_day, Some(48.5));

    // 6. Correction & soft-delete + restore!
    let corrected = nutrition::update_food_entry(
        &database,
        user.household_id,
        &user.audit_actor(),
        meal.entries[0].id,
        FoodIntakePatch {
            consumed_amount: Some(Some(1.0)),
            intake_status: Some("consumed_all".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    // Still uses historical snapshot (100 kcal/100g, NOT updated 200 kcal/100g!) -> 85 kcal.
    assert_eq!(corrected.computed_kcal, Some(85.0));

    let deleted = nutrition::delete_food_entry(
        &database,
        user.household_id,
        &user.audit_actor(),
        meal.entries[1].id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(deleted.status, "deleted");

    let restored = nutrition::restore_food_entry(
        &database,
        user.household_id,
        &user.audit_actor(),
        meal.entries[1].id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(restored.status, "active");
}

#[tokio::test]
async fn household_isolation_and_existing_database_migration_work() {
    // Simulate a pre-food-diary SQLite database (pets + clinical_case_updates without linked_food_entry_id).
    let database = db::connect("sqlite::memory:").await.unwrap();
    database
        .execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE households (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, created_at TEXT NOT NULL);
            CREATE TABLE pets (id INTEGER PRIMARY KEY AUTOINCREMENT, household_id INTEGER NOT NULL, name TEXT NOT NULL, species TEXT NOT NULL, breed TEXT, date_of_birth TEXT, weight_kg REAL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
            CREATE TABLE clinical_cases (id INTEGER PRIMARY KEY AUTOINCREMENT, household_id INTEGER NOT NULL, pet_id INTEGER NOT NULL, title TEXT NOT NULL, presenting_concern TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
            CREATE TABLE clinical_case_updates (id INTEGER PRIMARY KEY AUTOINCREMENT, household_id INTEGER NOT NULL, pet_id INTEGER NOT NULL, case_id INTEGER NOT NULL, kind TEXT NOT NULL, original_wording TEXT NOT NULL, observed_on TEXT, source TEXT NOT NULL, linked_event_id INTEGER, created_at TEXT NOT NULL);
            INSERT INTO households(id,name,created_at) VALUES (1,'Legacy','2026-01-01T00:00:00Z');
            INSERT INTO pets(id,household_id,name,species,created_at,updated_at) VALUES (1,1,'LegacyCat','Cat','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
            "#.to_owned(),
        ))
        .await
        .unwrap();

    // Run full safe migration over existing database.
    db::migrate(&database).await.unwrap();
    // Running migrate a second time must also be idempotent.
    db::migrate(&database).await.unwrap();

    let alice = db::create_account(&database, "alice@test.local", "Alice", "hash")
        .await
        .unwrap();
    let bob = db::create_account(&database, "bob@test.local", "Bob", "hash")
        .await
        .unwrap();
    let alice_pet = db::create_pet(
        &database,
        alice.household_id,
        &alice.audit_actor(),
        "Cleo",
        "Cat",
        None,
        None,
    )
    .await
    .unwrap();
    let bob_pet = db::create_pet(
        &database,
        bob.household_id,
        &bob.audit_actor(),
        "Rex",
        "Dog",
        None,
        None,
    )
    .await
    .unwrap();

    let alice_food = nutrition::create_food(
        &database,
        alice.household_id,
        &alice.audit_actor(),
        NewFoodItem {
            name: "Alice Secret Diet".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Bob cannot see, update, or use Alice's catalogue food.
    assert!(
        nutrition::list_foods(&database, bob.household_id, None, None, false, false, 50)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        nutrition::update_food(
            &database,
            bob.household_id,
            &bob.audit_actor(),
            alice_food.id,
            FoodItemPatch {
                name: Some("Hacked".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .is_none()
    );
    let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
    assert!(
        nutrition::record_meal_entries(
            &database,
            bob.household_id,
            bob_pet,
            &bob.audit_actor(),
            "mcp",
            &today,
            None,
            "date_only",
            None,
            None,
            vec![NewFoodIntakeItem {
                food_item_id: Some(alice_food.id),
                intake_status: "consumed_all".into(),
                original_wording: "cross-tenant food attempt".into(),
                ..Default::default()
            }],
        )
        .await
        .is_err()
    );

    // Bob cannot record onto Alice's pet.
    assert!(
        nutrition::record_meal_entries(
            &database,
            bob.household_id,
            alice_pet,
            &bob.audit_actor(),
            "mcp",
            &today,
            None,
            "date_only",
            None,
            None,
            vec![NewFoodIntakeItem {
                inline_snapshot: Some(NutritionSnapshot {
                    food_name: "Cross pet".into(),
                    nutrition_confidence: "unknown".into(),
                    ..Default::default()
                }),
                intake_status: "consumed_all".into(),
                original_wording: "cross-tenant pet attempt".into(),
                ..Default::default()
            }],
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn web_htmx_food_capture_edit_undo_restore_and_vet_share() {
    let tmp = tempfile::tempdir().unwrap();
    let config = crate::ocr::tests::test_config(tmp.path());
    let database = db::connect("sqlite::memory:").await.unwrap();
    db::migrate(&database).await.unwrap();
    let user = db::create_account(&database, "web@test.local", "Owner", "hash")
        .await
        .unwrap();
    let session = db::create_session(&database, user.id).await.unwrap();
    let pet_id = db::create_pet(
        &database,
        user.household_id,
        &user.audit_actor(),
        "Luna",
        "Cat",
        Some("British Shorthair"),
        Some(4.1),
    )
    .await
    .unwrap();

    let state = AppState {
        db: database.clone(),
        agent: Arc::new(agent::CaptureAgent::new(&config)),
        chat: Arc::new(agent::ChatAgent::new(&config)),
        mailer: Arc::new(crate::email::Mailer::Disabled),
        files: Arc::new(crate::storage::FileStore::from_config(&config).unwrap()),
        config: Arc::new(config),
    };
    let app = web::router(state);
    let cookie = format!("pethealth_session={session}");

    // 1. Load Food tab directly via HTMX.
    let res = app
        .clone()
        .oneshot(
            Request::get(format!("/app/tab/food?pet={pet_id}"))
                .header(header::COOKIE, &cookie)
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("id=\"food-tab\""));
    assert!(html.contains("Missing days are never interpreted as fasting"));

    // 2. Add a catalogue food via HTMX POST.
    let res = app
        .clone()
        .oneshot(
            Request::post("/food/catalogue")
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://localhost:3000")
                .header(header::HOST, "localhost:3000")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("hx-request", "true")
                .body(Body::from(format!(
                    "pet_id={pet_id}&name=Renal+Mousse&brand=VetCare&category=prescription_diet&completeness=complete&serving_label=pouch&serving_size_value=85&serving_size_unit=g&nutrition_basis=per_100g&kcal_per_basis=95&protein_g=7.2&phosphorus_mg=85&nutrition_confidence=verified&nutrition_source=Pouch+label&is_favorite=1"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("VetCare · Renal Mousse"));

    let foods = nutrition::list_foods(&database, user.household_id, None, None, false, false, 10)
        .await
        .unwrap();
    assert_eq!(foods.len(), 1);
    let food_id = foods[0].id;

    // 3. Log a partial meal via HTMX POST.
    let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
    let res = app
        .clone()
        .oneshot(
            Request::post("/food/entries")
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://localhost:3000")
                .header(header::HOST, "localhost:3000")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("hx-request", "true")
                .body(Body::from(format!(
                    "pet_id={pet_id}&selected_date={today}&fed_on={today}&food_item_id={food_id}&intake_status=consumed_partial&consumed_amount=0.5&consumed_unit=pouch&appetite=picky&original_wording=Ate+half+a+pouch+slowly"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("Ate half a pouch slowly"));
    assert!(html.contains("40 kcal")); // 0.5 * 85 * 95 / 100 = 40.38 -> 40 kcal

    let entries = nutrition::list_food_entries(
        &database,
        user.household_id,
        pet_id,
        FoodEntryFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.items.len(), 1);
    let entry_id = entries.items[0].id;

    // 4. Undo (delete) via HTMX POST -> shows Restore button, then restore it!
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/food/entries/{entry_id}/delete"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://localhost:3000")
                .header(header::HOST, "localhost:3000")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("hx-request", "true")
                .body(Body::from(format!("pet_id={pet_id}&selected_date={today}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("Restore entry"));

    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/food/entries/{entry_id}/restore"))
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://localhost:3000")
                .header(header::HOST, "localhost:3000")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("hx-request", "true")
                .body(Body::from(format!("pet_id={pet_id}&selected_date={today}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("Restored"));
    assert!(html.contains("Ate half a pouch slowly"));

    // 5. Vet share shows nutrition summary & food timeline entry, and revoking blocks access.
    let share = db::create_share(
        &database,
        user.household_id,
        &user.audit_actor(),
        pet_id,
        "Vet Clinic",
        7,
    )
    .await
    .unwrap();
    let token = share.token.unwrap();
    let res = app
        .clone()
        .oneshot(
            Request::get(format!("/share/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let share_html =
        String::from_utf8(to_bytes(res.into_body(), 2_000_000).await.unwrap().to_vec()).unwrap();
    assert!(share_html.contains("id=\"nutrition\""));
    assert!(share_html.contains("VetCare · Renal Mousse"));
    assert!(share_html.contains("Ate half a pouch slowly"));

    db::revoke_share(&database, user.household_id, &user.audit_actor(), share.id)
        .await
        .unwrap();
    let revoked = app
        .clone()
        .oneshot(
            Request::get(format!("/share/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NOT_FOUND);

    // 6. Chart helper sanity check with date range.
    let chart = nutrition::build_weight_calorie_chart(
        &[summarize_day(
            &NaiveDate::from_ymd_opt(2026, 11, 20)
                .unwrap()
                .format("%Y-%m-%d")
                .to_string(),
            &[],
            None,
        )],
        &[],
        None,
    );
    assert!(!chart.has_data);
}
