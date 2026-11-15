//! Turning OCR text of a lab report into structured results.
//!
//! Real reports mix several layouts, often in one document:
//!
//! 1. Markdown table rows: `| HEMATOCRITO | 35,9 | % | (29,0-52,0) |`,
//!    sometimes with a method column or the unit inside the value cell.
//! 2. Heading + method + leader line, one analyte per block:
//!    `UREA / SUERO` / `Química seca…` / `RESULTADO... 63 ↑ mg/dL (30-60)`,
//!    plus sub-results such as `ALBUMINA... 30 g/L (24-37)`.
//! 3. Plain lines, the name sometimes wrapped over the lines above:
//!    `Volumen` / `corpuscular` / `medio 48.4 fl 39.0 - 53.0`.
//! 4. Name, value and range on separate lines:
//!    `Plaquetocrito` / `0.5 %` / `0.0 - 2.9`.
//!
//! OCR output repeats pages (page markdown plus page blocks), so identical
//! results are de-duplicated. Numbers are read with the report's own decimal
//! convention (see `labvalue`); the printed text is kept as printed.

use crate::labvalue::{self, NumberLocale};
use crate::ocr::ParsedLabResult;
use regex::Regex;
use std::sync::LazyLock;

pub struct ParsedReport {
    pub results: Vec<ParsedLabResult>,
    pub locale: NumberLocale,
    /// Result rows found more than once (usually a repeated OCR page).
    pub duplicates_removed: usize,
    pub test_date: Option<String>,
}

struct Candidate {
    name: String,
    value: String,
    marker_flag: Option<String>,
    unit: Option<String>,
    range: Option<String>,
}

static LEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?[^\s.])\s*(?:\.{2,}|…)\s*(.+)$").unwrap());
static COLON_RESULT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:result|resultado)\s*:\s*(.+)$").unwrap());
static COLON_TEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:test|prueba)\s*:\s*(.+)$").unwrap());
static RANGE_AT_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|\s)(\(\s*[<>≤≥]?\s*[\d.,]+(?:\s*[-–]\s*[\d.,]+)?\s*\)|[<>≤≥]\s*[\d.,]+|[\d.,]+\s*[-–]\s*[\d.,]+)\s*$",
    )
    .unwrap()
});
static RANGE_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\(?\s*(?:[<>≤≥]\s*[\d.,]+|[\d.,]+\s*[-–]\s*[\d.,]+)\s*\)?$").unwrap()
});

/// Leading markers some labs print before an out-of-range value.
fn marker_flag(token: &str) -> Option<&'static str> {
    match token {
        "^" | "↑" | "▲" | "H" | "h" => Some("high"),
        "↓" | "▼" | "L" | "l" => Some("low"),
        "●" | "•" | "*" | "!" => Some("abnormal"),
        _ => None,
    }
}

fn is_value_token(token: &str) -> bool {
    let t = token.trim_start_matches(['<', '>', '≤', '≥', '=']);
    let t = t.strip_prefix('-').unwrap_or(t);
    !t.is_empty()
        && t.starts_with(|c: char| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == '↑' || c == '↓')
}

fn looks_like_unit(unit: &str) -> bool {
    let u = unit.trim();
    if u.is_empty() || u.len() > 24 {
        return false;
    }
    let lower = u.to_lowercase();
    u.contains('/')
        || u.contains('%')
        || lower.starts_with("10^")
        || lower.starts_with("x10")
        || lower.starts_with("×10")
        || matches!(
            lower.as_str(),
            "fl" | "pg"
                | "g"
                | "mg"
                | "kg"
                | "ui"
                | "iu"
                | "u"
                | "s"
                | "seg"
                | "sec"
                | "mmhg"
                | "ratio"
                | "mosm/kg"
                | "ng"
                | "µg"
                | "ug"
                | "mm"
                | "cel"
                | "cells"
        )
}

