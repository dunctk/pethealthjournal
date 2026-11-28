//! Household-scoped food catalogue, pet feeding diary, deterministic nutrition
//! calculations, rolling trends, and clinical/vet-share summaries.
//!
//! Design invariants:
//! - Every query and mutation is strictly household-scoped.
//! - Missing nutritional data never blocks recording a meal.
//! - Nutrient values are never fabricated; incompatible bases/units are never
//!   silently mixed.
//! - Updating a catalogue food never rewrites historical diary snapshots.
//! - Unknown intake is never 0 kcal; unlogged days are never interpreted as fasting.
//! - No automatic calorie or nutrient prescriptions; optional targets are strictly
//!   owner- or vet-supplied with explicit provenance.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, QueryResult, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS food_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    household_id INTEGER NOT NULL REFERENCES households(id),
    name TEXT NOT NULL,
    brand TEXT,
    variant TEXT,
    category TEXT NOT NULL DEFAULT 'commercial_wet'
        CHECK(category IN ('commercial_wet','commercial_dry','prescription_diet','treat','supplement','human_food','custom_prep')),
    food_form TEXT,
    completeness TEXT NOT NULL DEFAULT 'unknown'
        CHECK(completeness IN ('complete','complementary','unknown')),
    serving_size_value REAL,
    serving_size_unit TEXT,
    serving_label TEXT,
    nutrition_basis TEXT
        CHECK(nutrition_basis IS NULL OR nutrition_basis IN ('per_100g','per_100ml','per_serving','dry_matter')),
    kcal_per_basis REAL,
    protein_g REAL,
    fat_g REAL,
    phosphorus_mg REAL,
    sodium_mg REAL,
    moisture_pct REAL,
    nutrition_confidence TEXT NOT NULL DEFAULT 'unknown'
        CHECK(nutrition_confidence IN ('verified','estimated','unknown')),
    nutrition_source TEXT,
    notes TEXT,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_food_items_household
    ON food_items(household_id, archived, is_favorite DESC, name ASC);

