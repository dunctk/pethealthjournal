//! Standard names and units for common veterinary blood-test analytes.
//!
//! Labs print the same analyte differently ("CREA", "Creatinine",
//! "Creatinina") and in different unit systems (mg/dL vs µmol/L). Trend and
//! delta calculations must compare like with like, so each printed result is
//! mapped to a standard key and, where a fixed conversion factor exists, to
//! one canonical unit. Printed values are never altered in storage; this is a
//! read-time view. Unknown analytes or units fall back to the printed name and
//! unit, so nothing is silently merged.

/// A lab value expressed in the standard name and unit when recognised.
#[derive(Clone, Debug, PartialEq)]
pub struct Standardized {
    /// Stable key, e.g. "creatinine". Lowercased printed name if unknown.
    pub key: String,
    /// Human label, e.g. "Creatinine".
    pub label: String,
    /// Canonical unit label (e.g. "µmol/L"), or the printed unit if unknown.
    pub unit: Option<String>,
    /// Value converted to `unit`, when numeric and convertible.
    pub value: Option<f64>,
    /// True when both the analyte and its unit were recognised.
    pub recognized: bool,
}

struct Analyte {
    key: &'static str,
    label: &'static str,
    aliases: &'static [&'static str],
    canonical_unit: &'static str,
    /// (normalised unit, factor to multiply into the canonical unit)
    units: &'static [(&'static str, f64)],
}