/// Split "63 ↑ mg/dL (30-60)" / "● 28.4 g/dl 29.0 - 37.0" / "16.9 0.0 - 50.0".
fn parse_rest(rest: &str) -> Option<(String, Option<String>, Option<String>, Option<String>)> {
    let mut tokens = rest.split_whitespace().peekable();
    let mut flag: Option<String> = None;
    while let Some(token) = tokens.peek() {
        if let Some(f) = marker_flag(token) {
            flag.get_or_insert_with(|| f.to_owned());
            tokens.next();
        } else {
            break;
        }
    }
    let first = tokens.next()?;
    // Comparator printed as its own token: "< 0,10".
    let value = if matches!(first, "<" | ">" | "≤" | "≥") {
        format!("{first}{}", tokens.next()?)
    } else {
        first.to_owned()
    };
    if !is_value_token(&value) {
        return None;
    }
    let mut value = value;
    for arrow in ['↑', '↓'] {
        if value.contains(arrow) {
            flag.get_or_insert_with(|| marker_flag(&arrow.to_string()).unwrap().to_owned());
            value = value.replace(arrow, "");
        }
    }
    let mut remainder: Vec<&str> = Vec::new();
    for token in tokens {
        if remainder.is_empty()
            && let Some(f) = marker_flag(token)
        {
            flag.get_or_insert_with(|| f.to_owned());
            continue;
        }
        remainder.push(token);
    }
    let remainder = remainder.join(" ");
    let (unit_part, range) = match RANGE_AT_END.captures(&remainder) {
        Some(c) => {
            let m = c.get(1).unwrap();
            (
                remainder[..m.start()].trim().to_owned(),
                Some(clean_range(m.as_str())),
            )
        }
        None => (remainder.trim().to_owned(), None),
    };
    if !unit_part.is_empty() && !looks_like_unit(&unit_part) {
        return None;
    }
    Some((
        value,
        flag,
        (!unit_part.is_empty()).then_some(unit_part),
        range,
    ))
}

/// Older imports stored the value and unit together ("90 10^3/ul",
/// "^ 1,68 mg/dl"). Split such a value the way the parser would today:
/// (value, flag, unit).
pub fn split_stored_value(value_text: &str) -> Option<(String, Option<String>, Option<String>)> {
    parse_rest(value_text).map(|(value, flag, unit, _)| (value, flag, unit))
}

/// A stored name as the parser would print it today ("**Plaquetas**" → "Plaquetas").
pub fn clean_stored_name(name: &str) -> String {
    clean_name(name)
}

fn clean_range(range: &str) -> String {
    range
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim()
        .to_owned()
}

fn clean_name(name: &str) -> String {
    name.replace("**", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == ':' || c == '|' || c == '-' || c.is_whitespace())
        .to_owned()
}

fn has_letter(s: &str) -> bool {
    s.chars().any(char::is_alphabetic)
}

fn is_metadata_name(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    // Interpretation bands printed beside a result ("Bajo < 15 nmol/l",
    // "Normal 15-40", "Elevado > 50") describe the scale, not this animal.
    let band = n.trim_end_matches(['<', '>', '≤', '≥', ' ']);
    if matches!(
        band,
        "bajo"
            | "normal"
            | "alto"
            | "elevado"
            | "en el límite"
            | "límite"
            | "dudoso"
            | "low"
            | "high"
            | "borderline"
            | "elevated"
            | "equivocal"
            | "grey zone"
            | "gray zone"
    ) {
        return true;
    }
    matches!(
        n.as_str(),
        "id" | "sexo"
            | "sex"
            | "edad"
            | "age"
            | "peso"
            | "weight"
            | "fecha"
            | "date"
            | "fecha de registro"
            | "registration date"
            | "id muestra"
            | "nº análisis"
            | "pág."
            | "page"
            | "tfno."
    ) || n.starts_with("fecha")
        || n.starts_with("tfno")
        || n.starts_with("tel")
        || n.starts_with("c/")
        || n.starts_with("calle")
        || n.starts_with("referencia")
        || n.starts_with("validador")
}

fn is_result_word(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "resultado" | "result" | "results" | "resultados" | "valor" | "value"
    )
}

fn is_structural(line: &str) -> bool {
    line.starts_with('#') || line.starts_with('!') || line.starts_with('[') || line.starts_with('|')
}

/// A block heading such as "UREA / SUERO", "T4" or "SDMA (Dimetilarginina…)":
/// its first word is upper-case, unlike method lines ("Química seca…").
fn is_heading(line: &str) -> bool {
    if is_structural(line) || LEADER.is_match(line) || line.ends_with(':') {
        return false;
    }
    let first = line.split_whitespace().next().unwrap_or_default();
    let letters: Vec<char> = first.chars().filter(|c| c.is_alphabetic()).collect();
    first.chars().count() >= 2
        && !letters.is_empty()
        && letters.iter().all(|c| c.is_uppercase())
        && !line
            .chars()
            .any(|c| c.is_ascii_digit() && !first.contains(c))
}

