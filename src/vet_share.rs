//! Read-only veterinary shared-view adapters.
//! No network access, model inference, mutation or unauthenticated endpoints.
//! The sole caller verifies the expiring/revocable share token first.
use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

use crate::{
    analytes,
    clinical_cases::CaseDetail,
    domain::{LabReport, TimelineEntry, lab_effective_at},
};

#[derive(Clone, Debug)]
pub struct ShareFact {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct ShareTimelineItem {
    pub anchor: String,
    pub date: DateTime<Utc>,
    pub date_label: String,
    pub date_note: String,
    pub kind: String,
    pub title: String,
    pub original_words: String,
    pub provenance: String,
    pub facts: Vec<ShareFact>,
}

fn fact(label: &str, value: impl ToString) -> ShareFact {
    ShareFact {
        label: label.to_owned(),
        value: value.to_string(),
    }
}
fn optional(facts: &mut Vec<ShareFact>, label: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
        facts.push(fact(label, value));
    }
}

fn dated(at: DateTime<Utc>) -> String {
    at.format("%d %b %Y, %H:%M UTC").to_string()
}

pub fn build_timeline(entries: Vec<TimelineEntry>, cases: &[CaseDetail]) -> Vec<ShareTimelineItem> {
    let mut items: Vec<ShareTimelineItem> = entries
        .into_iter()
        .map(|entry| match entry {
            TimelineEntry::Event(event) => {
                let mut facts = vec![
                    fact("Event category", &event.event_type),
                    fact("Concept", &event.concept),
                ];
                optional(&mut facts, "Additional detail", event.details.as_deref());
                if let Some(symptom) = &event.symptom {
                    optional(&mut facts, "Symptom", Some(&symptom.symptom));
                    if let Some(count) = symptom.occurrence_count {
                        facts.push(fact("Occurrence count", count));
                    }
                    for (label, value) in [
                        ("Amount", symptom.amount.as_deref()),
                        ("Contents", symptom.contents.as_deref()),
                        ("Meal relation", symptom.meal_relation.as_deref()),
                        ("Drinking", symptom.water_status.as_deref()),
                        ("Appetite", symptom.appetite_status.as_deref()),
                        ("Energy", symptom.energy_status.as_deref()),
                        ("Pain", symptom.pain_status.as_deref()),
                        ("Symptom note", symptom.note.as_deref()),
                    ] {
                        optional(&mut facts, label, value);
                    }
                }
                ShareTimelineItem {
                    anchor: format!("event-{}", event.id),
                    date: event.occurred_at,
                    date_label: dated(event.occurred_at),
                    date_note: "Observation time as recorded".into(),
                    kind: "Health event".into(),
                    title: event.summary,
                    original_words: event.raw_input,
                    provenance: event.source,
                    facts,
                }
            }
            TimelineEntry::Weight(weight) => {
                let mut facts = Vec::new();
                optional(&mut facts, "Measurement note", weight.note.as_deref());
                ShareTimelineItem {
                    anchor: format!("weight-{}", weight.id),
                    date: weight.measured_at,
                    date_label: dated(weight.measured_at),
                    date_note: "Measured / dated weight".into(),
                    kind: "Weight".into(),
                    title: format!("Weight: {} kg", weight.weight_kg),
                    original_words: String::new(),
                    provenance: "Weight record".into(),
                    facts,
                }
            }
            TimelineEntry::Dose(dose) => {
                let mut facts = Vec::new();
                optional(
                    &mut facts,
                    "Active ingredient",
                    dose.active_ingredient.as_deref(),
                );
                if let Some(value) = dose.dose_value {
                    facts.push(fact(
                        "Dose",
                        format!(
                            "{} {}",
                            value,
                            dose.dose_unit.as_deref().unwrap_or("unit not recorded")
                        ),
                    ));
                }
                optional(&mut facts, "Route", dose.route.as_deref());
                facts.push(fact("Administration status", &dose.status));
                if let Some(at) = dose.scheduled_at {
                    facts.push(fact("Scheduled", dated(at)));
                }
                ShareTimelineItem {
                    anchor: format!("dose-{}", dose.id),
                    date: dose.administered_at,
                    date_label: dated(dose.administered_at),
                    date_note: "Administration timestamp".into(),
                    kind: "Medication".into(),
                    title: dose.name,
                    original_words: dose.raw_input.unwrap_or_default(),
                    provenance: "Administration record".into(),
                    facts,
                }
            }
            TimelineEntry::Lab(lab) => {
                let mut facts = vec![fact("Report file", &lab.source_filename)];
                facts.push(fact("Import / review state", &lab.parse_status));
                for result in &lab.results {
                    let mut value = result.value_text.clone();
                    if let Some(unit) = &result.unit {
                        value.push(' ');
                        value.push_str(unit);
                    }
                    if let Some(range) = &result.reference_range {
                        value.push_str(" · lab reference: ");
                        value.push_str(range);
                    }
                    if let Some(flag) = &result.flag {
                        value.push_str(" · flag: ");
                        value.push_str(flag);
                    }
                    facts.push(fact(&result.test_name, value));
                }
                ShareTimelineItem {
                    anchor: format!("lab-{}", lab.id),
                    date: lab_effective_at(&lab),
                    date_label: lab
                        .test_date
                        .as_deref()
                        .and_then(parse_test_date)
                        .map(|date| date.format("%d %b %Y").to_string())
                        .unwrap_or_else(|| dated(lab.imported_at)),
                    date_note: if lab.test_date.as_deref().and_then(parse_test_date).is_some() {
                        "Test date provided by lab".into()
                    } else {
                        "Test date unknown; sorted by import time".into()
                    },
                    kind: "Lab report".into(),
                    title: format!("Blood test · {} markers", lab.results.len()),
                    original_words: String::new(),
                    provenance: "Reported laboratory values; verify against original report".into(),
                    facts,
                }
            }
            TimelineEntry::Food(food) => {
                let mut facts = vec![
                    fact("Food", food.display_food_name()),
                    fact("Intake status", food.intake_status_label()),
                    fact("Amount", food.amount_label()),
                    fact("Calories", food.kcal_label()),
                ];
                optional(&mut facts, "Meal", food.meal_label.as_deref());
                optional(&mut facts, "Appetite", food.appetite.as_deref());
                optional(&mut facts, "Preparation", food.preparation.as_deref());
                if let Some(p) = food.computed_protein_g {
                    facts.push(fact("Protein", format!("{p:.1} g")));
                }
                if let Some(f) = food.computed_fat_g {
                    facts.push(fact("Fat", format!("{f:.1} g")));
                }
                if let Some(ph) = food.computed_phosphorus_mg {
                    facts.push(fact("Phosphorus", format!("{ph:.0} mg")));
                }
                if let Some(na) = food.computed_sodium_mg {
                    facts.push(fact("Sodium", format!("{na:.0} mg")));
                }
                optional(
                    &mut facts,
                    "Nutrition basis",
                    food.snapshot.nutrition_basis.as_deref(),
                );
                optional(
                    &mut facts,
                    "Nutrition source",
                    food.snapshot.nutrition_source.as_deref(),
                );
                optional(
                    &mut facts,
                    "Calculation note",
                    food.calculation_note.as_deref(),
                );
                optional(&mut facts, "Notes", food.notes.as_deref());
                let date = food.effective_at();
                ShareTimelineItem {
                    anchor: format!("food-{}", food.id),
                    date,
                    date_label: if food.fed_at.is_some() {
                        dated(date)
                    } else {
                        food.fed_on.clone()
                    },
                    date_note: match food.fed_time_precision.as_str() {
                        "exact" => "Stated feeding time".into(),
                        "approximate_time" => "Approximate feeding time".into(),
                        "date_only" => "Feeding date recorded (time not specified)".into(),
                        _ => "Recorded feeding entry".into(),
                    },
                    kind: format!("Food · {}", food.intake_status_label()),
                    title: format!("{} · {}", food.display_food_name(), food.amount_label()),
                    original_words: food.original_wording,
                    provenance: format!(
                        "Owner-recorded food diary ({})",
                        food.snapshot.nutrition_confidence
                    ),
                    facts,
                }
            }
        })
        .collect();

    for case in cases {
        for update in &case.updates {
            let observed = update.observed_on.as_deref().and_then(parse_test_date);
            let recorded = DateTime::parse_from_rfc3339(&update.created_at)
                .map(|date| date.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let date = observed
                .and_then(|date| date.and_hms_opt(12, 0, 0).map(|v| v.and_utc()))
                .unwrap_or(recorded);
            let mut facts = vec![fact("Investigation", &case.investigation.title)];
            if let Some(event_id) = update.linked_event_id {
                facts.push(fact("Linked record", format!("Health event #{event_id}")));
            }
            if let Some(food_id) = update.linked_food_entry_id {
                facts.push(fact("Linked feeding", format!("Food entry #{food_id}")));
            }
            items.push(ShareTimelineItem {
                anchor: format!("case-{}-update-{}", case.investigation.id, update.id),
                date,
                date_label: observed
                    .map(|date| date.format("%d %b %Y").to_string())
                    .unwrap_or_else(|| dated(recorded)),
                date_note: if observed.is_some() {
                    "Owner-reported observation date".into()
                } else {
                    "Occurrence date unknown; sorted by recorded time".into()
                },
                kind: format!("Investigation · {}", update.kind.replace('_', " ")),
                title: case.investigation.title.clone(),
                original_words: update.original_wording.clone(),
                provenance: if update.source == "owner_relayed_vet" {
                    "Owner relaying veterinarian; not independently verified".into()
                } else {
                    "Owner-reported".into()
                },
                facts,
            });
        }
    }
    // Preserve date ordering with a stable tie-breaker. A missing observation date
    // never masquerades as a known clinical onset.
    items.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.anchor.cmp(&b.anchor)));
    items
}