const ANALYTES: &[Analyte] = &[
    Analyte {
        key: "creatinine",
        label: "Creatinine",
        aliases: &[
            "creatinine",
            "crea",
            "creat",
            "cre",
            "creatinina",
            "créatinine",
            "kreatinin",
        ],
        canonical_unit: "µmol/L",
        units: &[("umol/l", 1.0), ("mg/dl", 88.42)],
    },
    Analyte {
        key: "sdma",
        label: "SDMA",
        aliases: &["sdma", "idexx sdma", "symmetric dimethylarginine"],
        canonical_unit: "µg/dL",
        units: &[("ug/dl", 1.0)],
    },
    Analyte {
        key: "bun",
        label: "Urea nitrogen (BUN)",
        aliases: &[
            "bun",
            "urea nitrogen",
            "blood urea nitrogen",
            "nitrogeno ureico",
            "nitrógeno ureico",
        ],
        // Expressed as urea mmol/L so it lines up with "Urea" from EU labs.
        canonical_unit: "mmol/L urea",
        units: &[("mg/dl", 0.357), ("mmol/l", 1.0)],
    },
    Analyte {
        key: "urea",
        label: "Urea",
        aliases: &["urea", "ure", "urée", "harnstoff"],
        canonical_unit: "mmol/L urea",
        units: &[("mmol/l", 1.0), ("mg/dl", 0.1665), ("g/l", 16.65)],
    },
    Analyte {
        key: "glucose",
        label: "Glucose",
        aliases: &["glucose", "glu", "gluc", "glucosa", "glukose"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("mg/dl", 0.0555)],
    },
    Analyte {
        key: "phosphorus",
        label: "Phosphorus",
        aliases: &[
            "phosphorus",
            "phos",
            "phosphate",
            "p",
            "fosforo",
            "fósforo",
            "phosphore",
            "phosphat",
        ],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("mg/dl", 0.3229)],
    },
    Analyte {
        key: "calcium",
        label: "Calcium",
        aliases: &["calcium", "ca", "calcio", "total calcium", "tca"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("mg/dl", 0.2495)],
    },
    Analyte {
        key: "potassium",
        label: "Potassium",
        aliases: &["potassium", "k", "k+", "potasio", "kalium"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("meq/l", 1.0)],
    },
    Analyte {
        key: "sodium",
        label: "Sodium",
        aliases: &["sodium", "na", "na+", "sodio", "natrium"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("meq/l", 1.0)],
    },
    Analyte {
        key: "chloride",
        label: "Chloride",
        aliases: &["chloride", "cl", "cl-", "cloro", "cloruro", "chlorid"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("meq/l", 1.0)],
    },
    Analyte {
        key: "alt",
        label: "ALT",
        aliases: &[
            "alt",
            "alt (gpt)",
            "alt/gpt",
            "gpt/alt",
            "gpt",
            "sgpt",
            "alat",
            "alanine aminotransferase",
            "alanina aminotransferasa",
        ],
        canonical_unit: "U/L",
        units: &[("u/l", 1.0), ("iu/l", 1.0)],
    },
    Analyte {
        key: "ast",
        label: "AST",
        aliases: &[
            "ast",
            "ast (got)",
            "ast/got",
            "got/ast",
            "got",
            "sgot",
            "asat",
            "aspartate aminotransferase",
        ],
        canonical_unit: "U/L",
        units: &[("u/l", 1.0), ("iu/l", 1.0)],
    },
    Analyte {
        key: "alp",
        label: "ALP",
        aliases: &[
            "alp",
            "alkp",
            "alk phos",
            "alkaline phosphatase",
            "fa",
            "fosfatasa alcalina",
            "pal",
        ],
        canonical_unit: "U/L",
        units: &[("u/l", 1.0), ("iu/l", 1.0)],
    },
    Analyte {
        key: "ggt",
        label: "GGT",
        aliases: &[
            "ggt",
            "gamma gt",
            "gamma-gt",
            "γgt",
            "gamma glutamyl transferase",
        ],
        canonical_unit: "U/L",
        units: &[("u/l", 1.0), ("iu/l", 1.0)],
    },
    Analyte {
        key: "total_protein",
        label: "Total protein",
        aliases: &[
            "total protein",
            "tp",
            "prot",
            "protein",
            "proteinas totales",
            "proteínas totales",
            "protéines totales",
        ],
        canonical_unit: "g/L",
        units: &[("g/l", 1.0), ("g/dl", 10.0)],
    },
    Analyte {
        key: "albumin",
        label: "Albumin",
        aliases: &["albumin", "alb", "albumina", "albúmina", "albumine"],
        canonical_unit: "g/L",
        units: &[("g/l", 1.0), ("g/dl", 10.0)],
    },
    Analyte {
        key: "globulin",
        label: "Globulin",
        aliases: &["globulin", "glob", "globulina", "globulines", "globulins"],
        canonical_unit: "g/L",
        units: &[("g/l", 1.0), ("g/dl", 10.0)],
    },
    Analyte {
        key: "total_bilirubin",
        label: "Total bilirubin",
        aliases: &[
            "total bilirubin",
            "tbil",
            "tbili",
            "bilirubin",
            "bilirrubina total",
            "bilirrubina",
        ],
        canonical_unit: "µmol/L",
        units: &[("umol/l", 1.0), ("mg/dl", 17.1)],
    },
    Analyte {
        key: "cholesterol",
        label: "Cholesterol",
        aliases: &["cholesterol", "chol", "colesterol", "cholestérol"],
        canonical_unit: "mmol/L",
        units: &[("mmol/l", 1.0), ("mg/dl", 0.02586)],
    },
    Analyte {
        key: "total_t4",
        label: "Total T4",
        aliases: &["total t4", "tt4", "t4", "t4 total", "thyroxine"],
        canonical_unit: "nmol/L",
        units: &[("nmol/l", 1.0), ("ug/dl", 12.87)],
    },
    Analyte {
        key: "hematocrit",
        label: "Hematocrit",
        aliases: &[
            "hematocrit",
            "hct",
            "pcv",
            "haematocrit",
            "hematocrito",
            "hématocrite",
        ],
        canonical_unit: "%",
        units: &[("%", 1.0), ("l/l", 100.0)],
    },
    Analyte {
        key: "hemoglobin",
        label: "Hemoglobin",
        aliases: &[
            "hemoglobin",
            "hgb",
            "hb",
            "haemoglobin",
            "hemoglobina",
            "hémoglobine",
        ],
        canonical_unit: "g/L",
        units: &[("g/l", 1.0), ("g/dl", 10.0)],
    },
    Analyte {
        key: "wbc",
        label: "White blood cells",
        aliases: &[
            "wbc",
            "white blood cells",
            "leukocytes",
            "leucocitos",
            "leucocytes",
        ],
        canonical_unit: "×10⁹/L",
        units: &[("10^9/l", 1.0), ("10^3/ul", 1.0)],
    },
    Analyte {
        key: "rbc",
        label: "Red blood cells",
        aliases: &[
            "rbc",
            "red blood cells",
            "erythrocytes",
            "eritrocitos",
            "hematies",
            "hematíes",
            "érythrocytes",
        ],
        canonical_unit: "×10¹²/L",
        units: &[("10^12/l", 1.0), ("10^6/ul", 1.0)],
    },
    Analyte {
        key: "platelets",
        label: "Platelets",
        aliases: &[
            "platelets",
            "plt",
            "plaquetas",
            "plaquettes",
            "thrombocytes",
        ],
        canonical_unit: "×10⁹/L",
        units: &[("10^9/l", 1.0), ("10^3/ul", 1.0)],
    },
    Analyte {
        key: "usg",
        label: "Urine specific gravity",
        aliases: &[
            "usg",
            "urine specific gravity",
            "specific gravity",
            "densidad urinaria",
            "densidad",
        ],
        canonical_unit: "",
        units: &[("", 1.0)],
    },
];

fn normalize_name(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    // Drop trailing qualifiers such as "(serum)" or ", total" isn't safe in
    // general; only collapse whitespace and strip surrounding punctuation.
    lower
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == ':' || c == '*' || c == '.')
        .to_owned()
}