fn heading_before(lines: &[&str], index: usize) -> Option<String> {
    let mut seen = 0;
    for line in lines[..index].iter().rev() {
        if line.is_empty() {
            continue;
        }
        seen += 1;
        if seen > 8 {
            return None;
        }
        if let Some(c) = LEADER.captures(line) {
            if is_result_word(&c[1]) {
                return None; // reached the previous block
            }
            continue; // a sub-result inside this block
        }
        if is_heading(line) {
            return Some(clean_name(line));
        }
    }
    None
}

pub fn parse_report(text: &str) -> ParsedReport {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let next_nonempty = |from: usize| (from..lines.len()).find(|&j| !lines[j].is_empty());
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut consumed = vec![false; lines.len()];
    let legacy_test_name = lines
        .iter()
        .filter_map(|l| COLON_TEST.captures(l))
        .last()
        .map(|c| clean_name(&c[1]));

    for i in 0..lines.len() {
        let line = lines[i];
        if line.is_empty() || consumed[i] {
            continue;
        }
        // 1. Table rows.
        if line.starts_with('|') {
            if let Some(c) = table_row(line) {
                candidates.push(c);
            }
            continue;
        }
        if is_structural(line) {
            continue;
        }
        // 2. Leader lines: "RESULTADO... 63 ↑ mg/dL (30-60)", "ALBUMINA... 30 g/L".
        if let Some(c) = LEADER.captures(line) {
            let printed_name = clean_name(&c[1]);
            if let Some((value, flag, unit, mut range)) = parse_rest(&c[2]) {
                let name = if is_result_word(&printed_name) {
                    heading_before(&lines, i)
                } else {
                    Some(printed_name)
                };
                if range.is_none()
                    && let Some(j) = next_nonempty(i + 1)
                    && RANGE_ONLY.is_match(lines[j])
                {
                    range = Some(clean_range(lines[j]));
                    consumed[j] = true;
                }
                if let Some(name) = name.filter(|n| has_letter(n) && !is_metadata_name(n)) {
                    candidates.push(Candidate {
                        name,
                        value,
                        marker_flag: flag,
                        unit,
                        range,
                    });
                }
            }
            continue;
        }
        // Legacy "Result : 44.72 nmol/L" with "Test : T4".
        if let Some(c) = COLON_RESULT.captures(line) {
            if let Some((value, flag, unit, range)) = parse_rest(&c[1]) {
                let name = legacy_test_name
                    .clone()
                    .or_else(|| heading_before(&lines, i))
                    .unwrap_or_else(|| "Blood test result".into());
                candidates.push(Candidate {
                    name,
                    value,
                    marker_flag: flag,
                    unit,
                    range,
                });
            }
            continue;
        }
        // 4. Value-only line with the name above and the range below.
        if !line
            .split_whitespace()
            .any(|t| has_letter(t) && !looks_like_unit(t))
            && let Some((value, flag, unit, None)) = parse_rest(line)
            && let Some(j) = next_nonempty(i + 1)
            && RANGE_ONLY.is_match(lines[j])
            && let Some(name) = name_lines_above(&lines, &consumed, i, true)
            && !is_metadata_name(&name)
        {
            consumed[j] = true;
            candidates.push(Candidate {
                name,
                value,
                marker_flag: flag,
                unit,
                range: Some(clean_range(lines[j])),
            });
            continue;
        }
        // 3. Plain line: "Hemoglobina 9.3 g/dl 9.0 - 15.3".
        if let Some(c) = plain_line(line) {
            let mut c = c;
            if c.name.starts_with(|ch: char| ch.is_lowercase())
                && let Some(above) = name_lines_above(&lines, &consumed, i, false)
            {
                c.name = format!("{above} {}", c.name);
            }
            if !is_metadata_name(&c.name) {
                candidates.push(c);
            }
        }
    }

    // De-duplicate repeated pages and sub-results printed twice.
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    let mut duplicates_removed = 0;
    for c in candidates {
        let key = (
            crate::analytes::standardize(&c.name, c.unit.as_deref(), None).key,
            c.value.replace(' ', ""),
            c.unit
                .as_deref()
                .map(crate::analytes::normalize_unit)
                .unwrap_or_default(),
        );
        if seen.insert(key) {
            unique.push(c);
        } else {
            duplicates_removed += 1;
        }
    }

    let locale = labvalue::detect_locale(
        unique
            .iter()
            .flat_map(|c| [Some(c.value.as_str()), c.range.as_deref()])
            .flatten(),
    );
    let results = unique
        .into_iter()
        .map(|c| {
            let number = labvalue::parse(&c.value, locale);
            ParsedLabResult {
                test_name: c.name,
                value_numeric: number.value,
                parse_status: number.status.as_str().to_owned(),
                value_text: c.value,
                unit: c.unit,
                reference_range: c.range,
                flag: c.marker_flag,
            }
        })
        .collect();
    ParsedReport {
        results,
        locale,
        duplicates_removed,
        test_date: find_date(text),
    }
}

