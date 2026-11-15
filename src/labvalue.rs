//! Reading printed lab numbers without guessing.
//!
//! Labs print numbers in two conventions: `1.096,20` (Spanish and most of
//! Europe: dot groups thousands, comma marks decimals) and `1,096.20`
//! (English). Reading one as the other turns 1,096.20 into 1.096 — which
//! produced a "+70,612%" eosinophil trend. So the convention is detected
//! from the whole report first, and any number that could mean two things
//! under the detected convention is marked ambiguous and gets no numeric
//! value, which keeps it out of every trend.
//!
//! The printed text is never changed; this module only produces the parsed
//! number beside it.

use serde::Serialize;

/// How a report writes decimals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberLocale {
    /// `1.234,5` — comma is the decimal separator.
    DecimalComma,
    /// `1,234.5` — dot is the decimal separator.
    DecimalDot,
    /// Not enough evidence either way.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    /// One reading only; `value` is set.
    Validated,
    /// Could be two different numbers (e.g. `1.500` with no locale); `value` is None.
    Ambiguous,
    /// Looked numeric but is malformed (e.g. `1.23.4`); `value` is None.
    Failed,
    /// A qualitative result such as "Positive" or "Negativo"; `value` is None.
    Text,
    /// Printed as a bound such as `<5` or `>500`: the true value is unknown,
    /// so `value` is None and it never enters a trend. `comparator` and
    /// `bound` keep what was printed.
    Censored,
}

impl ParseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validated => "validated",
            Self::Ambiguous => "ambiguous",
            Self::Failed => "failed",
            Self::Text => "text",
            Self::Censored => "censored",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LabNumber {
    pub value: Option<f64>,
    pub status: ParseStatus,
    /// `<` or `>` when printed (e.g. `<0,10`).
    pub comparator: Option<char>,
    /// The printed bound for a censored value.
    pub bound: Option<f64>,
    /// `high`/`low` when the value carries an arrow (`63 ↑`).
    pub arrow_flag: Option<&'static str>,
}

/// Guess the convention from every number-like token in a report.
/// Only unambiguous tokens count as evidence.
pub fn detect_locale<'a>(texts: impl IntoIterator<Item = &'a str>) -> NumberLocale {
    let (mut comma, mut dot) = (0usize, 0usize);
    for text in texts {
        for token in number_tokens(text) {
            match evidence(token) {
                Some(NumberLocale::DecimalComma) => comma += 1,
                Some(NumberLocale::DecimalDot) => dot += 1,
                _ => {}
            }
        }
    }
    match comma.cmp(&dot) {
        std::cmp::Ordering::Greater => NumberLocale::DecimalComma,
        std::cmp::Ordering::Less => NumberLocale::DecimalDot,
        std::cmp::Ordering::Equal => NumberLocale::Unknown,
    }
}

/// Runs of digits joined by `.` or `,`, e.g. `1.096,20`, `35,9`, `7.810.000`.
fn number_tokens(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || ((bytes[i] == b'.' || bytes[i] == b',')
                        && i + 1 < bytes.len()
                        && bytes[i + 1].is_ascii_digit()))
            {
                i += 1;
            }
            // Skip dates like 16/05/2023 and times 11:40:42 entirely.
            let joined_to_date = (start > 0 && matches!(bytes[start - 1], b'/' | b':' | b'-'))
                || (i < bytes.len() && matches!(bytes[i], b'/' | b':'));
            if !joined_to_date {
                tokens.push(&text[start..i]);
            }
        } else {
            i += 1;
        }
    }
    tokens.into_iter()
}

fn evidence(token: &str) -> Option<NumberLocale> {
    let commas = token.matches(',').count();
    let dots = token.matches('.').count();
    match (commas, dots) {
        (0, 0) => None,
        (_, _) if commas > 0 && dots > 0 => {
            if token.rfind(',') > token.rfind('.') {
                Some(NumberLocale::DecimalComma)
            } else {
                Some(NumberLocale::DecimalDot)
            }
        }
        (c, 0) if c > 1 => Some(NumberLocale::DecimalDot),
        (0, d) if d > 1 => Some(NumberLocale::DecimalComma),
        (1, 0) => (digits_after(token, ',') != 3 || starts_with_zero_unit(token))
            .then_some(NumberLocale::DecimalComma),
        (0, 1) => (digits_after(token, '.') != 3 || starts_with_zero_unit(token))
            .then_some(NumberLocale::DecimalDot),
        _ => None,
    }
}