CREATE TABLE IF NOT EXISTS food_diary_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    household_id INTEGER NOT NULL REFERENCES households(id),
    pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
    food_item_id INTEGER REFERENCES food_items(id) ON DELETE SET NULL,
    meal_group_id TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','deleted')),
    intake_status TEXT NOT NULL
        CHECK(intake_status IN ('consumed_all','consumed_partial','refused','consumed_unknown_amount','not_offered')),
    appetite TEXT
        CHECK(appetite IS NULL OR appetite IN ('eager','normal','reduced','picky','refused','unknown')),
    offered_amount REAL,
    offered_unit TEXT,
    consumed_amount REAL,
    consumed_unit TEXT,
    is_approximate INTEGER NOT NULL DEFAULT 0,
    fed_on TEXT NOT NULL,
    fed_at TEXT,
    fed_time_precision TEXT NOT NULL DEFAULT 'recorded'
        CHECK(fed_time_precision IN ('exact','approximate_time','date_only','recorded')),
    meal_label TEXT,
    preparation TEXT,
    original_wording TEXT NOT NULL,
    notes TEXT,
    source TEXT NOT NULL DEFAULT 'manual_web',
    recorded_by TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    snapshot_food_name TEXT NOT NULL,
    snapshot_brand TEXT,
    snapshot_variant TEXT,
    snapshot_category TEXT,
    snapshot_completeness TEXT,
    snapshot_serving_size_value REAL,
    snapshot_serving_size_unit TEXT,
    snapshot_serving_label TEXT,
    snapshot_nutrition_basis TEXT,
    snapshot_kcal_per_basis REAL,
    snapshot_protein_g REAL,
    snapshot_fat_g REAL,
    snapshot_phosphorus_mg REAL,
    snapshot_sodium_mg REAL,
    snapshot_moisture_pct REAL,
    snapshot_nutrition_confidence TEXT NOT NULL DEFAULT 'unknown',
    snapshot_nutrition_source TEXT,
    computed_kcal REAL,
    computed_protein_g REAL,
    computed_fat_g REAL,
    computed_phosphorus_mg REAL,
    computed_sodium_mg REAL,
    calculation_status TEXT NOT NULL,
    calculation_note TEXT
);
CREATE INDEX IF NOT EXISTS idx_food_entries_household_pet_day
    ON food_diary_entries(household_id, pet_id, status, fed_on DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_food_entries_meal_group
    ON food_diary_entries(household_id, pet_id, meal_group_id) WHERE meal_group_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS food_intake_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    household_id INTEGER NOT NULL REFERENCES households(id),
    pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL,
    meal_group_id TEXT NOT NULL,
    entry_ids_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(household_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS pet_nutrition_targets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    household_id INTEGER NOT NULL REFERENCES households(id),
    pet_id INTEGER NOT NULL REFERENCES pets(id) ON DELETE CASCADE,
    daily_kcal_min REAL,
    daily_kcal_max REAL,
    daily_protein_g_min REAL,
    daily_protein_g_max REAL,
    daily_fat_g_min REAL,
    daily_fat_g_max REAL,
    daily_phosphorus_mg_max REAL,
    daily_sodium_mg_max REAL,
    source TEXT NOT NULL CHECK(source IN ('owner_entered','owner_relayed_vet')),
    source_note TEXT NOT NULL,
    effective_from TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_nutrition_targets_pet
    ON pet_nutrition_targets(household_id, pet_id, active, updated_at DESC);
"#;

pub const ALLOWED_CATEGORIES: &[&str] = &[
    "commercial_wet",
    "commercial_dry",
    "prescription_diet",
    "treat",
    "supplement",
    "human_food",
    "custom_prep",
];
pub const ALLOWED_COMPLETENESS: &[&str] = &["complete", "complementary", "unknown"];
pub const ALLOWED_BASES: &[&str] = &["per_100g", "per_100ml", "per_serving", "dry_matter"];
pub const ALLOWED_CONFIDENCE: &[&str] = &["verified", "estimated", "unknown"];
pub const ALLOWED_INTAKE_STATUS: &[&str] = &[
    "consumed_all",
    "consumed_partial",
    "refused",
    "consumed_unknown_amount",
    "not_offered",
];
pub const ALLOWED_APPETITE: &[&str] =
    &["eager", "normal", "reduced", "picky", "refused", "unknown"];
pub const ALLOWED_TIME_PRECISION: &[&str] = &["exact", "approximate_time", "date_only", "recorded"];
pub const ALLOWED_TARGET_SOURCES: &[&str] = &["owner_entered", "owner_relayed_vet"];

pub const MAX_QUANTITY: f64 = 100_000.0;
pub const MAX_NUTRIENT_VALUE: f64 = 1_000_000.0;

// ---------------------------------------------------------------------------
// Domain models
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FoodItem {
    pub id: i64,
    pub household_id: i64,
    pub name: String,
    pub brand: Option<String>,
    pub variant: Option<String>,
    pub category: String,
    pub food_form: Option<String>,
    pub completeness: String,
    pub serving_size_value: Option<f64>,
    pub serving_size_unit: Option<String>,
    pub serving_label: Option<String>,
    pub nutrition_basis: Option<String>,
    pub kcal_per_basis: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub phosphorus_mg: Option<f64>,
    pub sodium_mg: Option<f64>,
    pub moisture_pct: Option<f64>,
    pub nutrition_confidence: String,
    pub nutrition_source: Option<String>,
    pub notes: Option<String>,
    pub is_favorite: bool,
    pub archived: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl FoodItem {
    pub fn display_name(&self) -> String {
        let mut parts = Vec::new();
        if let Some(brand) = self.brand.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(brand.trim());
        }
        parts.push(self.name.trim());
        if let Some(variant) = self.variant.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(variant.trim());
        }
        parts.join(" · ")
    }

    pub fn basis_summary(&self) -> String {
        match (self.kcal_per_basis, self.nutrition_basis.as_deref()) {
            (Some(kcal), Some("per_100g")) => format!("{kcal:.0} kcal / 100 g"),
            (Some(kcal), Some("per_100ml")) => format!("{kcal:.0} kcal / 100 ml"),
            (Some(kcal), Some("per_serving")) => {
                let label = self.serving_label.as_deref().unwrap_or("serving");
                format!("{kcal:.0} kcal / {label}")
            }
            (Some(kcal), Some("dry_matter")) => format!("{kcal:.0} kcal / 100 g DM"),
            _ => "Calories unknown".to_owned(),
        }
    }

    pub fn default_quick_unit(&self) -> String {
        if let Some(label) = self
            .serving_label
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            return label.to_owned();
        }
        match self.nutrition_basis.as_deref() {
            Some("per_100ml") => "ml".to_owned(),
            Some("per_serving") => "serving".to_owned(),
            _ => "g".to_owned(),
        }
    }

    pub fn to_snapshot(&self) -> NutritionSnapshot {
        NutritionSnapshot {
            food_name: self.name.clone(),
            brand: self.brand.clone(),
            variant: self.variant.clone(),
            category: Some(self.category.clone()),
            completeness: Some(self.completeness.clone()),
            serving_size_value: self.serving_size_value,
            serving_size_unit: self.serving_size_unit.clone(),
            serving_label: self.serving_label.clone(),
            nutrition_basis: self.nutrition_basis.clone(),
            kcal_per_basis: self.kcal_per_basis,
            protein_g: self.protein_g,
            fat_g: self.fat_g,
            phosphorus_mg: self.phosphorus_mg,
            sodium_mg: self.sodium_mg,
            moisture_pct: self.moisture_pct,
            nutrition_confidence: self.nutrition_confidence.clone(),
            nutrition_source: self.nutrition_source.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NutritionSnapshot {
    pub food_name: String,
    pub brand: Option<String>,
    pub variant: Option<String>,
    pub category: Option<String>,
    pub completeness: Option<String>,
    pub serving_size_value: Option<f64>,
    pub serving_size_unit: Option<String>,
    pub serving_label: Option<String>,
    pub nutrition_basis: Option<String>,
    pub kcal_per_basis: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub phosphorus_mg: Option<f64>,
    pub sodium_mg: Option<f64>,
    pub moisture_pct: Option<f64>,
    pub nutrition_confidence: String,
    pub nutrition_source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FoodDiaryEntry {
    pub id: i64,
    pub household_id: i64,
    pub pet_id: i64,
    pub food_item_id: Option<i64>,
    pub meal_group_id: Option<String>,
    pub status: String,
    pub intake_status: String,
    pub appetite: Option<String>,
    pub offered_amount: Option<f64>,
    pub offered_unit: Option<String>,
    pub consumed_amount: Option<f64>,
    pub consumed_unit: Option<String>,
    pub is_approximate: bool,
    pub fed_on: String,
    pub fed_at: Option<String>,
    pub fed_time_precision: String,
    pub meal_label: Option<String>,
    pub preparation: Option<String>,
    pub original_wording: String,
    pub notes: Option<String>,
    pub source: String,
    pub recorded_by: String,
    pub recorded_at: String,
    pub updated_at: String,
    pub snapshot: NutritionSnapshot,
    pub computed_kcal: Option<f64>,
    pub computed_protein_g: Option<f64>,
    pub computed_fat_g: Option<f64>,
    pub computed_phosphorus_mg: Option<f64>,
    pub computed_sodium_mg: Option<f64>,
    pub calculation_status: String,
    pub calculation_note: Option<String>,
}

impl FoodDiaryEntry {
    pub fn effective_at(&self) -> DateTime<Utc> {
        if let Some(raw) = self.fed_at.as_deref()
            && let Ok(parsed) = DateTime::parse_from_rfc3339(raw)
        {
            return parsed.with_timezone(&Utc);
        }
        if let Ok(date) = NaiveDate::parse_from_str(&self.fed_on, "%Y-%m-%d")
            && let Some(naive) = date.and_hms_opt(12, 0, 0)
        {
            return DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        }
        DateTime::parse_from_rfc3339(&self.recorded_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now())
    }

    pub fn display_food_name(&self) -> String {
        let mut parts = Vec::new();
        if let Some(brand) = self
            .snapshot
            .brand
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            parts.push(brand.trim());
        }
        parts.push(self.snapshot.food_name.trim());
        if let Some(variant) = self
            .snapshot
            .variant
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            parts.push(variant.trim());
        }
        parts.join(" · ")
    }

    pub fn intake_status_label(&self) -> &'static str {
        match self.intake_status.as_str() {
            "consumed_all" => "Ate all",
            "consumed_partial" => "Ate part",
            "refused" => "Refused",
            "consumed_unknown_amount" => "Ate (unknown qty)",
            "not_offered" => "Not offered",
            _ => "Recorded",
        }
    }

    pub fn amount_label(&self) -> String {
        let approx = if self.is_approximate { "~" } else { "" };
        match (self.consumed_amount, self.consumed_unit.as_deref()) {
            (Some(val), Some(unit)) => format!("{approx}{} {unit}", fmt_num(val)),
            (Some(val), None) => format!("{approx}{}", fmt_num(val)),
            (None, _) => match (self.offered_amount, self.offered_unit.as_deref()) {
                (Some(val), Some(unit)) if self.intake_status == "refused" => {
                    format!("0 eaten (offered {approx}{} {unit})", fmt_num(val))
                }
                (Some(val), Some(unit)) => {
                    format!("qty unknown (offered {approx}{} {unit})", fmt_num(val))
                }
                _ if self.intake_status == "refused" => "0 eaten (refused)".to_owned(),
                _ if self.intake_status == "not_offered" => "Not offered".to_owned(),
                _ => "Quantity unknown".to_owned(),
            },
        }
    }

    pub fn kcal_label(&self) -> String {
        match (self.computed_kcal, self.calculation_status.as_str()) {
            (Some(kcal), "computed") => format!("{:.0} kcal (verified)", kcal),
            (Some(kcal), "estimated") => format!("~{:.0} kcal (estimated)", kcal),
            (Some(_), "zero_refused") => "0 kcal (refused)".to_owned(),
            (_, "not_offered") => "Not offered".to_owned(),
            (_, "unknown_amount") => "kcal unknown (quantity unknown)".to_owned(),
            (_, "incompatible_basis") => "kcal unknown (unit/basis mismatch)".to_owned(),
            _ => "kcal unknown".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NutritionTarget {
    pub id: i64,
    pub household_id: i64,
    pub pet_id: i64,
    pub daily_kcal_min: Option<f64>,
    pub daily_kcal_max: Option<f64>,
    pub daily_protein_g_min: Option<f64>,
    pub daily_protein_g_max: Option<f64>,
    pub daily_fat_g_min: Option<f64>,
    pub daily_fat_g_max: Option<f64>,
    pub daily_phosphorus_mg_max: Option<f64>,
    pub daily_sodium_mg_max: Option<f64>,
    pub source: String,
    pub source_note: String,
    pub effective_from: Option<String>,
    pub active: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl NutritionTarget {
    pub fn kcal_range_label(&self) -> Option<String> {
        match (self.daily_kcal_min, self.daily_kcal_max) {
            (Some(lo), Some(hi)) => Some(format!("{:.0}–{:.0} kcal/day", lo, hi)),
            (Some(lo), None) => Some(format!("≥ {:.0} kcal/day", lo)),
            (None, Some(hi)) => Some(format!("≤ {:.0} kcal/day", hi)),
            (None, None) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Input DTOs
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct NewFoodItem {
    pub name: String,
    pub brand: Option<String>,
    pub variant: Option<String>,
    pub category: String,
    pub food_form: Option<String>,
    pub completeness: String,
    pub serving_size_value: Option<f64>,
    pub serving_size_unit: Option<String>,
    pub serving_label: Option<String>,
    pub nutrition_basis: Option<String>,
    pub kcal_per_basis: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub phosphorus_mg: Option<f64>,
    pub sodium_mg: Option<f64>,
    pub moisture_pct: Option<f64>,
    pub nutrition_confidence: String,
    pub nutrition_source: Option<String>,
    pub notes: Option<String>,
    pub is_favorite: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FoodItemPatch {
    pub name: Option<String>,
    pub brand: Option<Option<String>>,
    pub variant: Option<Option<String>>,
    pub category: Option<String>,
    pub food_form: Option<Option<String>>,
    pub completeness: Option<String>,
    pub serving_size_value: Option<Option<f64>>,
    pub serving_size_unit: Option<Option<String>>,
    pub serving_label: Option<Option<String>>,
    pub nutrition_basis: Option<Option<String>>,
    pub kcal_per_basis: Option<Option<f64>>,
    pub protein_g: Option<Option<f64>>,
    pub fat_g: Option<Option<f64>>,
    pub phosphorus_mg: Option<Option<f64>>,
    pub sodium_mg: Option<Option<f64>>,
    pub moisture_pct: Option<Option<f64>>,
    pub nutrition_confidence: Option<String>,
    pub nutrition_source: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub is_favorite: Option<bool>,
    pub archived: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct NewFoodIntakeItem {
    pub food_item_id: Option<i64>,
    pub inline_snapshot: Option<NutritionSnapshot>,
    pub intake_status: String,
    pub appetite: Option<String>,
    pub offered_amount: Option<f64>,
    pub offered_unit: Option<String>,
    pub consumed_amount: Option<f64>,
    pub consumed_unit: Option<String>,
    pub is_approximate: bool,
    pub preparation: Option<String>,
    pub original_wording: String,
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FoodIntakePatch {
    pub intake_status: Option<String>,
    pub appetite: Option<Option<String>>,
    pub offered_amount: Option<Option<f64>>,
    pub offered_unit: Option<Option<String>>,
    pub consumed_amount: Option<Option<f64>>,
    pub consumed_unit: Option<Option<String>>,
    pub is_approximate: Option<bool>,
    pub fed_on: Option<String>,
    pub meal_label: Option<Option<String>>,
    pub preparation: Option<Option<String>>,
    pub original_wording: Option<String>,
    pub notes: Option<Option<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct NewNutritionTarget {
    pub daily_kcal_min: Option<f64>,
    pub daily_kcal_max: Option<f64>,
    pub daily_protein_g_min: Option<f64>,
    pub daily_protein_g_max: Option<f64>,
    pub daily_fat_g_min: Option<f64>,
    pub daily_fat_g_max: Option<f64>,
    pub daily_phosphorus_mg_max: Option<f64>,
    pub daily_sodium_mg_max: Option<f64>,
    pub source: String,
    pub source_note: String,
    pub effective_from: Option<String>,
}

// ---------------------------------------------------------------------------
// Deterministic calculation engine (100% pure, no DB or LLM)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ComputedNutrition {
    pub kcal: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub phosphorus_mg: Option<f64>,
    pub sodium_mg: Option<f64>,
    pub calculation_status: String,
    pub calculation_note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitKind {
    MassGrams,
    DryMatterGrams,
    VolumeMl,
    Serving,
}

fn normalize_unit(raw: &str) -> String {
    let cleaned = raw.trim().to_ascii_lowercase();
    match cleaned.as_str() {
        "g" | "gram" | "grams" | "gr" | "gm" => "g".into(),
        "kg" | "kilogram" | "kilograms" => "kg".into(),
        "mg" | "milligram" | "milligrams" => "mg".into(),
        "oz" | "ounce" | "ounces" => "oz".into(),
        "g_dm" | "g dm" | "g_dry_matter" | "dry_matter_g" => "g_dm".into(),
        "ml" | "milliliter" | "milliliters" | "millilitre" | "millilitres" => "ml".into(),
        "l" | "liter" | "liters" | "litre" | "litres" => "l".into(),
        "tsp" | "teaspoon" | "teaspoons" => "tsp".into(),
        "tbsp" | "tablespoon" | "tablespoons" => "tbsp".into(),
        "fl_oz" | "fl oz" | "floz" | "fluid ounce" | "fluid ounces" => "fl_oz".into(),
        "cup" | "cups" => "cup".into(),
        "serving" | "servings" | "portion" | "portions" => "serving".into(),
        "pouch" | "pouches" => "pouch".into(),
        "can" | "cans" => "can".into(),
        "tin" | "tins" => "tin".into(),
        "tube" | "tubes" | "sachet" | "sachets" | "stick" | "sticks" => "tube".into(),
        "packet" | "packets" | "pack" | "packs" => "packet".into(),
        "scoop" | "scoops" => "scoop".into(),
        "piece" | "pieces" => "piece".into(),
        "treat" | "treats" => "treat".into(),
        "biscuit" | "biscuits" => "biscuit".into(),
        "capsule" | "capsules" => "capsule".into(),
        "tablet" | "tablets" => "tablet".into(),
        "pump" | "pumps" => "pump".into(),
        "drop" | "drops" => "drop".into(),
        other => other.to_owned(),
    }
}

fn unit_to_base(unit: &str) -> Option<(UnitKind, f64)> {
    match unit {
        "g" => Some((UnitKind::MassGrams, 1.0)),
        "kg" => Some((UnitKind::MassGrams, 1000.0)),
        "mg" => Some((UnitKind::MassGrams, 0.001)),
        "oz" => Some((UnitKind::MassGrams, 28.349_523_125)),
        "g_dm" => Some((UnitKind::DryMatterGrams, 1.0)),
        "ml" => Some((UnitKind::VolumeMl, 1.0)),
        "l" => Some((UnitKind::VolumeMl, 1000.0)),
        "tsp" => Some((UnitKind::VolumeMl, 5.0)),
        "tbsp" => Some((UnitKind::VolumeMl, 15.0)),
        "fl_oz" => Some((UnitKind::VolumeMl, 29.573_529_562_5)),
        "cup" => Some((UnitKind::VolumeMl, 240.0)),
        "serving" | "pouch" | "can" | "tin" | "tube" | "packet" | "scoop" | "piece" | "treat"
        | "biscuit" | "capsule" | "tablet" | "pump" | "drop" => Some((UnitKind::Serving, 1.0)),
        _ => None,
    }
}

fn serving_unit_matches_label(unit_norm: &str, serving_label: Option<&str>) -> bool {
    if unit_norm == "serving" {
        return true;
    }
    if let Some(label) = serving_label {
        let norm_label = normalize_unit(label);
        if norm_label == unit_norm {
            return true;
        }
    }
    false
}

/// Deterministic calculation of energy and nutrients for one feeding entry.
/// Never invents missing nutritional values, never assumes universal serving
/// sizes, and never silently mixes incompatible measurement bases.
pub fn compute_entry_nutrition(
    intake_status: &str,
    consumed_amount: Option<f64>,
    consumed_unit: Option<&str>,
    is_approximate: bool,
    snapshot: &NutritionSnapshot,
) -> ComputedNutrition {
    if intake_status == "not_offered" {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "not_offered".into(),
            calculation_note: Some("Food was not offered.".into()),
        };
    }

    if intake_status == "refused" {
        return ComputedNutrition {
            kcal: Some(0.0),
            protein_g: snapshot.protein_g.map(|_| 0.0),
            fat_g: snapshot.fat_g.map(|_| 0.0),
            phosphorus_mg: snapshot.phosphorus_mg.map(|_| 0.0),
            sodium_mg: snapshot.sodium_mg.map(|_| 0.0),
            calculation_status: "zero_refused".into(),
            calculation_note: Some("Offered food was refused (0 consumed for this item).".into()),
        };
    }

    if intake_status == "consumed_unknown_amount" {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "unknown_amount".into(),
            calculation_note: Some("Consumed quantity is unknown; not counted as 0 kcal.".into()),
        };
    }

    let (Some(amount), Some(raw_unit)) = (consumed_amount, consumed_unit) else {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "unknown_amount".into(),
            calculation_note: Some(
                "Consumed quantity or unit is missing; not counted as 0 kcal.".into(),
            ),
        };
    };

    if !amount.is_finite() || amount < 0.0 {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "unknown_amount".into(),
            calculation_note: Some("Invalid consumed quantity.".into()),
        };
    }

    let has_any_nutrient = snapshot.kcal_per_basis.is_some()
        || snapshot.protein_g.is_some()
        || snapshot.fat_g.is_some()
        || snapshot.phosphorus_mg.is_some()
        || snapshot.sodium_mg.is_some();

    let Some(basis) = snapshot.nutrition_basis.as_deref() else {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "unknown_nutrition".into(),
            calculation_note: Some(
                "Nutritional information is not recorded for this food yet.".into(),
            ),
        };
    };

    if !has_any_nutrient {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "unknown_nutrition".into(),
            calculation_note: Some(
                "No energy or nutrient values are recorded for this food.".into(),
            ),
        };
    }

    let unit_norm = normalize_unit(raw_unit);
    let Some((unit_kind, unit_factor)) = unit_to_base(&unit_norm) else {
        return ComputedNutrition {
            kcal: None,
            protein_g: None,
            fat_g: None,
            phosphorus_mg: None,
            sodium_mg: None,
            calculation_status: "incompatible_basis".into(),
            calculation_note: Some(format!(
                "Unit '{raw_unit}' cannot be converted automatically to basis '{basis}'."
            )),
        };
    };

    // Determine multiplier against the snapshot's nutrition_basis.
    let mut note_parts: Vec<String> = Vec::new();
    let multiplier: Result<f64, String> = match basis {
        "per_100g" => match unit_kind {
            UnitKind::MassGrams => Ok((amount * unit_factor) / 100.0),
            UnitKind::Serving => {
                if !serving_unit_matches_label(&unit_norm, snapshot.serving_label.as_deref()) {
                    Err(format!(
                        "Unit '{raw_unit}' does not match this food's serving label ({}); cannot assume serving weight.",
                        snapshot.serving_label.as_deref().unwrap_or("unspecified")
                    ))
                } else {
                    match (
                        snapshot.serving_size_value,
                        snapshot.serving_size_unit.as_deref(),
                    ) {
                        (Some(sv), Some(su)) if sv.is_finite() && sv > 0.0 => {
                            let su_norm = normalize_unit(su);
                            match unit_to_base(&su_norm) {
                                Some((UnitKind::MassGrams, sf)) => {
                                    Ok((amount * sv * sf) / 100.0)
                                }
                                _ => Err(format!(
                                    "Serving size unit '{su}' is not a mass unit and cannot convert to per_100g."
                                )),
                            }
                        }
                        _ => Err(format!(
                            "Serving size in grams is not recorded for '{raw_unit}'; will not assume a universal serving weight."
                        )),
                    }
                }
            }
            UnitKind::VolumeMl => Err(format!(
                "Volume unit '{raw_unit}' cannot be converted to mass basis 'per_100g' without density."
            )),
            UnitKind::DryMatterGrams => Err(
                "Dry-matter grams cannot be mixed directly with as-fed 'per_100g' nutrition basis."
                    .into(),
            ),
        },
        "per_100ml" => match unit_kind {
            UnitKind::VolumeMl => Ok((amount * unit_factor) / 100.0),
            UnitKind::Serving => {
                if !serving_unit_matches_label(&unit_norm, snapshot.serving_label.as_deref()) {
                    Err(format!(
                        "Unit '{raw_unit}' does not match this food's serving label ({}); cannot assume serving volume.",
                        snapshot.serving_label.as_deref().unwrap_or("unspecified")
                    ))
                } else {
                    match (
                        snapshot.serving_size_value,
                        snapshot.serving_size_unit.as_deref(),
                    ) {
                        (Some(sv), Some(su)) if sv.is_finite() && sv > 0.0 => {
                            let su_norm = normalize_unit(su);
                            match unit_to_base(&su_norm) {
                                Some((UnitKind::VolumeMl, sf)) => Ok((amount * sv * sf) / 100.0),
                                _ => Err(format!(
                                    "Serving size unit '{su}' is not a volume unit and cannot convert to per_100ml."
                                )),
                            }
                        }
                        _ => Err(format!(
                            "Serving volume is not recorded for '{raw_unit}'; will not assume a universal volume."
                        )),
                    }
                }
            }
            UnitKind::MassGrams => Err(format!(
                "Mass unit '{raw_unit}' cannot be converted to volume basis 'per_100ml' without density."
            )),
            UnitKind::DryMatterGrams => Err(
                "Dry-matter grams cannot be converted to volume basis 'per_100ml'.".into(),
            ),
        },
        "per_serving" => match unit_kind {
            UnitKind::Serving => {
                if serving_unit_matches_label(&unit_norm, snapshot.serving_label.as_deref()) {
                    Ok(amount)
                } else {
                    Err(format!(
                        "Unit '{raw_unit}' does not match this food's per-serving unit ({}).",
                        snapshot.serving_label.as_deref().unwrap_or("serving")
                    ))
                }
            }
            UnitKind::MassGrams => match (
                snapshot.serving_size_value,
                snapshot.serving_size_unit.as_deref(),
            ) {
                (Some(sv), Some(su)) if sv.is_finite() && sv > 0.0 => {
                    let su_norm = normalize_unit(su);
                    match unit_to_base(&su_norm) {
                        Some((UnitKind::MassGrams, sf)) => Ok((amount * unit_factor) / (sv * sf)),
                        _ => Err(format!(
                            "Cannot convert mass '{raw_unit}' to per_serving when serving size unit is '{su}'."
                        )),
                    }
                }
                _ => Err(
                    "Serving size in grams is not recorded; cannot convert grams to servings."
                        .into(),
                ),
            },
            UnitKind::VolumeMl => match (
                snapshot.serving_size_value,
                snapshot.serving_size_unit.as_deref(),
            ) {
                (Some(sv), Some(su)) if sv.is_finite() && sv > 0.0 => {
                    let su_norm = normalize_unit(su);
                    match unit_to_base(&su_norm) {
                        Some((UnitKind::VolumeMl, sf)) => Ok((amount * unit_factor) / (sv * sf)),
                        _ => Err(format!(
                            "Cannot convert volume '{raw_unit}' to per_serving when serving size unit is '{su}'."
                        )),
                    }
                }
                _ => Err(
                    "Serving size in ml is not recorded; cannot convert volume to servings.".into(),
                ),
            },
            UnitKind::DryMatterGrams => Err(
                "Dry-matter grams cannot be converted directly to 'per_serving'.".into(),
            ),
        },
        "dry_matter" => match unit_kind {
            UnitKind::DryMatterGrams => Ok((amount * unit_factor) / 100.0),
            UnitKind::MassGrams => match snapshot.moisture_pct {
                Some(m) if m.is_finite() && (0.0..100.0).contains(&m) => {
                    let dm_fraction = (100.0 - m) / 100.0;
                    note_parts.push(format!(
                        "Converted as-fed weight to dry matter using recorded {m:.1}% moisture."
                    ));
                    Ok((amount * unit_factor * dm_fraction) / 100.0)
                }
                _ => Err(
                    "Nutrition is on a dry-matter basis without recorded moisture %; refusing to silently convert as-fed grams."
                        .into(),
                ),
            },
            UnitKind::Serving => {
                if !serving_unit_matches_label(&unit_norm, snapshot.serving_label.as_deref()) {
                    Err(format!(
                        "Unit '{raw_unit}' does not match this food's serving label ({}).",
                        snapshot.serving_label.as_deref().unwrap_or("unspecified")
                    ))
                } else {
                    match (
                        snapshot.serving_size_value,
                        snapshot.serving_size_unit.as_deref(),
                        snapshot.moisture_pct,
                    ) {
                        (Some(sv), Some(su), Some(m))
                            if sv.is_finite() && sv > 0.0 && m.is_finite() && (0.0..100.0).contains(&m) =>
                        {
                            let su_norm = normalize_unit(su);
                            match unit_to_base(&su_norm) {
                                Some((UnitKind::MassGrams, sf)) => {
                                    let dm_fraction = (100.0 - m) / 100.0;
                                    note_parts.push(format!(
                                        "Converted serving weight to dry matter using recorded {m:.1}% moisture."
                                    ));
                                    Ok((amount * sv * sf * dm_fraction) / 100.0)
                                }
                                _ => Err(format!(
                                    "Serving size unit '{su}' is not a mass unit for dry-matter conversion."
                                )),
                            }
                        }
                        _ => Err(
                            "Dry-matter basis requires both serving weight in grams and recorded moisture % to convert servings."
                                .into(),
                        ),
                    }
                }
            }
            UnitKind::VolumeMl => Err(format!(
                "Volume unit '{raw_unit}' cannot be converted to 'dry_matter' basis."
            )),
        },
        other => Err(format!("Unsupported nutrition basis '{other}'.")),
    };

    let factor = match multiplier {
        Ok(f) if f.is_finite() && f >= 0.0 => f,
        Ok(_) => {
            return ComputedNutrition {
                kcal: None,
                protein_g: None,
                fat_g: None,
                phosphorus_mg: None,
                sodium_mg: None,
                calculation_status: "incompatible_basis".into(),
                calculation_note: Some("Calculated scaling factor was not finite.".into()),
            };
        }
        Err(reason) => {
            return ComputedNutrition {
                kcal: None,
                protein_g: None,
                fat_g: None,
                phosphorus_mg: None,
                sodium_mg: None,
                calculation_status: "incompatible_basis".into(),
                calculation_note: Some(reason),
            };
        }
    };

    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let kcal = snapshot.kcal_per_basis.map(|v| round2(v * factor));
    let protein_g = snapshot.protein_g.map(|v| round2(v * factor));
    let fat_g = snapshot.fat_g.map(|v| round2(v * factor));
    let phosphorus_mg = snapshot.phosphorus_mg.map(|v| round2(v * factor));
    let sodium_mg = snapshot.sodium_mg.map(|v| round2(v * factor));

    let calculation_status = if kcal.is_none() {
        note_parts.push("Energy density (kcal) is not recorded for this food.".into());
        "unknown_nutrition".to_owned()
    } else if is_approximate || snapshot.nutrition_confidence != "verified" {
        if is_approximate {
            note_parts.push("Quantity is approximate.".into());
        }
        if snapshot.nutrition_confidence != "verified" {
            note_parts.push(format!(
                "Nutrition confidence is '{}'.",
                snapshot.nutrition_confidence
            ));
        }
        "estimated".to_owned()
    } else {
        "computed".to_owned()
    };

    let calculation_note = if note_parts.is_empty() {
        None
    } else {
        Some(note_parts.join(" "))
    };

    ComputedNutrition {
        kcal,
        protein_g,
        fat_g,
        phosphorus_mg,
        sodium_mg,
        calculation_status,
        calculation_note,
    }
}

// ---------------------------------------------------------------------------
// Daily summary & rolling trend aggregations (pure, deterministic)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct TargetComparison {
    pub source: String,
    pub source_note: String,
    pub daily_kcal_min: Option<f64>,
    pub daily_kcal_max: Option<f64>,
    pub status: String, // 'below_range', 'within_range', 'above_range', 'incomplete_data', 'no_kcal_target'
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DailyNutritionSummary {
    pub date: String,
    pub entry_count: usize,
    pub meals_logged: usize,
    pub recorded_kcal: Option<f64>,
    pub verified_kcal: f64,
    pub estimated_kcal: f64,
    pub recorded_protein_g: Option<f64>,
    pub recorded_fat_g: Option<f64>,
    pub recorded_phosphorus_mg: Option<f64>,
    pub recorded_sodium_mg: Option<f64>,
    pub entries_with_protein: usize,
    pub entries_with_fat: usize,
    pub entries_with_phosphorus: usize,
    pub entries_with_sodium: usize,
    pub computed_entries: usize,
    pub estimated_entries: usize,
    pub missing_nutrition_entries: usize,
    pub unknown_amount_entries: usize,
    pub incompatible_basis_entries: usize,
    pub refused_entries: usize,
    pub not_offered_entries: usize,
    pub appetite_counts: BTreeMap<String, usize>,
    /// One of: 'complete_for_logged_entries', 'partial_subtotal', 'refused_only', 'unknown', 'no_entries'
    pub kcal_completeness: String,
    pub coverage_note: String,
    pub target_comparison: Option<TargetComparison>,
}

impl DailyNutritionSummary {
    pub fn is_incomplete(&self) -> bool {
        matches!(
            self.kcal_completeness.as_str(),
            "partial_subtotal" | "unknown"
        )
    }

    pub fn kcal_display(&self) -> String {
        match (self.recorded_kcal, self.kcal_completeness.as_str()) {
            (_, "no_entries") => "No entries".to_owned(),
            (_, "unknown") => "Unknown".to_owned(),
            (Some(kcal), "partial_subtotal") => format!("{kcal:.0} kcal (subtotal)"),
            (Some(kcal), "refused_only") => format!("{kcal:.0} kcal (refused)"),
            (Some(kcal), _) if self.estimated_kcal > 0.0 => format!("~{kcal:.0} kcal"),
            (Some(kcal), _) => format!("{kcal:.0} kcal"),
            (None, _) => "Unknown".to_owned(),
        }
    }

    pub fn kcal_split_label(&self) -> String {
        if self.verified_kcal > 0.0 || self.estimated_kcal > 0.0 {
            format!(
                "Verified {:.0} · Est. {:.0}",
                self.verified_kcal, self.estimated_kcal
            )
        } else {
            format!("{} item(s) logged", self.entry_count)
        }
    }

    pub fn protein_fat_label(&self) -> String {
        let p = self
            .recorded_protein_g
            .map(|v| format!("{v:.1} g"))
            .unwrap_or_else(|| "—".into());
        let f = self
            .recorded_fat_g
            .map(|v| format!("{v:.1} g"))
            .unwrap_or_else(|| "—".into());
        format!("{p} / {f}")
    }

    pub fn phosphorus_sodium_label(&self) -> String {
        let ph = self
            .recorded_phosphorus_mg
            .map(|v| format!("{v:.0} mg"))
            .unwrap_or_else(|| "—".into());
        let na = self
            .recorded_sodium_mg
            .map(|v| format!("{v:.0} mg"))
            .unwrap_or_else(|| "—".into());
        format!("{ph} / {na}")
    }
}

pub fn summarize_day(
    date: &str,
    entries: &[FoodDiaryEntry],
    target: Option<&NutritionTarget>,
) -> DailyNutritionSummary {
    let day_entries: Vec<&FoodDiaryEntry> = entries
        .iter()
        .filter(|e| e.status == "active" && e.fed_on == date)
        .collect();

    if day_entries.is_empty() {
        return DailyNutritionSummary {
            date: date.to_owned(),
            entry_count: 0,
            meals_logged: 0,
            recorded_kcal: None,
            verified_kcal: 0.0,
            estimated_kcal: 0.0,
            recorded_protein_g: None,
            recorded_fat_g: None,
            recorded_phosphorus_mg: None,
            recorded_sodium_mg: None,
            entries_with_protein: 0,
            entries_with_fat: 0,
            entries_with_phosphorus: 0,
            entries_with_sodium: 0,
            computed_entries: 0,
            estimated_entries: 0,
            missing_nutrition_entries: 0,
            unknown_amount_entries: 0,
            incompatible_basis_entries: 0,
            refused_entries: 0,
            not_offered_entries: 0,
            appetite_counts: BTreeMap::new(),
            kcal_completeness: "no_entries".into(),
            coverage_note: "No meals recorded on this date (not interpreted as fasting).".into(),
            target_comparison: None,
        };
    }

    let mut meal_keys = std::collections::HashSet::new();
    let mut verified_kcal = 0.0;
    let mut estimated_kcal = 0.0;
    let mut protein_sum = 0.0;
    let mut fat_sum = 0.0;
    let mut phos_sum = 0.0;
    let mut sodium_sum = 0.0;
    let mut entries_with_protein = 0usize;
    let mut entries_with_fat = 0usize;
    let mut entries_with_phosphorus = 0usize;
    let mut entries_with_sodium = 0usize;
    let mut computed_entries = 0usize;
    let mut estimated_entries = 0usize;
    let mut missing_nutrition_entries = 0usize;
    let mut unknown_amount_entries = 0usize;
    let mut incompatible_basis_entries = 0usize;
    let mut refused_entries = 0usize;
    let mut not_offered_entries = 0usize;
    let mut appetite_counts: BTreeMap<String, usize> = BTreeMap::new();

    for e in &day_entries {
        if let Some(group) = e.meal_group_id.as_deref() {
            meal_keys.insert(format!("g:{group}"));
        } else {
            meal_keys.insert(format!("e:{}", e.id));
        }
        if let Some(app) = e.appetite.as_deref() {
            *appetite_counts.entry(app.to_owned()).or_insert(0) += 1;
        }
        match e.calculation_status.as_str() {
            "computed" => {
                computed_entries += 1;
                if let Some(k) = e.computed_kcal {
                    verified_kcal += k;
                }
            }
            "estimated" => {
                computed_entries += 1;
                estimated_entries += 1;
                if let Some(k) = e.computed_kcal {
                    estimated_kcal += k;
                }
            }
            "zero_refused" => refused_entries += 1,
            "not_offered" => not_offered_entries += 1,
            "unknown_amount" => unknown_amount_entries += 1,
            "incompatible_basis" => incompatible_basis_entries += 1,
            _ => missing_nutrition_entries += 1,
        }
        if e.intake_status != "not_offered" {
            if let Some(v) = e.computed_protein_g {
                protein_sum += v;
                entries_with_protein += 1;
            }
            if let Some(v) = e.computed_fat_g {
                fat_sum += v;
                entries_with_fat += 1;
            }
            if let Some(v) = e.computed_phosphorus_mg {
                phos_sum += v;
                entries_with_phosphorus += 1;
            }
            if let Some(v) = e.computed_sodium_mg {
                sodium_sum += v;
                entries_with_sodium += 1;
            }
        }
    }

    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let incomplete_consumed =
        missing_nutrition_entries + unknown_amount_entries + incompatible_basis_entries;

    let (recorded_kcal, kcal_completeness, coverage_note) = if computed_entries > 0
        && incomplete_consumed == 0
    {
        let total = round2(verified_kcal + estimated_kcal);
        let note = if estimated_entries > 0 {
            format!(
                "Computed from all {computed_entries} consumed item(s); includes {estimated_entries} estimated/approximate item(s)."
            )
        } else {
            format!(
                "Computed from all {computed_entries} consumed item(s) with verified nutrition."
            )
        };
        (Some(total), "complete_for_logged_entries".to_owned(), note)
    } else if computed_entries > 0 && incomplete_consumed > 0 {
        let total = round2(verified_kcal + estimated_kcal);
        (
            Some(total),
            "partial_subtotal".to_owned(),
            format!(
                "Subtotal from {computed_entries} item(s) only; {incomplete_consumed} consumed item(s) had missing nutrition, unknown quantity, or incompatible units."
            ),
        )
    } else if incomplete_consumed > 0 {
        (
            None,
            "unknown".to_owned(),
            format!(
                "{incomplete_consumed} consumed item(s) recorded without computable calories (unknown quantity, missing nutrition, or incompatible units)."
            ),
        )
    } else if refused_entries > 0 {
        (
            Some(0.0),
            "refused_only".to_owned(),
            format!(
                "{refused_entries} offered item(s) refused; no consumed items logged on this date."
            ),
        )
    } else {
        (
            None,
            "unknown".to_owned(),
            "Only 'not offered' entries recorded on this date.".into(),
        )
    };

    let target_comparison =
        target.map(|t| compare_day_to_target(recorded_kcal, &kcal_completeness, t));

    DailyNutritionSummary {
        date: date.to_owned(),
        entry_count: day_entries.len(),
        meals_logged: meal_keys.len(),
        recorded_kcal,
        verified_kcal: round2(verified_kcal),
        estimated_kcal: round2(estimated_kcal),
        recorded_protein_g: (entries_with_protein > 0).then(|| round2(protein_sum)),
        recorded_fat_g: (entries_with_fat > 0).then(|| round2(fat_sum)),
        recorded_phosphorus_mg: (entries_with_phosphorus > 0).then(|| round2(phos_sum)),
        recorded_sodium_mg: (entries_with_sodium > 0).then(|| round2(sodium_sum)),
        entries_with_protein,
        entries_with_fat,
        entries_with_phosphorus,
        entries_with_sodium,
        computed_entries,
        estimated_entries,
        missing_nutrition_entries,
        unknown_amount_entries,
        incompatible_basis_entries,
        refused_entries,
        not_offered_entries,
        appetite_counts,
        kcal_completeness,
        coverage_note,
        target_comparison,
    }
}

fn compare_day_to_target(
    recorded_kcal: Option<f64>,
    completeness: &str,
    target: &NutritionTarget,
) -> TargetComparison {
    let (status, summary) = match (
        recorded_kcal,
        target.daily_kcal_min,
        target.daily_kcal_max,
        completeness,
    ) {
        (_, None, None, _) => (
            "no_kcal_target".to_owned(),
            "Owner/vet target has nutrient limits only (no daily calorie range).".to_owned(),
        ),
        (None, _, _, _) => (
            "incomplete_data".to_owned(),
            "Recorded calories are unknown; cannot compare against target.".to_owned(),
        ),
        (Some(kcal), _min, max, "partial_subtotal") => {
            if let Some(hi) = max
                && kcal > hi
            {
                (
                    "above_range".to_owned(),
                    format!(
                        "Recorded subtotal ({kcal:.0} kcal) already exceeds target max ({hi:.0} kcal)."
                    ),
                )
            } else {
                (
                    "incomplete_data".to_owned(),
                    format!(
                        "Recorded subtotal is {kcal:.0} kcal, but other items lack calorie data."
                    ),
                )
            }
        }
        (Some(kcal), min, max, _) => {
            if let Some(lo) = min
                && kcal < lo
            {
                (
                    "below_range".to_owned(),
                    format!(
                        "Recorded {kcal:.0} kcal is below owner/vet target min ({lo:.0} kcal)."
                    ),
                )
            } else if let Some(hi) = max
                && kcal > hi
            {
                (
                    "above_range".to_owned(),
                    format!(
                        "Recorded {kcal:.0} kcal is above owner/vet target max ({hi:.0} kcal)."
                    ),
                )
            } else {
                (
                    "within_range".to_owned(),
                    format!("Recorded {kcal:.0} kcal is within the owner/vet-entered target."),
                )
            }
        }
    };
    TargetComparison {
        source: target.source.clone(),
        source_note: target.source_note.clone(),
        daily_kcal_min: target.daily_kcal_min,
        daily_kcal_max: target.daily_kcal_max,
        status,
        summary,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FoodAcceptanceStat {
    pub food_item_id: Option<i64>,
    pub food_name: String,
    pub times_offered: usize,
    pub consumed_all: usize,
    pub consumed_partial: usize,
    pub refused: usize,
    pub consumed_unknown_amount: usize,
    pub acceptance_rate_pct: Option<f64>,
}

impl FoodAcceptanceStat {
    pub fn acceptance_label(&self) -> String {
        self.acceptance_rate_pct
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "—".into())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WindowAverages {
    pub window_days: usize,
    pub days_with_entries: usize,
    pub days_with_computable_kcal: usize,
    pub days_with_complete_kcal: usize,
    pub days_with_partial_kcal: usize,
    pub days_with_no_entries: usize,
    pub avg_kcal_per_computable_day: Option<f64>,
    pub avg_kcal_per_complete_day: Option<f64>,
    pub avg_protein_g_per_logged_day: Option<f64>,
    pub avg_fat_g_per_logged_day: Option<f64>,
    pub avg_phosphorus_mg_per_logged_day: Option<f64>,
    pub avg_sodium_mg_per_logged_day: Option<f64>,
    pub total_entries: usize,
    pub refused_entries: usize,
    pub refusal_rate_pct: Option<f64>,
}

impl WindowAverages {
    pub fn avg_kcal_label(&self) -> String {
        self.avg_kcal_per_computable_day
            .map(|v| format!("{v:.0} kcal"))
            .unwrap_or_else(|| "—".into())
    }
    pub fn avg_complete_kcal_label(&self) -> String {
        self.avg_kcal_per_complete_day
            .map(|v| format!("{v:.0} kcal"))
            .unwrap_or_else(|| "—".into())
    }
    pub fn refusal_rate_label(&self) -> String {
        self.refusal_rate_pct
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "—".into())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NutritionTrendReport {
    pub since: String,
    pub until: String,
    pub window_7d: WindowAverages,
    pub window_30d: WindowAverages,
    pub selected_window: WindowAverages,
    pub daily: Vec<DailyNutritionSummary>,
    pub food_acceptance: Vec<FoodAcceptanceStat>,
    pub active_target: Option<NutritionTarget>,
    pub clinical_note: String,
}

pub fn compute_window_averages(
    start: NaiveDate,
    end: NaiveDate,
    entries: &[FoodDiaryEntry],
    target: Option<&NutritionTarget>,
) -> (WindowAverages, Vec<DailyNutritionSummary>) {
    let mut daily = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let day_str = cursor.format("%Y-%m-%d").to_string();
        daily.push(summarize_day(&day_str, entries, target));
        cursor += Duration::days(1);
    }

    let window_days = daily.len();
    let days_with_entries = daily.iter().filter(|d| d.entry_count > 0).count();
    let days_with_no_entries = window_days.saturating_sub(days_with_entries);

    let computable_days: Vec<&DailyNutritionSummary> =
        daily.iter().filter(|d| d.recorded_kcal.is_some()).collect();
    let complete_days: Vec<&DailyNutritionSummary> = daily
        .iter()
        .filter(|d| {
            matches!(
                d.kcal_completeness.as_str(),
                "complete_for_logged_entries" | "refused_only"
            ) && d.recorded_kcal.is_some()
        })
        .collect();
    let days_with_partial_kcal = daily
        .iter()
        .filter(|d| d.kcal_completeness == "partial_subtotal")
        .count();

    let round1 = |v: f64| (v * 10.0).round() / 10.0;
    let avg_opt = |vals: Vec<f64>| -> Option<f64> {
        if vals.is_empty() {
            None
        } else {
            Some(round1(vals.iter().sum::<f64>() / vals.len() as f64))
        }
    };

    let avg_kcal_per_computable_day = avg_opt(
        computable_days
            .iter()
            .filter_map(|d| d.recorded_kcal)
            .collect(),
    );
    let avg_kcal_per_complete_day = avg_opt(
        complete_days
            .iter()
            .filter_map(|d| d.recorded_kcal)
            .collect(),
    );
    let avg_protein_g_per_logged_day =
        avg_opt(daily.iter().filter_map(|d| d.recorded_protein_g).collect());
    let avg_fat_g_per_logged_day = avg_opt(daily.iter().filter_map(|d| d.recorded_fat_g).collect());
    let avg_phosphorus_mg_per_logged_day = avg_opt(
        daily
            .iter()
            .filter_map(|d| d.recorded_phosphorus_mg)
            .collect(),
    );
    let avg_sodium_mg_per_logged_day =
        avg_opt(daily.iter().filter_map(|d| d.recorded_sodium_mg).collect());

    let total_entries: usize = daily.iter().map(|d| d.entry_count).sum();
    let offered_entries: usize = daily
        .iter()
        .map(|d| d.entry_count.saturating_sub(d.not_offered_entries))
        .sum();
    let refused_entries: usize = daily.iter().map(|d| d.refused_entries).sum();
    let refusal_rate_pct = (offered_entries > 0)
        .then(|| round1((refused_entries as f64 / offered_entries as f64) * 100.0));

    let averages = WindowAverages {
        window_days,
        days_with_entries,
        days_with_computable_kcal: computable_days.len(),
        days_with_complete_kcal: complete_days.len(),
        days_with_partial_kcal,
        days_with_no_entries,
        avg_kcal_per_computable_day,
        avg_kcal_per_complete_day,
        avg_protein_g_per_logged_day,
        avg_fat_g_per_logged_day,
        avg_phosphorus_mg_per_logged_day,
        avg_sodium_mg_per_logged_day,
        total_entries,
        refused_entries,
        refusal_rate_pct,
    };
    (averages, daily)
}

pub fn compute_food_acceptance(entries: &[FoodDiaryEntry]) -> Vec<FoodAcceptanceStat> {
    let mut map: HashMap<String, FoodAcceptanceStat> = HashMap::new();
    for e in entries {
        if e.status != "active" || e.intake_status == "not_offered" {
            continue;
        }
        let display = e.display_food_name();
        let key = match e.food_item_id {
            Some(id) => format!("id:{id}"),
            None => format!("name:{}", display.to_ascii_lowercase()),
        };
        let stat = map.entry(key).or_insert_with(|| FoodAcceptanceStat {
            food_item_id: e.food_item_id,
            food_name: display,
            times_offered: 0,
            consumed_all: 0,
            consumed_partial: 0,
            refused: 0,
            consumed_unknown_amount: 0,
            acceptance_rate_pct: None,
        });
        stat.times_offered += 1;
        match e.intake_status.as_str() {
            "consumed_all" => stat.consumed_all += 1,
            "consumed_partial" => stat.consumed_partial += 1,
            "refused" => stat.refused += 1,
            "consumed_unknown_amount" => stat.consumed_unknown_amount += 1,
            _ => {}
        }
    }
    let mut list: Vec<FoodAcceptanceStat> = map.into_values().collect();
    for stat in &mut list {
        let accepted = stat.consumed_all + stat.consumed_partial + stat.consumed_unknown_amount;
        if stat.times_offered > 0 {
            stat.acceptance_rate_pct =
                Some(((accepted as f64 / stat.times_offered as f64) * 1000.0).round() / 10.0);
        }
    }
    list.sort_by(|a, b| {
        b.times_offered
            .cmp(&a.times_offered)
            .then_with(|| a.food_name.cmp(&b.food_name))
    });
    list
}

pub fn build_trend_report(
    since: NaiveDate,
    until: NaiveDate,
    all_entries_in_range: &[FoodDiaryEntry],
    active_target: Option<NutritionTarget>,
) -> NutritionTrendReport {
    let (selected_window, daily) =
        compute_window_averages(since, until, all_entries_in_range, active_target.as_ref());
    let start_7 = until - Duration::days(6);
    let (window_7d, _) =
        compute_window_averages(start_7, until, all_entries_in_range, active_target.as_ref());
    let start_30 = until - Duration::days(29);
    let (window_30d, _) = compute_window_averages(
        start_30,
        until,
        all_entries_in_range,
        active_target.as_ref(),
    );

    let since_str = since.format("%Y-%m-%d").to_string();
    let until_str = until.format("%Y-%m-%d").to_string();
    let scoped: Vec<FoodDiaryEntry> = all_entries_in_range
        .iter()
        .filter(|e| e.fed_on >= since_str && e.fed_on <= until_str)
        .cloned()
        .collect();
    let food_acceptance = compute_food_acceptance(&scoped);

    NutritionTrendReport {
        since: since_str,
        until: until_str,
        window_7d,
        window_30d,
        selected_window,
        daily,
        food_acceptance,
        active_target,
        clinical_note: "Averages reflect logged days with computable data only; unlogged days are never treated as fasting. Recorded totals and clinical timelines are context, not a diagnosis or dietary prescription.".into(),
    }
}

// ---------------------------------------------------------------------------
// Server-side SVG chart: Weight vs Recorded Daily Calories (zero JS deps)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CalorieBarPoint {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub is_partial: bool,
    pub tooltip: String,
}

#[derive(Clone, Debug)]
pub struct WeightOverlayPoint {
    pub x: f64,
    pub y: f64,
    pub tooltip: String,
}

#[derive(Clone, Debug)]
pub struct WeightCalorieChart {
    pub has_data: bool,
    pub start_label: String,
    pub end_label: String,
    pub kcal_max_label: String,
    pub weight_min_label: String,
    pub weight_max_label: String,
    pub bars: Vec<CalorieBarPoint>,
    pub weight_points: Vec<WeightOverlayPoint>,
    pub weight_line: String,
    pub show_weight_line: bool,
    pub target_min_y: Option<f64>,
    pub target_max_y: Option<f64>,
}

pub fn build_weight_calorie_chart(
    daily: &[DailyNutritionSummary],
    weights: &[crate::domain::WeightEntry],
    target: Option<&NutritionTarget>,
) -> WeightCalorieChart {
    let empty = WeightCalorieChart {
        has_data: false,
        start_label: String::new(),
        end_label: String::new(),
        kcal_max_label: String::new(),
        weight_min_label: String::new(),
        weight_max_label: String::new(),
        bars: Vec::new(),
        weight_points: Vec::new(),
        weight_line: String::new(),
        show_weight_line: false,
        target_min_y: None,
        target_max_y: None,
    };
    if daily.is_empty() {
        return empty;
    }

    let first_date = NaiveDate::parse_from_str(&daily[0].date, "%Y-%m-%d").ok();
    let last_date = NaiveDate::parse_from_str(&daily[daily.len() - 1].date, "%Y-%m-%d").ok();
    let (Some(start), Some(end)) = (first_date, last_date) else {
        return empty;
    };

    let mut max_kcal = daily
        .iter()
        .filter_map(|d| d.recorded_kcal)
        .fold(0.0_f64, f64::max);
    if let Some(t) = target {
        if let Some(hi) = t.daily_kcal_max {
            max_kcal = max_kcal.max(hi);
        }
        if let Some(lo) = t.daily_kcal_min {
            max_kcal = max_kcal.max(lo);
        }
    }
    let has_kcal = daily.iter().any(|d| d.recorded_kcal.is_some());
    let kcal_scale = if max_kcal > 0.0 {
        max_kcal * 1.15
    } else {
        100.0
    };

    // Filter weights inside the chart window (latest reading per day if multiple).
    let mut weights_by_day: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for w in weights {
        let day = w.measured_at.date_naive();
        if day >= start && day <= end && w.weight_kg.is_finite() {
            weights_by_day.entry(day).or_insert(w.weight_kg);
        }
    }
    let has_weights = !weights_by_day.is_empty();
    if !has_kcal && !has_weights {
        return empty;
    }

    let plot_left = 48.0;
    let plot_right = 552.0;
    let plot_top = 24.0;
    let plot_bottom = 172.0;
    let plot_w = plot_right - plot_left;
    let plot_h = plot_bottom - plot_top;
    let n = daily.len().max(1) as f64;
    let step = plot_w / n;
    let bar_w = (step * 0.65).clamp(4.0, 22.0);

    let mut bars = Vec::new();
    for (idx, d) in daily.iter().enumerate() {
        let Some(kcal) = d.recorded_kcal else {
            continue;
        };
        let cx = plot_left + (idx as f64 + 0.5) * step;
        let h = ((kcal / kcal_scale) * plot_h).clamp(2.0, plot_h);
        let y = plot_bottom - h;
        let is_partial = d.kcal_completeness == "partial_subtotal";
        let tooltip = format!(
            "{}: {:.0} kcal ({}; {} entries{})",
            d.date,
            kcal,
            if is_partial {
                "partial subtotal"
            } else if d.kcal_completeness == "refused_only" {
                "refused only"
            } else {
                "logged total"
            },
            d.entry_count,
            if d.refused_entries > 0 {
                format!(", {} refused", d.refused_entries)
            } else {
                String::new()
            }
        );
        bars.push(CalorieBarPoint {
            x: cx - bar_w / 2.0,
            y,
            width: bar_w,
            height: h,
            is_partial,
            tooltip,
        });
    }

    let (w_min, w_max, w_min_label, w_max_label) = if has_weights {
        let mut lo = weights_by_day
            .values()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let mut hi = weights_by_day
            .values()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let pad = ((hi - lo).abs() * 0.15).max(0.15);
        lo = (lo - pad).max(0.0);
        hi += pad;
        (lo, hi, format!("{lo:.1} kg"), format!("{hi:.1} kg"))
    } else {
        (0.0, 1.0, String::new(), String::new())
    };
    let w_span = (w_max - w_min).max(0.01);

    let mut weight_points = Vec::new();
    for (day, kg) in &weights_by_day {
        let day_idx = (*day - start).num_days() as f64;
        let cx = plot_left + (day_idx + 0.5) * step;
        let y = plot_bottom - ((*kg - w_min) / w_span) * plot_h;
        weight_points.push(WeightOverlayPoint {
            x: cx,
            y,
            tooltip: format!("{}: {:.2} kg", day.format("%Y-%m-%d"), kg),
        });
    }
    let show_weight_line = weight_points.len() > 1;
    let weight_line = weight_points
        .iter()
        .map(|p| format!("{:.1},{:.1}", p.x, p.y))
        .collect::<Vec<_>>()
        .join(" ");

    let target_min_y = target
        .and_then(|t| t.daily_kcal_min)
        .map(|v| plot_bottom - ((v / kcal_scale) * plot_h).clamp(0.0, plot_h));
    let target_max_y = target
        .and_then(|t| t.daily_kcal_max)
        .map(|v| plot_bottom - ((v / kcal_scale) * plot_h).clamp(0.0, plot_h));

    WeightCalorieChart {
        has_data: true,
        start_label: start.format("%d %b").to_string(),
        end_label: end.format("%d %b").to_string(),
        kcal_max_label: if has_kcal {
            format!("{:.0} kcal", kcal_scale)
        } else {
            String::new()
        },
        weight_min_label: w_min_label,
        weight_max_label: w_max_label,
        bars,
        weight_points,
        weight_line,
        show_weight_line,
        target_min_y,
        target_max_y,
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn bounded(value: &str, max_len: usize, field: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > max_len {
        return Err(anyhow!("{field} must be 1–{max_len} characters."));
    }
    Ok(trimmed.to_owned())
}

fn bounded_opt(value: Option<&str>, max_len: usize, field: &str) -> Result<Option<String>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > max_len {
        return Err(anyhow!("{field} must be at most {max_len} characters."));
    }
    Ok(Some(trimmed.to_owned()))
}

fn one_of<'a>(value: &'a str, allowed: &[&str], field: &str) -> Result<&'a str> {
    if !allowed.contains(&value) {
        return Err(anyhow!(
            "Invalid {field} '{value}'; expected one of: {}.",
            allowed.join(", ")
        ));
    }
    Ok(value)
}

fn check_non_negative(value: Option<f64>, max: f64, field: &str) -> Result<Option<f64>> {
    let Some(v) = value else {
        return Ok(None);
    };
    if !v.is_finite() || v < 0.0 || v > max {
        return Err(anyhow!(
            "{field} must be a finite number between 0 and {max}."
        ));
    }
    Ok(Some(v))
}

fn check_positive(value: Option<f64>, max: f64, field: &str) -> Result<Option<f64>> {
    let Some(v) = value else {
        return Ok(None);
    };
    if !v.is_finite() || v <= 0.0 || v > max {
        return Err(anyhow!(
            "{field} must be a positive finite number up to {max}."
        ));
    }
    Ok(Some(v))
}

pub fn validate_fed_on(raw: &str) -> Result<String> {
    let date = NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("fed_on must be YYYY-MM-DD."))?;
    let max_date = Utc::now().date_naive() + Duration::days(1);
    if date > max_date {
        return Err(anyhow!("Feeding date cannot be in the future."));
    }
    Ok(date.format("%Y-%m-%d").to_string())
}

pub fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-6 {
        format!("{:.0}", v.round())
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

fn validate_nutrition_fields(
    serving_size_value: Option<f64>,
    serving_size_unit: Option<&str>,
    nutrition_basis: Option<&str>,
    kcal_per_basis: Option<f64>,
    protein_g: Option<f64>,
    fat_g: Option<f64>,
    phosphorus_mg: Option<f64>,
    sodium_mg: Option<f64>,
    moisture_pct: Option<f64>,
    confidence: &str,
) -> Result<()> {
    one_of(confidence, ALLOWED_CONFIDENCE, "nutrition_confidence")?;
    if let Some(basis) = nutrition_basis {
        one_of(basis, ALLOWED_BASES, "nutrition_basis")?;
    }
    check_positive(serving_size_value, MAX_QUANTITY, "serving_size_value")?;
    if serving_size_value.is_some() && serving_size_unit.is_none_or(|u| u.trim().is_empty()) {
        return Err(anyhow!(
            "serving_size_unit is required when serving_size_value is set."
        ));
    }
    let kcal = check_non_negative(kcal_per_basis, MAX_NUTRIENT_VALUE, "kcal_per_basis")?;
    let protein = check_non_negative(protein_g, MAX_NUTRIENT_VALUE, "protein_g")?;
    let fat = check_non_negative(fat_g, MAX_NUTRIENT_VALUE, "fat_g")?;
    let phos = check_non_negative(phosphorus_mg, MAX_NUTRIENT_VALUE, "phosphorus_mg")?;
    let sodium = check_non_negative(sodium_mg, MAX_NUTRIENT_VALUE, "sodium_mg")?;
    if let Some(m) = check_non_negative(moisture_pct, 100.0, "moisture_pct")?
        && m > 100.0
    {
        return Err(anyhow!("moisture_pct must be between 0 and 100."));
    }
    let has_any =
        kcal.is_some() || protein.is_some() || fat.is_some() || phos.is_some() || sodium.is_some();
    if has_any && nutrition_basis.is_none() {
        return Err(anyhow!(
            "nutrition_basis (per_100g, per_100ml, per_serving, or dry_matter) is required when storing calorie or nutrient values."
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Database row mappers & audit helper
// ---------------------------------------------------------------------------

async fn audit(
    tx: &sea_orm::DatabaseTransaction,
    household_id: i64,
    actor: &str,
    action: &str,
    record_type: &str,
    record_id: i64,
    detail: &str,
    at: &str,
) -> Result<()> {
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,?,?,?,?,?)",
        [
            household_id.into(),
            actor.into(),
            action.into(),
            record_type.into(),
            record_id.into(),
            detail.into(),
            at.into(),
        ],
    ))
    .await?;
    Ok(())
}

fn food_item_from_row(row: QueryResult) -> Result<FoodItem> {
    let fav: i64 = row.try_get("", "is_favorite")?;
    let arch: i64 = row.try_get("", "archived")?;
    Ok(FoodItem {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        name: row.try_get("", "name")?,
        brand: row.try_get("", "brand")?,
        variant: row.try_get("", "variant")?,
        category: row.try_get("", "category")?,
        food_form: row.try_get("", "food_form")?,
        completeness: row.try_get("", "completeness")?,
        serving_size_value: row.try_get("", "serving_size_value")?,
        serving_size_unit: row.try_get("", "serving_size_unit")?,
        serving_label: row.try_get("", "serving_label")?,
        nutrition_basis: row.try_get("", "nutrition_basis")?,
        kcal_per_basis: row.try_get("", "kcal_per_basis")?,
        protein_g: row.try_get("", "protein_g")?,
        fat_g: row.try_get("", "fat_g")?,
        phosphorus_mg: row.try_get("", "phosphorus_mg")?,
        sodium_mg: row.try_get("", "sodium_mg")?,
        moisture_pct: row.try_get("", "moisture_pct")?,
        nutrition_confidence: row.try_get("", "nutrition_confidence")?,
        nutrition_source: row.try_get("", "nutrition_source")?,
        notes: row.try_get("", "notes")?,
        is_favorite: fav != 0,
        archived: arch != 0,
        created_by: row.try_get("", "created_by")?,
        created_at: row.try_get("", "created_at")?,
        updated_at: row.try_get("", "updated_at")?,
    })
}

pub(crate) fn diary_entry_from_row(row: QueryResult) -> Result<FoodDiaryEntry> {
    let approx: i64 = row.try_get("", "is_approximate")?;
    Ok(FoodDiaryEntry {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        food_item_id: row.try_get("", "food_item_id")?,
        meal_group_id: row.try_get("", "meal_group_id")?,
        status: row.try_get("", "status")?,
        intake_status: row.try_get("", "intake_status")?,
        appetite: row.try_get("", "appetite")?,
        offered_amount: row.try_get("", "offered_amount")?,
        offered_unit: row.try_get("", "offered_unit")?,
        consumed_amount: row.try_get("", "consumed_amount")?,
        consumed_unit: row.try_get("", "consumed_unit")?,
        is_approximate: approx != 0,
        fed_on: row.try_get("", "fed_on")?,
        fed_at: row.try_get("", "fed_at")?,
        fed_time_precision: row.try_get("", "fed_time_precision")?,
        meal_label: row.try_get("", "meal_label")?,
        preparation: row.try_get("", "preparation")?,
        original_wording: row.try_get("", "original_wording")?,
        notes: row.try_get("", "notes")?,
        source: row.try_get("", "source")?,
        recorded_by: row.try_get("", "recorded_by")?,
        recorded_at: row.try_get("", "recorded_at")?,
        updated_at: row.try_get("", "updated_at")?,
        snapshot: NutritionSnapshot {
            food_name: row.try_get("", "snapshot_food_name")?,
            brand: row.try_get("", "snapshot_brand")?,
            variant: row.try_get("", "snapshot_variant")?,
            category: row.try_get("", "snapshot_category")?,
            completeness: row.try_get("", "snapshot_completeness")?,
            serving_size_value: row.try_get("", "snapshot_serving_size_value")?,
            serving_size_unit: row.try_get("", "snapshot_serving_size_unit")?,
            serving_label: row.try_get("", "snapshot_serving_label")?,
            nutrition_basis: row.try_get("", "snapshot_nutrition_basis")?,
            kcal_per_basis: row.try_get("", "snapshot_kcal_per_basis")?,
            protein_g: row.try_get("", "snapshot_protein_g")?,
            fat_g: row.try_get("", "snapshot_fat_g")?,
            phosphorus_mg: row.try_get("", "snapshot_phosphorus_mg")?,
            sodium_mg: row.try_get("", "snapshot_sodium_mg")?,
            moisture_pct: row.try_get("", "snapshot_moisture_pct")?,
            nutrition_confidence: row.try_get("", "snapshot_nutrition_confidence")?,
            nutrition_source: row.try_get("", "snapshot_nutrition_source")?,
        },
        computed_kcal: row.try_get("", "computed_kcal")?,
        computed_protein_g: row.try_get("", "computed_protein_g")?,
        computed_fat_g: row.try_get("", "computed_fat_g")?,
        computed_phosphorus_mg: row.try_get("", "computed_phosphorus_mg")?,
        computed_sodium_mg: row.try_get("", "computed_sodium_mg")?,
        calculation_status: row.try_get("", "calculation_status")?,
        calculation_note: row.try_get("", "calculation_note")?,
    })
}

fn target_from_row(row: QueryResult) -> Result<NutritionTarget> {
    let active: i64 = row.try_get("", "active")?;
    Ok(NutritionTarget {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        daily_kcal_min: row.try_get("", "daily_kcal_min")?,
        daily_kcal_max: row.try_get("", "daily_kcal_max")?,
        daily_protein_g_min: row.try_get("", "daily_protein_g_min")?,
        daily_protein_g_max: row.try_get("", "daily_protein_g_max")?,
        daily_fat_g_min: row.try_get("", "daily_fat_g_min")?,
        daily_fat_g_max: row.try_get("", "daily_fat_g_max")?,
        daily_phosphorus_mg_max: row.try_get("", "daily_phosphorus_mg_max")?,
        daily_sodium_mg_max: row.try_get("", "daily_sodium_mg_max")?,
        source: row.try_get("", "source")?,
        source_note: row.try_get("", "source_note")?,
        effective_from: row.try_get("", "effective_from")?,
        active: active != 0,
        created_by: row.try_get("", "created_by")?,
        created_at: row.try_get("", "created_at")?,
        updated_at: row.try_get("", "updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Food catalogue CRUD (household-scoped)
// ---------------------------------------------------------------------------

pub async fn list_foods(
    db: &DatabaseConnection,
    household_id: i64,
    query: Option<&str>,
    category: Option<&str>,
    favorites_only: bool,
    include_archived: bool,
    limit: u64,
) -> Result<Vec<FoodItem>> {
    let mut sql = String::from("SELECT * FROM food_items WHERE household_id=?");
    let mut values: Vec<sea_orm::Value> = vec![household_id.into()];
    if !include_archived {
        sql.push_str(" AND archived=0");
    }
    if favorites_only {
        sql.push_str(" AND is_favorite=1");
    }
    if let Some(cat) = category.filter(|s| !s.trim().is_empty()) {
        one_of(cat, ALLOWED_CATEGORIES, "category")?;
        sql.push_str(" AND category=?");
        values.push(cat.into());
    }
    if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
        let pattern = format!("%{}%", q.to_ascii_lowercase());
        sql.push_str(
            " AND (LOWER(name) LIKE ? OR LOWER(COALESCE(brand,'')) LIKE ? OR LOWER(COALESCE(variant,'')) LIKE ?)",
        );
        values.push(pattern.clone().into());
        values.push(pattern.clone().into());
        values.push(pattern.into());
    }
    sql.push_str(" ORDER BY is_favorite DESC, name ASC, id ASC LIMIT ?");
    values.push(limit.clamp(1, 200).into());

    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(food_item_from_row).collect()
}

pub async fn get_food(
    db: &DatabaseConnection,
    household_id: i64,
    food_id: i64,
) -> Result<Option<FoodItem>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT * FROM food_items WHERE household_id=? AND id=?",
            [household_id.into(), food_id.into()],
        ))
        .await?;
    row.map(food_item_from_row).transpose()
}

pub async fn create_food(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    input: NewFoodItem,
) -> Result<FoodItem> {
    let name = bounded(&input.name, 140, "name")?;
    let brand = bounded_opt(input.brand.as_deref(), 100, "brand")?;
    let variant = bounded_opt(input.variant.as_deref(), 120, "variant")?;
    let category = if input.category.trim().is_empty() {
        "commercial_wet"
    } else {
        one_of(input.category.trim(), ALLOWED_CATEGORIES, "category")?
    };
    let food_form = bounded_opt(input.food_form.as_deref(), 60, "food_form")?;
    let completeness = if input.completeness.trim().is_empty() {
        "unknown"
    } else {
        one_of(
            input.completeness.trim(),
            ALLOWED_COMPLETENESS,
            "completeness",
        )?
    };
    let serving_size_unit =
        bounded_opt(input.serving_size_unit.as_deref(), 30, "serving_size_unit")?
            .map(|u| normalize_unit(&u));
    let serving_label = bounded_opt(input.serving_label.as_deref(), 40, "serving_label")?
        .map(|u| normalize_unit(&u));
    let nutrition_basis = bounded_opt(input.nutrition_basis.as_deref(), 30, "nutrition_basis")?;
    let nutrition_source = bounded_opt(input.nutrition_source.as_deref(), 500, "nutrition_source")?;
    let notes = bounded_opt(input.notes.as_deref(), 1000, "notes")?;

    let has_any_nutrient = input.kcal_per_basis.is_some()
        || input.protein_g.is_some()
        || input.fat_g.is_some()
        || input.phosphorus_mg.is_some()
        || input.sodium_mg.is_some();
    let confidence = if input.nutrition_confidence.trim().is_empty() {
        if has_any_nutrient {
            "estimated"
        } else {
            "unknown"
        }
    } else {
        one_of(
            input.nutrition_confidence.trim(),
            ALLOWED_CONFIDENCE,
            "nutrition_confidence",
        )?
    };

    validate_nutrition_fields(
        input.serving_size_value,
        serving_size_unit.as_deref(),
        nutrition_basis.as_deref(),
        input.kcal_per_basis,
        input.protein_g,
        input.fat_g,
        input.phosphorus_mg,
        input.sodium_mg,
        input.moisture_pct,
        confidence,
    )?;

    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO food_items(
                household_id,name,brand,variant,category,food_form,completeness,
                serving_size_value,serving_size_unit,serving_label,
                nutrition_basis,kcal_per_basis,protein_g,fat_g,phosphorus_mg,sodium_mg,moisture_pct,
                nutrition_confidence,nutrition_source,notes,is_favorite,archived,
                created_by,created_at,updated_at
            ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,0,?,?,?) RETURNING *"#,
            [
                household_id.into(),
                name.clone().into(),
                brand.into(),
                variant.into(),
                category.into(),
                food_form.into(),
                completeness.into(),
                input.serving_size_value.into(),
                serving_size_unit.into(),
                serving_label.into(),
                nutrition_basis.into(),
                input.kcal_per_basis.into(),
                input.protein_g.into(),
                input.fat_g.into(),
                input.phosphorus_mg.into(),
                input.sodium_mg.into(),
                input.moisture_pct.into(),
                confidence.into(),
                nutrition_source.into(),
                notes.into(),
                i64::from(input.is_favorite).into(),
                actor.into(),
                now.clone().into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("failed to insert food item"))?;
    let item = food_item_from_row(row)?;
    audit(
        &tx,
        household_id,
        actor,
        "food_item.created",
        "food_item",
        item.id,
        &item.display_name(),
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(item)
}

/// Updates a household catalogue item. Deliberately NEVER mutates existing
/// `food_diary_entries` snapshots so historical consumption records remain stable.
pub async fn update_food(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    food_id: i64,
    patch: FoodItemPatch,
) -> Result<Option<FoodItem>> {
    let Some(existing) = get_food(db, household_id, food_id).await? else {
        return Ok(None);
    };

    let name = match patch.name {
        Some(v) => bounded(&v, 140, "name")?,
        None => existing.name.clone(),
    };
    let brand = match patch.brand {
        Some(v) => bounded_opt(v.as_deref(), 100, "brand")?,
        None => existing.brand.clone(),
    };
    let variant = match patch.variant {
        Some(v) => bounded_opt(v.as_deref(), 120, "variant")?,
        None => existing.variant.clone(),
    };
    let category = match patch.category {
        Some(v) => one_of(v.trim(), ALLOWED_CATEGORIES, "category")?.to_owned(),
        None => existing.category.clone(),
    };
    let food_form = match patch.food_form {
        Some(v) => bounded_opt(v.as_deref(), 60, "food_form")?,
        None => existing.food_form.clone(),
    };
    let completeness = match patch.completeness {
        Some(v) => one_of(v.trim(), ALLOWED_COMPLETENESS, "completeness")?.to_owned(),
        None => existing.completeness.clone(),
    };
    let serving_size_value = patch
        .serving_size_value
        .unwrap_or(existing.serving_size_value);
    let serving_size_unit = match patch.serving_size_unit {
        Some(v) => bounded_opt(v.as_deref(), 30, "serving_size_unit")?.map(|u| normalize_unit(&u)),
        None => existing.serving_size_unit.clone(),
    };
    let serving_label = match patch.serving_label {
        Some(v) => bounded_opt(v.as_deref(), 40, "serving_label")?.map(|u| normalize_unit(&u)),
        None => existing.serving_label.clone(),
    };
    let nutrition_basis = match patch.nutrition_basis {
        Some(v) => bounded_opt(v.as_deref(), 30, "nutrition_basis")?,
        None => existing.nutrition_basis.clone(),
    };
    let kcal_per_basis = patch.kcal_per_basis.unwrap_or(existing.kcal_per_basis);
    let protein_g = patch.protein_g.unwrap_or(existing.protein_g);
    let fat_g = patch.fat_g.unwrap_or(existing.fat_g);
    let phosphorus_mg = patch.phosphorus_mg.unwrap_or(existing.phosphorus_mg);
    let sodium_mg = patch.sodium_mg.unwrap_or(existing.sodium_mg);
    let moisture_pct = patch.moisture_pct.unwrap_or(existing.moisture_pct);
    let nutrition_confidence = match patch.nutrition_confidence {
        Some(v) => one_of(v.trim(), ALLOWED_CONFIDENCE, "nutrition_confidence")?.to_owned(),
        None => existing.nutrition_confidence.clone(),
    };
    let nutrition_source = match patch.nutrition_source {
        Some(v) => bounded_opt(v.as_deref(), 500, "nutrition_source")?,
        None => existing.nutrition_source.clone(),
    };
    let notes = match patch.notes {
        Some(v) => bounded_opt(v.as_deref(), 1000, "notes")?,
        None => existing.notes.clone(),
    };
    let is_favorite = patch.is_favorite.unwrap_or(existing.is_favorite);
    let archived = patch.archived.unwrap_or(existing.archived);

    validate_nutrition_fields(
        serving_size_value,
        serving_size_unit.as_deref(),
        nutrition_basis.as_deref(),
        kcal_per_basis,
        protein_g,
        fat_g,
        phosphorus_mg,
        sodium_mg,
        moisture_pct,
        &nutrition_confidence,
    )?;

    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE food_items SET
                name=?,brand=?,variant=?,category=?,food_form=?,completeness=?,
                serving_size_value=?,serving_size_unit=?,serving_label=?,
                nutrition_basis=?,kcal_per_basis=?,protein_g=?,fat_g=?,phosphorus_mg=?,sodium_mg=?,moisture_pct=?,
                nutrition_confidence=?,nutrition_source=?,notes=?,is_favorite=?,archived=?,updated_at=?
               WHERE household_id=? AND id=? RETURNING *"#,
            [
                name.into(),
                brand.into(),
                variant.into(),
                category.into(),
                food_form.into(),
                completeness.into(),
                serving_size_value.into(),
                serving_size_unit.into(),
                serving_label.into(),
                nutrition_basis.into(),
                kcal_per_basis.into(),
                protein_g.into(),
                fat_g.into(),
                phosphorus_mg.into(),
                sodium_mg.into(),
                moisture_pct.into(),
                nutrition_confidence.into(),
                nutrition_source.into(),
                notes.into(),
                i64::from(is_favorite).into(),
                i64::from(archived).into(),
                now.clone().into(),
                household_id.into(),
                food_id.into(),
            ],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let updated = food_item_from_row(row)?;
    let detail = serde_json::json!({
        "before_kcal": existing.kcal_per_basis,
        "after_kcal": updated.kcal_per_basis,
        "before_basis": existing.nutrition_basis,
        "after_basis": updated.nutrition_basis,
    })
    .to_string();
    audit(
        &tx,
        household_id,
        actor,
        "food_item.updated",
        "food_item",
        food_id,
        &detail,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(updated))
}

/// Returns the most frequently logged (and favourited) household foods for a pet
/// so owners can log common meals in one tap.
pub async fn list_frequent_foods(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> Result<Vec<FoodItem>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT f.*, COUNT(e.id) AS usage_count
               FROM food_items f
               LEFT JOIN food_diary_entries e
                 ON e.food_item_id = f.id
                 AND e.household_id = f.household_id
                 AND e.pet_id = ?
                 AND e.status = 'active'
               WHERE f.household_id = ? AND f.archived = 0
               GROUP BY f.id
               ORDER BY f.is_favorite DESC, usage_count DESC, f.name ASC
               LIMIT ?"#,
            [
                pet_id.into(),
                household_id.into(),
                limit.clamp(1, 30).into(),
            ],
        ))
        .await?;
    rows.into_iter().map(food_item_from_row).collect()
}

// ---------------------------------------------------------------------------
// Food diary CRUD (atomic multi-item meals, idempotency, edit, undo/restore)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct RecordedMealResult {
    pub meal_group_id: String,
    pub deduplicated: bool,
    pub entries: Vec<FoodDiaryEntry>,
}

#[allow(clippy::too_many_arguments)]
pub async fn record_meal_entries(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    actor: &str,
    source: &str,
    fed_on: &str,
    fed_at: Option<&str>,
    fed_time_precision: &str,
    meal_label: Option<&str>,
    idempotency_key: Option<&str>,
    items: Vec<NewFoodIntakeItem>,
) -> Result<RecordedMealResult> {
    if items.is_empty() || items.len() > 25 {
        return Err(anyhow!("Provide between 1 and 25 food items per meal."));
    }
    let fed_on = validate_fed_on(fed_on)?;
    one_of(
        fed_time_precision,
        ALLOWED_TIME_PRECISION,
        "fed_time_precision",
    )?;
    let meal_label = bounded_opt(meal_label, 40, "meal_label")?;
    let idempotency_key = bounded_opt(idempotency_key, 120, "idempotency_key")?;
    let fed_at_clean = match fed_at.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let parsed = DateTime::parse_from_rfc3339(raw)
                .map_err(|_| anyhow!("fed_at must be a valid RFC3339 timestamp."))?;
            Some(parsed.with_timezone(&Utc).to_rfc3339())
        }
        None => None,
    };

    // Verify pet belongs to this household.
    let pet_exists = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet_id.into()],
        ))
        .await?
        .is_some();
    if !pet_exists {
        return Err(anyhow!("Pet not found in this household."));
    }

    // Retry-safe idempotency check.
    if let Some(key) = idempotency_key.as_deref()
        && let Some(prev) = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT pet_id, meal_group_id, entry_ids_json FROM food_intake_requests WHERE household_id=? AND idempotency_key=?",
                [household_id.into(), key.into()],
            ))
            .await?
    {
        let prev_pet_id: i64 = prev.try_get("", "pet_id")?;
        if prev_pet_id != pet_id {
            return Err(anyhow!(
                "That idempotency_key was already used for a different pet."
            ));
        }
        let meal_group_id: String = prev.try_get("", "meal_group_id")?;
        let ids_json: String = prev.try_get("", "entry_ids_json")?;
        let ids: Vec<i64> = serde_json::from_str(&ids_json).unwrap_or_default();
        let mut entries = Vec::new();
        for id in ids {
            if let Some(entry) = get_food_entry(db, household_id, id).await? {
                entries.push(entry);
            }
        }
        return Ok(RecordedMealResult {
            meal_group_id,
            deduplicated: true,
            entries,
        });
    }

    let now = Utc::now().to_rfc3339();
    let meal_group_id = format!("meal_{}", crate::auth::new_session_token());
    let tx = db.begin().await?;
    let mut created = Vec::with_capacity(items.len());
    let mut created_ids = Vec::with_capacity(items.len());

    for item in items {
        let intake_status = one_of(
            item.intake_status.trim(),
            ALLOWED_INTAKE_STATUS,
            "intake_status",
        )?;
        let appetite = match item
            .appetite
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(a) => Some(one_of(a, ALLOWED_APPETITE, "appetite")?.to_owned()),
            None => (intake_status == "refused").then(|| "refused".to_owned()),
        };
        let offered_amount =
            check_non_negative(item.offered_amount, MAX_QUANTITY, "offered_amount")?;
        let offered_unit = bounded_opt(item.offered_unit.as_deref(), 40, "offered_unit")?
            .map(|u| normalize_unit(&u));
        if offered_amount.is_some() && offered_unit.is_none() {
            return Err(anyhow!(
                "offered_unit is required when offered_amount is provided."
            ));
        }

        let mut consumed_amount =
            check_non_negative(item.consumed_amount, MAX_QUANTITY, "consumed_amount")?;
        let mut consumed_unit = bounded_opt(item.consumed_unit.as_deref(), 40, "consumed_unit")?
            .map(|u| normalize_unit(&u));

        match intake_status {
            "consumed_all" => {
                if consumed_amount.is_none() && offered_amount.is_some() {
                    consumed_amount = offered_amount;
                    consumed_unit = offered_unit.clone();
                }
                if let Some(v) = consumed_amount
                    && v <= 0.0
                {
                    return Err(anyhow!(
                        "consumed_amount must be > 0 for consumed_all; use 'refused' for 0 eaten."
                    ));
                }
            }
            "consumed_partial" => {
                if let Some(v) = consumed_amount
                    && v <= 0.0
                {
                    return Err(anyhow!(
                        "consumed_amount must be > 0 for consumed_partial; use 'refused' for 0 eaten."
                    ));
                }
            }
            "refused" => {
                if let Some(v) = consumed_amount
                    && v > 0.0
                {
                    return Err(anyhow!(
                        "consumed_amount cannot be > 0 when intake_status is 'refused'."
                    ));
                }
                consumed_amount = Some(0.0);
                if consumed_unit.is_none() {
                    consumed_unit = offered_unit.clone();
                }
            }
            "consumed_unknown_amount" | "not_offered" => {
                consumed_amount = None;
                consumed_unit = None;
            }
            _ => {}
        }

        if consumed_amount.is_some() && intake_status != "refused" && consumed_unit.is_none() {
            return Err(anyhow!(
                "consumed_unit is required when consumed_amount is provided."
            ));
        }

        let wording = bounded(&item.original_wording, 2000, "original_wording")?;
        let preparation = bounded_opt(item.preparation.as_deref(), 200, "preparation")?;
        let notes = bounded_opt(item.notes.as_deref(), 1000, "notes")?;

        // Resolve food snapshot: from household catalogue item or ad-hoc snapshot.
        let (resolved_food_id, snapshot) = if let Some(fid) = item.food_item_id {
            let row = tx
                .query_one(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "SELECT * FROM food_items WHERE household_id=? AND id=?",
                    [household_id.into(), fid.into()],
                ))
                .await?
                .ok_or_else(|| anyhow!("Food item #{fid} not found in this household."))?;
            let food = food_item_from_row(row)?;
            (Some(food.id), food.to_snapshot())
        } else if let Some(mut snap) = item.inline_snapshot {
            snap.food_name = bounded(&snap.food_name, 140, "food_name")?;
            snap.brand = bounded_opt(snap.brand.as_deref(), 100, "brand")?;
            snap.variant = bounded_opt(snap.variant.as_deref(), 120, "variant")?;
            snap.serving_size_unit =
                bounded_opt(snap.serving_size_unit.as_deref(), 30, "serving_size_unit")?
                    .map(|u| normalize_unit(&u));
            snap.serving_label = bounded_opt(snap.serving_label.as_deref(), 40, "serving_label")?
                .map(|u| normalize_unit(&u));
            snap.nutrition_basis =
                bounded_opt(snap.nutrition_basis.as_deref(), 30, "nutrition_basis")?;
            snap.nutrition_source =
                bounded_opt(snap.nutrition_source.as_deref(), 500, "nutrition_source")?;
            if let Some(cat) = snap.category.as_deref() {
                one_of(cat, ALLOWED_CATEGORIES, "category")?;
            }
            if let Some(comp) = snap.completeness.as_deref() {
                one_of(comp, ALLOWED_COMPLETENESS, "completeness")?;
            }
            let has_nutr = snap.kcal_per_basis.is_some()
                || snap.protein_g.is_some()
                || snap.fat_g.is_some()
                || snap.phosphorus_mg.is_some()
                || snap.sodium_mg.is_some();
            if snap.nutrition_confidence.trim().is_empty() {
                snap.nutrition_confidence = if has_nutr {
                    "estimated".into()
                } else {
                    "unknown".into()
                };
            }
            validate_nutrition_fields(
                snap.serving_size_value,
                snap.serving_size_unit.as_deref(),
                snap.nutrition_basis.as_deref(),
                snap.kcal_per_basis,
                snap.protein_g,
                snap.fat_g,
                snap.phosphorus_mg,
                snap.sodium_mg,
                snap.moisture_pct,
                &snap.nutrition_confidence,
            )?;
            (None, snap)
        } else {
            return Err(anyhow!(
                "Each item requires either food_item_id or food_name."
            ));
        };

        let computed = compute_entry_nutrition(
            intake_status,
            consumed_amount,
            consumed_unit.as_deref(),
            item.is_approximate,
            &snapshot,
        );

        let row = tx
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                r#"INSERT INTO food_diary_entries(
                    household_id,pet_id,food_item_id,meal_group_id,status,
                    intake_status,appetite,
                    offered_amount,offered_unit,consumed_amount,consumed_unit,is_approximate,
                    fed_on,fed_at,fed_time_precision,meal_label,preparation,
                    original_wording,notes,source,recorded_by,recorded_at,updated_at,
                    snapshot_food_name,snapshot_brand,snapshot_variant,snapshot_category,snapshot_completeness,
                    snapshot_serving_size_value,snapshot_serving_size_unit,snapshot_serving_label,
                    snapshot_nutrition_basis,snapshot_kcal_per_basis,snapshot_protein_g,snapshot_fat_g,
                    snapshot_phosphorus_mg,snapshot_sodium_mg,snapshot_moisture_pct,
                    snapshot_nutrition_confidence,snapshot_nutrition_source,
                    computed_kcal,computed_protein_g,computed_fat_g,computed_phosphorus_mg,computed_sodium_mg,
                    calculation_status,calculation_note
                ) VALUES (
                    ?,?,?,?,'active',
                    ?,?,
                    ?,?,?,?,?,
                    ?,?,?,?,?,
                    ?,?,?,?,?,?,
                    ?,?,?,?,?,
                    ?,?,?,
                    ?,?,?,?,
                    ?,?,?,
                    ?,?,
                    ?,?,?,?,?,
                    ?,?
                ) RETURNING *"#,
                [
                    household_id.into(),
                    pet_id.into(),
                    resolved_food_id.into(),
                    meal_group_id.clone().into(),
                    intake_status.into(),
                    appetite.into(),
                    offered_amount.into(),
                    offered_unit.into(),
                    consumed_amount.into(),
                    consumed_unit.into(),
                    i64::from(item.is_approximate).into(),
                    fed_on.clone().into(),
                    fed_at_clean.clone().into(),
                    fed_time_precision.into(),
                    meal_label.clone().into(),
                    preparation.into(),
                    wording.clone().into(),
                    notes.into(),
                    source.into(),
                    actor.into(),
                    now.clone().into(),
                    now.clone().into(),
                    snapshot.food_name.into(),
                    snapshot.brand.into(),
                    snapshot.variant.into(),
                    snapshot.category.into(),
                    snapshot.completeness.into(),
                    snapshot.serving_size_value.into(),
                    snapshot.serving_size_unit.into(),
                    snapshot.serving_label.into(),
                    snapshot.nutrition_basis.into(),
                    snapshot.kcal_per_basis.into(),
                    snapshot.protein_g.into(),
                    snapshot.fat_g.into(),
                    snapshot.phosphorus_mg.into(),
                    snapshot.sodium_mg.into(),
                    snapshot.moisture_pct.into(),
                    snapshot.nutrition_confidence.into(),
                    snapshot.nutrition_source.into(),
                    computed.kcal.into(),
                    computed.protein_g.into(),
                    computed.fat_g.into(),
                    computed.phosphorus_mg.into(),
                    computed.sodium_mg.into(),
                    computed.calculation_status.into(),
                    computed.calculation_note.into(),
                ],
            ))
            .await?
            .ok_or_else(|| anyhow!("failed to insert food diary entry"))?;
        let entry = diary_entry_from_row(row)?;
        audit(
            &tx,
            household_id,
            actor,
            "food_entry.created",
            "food_diary_entry",
            entry.id,
            &wording,
            &now,
        )
        .await?;
        created_ids.push(entry.id);
        created.push(entry);
    }

    if let Some(key) = idempotency_key {
        let ids_json = serde_json::to_string(&created_ids)?;
        tx.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO food_intake_requests(household_id,pet_id,idempotency_key,meal_group_id,entry_ids_json,created_at) VALUES(?,?,?,?,?,?)",
            [
                household_id.into(),
                pet_id.into(),
                key.into(),
                meal_group_id.clone().into(),
                ids_json.into(),
                now.into(),
            ],
        ))
        .await?;
    }

    tx.commit().await?;
    Ok(RecordedMealResult {
        meal_group_id,
        deduplicated: false,
        entries: created,
    })
}