/// Normalises unit spellings: "µmol/L", "umol/l", "μmol/L" → "umol/l";
/// "x10^9/L", "10*9/L", "10e9/l", "G/L" (cells) → "10^9/l"; "K/µL" → "10^3/ul".
pub fn normalize_unit(unit: &str) -> String {
    let mut u = unit.trim().to_lowercase().replace(['µ', 'μ'], "u");
    u.retain(|c| !c.is_whitespace());
    let u = u
        .trim_start_matches(['x', '×'])
        .replace("10*", "10^")
        .replace("10e", "10^")
        .replace("³", "^3")
        .replace("⁹", "^9")
        .replace("¹²", "^12")
        .replace("^^", "^")
        .replace("mcg", "ug")
        .replace("/mm3", "/ul")
        .replace("/mm^3", "/ul");
    match u.as_str() {
        // Bare "10^3"/"10^6" on cell counts means per µL.
        "k/ul" | "thou/ul" | "10^3/ul" | "10^3/mm^3" | "10^3" => "10^3/ul".into(),
        "m/ul" | "mil/ul" | "10^6/ul" | "10^6" => "10^6/ul".into(),
        "g/l(cells)" => "10^9/l".into(),
        "iu/l" | "ui/l" => "iu/l".into(),
        "mg/100ml" => "mg/dl".into(),
        "g/100ml" => "g/dl".into(),
        other => other.to_owned(),
    }
}

/// Map a printed result to its standard key/unit. `value` is the numeric
/// value as printed (already parsed), if any.
pub fn standardize(test_name: &str, unit: Option<&str>, value: Option<f64>) -> Standardized {
    let name = normalize_name(test_name);
    let unit_norm = unit.map(normalize_unit).unwrap_or_default();
    if let Some(analyte) = ANALYTES
        .iter()
        .find(|a| a.aliases.iter().any(|alias| *alias == name))
    {
        if let Some((_, factor)) = analyte.units.iter().find(|(u, _)| *u == unit_norm) {
            return Standardized {
                key: analyte.key.to_owned(),
                label: analyte.label.to_owned(),
                unit: (!analyte.canonical_unit.is_empty())
                    .then(|| analyte.canonical_unit.to_owned()),
                value: value.map(|v| round_sig(v * factor)),
                recognized: true,
            };
        }
        // Known analyte, unknown unit: keep it separate by unit so values in
        // different unit systems are never compared.
        return Standardized {
            key: format!("{}|{}", analyte.key, unit_norm),
            label: analyte.label.to_owned(),
            unit: unit.map(str::to_owned),
            value,
            recognized: false,
        };
    }
    Standardized {
        key: format!("{name}|{unit_norm}"),
        label: test_name.trim().to_owned(),
        unit: unit.map(str::to_owned),
        value,
        recognized: false,
    }
}

/// Round converted values to 4 significant figures to avoid float noise
/// (e.g. 1.8 mg/dL → 159.2 µmol/L, not 159.156).
fn round_sig(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let digits = 4 - value.abs().log10().floor() as i32 - 1;
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creatinine_aliases_and_units_converge() {
        let us = standardize("CREA", Some("mg/dL"), Some(1.8));
        let eu = standardize("Creatinina", Some("µmol/L"), Some(159.2));
        assert_eq!(us.key, "creatinine");
        assert_eq!(eu.key, "creatinine");
        assert_eq!(us.unit.as_deref(), Some("µmol/L"));
        assert_eq!(us.value, Some(159.2));
        assert!(us.recognized && eu.recognized);
    }

    #[test]
    fn bun_and_urea_share_a_canonical_unit() {
        let bun = standardize("BUN", Some("mg/dL"), Some(28.0));
        let urea = standardize("Urea", Some("mmol/L"), Some(10.0));
        assert_eq!(bun.unit, urea.unit);
        assert_eq!(bun.value, Some(9.996));
        // Different keys: BUN and urea are reported as separate analytes,
        // but values are comparable if a caller chooses to.
        assert_ne!(bun.key, urea.key);
    }

    #[test]
    fn unit_spellings_normalise() {
        assert_eq!(normalize_unit("x10^9/L"), "10^9/l");
        assert_eq!(normalize_unit("10*9/L"), "10^9/l");
        assert_eq!(normalize_unit("K/µL"), "10^3/ul");
        assert_eq!(normalize_unit("μmol/L"), "umol/l");
        assert_eq!(normalize_unit("mcg/dL"), "ug/dl");
        assert_eq!(
            standardize("Plaquetas", Some("10^3"), Some(494.0)).key,
            "platelets"
        );
        assert_eq!(standardize("ALT/GPT", Some("UI/l"), Some(38.0)).key, "alt");
        let wbc = standardize("WBC", Some("K/uL"), Some(8.2));
        assert_eq!(wbc.key, "wbc");
        assert_eq!(wbc.value, Some(8.2));
    }

    #[test]
    fn unknown_unit_or_analyte_is_never_merged() {
        let odd = standardize("Creatinine", Some("mmol/L"), Some(0.15));
        assert!(!odd.recognized);
        assert_ne!(odd.key, "creatinine");
        let unknown = standardize("Fructosamine", Some("umol/L"), Some(300.0));
        assert_eq!(unknown.key, "fructosamine|umol/l");
        assert_eq!(unknown.value, Some(300.0));
    }
}