fn digits_after(token: &str, separator: char) -> usize {
    token
        .rsplit(separator)
        .next()
        .map(|tail| tail.chars().take_while(char::is_ascii_digit).count())
        .unwrap_or(0)
}

/// `0,125` / `0.125` can only be a decimal: thousands never start with 0.
fn starts_with_zero_unit(token: &str) -> bool {
    token.starts_with("0,") || token.starts_with("0.")
}

/// Parse one printed value (e.g. `18.270 ↑`, `<14,0`, `1.096,20`, `Negativo`).
pub fn parse(printed: &str, locale: NumberLocale) -> LabNumber {
    let mut text = printed.trim().to_owned();
    let mut arrow_flag = None;
    for (arrow, flag) in [('↑', "high"), ('↓', "low"), ('▲', "high"), ('▼', "low")] {
        if text.contains(arrow) {
            arrow_flag = Some(flag);
            text = text.replace(arrow, "");
        }
    }
    let text = text.trim();
    let (comparator, body) = match text.chars().next() {
        Some(c @ ('<' | '>')) => (Some(c), text[1..].trim_start_matches('=').trim()),
        Some('≤') => (Some('<'), text['≤'.len_utf8()..].trim()),
        Some('≥') => (Some('>'), text['≥'.len_utf8()..].trim()),
        _ => (None, text),
    };
    let (sign, digits) = match body.strip_prefix('-') {
        Some(rest) => (-1.0, rest.trim()),
        None => (1.0, body.strip_prefix('+').unwrap_or(body).trim()),
    };
    let result = |value: Option<f64>, status| match (comparator, status) {
        (Some(_), ParseStatus::Validated) => LabNumber {
            value: None,
            status: ParseStatus::Censored,
            comparator,
            bound: value.map(|v| v * sign),
            arrow_flag,
        },
        _ => LabNumber {
            value: value.map(|v| v * sign),
            status,
            comparator,
            bound: None,
            arrow_flag,
        },
    };
    if digits.is_empty() || !digits.chars().any(|c| c.is_ascii_digit()) {
        return result(None, ParseStatus::Text);
    }
    if !digits
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        || !digits.starts_with(|c: char| c.is_ascii_digit())
        || !digits.ends_with(|c: char| c.is_ascii_digit())
    {
        // Mixed text such as "1+ (trazas)" or "3-5/campo".
        return result(None, ParseStatus::Text);
    }
    match read_number(digits, locale) {
        Ok(value) => result(Some(value), ParseStatus::Validated),
        Err(status) => result(None, status),
    }
}

fn read_number(digits: &str, locale: NumberLocale) -> Result<f64, ParseStatus> {
    let commas = digits.matches(',').count();
    let dots = digits.matches('.').count();
    let (decimal, thousands) = match (commas, dots) {
        (0, 0) => return digits.parse().map_err(|_| ParseStatus::Failed),
        _ if commas > 0 && dots > 0 => {
            if digits.rfind(',') > digits.rfind('.') {
                (Some(','), Some('.'))
            } else {
                (Some('.'), Some(','))
            }
        }
        (c, 0) if c > 1 => (None, Some(',')),
        (0, d) if d > 1 => (None, Some('.')),
        (1, 0) => single_separator(digits, ',', locale)?,
        (0, 1) => single_separator(digits, '.', locale)?,
        _ => return Err(ParseStatus::Failed),
    };
    // A decimal separator may appear once, after every thousands separator.
    if let Some(d) = decimal
        && digits.matches(d).count() != 1
    {
        return Err(ParseStatus::Failed);
    }
    let (integer, fraction) = match decimal {
        Some(d) => digits.split_once(d).unwrap(),
        None => (digits, ""),
    };
    if let Some(t) = thousands
        && integer.contains(t)
    {
        let mut groups = integer.split(t);
        let first = groups.next().unwrap_or_default();
        if first.is_empty() || first.len() > 3 || groups.any(|g| g.len() != 3) {
            return Err(ParseStatus::Failed);
        }
    }
    if fraction.contains(['.', ',']) {
        return Err(ParseStatus::Failed);
    }
    let integer: String = integer.chars().filter(char::is_ascii_digit).collect();
    let normalised = if fraction.is_empty() {
        integer
    } else {
        format!("{integer}.{fraction}")
    };
    normalised.parse().map_err(|_| ParseStatus::Failed)
}