pub async fn get_food_entry(
    db: &DatabaseConnection,
    household_id: i64,
    entry_id: i64,
) -> Result<Option<FoodDiaryEntry>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT * FROM food_diary_entries WHERE household_id=? AND id=?",
            [household_id.into(), entry_id.into()],
        ))
        .await?;
    row.map(diary_entry_from_row).transpose()
}

pub async fn get_active_food_entry_for_pet(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    entry_id: i64,
) -> Result<Option<FoodDiaryEntry>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT * FROM food_diary_entries WHERE household_id=? AND pet_id=? AND id=? AND status='active'",
            [household_id.into(), pet_id.into(), entry_id.into()],
        ))
        .await?;
    row.map(diary_entry_from_row).transpose()
}

#[derive(Clone, Debug, Default)]
pub struct FoodEntryFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub intake_status: Option<String>,
    pub food_item_id: Option<i64>,
    pub limit: u64,
    pub offset: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FoodEntryPage {
    pub items: Vec<FoodDiaryEntry>,
    pub total_matching: i64,
    pub limit: u64,
    pub offset: u64,
    pub has_more: bool,
}

pub async fn list_food_entries(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    filter: FoodEntryFilter,
) -> Result<FoodEntryPage> {
    let mut where_clause = String::from("WHERE household_id=? AND pet_id=? AND status='active'");
    let mut values: Vec<sea_orm::Value> = vec![household_id.into(), pet_id.into()];

    if let Some(since) = filter
        .since
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        NaiveDate::parse_from_str(since, "%Y-%m-%d")
            .map_err(|_| anyhow!("since must be YYYY-MM-DD."))?;
        where_clause.push_str(" AND fed_on>=?");
        values.push(since.into());
    }
    if let Some(until) = filter
        .until
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        NaiveDate::parse_from_str(until, "%Y-%m-%d")
            .map_err(|_| anyhow!("until must be YYYY-MM-DD."))?;
        where_clause.push_str(" AND fed_on<=?");
        values.push(until.into());
    }
    if let Some(status) = filter
        .intake_status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        one_of(status, ALLOWED_INTAKE_STATUS, "intake_status")?;
        where_clause.push_str(" AND intake_status=?");
        values.push(status.into());
    }
    if let Some(fid) = filter.food_item_id {
        where_clause.push_str(" AND food_item_id=?");
        values.push(fid.into());
    }

    let count_sql = format!("SELECT COUNT(*) AS n FROM food_diary_entries {where_clause}");
    let total_matching: i64 = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            count_sql,
            values.clone(),
        ))
        .await?
        .map(|row| row.try_get("", "n"))
        .transpose()?
        .unwrap_or(0);

    let limit = if filter.limit == 0 {
        50
    } else {
        filter.limit.clamp(1, 500)
    };
    let offset = filter.offset;
    let query_sql = format!(
        "SELECT * FROM food_diary_entries {where_clause} ORDER BY fed_on DESC, COALESCE(datetime(fed_at), datetime(recorded_at)) DESC, id DESC LIMIT ? OFFSET ?"
    );
    values.push(limit.into());
    values.push(offset.into());

    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            query_sql,
            values,
        ))
        .await?;
    let items: Vec<FoodDiaryEntry> = rows
        .into_iter()
        .map(diary_entry_from_row)
        .collect::<Result<Vec<_>>>()?;
    let has_more = (offset as i64 + items.len() as i64) < total_matching;
    Ok(FoodEntryPage {
        items,
        total_matching,
        limit,
        offset,
        has_more,
    })
}