fn table_row(line: &str) -> Option<Candidate> {
    let cells: Vec<&str> = line
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    if cells.len() < 2
        || cells.iter().any(|c| {
            c.chars()
                .all(|ch| ch == '-' || ch == ':' || ch.is_whitespace())
        })
    {
        return None;
    }
    let name = clean_name(cells[0]);
    if name.len() < 2
        || !has_letter(&name)
        || matches!(name.to_lowercase().as_str(), "test" | "prueba")
        || is_metadata_name(&name)
    {
        return None;
    }
    // The first cell (after the name) that starts with a value; method
    // columns such as "Trinder. Enzim." are skipped.
    let (index, (value, mut flag, mut unit, mut range)) =
        cells.iter().enumerate().skip(1).find_map(|(i, cell)| {
            if RANGE_ONLY.is_match(cell) && cell.contains(['-', '–']) {
                return None;
            }
            parse_rest(cell).map(|parsed| (i, parsed))
        })?;
    for cell in &cells[index + 1..] {
        if unit.is_none() && range.is_none() && looks_like_unit(cell) && !RANGE_ONLY.is_match(cell)
        {
            unit = Some((*cell).to_owned());
        } else if range.is_none() && RANGE_ONLY.is_match(cell) {
            range = Some(clean_range(cell));
        } else if flag.is_none()
            && let Some(f) = marker_flag(cell).or(match cell.to_lowercase().as_str() {
                "alto" | "high" => Some("high"),
                "bajo" | "low" => Some("low"),
                _ => None,
            })
        {
            flag = Some(f.to_owned());
        }
    }
    Some(Candidate {
        name,
        value,
        marker_flag: flag,
        unit,
        range,
    })
}

fn plain_line(line: &str) -> Option<Candidate> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let position = tokens
        .iter()
        .position(|t| is_value_token(t) || marker_flag(t).is_some() && t.chars().count() == 1)?;
    if position == 0 {
        return None;
    }
    let name = clean_name(&tokens[..position].join(" "));
    if name.len() < 2 || !has_letter(&name) || name.ends_with(':') {
        return None;
    }
    let (value, flag, unit, range) = parse_rest(&tokens[position..].join(" "))?;
    // Without a unit or a reference range, "Pág. 1 de 5" and phone numbers
    // would look like results.
    if unit.is_none() && range.is_none() {
        return None;
    }
    Some(Candidate {
        name,
        value,
        marker_flag: flag,
        unit,
        range,
    })
}

/// Text-only lines directly above `index` that form an analyte name.
/// `allow_gaps` lets blank lines sit between them (layout 4).
fn name_lines_above(
    lines: &[&str],
    consumed: &[bool],
    index: usize,
    allow_gaps: bool,
) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut j = index;
    while j > 0 && parts.len() < 3 {
        j -= 1;
        let line = lines[j];
        if line.is_empty() {
            if allow_gaps {
                continue;
            }
            break;
        }
        if consumed[j]
            || is_structural(line)
            || line.chars().any(|c| c.is_ascii_digit())
            || !has_letter(line)
            || line.len() > 40
            || line.ends_with(':')
        {
            break;
        }
        parts.push(line);
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(clean_name(&parts.join(" ")))
}