/// One `,` or one `.`: a decimal unless exactly three digits follow, in
/// which case only the report's locale can decide.
fn single_separator(
    digits: &str,
    separator: char,
    locale: NumberLocale,
) -> Result<(Option<char>, Option<char>), ParseStatus> {
    let decimal_here = (Some(separator), None);
    let thousands_here = (None, Some(separator));
    if digits_after(digits, separator) != 3 || starts_with_zero_unit(digits) {
        return Ok(decimal_here);
    }
    let separator_is_decimal = match (separator, locale) {
        (',', NumberLocale::DecimalComma) | ('.', NumberLocale::DecimalDot) => true,
        (',', NumberLocale::DecimalDot) | ('.', NumberLocale::DecimalComma) => false,
        _ => return Err(ParseStatus::Ambiguous),
    };
    Ok(if separator_is_decimal {
        decimal_here
    } else {
        thousands_here
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use NumberLocale::*;

    fn value(printed: &str, locale: NumberLocale) -> Option<f64> {
        parse(printed, locale).value
    }

    #[test]
    fn spanish_report_values() {
        assert_eq!(value("1.096,20", DecimalComma), Some(1096.2));
        assert_eq!(value("775,00", DecimalComma), Some(775.0));
        assert_eq!(value("35,9", DecimalComma), Some(35.9));
        assert_eq!(value("7.810.000", DecimalComma), Some(7_810_000.0));
        assert_eq!(value("362.500", DecimalComma), Some(362_500.0));
        assert_eq!(value("12.606,30 ↑", DecimalComma), Some(12_606.3));
        assert_eq!(value("1,530", DecimalComma), Some(1.53));
    }

    #[test]
    fn english_report_values() {
        assert_eq!(value("1,096.20", DecimalDot), Some(1096.2));
        assert_eq!(value("18.27", DecimalDot), Some(18.27));
        assert_eq!(value("7,810,000", DecimalDot), Some(7_810_000.0));
        assert_eq!(value("1,530", DecimalDot), Some(1530.0));
        assert_eq!(value("1.530", DecimalDot), Some(1.53));
    }

    #[test]
    fn the_eosinophil_regression() {
        // These two values once produced a "+70,612%" change.
        let older = value("1.096,20", DecimalComma).unwrap();
        let newer = value("775,00", DecimalComma).unwrap();
        let change = (newer - older) / older * 100.0;
        assert!((change - -29.3).abs() < 0.05, "{change}");
    }

    #[test]
    fn ambiguous_numbers_get_no_value() {
        let n = parse("18.270", Unknown);
        assert_eq!(n.status, ParseStatus::Ambiguous);
        assert_eq!(n.value, None);
        assert_eq!(parse("1,500", Unknown).status, ParseStatus::Ambiguous);
        // Never ambiguous whatever the locale.
        assert_eq!(value("0.125", Unknown), Some(0.125));
        assert_eq!(value("45,97", Unknown), Some(45.97));
        assert_eq!(value("1.096,20", Unknown), Some(1096.2));
    }

    #[test]
    fn arrows_comparators_text_and_malformed() {
        let n = parse("63 ↑", DecimalComma);
        assert_eq!((n.value, n.arrow_flag), (Some(63.0), Some("high")));
        let n = parse("3,09 ↓", DecimalComma);
        assert_eq!((n.value, n.arrow_flag), (Some(3.09), Some("low")));
        let n = parse("<14,0", DecimalComma);
        assert_eq!(
            (n.value, n.bound, n.comparator),
            (None, Some(14.0), Some('<'))
        );
        assert_eq!(n.status, ParseStatus::Censored);
        assert_eq!(parse("Negativo", DecimalComma).status, ParseStatus::Text);
        assert_eq!(parse("1+ (trazas)", DecimalComma).status, ParseStatus::Text);
        assert_eq!(parse("1.23.4", DecimalComma).status, ParseStatus::Failed);
        assert_eq!(value("-2,5", DecimalComma), Some(-2.5));
    }

    #[test]
    fn locale_detection_from_report_text() {
        let spanish = "| HEMATOCRITO | 35,9 | % | (29,0-52,0) |\n| HEMATIES | 7.810.000 | /µL |\nFecha 16/05/2023";
        assert_eq!(detect_locale([spanish]), DecimalComma);
        let english = "HCT 44.7 % (30.3-52.3)\nWBC 11.20 K/uL\nRBC 8,420,000 /uL";
        assert_eq!(detect_locale([english]), DecimalDot);
        assert_eq!(detect_locale(["SDMA 15", "Date 2024-05-14"]), Unknown);
    }
}