pub async fn update_food_entry(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    entry_id: i64,
    patch: FoodIntakePatch,
) -> Result<Option<FoodDiaryEntry>> {
    let Some(existing) = get_food_entry(db, household_id, entry_id).await? else {
        return Ok(None);
    };
    if existing.status != "active" {
        return Err(anyhow!("Cannot edit a deleted food diary entry."));
    }

    let intake_status = match patch.intake_status {
        Some(v) => one_of(v.trim(), ALLOWED_INTAKE_STATUS, "intake_status")?.to_owned(),
        None => existing.intake_status.clone(),
    };
    let appetite = match patch.appetite {
        Some(Some(v)) if !v.trim().is_empty() => {
            Some(one_of(v.trim(), ALLOWED_APPETITE, "appetite")?.to_owned())
        }
        Some(_) => None,
        None => existing.appetite.clone(),
    };
    let offered_amount = match patch.offered_amount {
        Some(v) => check_non_negative(v, MAX_QUANTITY, "offered_amount")?,
        None => existing.offered_amount,
    };
    let offered_unit = match patch.offered_unit {
        Some(v) => bounded_opt(v.as_deref(), 40, "offered_unit")?.map(|u| normalize_unit(&u)),
        None => existing.offered_unit.clone(),
    };
    let mut consumed_amount = match patch.consumed_amount {
        Some(v) => check_non_negative(v, MAX_QUANTITY, "consumed_amount")?,
        None => existing.consumed_amount,
    };
    let mut consumed_unit = match patch.consumed_unit {
        Some(v) => bounded_opt(v.as_deref(), 40, "consumed_unit")?.map(|u| normalize_unit(&u)),
        None => existing.consumed_unit.clone(),
    };
    let is_approximate = patch.is_approximate.unwrap_or(existing.is_approximate);
    let fed_on = match patch.fed_on {
        Some(v) => validate_fed_on(&v)?,
        None => existing.fed_on.clone(),
    };
    let meal_label = match patch.meal_label {
        Some(v) => bounded_opt(v.as_deref(), 40, "meal_label")?,
        None => existing.meal_label.clone(),
    };
    let preparation = match patch.preparation {
        Some(v) => bounded_opt(v.as_deref(), 200, "preparation")?,
        None => existing.preparation.clone(),
    };
    let original_wording = match patch.original_wording {
        Some(v) => bounded(&v, 2000, "original_wording")?,
        None => existing.original_wording.clone(),
    };
    let notes = match patch.notes {
        Some(v) => bounded_opt(v.as_deref(), 1000, "notes")?,
        None => existing.notes.clone(),
    };

    match intake_status.as_str() {
        "refused" => {
            consumed_amount = Some(0.0);
        }
        "consumed_unknown_amount" | "not_offered" => {
            consumed_amount = None;
            consumed_unit = None;
        }
        "consumed_all" | "consumed_partial" => {
            if let Some(v) = consumed_amount
                && v <= 0.0
            {
                return Err(anyhow!(
                    "consumed_amount must be > 0 for {intake_status}; use 'refused' for 0 eaten."
                ));
            }
            if consumed_amount.is_some() && consumed_unit.is_none() {
                return Err(anyhow!(
                    "consumed_unit is required when consumed_amount is provided."
                ));
            }
        }
        _ => {}
    }

    let computed = compute_entry_nutrition(
        &intake_status,
        consumed_amount,
        consumed_unit.as_deref(),
        is_approximate,
        &existing.snapshot,
    );

    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"UPDATE food_diary_entries SET
                intake_status=?,appetite=?,
                offered_amount=?,offered_unit=?,consumed_amount=?,consumed_unit=?,is_approximate=?,
                fed_on=?,meal_label=?,preparation=?,original_wording=?,notes=?,updated_at=?,
                computed_kcal=?,computed_protein_g=?,computed_fat_g=?,computed_phosphorus_mg=?,computed_sodium_mg=?,
                calculation_status=?,calculation_note=?
               WHERE household_id=? AND id=? AND status='active' RETURNING *"#,
            [
                intake_status.into(),
                appetite.into(),
                offered_amount.into(),
                offered_unit.into(),
                consumed_amount.into(),
                consumed_unit.into(),
                i64::from(is_approximate).into(),
                fed_on.into(),
                meal_label.into(),
                preparation.into(),
                original_wording.into(),
                notes.into(),
                now.clone().into(),
                computed.kcal.into(),
                computed.protein_g.into(),
                computed.fat_g.into(),
                computed.phosphorus_mg.into(),
                computed.sodium_mg.into(),
                computed.calculation_status.into(),
                computed.calculation_note.into(),
                household_id.into(),
                entry_id.into(),
            ],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let updated = diary_entry_from_row(row)?;
    let detail = serde_json::json!({
        "before_status": existing.intake_status,
        "after_status": updated.intake_status,
        "before_consumed": existing.consumed_amount,
        "after_consumed": updated.consumed_amount,
        "before_kcal": existing.computed_kcal,
        "after_kcal": updated.computed_kcal,
    })
    .to_string();
    audit(
        &tx,
        household_id,
        actor,
        "food_entry.updated",
        "food_diary_entry",
        entry_id,
        &detail,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(updated))
}