#[derive(Clone, Debug)]
pub struct MarkerObservation {
    pub report_id: i64,
    pub test_date: String,
    pub test_date_known: bool,
    pub printed_name: String,
    pub printed_value: String,
    pub printed_unit: String,
    pub reference_range: String,
    pub flag: String,
    pub plot_value: Option<f64>,
    pub x: f64,
    pub y: f64,
    pub tooltip: String,
}
#[derive(Clone, Debug)]
pub struct MarkerChart {
    pub key: String,
    pub label: String,
    pub unit: String,
    pub observations: Vec<MarkerObservation>,
    pub chart_points: Vec<MarkerObservation>,
    pub line: String,
    pub show_line: bool,
    pub min_label: String,
    pub max_label: String,
    pub start_label: String,
    pub end_label: String,
}
pub fn parse_test_date(date: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}
pub fn charts(reports: &[LabReport]) -> Vec<MarkerChart> {
    let mut groups = BTreeMap::<String, MarkerChart>::new();
    for report in reports {
        let known = report.test_date.as_deref().and_then(parse_test_date);
        for result in &report.results {
            let standard = analytes::standardize(
                &result.test_name,
                result.unit.as_deref(),
                result.value_numeric.filter(|value| value.is_finite()),
            );
            // Known units are converted by the existing read-only analyte module.
            // Unknown / incompatible units remain separate data series.
            let label = standard.label;
            let unit = standard.unit.unwrap_or_else(|| "unit not recorded".into());
            let key = standard.key;
            let obs = MarkerObservation {
                report_id: report.id,
                test_date: known
                    .map(|day| day.to_string())
                    .unwrap_or_else(|| "Unknown (not plotted)".into()),
                test_date_known: known.is_some(),
                printed_name: result.test_name.clone(),
                printed_value: result.value_text.clone(),
                printed_unit: result.unit.clone().unwrap_or_else(|| "not stated".into()),
                reference_range: result
                    .reference_range
                    .clone()
                    .unwrap_or_else(|| "not provided".into()),
                flag: result.flag.clone().unwrap_or_else(|| "not marked".into()),
                plot_value: standard.value.filter(|value| value.is_finite()),
                x: 0.0,
                y: 0.0,
                tooltip: String::new(),
            };
            groups
                .entry(key.clone())
                .or_insert_with(|| MarkerChart {
                    key,
                    label,
                    unit,
                    observations: Vec::new(),
                    chart_points: Vec::new(),
                    line: String::new(),
                    show_line: false,
                    min_label: String::new(),
                    max_label: String::new(),
                    start_label: String::new(),
                    end_label: String::new(),
                })
                .observations
                .push(obs);
        }
    }
    let mut charts: Vec<MarkerChart> = groups.into_values().collect();
    charts.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.unit.cmp(&b.unit)));
    for chart in &mut charts {
        chart
            .observations
            .sort_by(|a, b| a.test_date.cmp(&b.test_date));
        let mut plotted: Vec<MarkerObservation> = chart
            .observations
            .iter()
            .filter(|v| v.test_date_known && v.plot_value.is_some())
            .cloned()
            .collect();
        if plotted.is_empty() {
            continue;
        }
        let dates: Vec<NaiveDate> = plotted
            .iter()
            .filter_map(|r| parse_test_date(&r.test_date))
            .collect();
        let earliest = *dates.iter().min().expect("has plotted dates");
        let latest = *dates.iter().max().expect("has plotted dates");
        let values: Vec<f64> = plotted.iter().filter_map(|r| r.plot_value).collect();
        let mut minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
        let mut maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let padding = ((maximum - minimum).abs() * 0.12)
            .max(maximum.abs() * 0.05)
            .max(0.01);
        minimum -= padding;
        maximum += padding;
        let span = (maximum - minimum).max(0.001);
        let days = (latest - earliest).num_days();
        chart.show_line = days > 0 && plotted.len() > 1;
        for point in &mut plotted {
            let date = parse_test_date(&point.test_date).expect("filtered valid date");
            point.x = if days == 0 {
                300.0
            } else {
                44.0 + 512.0 * (date - earliest).num_days() as f64 / days as f64
            };
            point.y =
                170.0 - 135.0 * (point.plot_value.expect("filtered numeric") - minimum) / span;
            point.tooltip = format!(
                "{}: {} {} (report #{}, reference {}, flag {})",
                point.test_date,
                point.printed_value,
                point.printed_unit,
                point.report_id,
                point.reference_range,
                point.flag
            );
        }
        chart.line = plotted
            .iter()
            .map(|v| format!("{:.1},{:.1}", v.x, v.y))
            .collect::<Vec<_>>()
            .join(" ");
        chart.start_label = earliest.format("%d %b %Y").to_string();
        chart.end_label = latest.format("%d %b %Y").to_string();
        chart.min_label = format!("{minimum:.2}");
        chart.max_label = format!("{maximum:.2}");
        chart.chart_points = plotted;
    }
    charts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LabReport, LabResult};
    fn report(
        id: i64,
        date: Option<&str>,
        name: &str,
        value: Option<f64>,
        unit: &str,
    ) -> LabReport {
        LabReport {
            id,
            pet_id: 1,
            pet_name: "Example".into(),
            source_filename: "report.pdf".into(),
            raw_text: String::new(),
            test_date: date.map(str::to_owned),
            imported_at: Utc::now(),
            parse_status: "confirmed".into(),
            has_original: false,
            results: vec![LabResult {
                id,
                test_name: name.into(),
                value_text: value
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "<0.5".into()),
                value_numeric: value,
                unit: Some(unit.into()),
                reference_range: Some("0.8–2.4".into()),
                flag: None,
                parse_status: None,
            }],
        }
    }
    #[test]
    fn known_units_convert_before_charting_and_keep_original_values() {
        let data = charts(&[
            report(1, Some("2026-01-02"), "CREA", Some(1.8), "mg/dL"),
            report(2, Some("2026-02-02"), "Creatinina", Some(159.2), "µmol/L"),
        ]);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].unit, "µmol/L");
        assert_eq!(data[0].chart_points.len(), 2);
        assert_eq!(data[0].chart_points[0].plot_value, Some(159.2));
        assert_eq!(data[0].observations[0].printed_value, "1.8");
        assert_eq!(data[0].observations[0].printed_unit, "mg/dL");
    }
    #[test]
    fn unknown_dates_and_non_numeric_results_remain_visible_but_not_plotted() {
        let data = charts(&[
            report(1, None, "SDMA", Some(18.0), "µg/dL"),
            report(2, Some("2026-01-04"), "SDMA", None, "µg/dL"),
        ]);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].observations.len(), 2);
        assert!(data[0].chart_points.is_empty());
    }
    #[test]
    fn incomparable_units_stay_separate() {
        let data = charts(&[
            report(1, Some("2026-01-02"), "CREA", Some(2.0), "mg/dL"),
            report(2, Some("2026-01-03"), "CREA", Some(0.13), "mmol/L"),
        ]);
        assert_eq!(data.len(), 2);
    }
}