/// The sample/collection date, preferring labelled dates over the first
/// date in the text. Returns YYYY-MM-DD.
pub fn find_date(text: &str) -> Option<String> {
    static DATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?:(20\d{2})[-/.](\d{1,2})[-/.](\d{1,2})|(\d{1,2})[-/.](\d{1,2})[-/.](20\d{2}))\b",
        )
        .unwrap()
    });
    let date_in = |s: &str| -> Option<String> {
        DATE.captures_iter(s).find_map(|c| {
            let (y, m, d) = if c.get(1).is_some() {
                (&c[1], &c[2], &c[3])
            } else {
                (&c[6], &c[5], &c[4])
            };
            let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
            chrono::NaiveDate::from_ymd_opt(y.parse().ok()?, m, d)
                .map(|date| date.format("%Y-%m-%d").to_string())
        })
    };
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let labels: [&[&str]; 3] = [
        &[
            "toma de muestra",
            "fecha de muestra",
            "fecha muestra",
            "fecha de toma",
            "collection date",
            "sample date",
            "date collected",
            "fecha de extracción",
        ],
        &["fecha de registro", "registration date", "received"],
        &["fecha", "date"],
    ];
    for group in labels {
        for (i, line) in lines.iter().enumerate() {
            let lower = line.to_lowercase();
            if !group.iter().any(|label| lower.contains(label))
                || lower.contains("validaci")
                || lower.contains("edici")
                || lower.contains("nacimiento")
                || lower.contains("birth")
            {
                continue;
            }
            for candidate in lines.iter().skip(i).take(4) {
                if let Some(date) = date_in(candidate) {
                    return Some(date);
                }
            }
        }
    }
    lines.iter().find_map(|l| date_in(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name<'a>(report: &'a ParsedReport, name: &str) -> Vec<&'a ParsedLabResult> {
        report
            .results
            .iter()
            .filter(|r| r.test_name == name)
            .collect()
    }

    /// De-identified excerpt in the layout of a Spanish reference lab:
    /// a table page, then leader-line pages, each page repeated by OCR.
    const LEADER_REPORT: &str = "Fecha toma de muestra\n16/05/2023\n\nFecha edición\n17/05/2023 (1)\n\
HEMOGRAMA\n|  HEMATOCRITO | 35,9 | % | (29,0-52,0)  |\n| --- | --- | --- | --- |\n\
|  HEMATIES | 7.810.000 | /µL | (6.500.000-11.500.000)  |\n|  LEUCOCITOS | 18.270 ↑ | /µL | (3.800-17.200)  |\n\
|  EOSINÓFILOS | 6 | % |   |\n|  EOSINÓFILOS | 1.096,20 | /µL | (100,00-1.200,00)  |\n\
1 de 3\n\nUREA / SUERO\n\nQuímica seca - Espectrofotometría Ultravioleta-Visible\n\nRESULTADO... 63 ↑ mg/dL (30-60)\n\n\
COCIENTE ALBÚMINA / GLOBULINA\n\nQuímica seca - Espectrofotometría Ultravioleta-Visible\n\nALBUMINA... 30 g/L (24-37)\n\n\
GLOBULINAS TOTALES... 38,00 g/L (26,00-47,00)\n\nRESULTADO... 0,8 (0,4-1,2)\n\n\
ALBÚMINA / SUERO\n\nQuímica seca - Espectrofotometría Ultravioleta-Visible\n\nRESULTADO... 30 g/L (24-37)\n\n\
T4\n\nQuimioluminiscencia\n\nRESULTADO... 2,51 µg/dL (1,00-4,00)\n\n\
ALT (GPT) / SUERO\n\nQuímica seca\n\nRESULTADO... 29 U/L\n\n(17-41)\n\n\
SDMA (Dimetilarginina simétrica)\n\nInmunoturbidimetría\n\nRESULTADO... 15,5 ↑ µg/dL (<14,0)\n\n\
Fecha Validación: 17/05/2023\n\n\
UREA / SUERO\n\nQuímica seca - Espectrofotometría Ultravioleta-Visible\n\nRESULTADO... 63 ↑ mg/dL (30-60)\n3 de 3";

    #[test]
    fn reads_every_page_of_a_leader_line_report() {
        let report = parse_report(LEADER_REPORT);
        assert_eq!(report.locale, NumberLocale::DecimalComma);
        assert_eq!(report.test_date.as_deref(), Some("2023-05-16"));
        let names: Vec<_> = report
            .results
            .iter()
            .map(|r| r.test_name.as_str())
            .collect();
        for expected in [
            "HEMATOCRITO",
            "HEMATIES",
            "LEUCOCITOS",
            "UREA / SUERO",
            "COCIENTE ALBÚMINA / GLOBULINA",
            "GLOBULINAS TOTALES",
            "T4",
            "ALT (GPT) / SUERO",
            "SDMA (Dimetilarginina simétrica)",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
        // Albumin appears as a sub-result and as its own block: kept once.
        let albumin = report
            .results
            .iter()
            .filter(|r| r.test_name.to_lowercase().starts_with("alb"))
            .count();
        assert_eq!(albumin, 1, "{names:?}");
        // The repeated UREA page is not a second result.
        assert_eq!(by_name(&report, "UREA / SUERO").len(), 1);
        assert!(report.duplicates_removed >= 2);

        let eos = by_name(&report, "EOSINÓFILOS");
        assert_eq!(eos.len(), 2, "% and /µL are separate results");
        let per_ul = eos
            .iter()
            .find(|r| r.unit.as_deref() == Some("/µL"))
            .unwrap();
        assert_eq!(per_ul.value_numeric, Some(1096.2));
        assert_eq!(per_ul.value_text, "1.096,20");
        assert_eq!(per_ul.reference_range.as_deref(), Some("100,00-1.200,00"));

        let rbc = &by_name(&report, "HEMATIES")[0];
        assert_eq!(rbc.value_numeric, Some(7_810_000.0));
        let wbc = &by_name(&report, "LEUCOCITOS")[0];
        assert_eq!(
            (wbc.value_numeric, wbc.flag.as_deref()),
            (Some(18_270.0), Some("high"))
        );
        let urea = &by_name(&report, "UREA / SUERO")[0];
        assert_eq!(urea.value_numeric, Some(63.0));
        assert_eq!(urea.unit.as_deref(), Some("mg/dL"));
        assert_eq!(urea.reference_range.as_deref(), Some("30-60"));
        assert_eq!(urea.flag.as_deref(), Some("high"));
        let alt = &by_name(&report, "ALT (GPT) / SUERO")[0];
        assert_eq!(
            alt.reference_range.as_deref(),
            Some("17-41"),
            "range on next line"
        );
        let sdma = &by_name(&report, "SDMA (Dimetilarginina simétrica)")[0];
        assert_eq!(sdma.reference_range.as_deref(), Some("<14,0"));
        assert_eq!(sdma.value_numeric, Some(15.5));
        let ratio = &by_name(&report, "COCIENTE ALBÚMINA / GLOBULINA")[0];
        assert_eq!(
            (ratio.value_numeric, ratio.unit.as_deref()),
            (Some(0.8), None)
        );
    }

    #[test]
    fn reads_plain_lines_wrapped_names_and_split_values() {
        let text = "|  Sexo | Macho | ID muestra | 19072  |\n|  Edad | 1 año 11 meses | Fecha de registro | 16/02/2026  |\n\
|  PRUEBA | VALOR | RANGO DE REFERENCIA  |\n| --- | --- | --- |\n|  Leucocitos | 14.68 10^3/ul | 5.50 - 19.50  |\n\
|  Eosinófilos % | 3.35 % | 1.00 - 11.00  |\n|  **Plaquetas grandes** | 12 10^3/ul |  |\n\
C/ Mayor 1 B. 00000 Villaejemplo (Provincia)\n\nTfno. Contacto 600000000\n\nPág. 1 de 5\n\n\
Células grandes\ninmaduras % 0.98 %\n\nHematies 6.77 10^6/ul 4.60 - 12.00\n\nHemoglobina 9.3 g/dl 9.0 - 15.3\n\n\
Volumen\ncorpuscular\nmedio 48.4 fl 39.0 - 53.0\n\nC. hem.\ncorpuscular\nmedia ● 28.4 g/dl 29.0 - 37.0\n\n\
Plaquetas 196 10^3 100 - 518\n\nCoef. disp.\ntamaño plaquetas 16.9 0.0 - 50.0\n\n\
Peso\n\n0,00 Kg\n\nFecha de registro\n\n12/02/2025\n\nPlaquetocrito\n\n0.5 %\n\n0.0 - 2.9\n\n\
Coef. disp.\n\ntamaño hematies\n\n● 13.1 fl\n\n35.2 - 45.3";
        let report = parse_report(text);
        assert_eq!(report.locale, NumberLocale::DecimalDot);
        let names: Vec<_> = report
            .results
            .iter()
            .map(|r| r.test_name.as_str())
            .collect();
        for expected in [
            "Leucocitos",
            "Eosinófilos %",
            "Plaquetas grandes",
            "Células grandes inmaduras %",
            "Hematies",
            "Hemoglobina",
            "Volumen corpuscular medio",
            "C. hem. corpuscular media",
            "Plaquetas",
            "Coef. disp. tamaño plaquetas",
            "Plaquetocrito",
            "Coef. disp. tamaño hematies",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
        for unwanted in ["C/ Mayor", "Tfno. Contacto", "Pág.", "Peso", "Sexo", "Edad"] {
            assert!(
                !names.iter().any(|n| n.starts_with(unwanted)),
                "{unwanted} is not a result: {names:?}"
            );
        }
        let mchc = &by_name(&report, "C. hem. corpuscular media")[0];
        assert_eq!(mchc.flag.as_deref(), Some("abnormal"));
        assert_eq!(mchc.reference_range.as_deref(), Some("29.0 - 37.0"));
        let wbc = &by_name(&report, "Leucocitos")[0];
        assert_eq!(
            (wbc.value_numeric, wbc.unit.as_deref()),
            (Some(14.68), Some("10^3/ul"))
        );
        let plt = &by_name(&report, "Plaquetas")[0];
        assert_eq!(plt.unit.as_deref(), Some("10^3"));
        let pct = &by_name(&report, "Plaquetocrito")[0];
        assert_eq!(pct.reference_range.as_deref(), Some("0.0 - 2.9"));
        assert_eq!(report.test_date.as_deref(), Some("2026-02-16"));
    }

    #[test]
    fn method_column_and_caret_marker() {
        let bands = parse_report(
            "Test : T4\nResult : 44.72 nmol/L\n\nBajo < 15 nmol/l (<1,17 µg/dl)\n\nNormal 15-40 nmol/l\n\nElevado\n\n> 50 nmol/l",
        );
        assert_eq!(
            bands.results.len(),
            1,
            "interpretation bands are not results"
        );
        let text = "Fecha de registro: 20-10-2022\n\
|  Creatinina | Trinder. Enzim. | ^ 1,68 mg/dl | 0,8-1,6  |\n\
|  BUN/Creatinina (UBC) | Calculado | 21,67 | 20-100  |\n|  Potasio | Piruvato Quinasa | 4,90 mmol/l | 3,7-5,8  |";
        let report = parse_report(text);
        assert_eq!(report.test_date.as_deref(), Some("2022-10-20"));
        let crea = &by_name(&report, "Creatinina")[0];
        assert_eq!(crea.value_numeric, Some(1.68));
        assert_eq!(crea.unit.as_deref(), Some("mg/dl"));
        assert_eq!(crea.flag.as_deref(), Some("high"));
        assert_eq!(crea.reference_range.as_deref(), Some("0,8-1,6"));
        let ratio = &by_name(&report, "BUN/Creatinina (UBC)")[0];
        assert_eq!(ratio.value_numeric, Some(21.67));
    }
}

#[cfg(test)]
mod real_reports {
    /// Manual check against real OCR text kept outside the repo:
    /// `PHJ_REPORTS_DIR=/path cargo test real_reports -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_parsed_real_reports() {
        let Ok(dir) = std::env::var("PHJ_REPORTS_DIR") else {
            return;
        };
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            let report = super::parse_report(&text);
            println!(
                "=== {} date={:?} locale={:?} results={} duplicates_removed={}",
                path.file_name().unwrap().to_string_lossy(),
                report.test_date,
                report.locale,
                report.results.len(),
                report.duplicates_removed
            );
            for r in &report.results {
                println!(
                    "  {:<38} {:>14} {:<10} {:<22} {:<8} {:?} [{}]",
                    r.test_name,
                    r.value_text,
                    r.unit.as_deref().unwrap_or(""),
                    r.reference_range.as_deref().unwrap_or(""),
                    r.flag.as_deref().unwrap_or(""),
                    r.value_numeric,
                    r.parse_status
                );
            }
        }
    }
}