pub async fn delete_food_entry(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    entry_id: i64,
) -> Result<Option<FoodDiaryEntry>> {
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE food_diary_entries SET status='deleted', updated_at=? WHERE household_id=? AND id=? AND status='active' RETURNING *",
            [now.clone().into(), household_id.into(), entry_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let deleted = diary_entry_from_row(row)?;
    audit(
        &tx,
        household_id,
        actor,
        "food_entry.deleted",
        "food_diary_entry",
        entry_id,
        &deleted.display_food_name(),
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(deleted))
}

pub async fn restore_food_entry(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    entry_id: i64,
) -> Result<Option<FoodDiaryEntry>> {
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE food_diary_entries SET status='active', updated_at=? WHERE household_id=? AND id=? AND status='deleted' RETURNING *",
            [now.clone().into(), household_id.into(), entry_id.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let restored = diary_entry_from_row(row)?;
    audit(
        &tx,
        household_id,
        actor,
        "food_entry.restored",
        "food_diary_entry",
        entry_id,
        &restored.display_food_name(),
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(restored))
}

// ---------------------------------------------------------------------------
// Owner/vet-entered nutrition targets (never auto-prescribed)
// ---------------------------------------------------------------------------

pub async fn get_active_target(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> Result<Option<NutritionTarget>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT * FROM pet_nutrition_targets WHERE household_id=? AND pet_id=? AND active=1 ORDER BY updated_at DESC, id DESC LIMIT 1",
            [household_id.into(), pet_id.into()],
        ))
        .await?;
    row.map(target_from_row).transpose()
}

pub async fn set_nutrition_target(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    actor: &str,
    input: NewNutritionTarget,
) -> Result<NutritionTarget> {
    let source = one_of(input.source.trim(), ALLOWED_TARGET_SOURCES, "source")?;
    let source_note = bounded(&input.source_note, 500, "source_note")?;
    let effective_from = match input
        .effective_from
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(validate_fed_on(raw)?),
        None => None,
    };

    let kcal_min = check_positive(input.daily_kcal_min, MAX_NUTRIENT_VALUE, "daily_kcal_min")?;
    let kcal_max = check_positive(input.daily_kcal_max, MAX_NUTRIENT_VALUE, "daily_kcal_max")?;
    if let (Some(lo), Some(hi)) = (kcal_min, kcal_max)
        && lo > hi
    {
        return Err(anyhow!("daily_kcal_min cannot exceed daily_kcal_max."));
    }
    let protein_min = check_non_negative(
        input.daily_protein_g_min,
        MAX_NUTRIENT_VALUE,
        "daily_protein_g_min",
    )?;
    let protein_max = check_non_negative(
        input.daily_protein_g_max,
        MAX_NUTRIENT_VALUE,
        "daily_protein_g_max",
    )?;
    if let (Some(lo), Some(hi)) = (protein_min, protein_max)
        && lo > hi
    {
        return Err(anyhow!(
            "daily_protein_g_min cannot exceed daily_protein_g_max."
        ));
    }
    let fat_min = check_non_negative(input.daily_fat_g_min, MAX_NUTRIENT_VALUE, "daily_fat_g_min")?;
    let fat_max = check_non_negative(input.daily_fat_g_max, MAX_NUTRIENT_VALUE, "daily_fat_g_max")?;
    if let (Some(lo), Some(hi)) = (fat_min, fat_max)
        && lo > hi
    {
        return Err(anyhow!("daily_fat_g_min cannot exceed daily_fat_g_max."));
    }
    let phos_max = check_non_negative(
        input.daily_phosphorus_mg_max,
        MAX_NUTRIENT_VALUE,
        "daily_phosphorus_mg_max",
    )?;
    let sodium_max = check_non_negative(
        input.daily_sodium_mg_max,
        MAX_NUTRIENT_VALUE,
        "daily_sodium_mg_max",
    )?;

    if kcal_min.is_none()
        && kcal_max.is_none()
        && protein_min.is_none()
        && protein_max.is_none()
        && fat_min.is_none()
        && fat_max.is_none()
        && phos_max.is_none()
        && sodium_max.is_none()
    {
        return Err(anyhow!(
            "Provide at least one calorie or nutrient target value."
        ));
    }

    let pet_exists = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM pets WHERE household_id=? AND id=?",
            [household_id.into(), pet_id.into()],
        ))
        .await?
        .is_some();
    if !pet_exists {
        return Err(anyhow!("Pet not found in this household."));
    }

    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    tx.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE pet_nutrition_targets SET active=0, updated_at=? WHERE household_id=? AND pet_id=? AND active=1",
        [now.clone().into(), household_id.into(), pet_id.into()],
    ))
    .await?;
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO pet_nutrition_targets(
                household_id,pet_id,
                daily_kcal_min,daily_kcal_max,
                daily_protein_g_min,daily_protein_g_max,
                daily_fat_g_min,daily_fat_g_max,
                daily_phosphorus_mg_max,daily_sodium_mg_max,
                source,source_note,effective_from,active,created_by,created_at,updated_at
            ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,1,?,?,?) RETURNING *"#,
            [
                household_id.into(),
                pet_id.into(),
                kcal_min.into(),
                kcal_max.into(),
                protein_min.into(),
                protein_max.into(),
                fat_min.into(),
                fat_max.into(),
                phos_max.into(),
                sodium_max.into(),
                source.into(),
                source_note.clone().into(),
                effective_from.into(),
                actor.into(),
                now.clone().into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("failed to save nutrition target"))?;
    let target = target_from_row(row)?;
    audit(
        &tx,
        household_id,
        actor,
        "nutrition_target.set",
        "pet_nutrition_target",
        target.id,
        &source_note,
        &now,
    )
    .await?;
    tx.commit().await?;
    Ok(target)
}

pub async fn clear_nutrition_target(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    actor: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let tx = db.begin().await?;
    let changed = tx
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE pet_nutrition_targets SET active=0, updated_at=? WHERE household_id=? AND pet_id=? AND active=1",
            [now.clone().into(), household_id.into(), pet_id.into()],
        ))
        .await?
        .rows_affected()
        > 0;
    if changed {
        audit(
            &tx,
            household_id,
            actor,
            "nutrition_target.cleared",
            "pet",
            pet_id,
            "Cleared active owner/vet nutrition target",
            &now,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Loads a concise 30-day nutrition trend report for clinical briefs, vet shares,
/// and the Food tab.
pub async fn load_trend_report(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<NutritionTrendReport> {
    let earliest = since.min(until - Duration::days(29));
    let filter = FoodEntryFilter {
        since: Some(earliest.format("%Y-%m-%d").to_string()),
        until: Some(until.format("%Y-%m-%d").to_string()),
        limit: 500,
        ..Default::default()
    };
    let page = list_food_entries(db, household_id, pet_id, filter).await?;
    let target = get_active_target(db, household_id, pet_id).await?;
    Ok(build_trend_report(since, until, &page.items, target))
}

/// Keyset-paginated food diary page for `db::list_timeline`.
pub async fn timeline_food_page(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    before: Option<(DateTime<Utc>, i64)>,
    limit: u64,
) -> Result<Vec<FoodDiaryEntry>> {
    let ts_expr = "COALESCE(datetime(fed_at), datetime(fed_on || 'T12:00:00Z'))";
    let base =
        "SELECT * FROM food_diary_entries WHERE household_id=? AND pet_id=? AND status='active'";
    let (sql, values) = match before {
        Some((at, id)) => (
            format!(
                "{base} AND ({ts_expr} < datetime(?) OR ({ts_expr} = datetime(?) AND id < ?)) \
                 ORDER BY {ts_expr} DESC, id DESC LIMIT ?"
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
            format!("{base} ORDER BY {ts_expr} DESC, id DESC LIMIT ?"),
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
    rows.into_iter().map(diary_entry_from_row).collect()
}
