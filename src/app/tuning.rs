use clap::ValueEnum;
use regex::Regex;
use toml::Value;

use super::cli::{Level, Timezone};

pub(super) const MIN_SIMILARITY: f64 = 0.50;
pub(super) const MAX_SIMILARITY: f64 = 1.00;
pub(super) const DEFAULT_SIMILARITY: f64 = 0.60;

pub(super) const MIN_BUCKET_CAP: u32 = 1;
pub(super) const MAX_BUCKET_CAP: u32 = 1_000;
pub(super) const DEFAULT_BUCKET_CAP: u32 = 100;

pub(super) const MIN_GROUPS: u32 = 1;
pub(super) const MAX_GROUPS: u32 = 1_000_000;
pub(super) const DEFAULT_GROUPS: u32 = 100_000;

pub(super) const MIN_EVENT_BYTES: u64 = 1;
pub(super) const MAX_EVENT_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const DEFAULT_EVENT_BYTES: u64 = 256 * 1024;

pub(super) const MIN_EVENT_LINES: u32 = 1;
pub(super) const MAX_EVENT_LINES: u32 = 100_000;
pub(super) const DEFAULT_EVENT_LINES: u32 = 2_000;

pub(super) const MIN_SAMPLE_BYTES: u64 = 1;
pub(super) const MAX_SAMPLE_BYTES: u64 = 1024 * 1024;
pub(super) const DEFAULT_SAMPLE_BYTES: u64 = 32 * 1024;

pub(super) const MIN_SAMPLE_BUDGET: u64 = 1;
pub(super) const MAX_SAMPLE_BUDGET: u64 = 1024 * 1024 * 1024;
pub(super) const DEFAULT_SAMPLE_BUDGET: u64 = 64 * 1024 * 1024;

pub(super) const MIN_SECS: u32 = 1;
pub(super) const MAX_SECS: u32 = 86_400;
pub(super) const DEFAULT_FAST_HALF_LIFE_SECS: u32 = 10;
pub(super) const DEFAULT_BASELINE_HALF_LIFE_SECS: u32 = 300;
pub(super) const DEFAULT_NEW_AGE_SECS: u32 = 60;
pub(super) const DEFAULT_INCREASING_MIN_AGE_SECS: u32 = 60;

pub(super) const MIN_RATIO: f64 = 1.00;
pub(super) const MAX_RATIO: f64 = 100.00;
pub(super) const DEFAULT_INCREASING_RATIO: f64 = 2.00;

pub(super) const MIN_RATE: f64 = 0.00;
pub(super) const MAX_RATE: f64 = 1_000_000.00;
pub(super) const DEFAULT_INCREASING_MIN_RATE: f64 = 5.00;

pub(super) const MAX_EXTRA_PATTERNS: usize = 32;
pub(super) const MAX_PATTERN_BYTES: usize = 1_024;

const ROOT_KEYS: &[&str] = &[
    "version",
    "levels",
    "timezone",
    "templates",
    "groups",
    "event",
    "sample",
    "rates",
    "redaction",
];

/// Validated version-1 analyzer tuning. Source identity, AIO, mute, and
/// force-merge/split rules are not representable.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(super) struct Tuning {
    pub levels: Vec<Level>,
    pub timezone: Timezone,
    pub similarity: f64,
    pub bucket_cap: u32,
    pub max_groups: u32,
    pub event_max_bytes: u64,
    pub event_max_lines: u32,
    pub sample_max_bytes: u64,
    pub sample_budget_bytes: u64,
    pub fast_half_life_secs: u32,
    pub baseline_half_life_secs: u32,
    pub new_age_secs: u32,
    pub increasing_min_age_secs: u32,
    pub increasing_ratio: f64,
    pub increasing_min_rate: f64,
    pub extra_patterns: Vec<Regex>,
}

impl PartialEq for Tuning {
    fn eq(&self, other: &Self) -> bool {
        self.levels == other.levels
            && self.timezone == other.timezone
            && self.similarity == other.similarity
            && self.bucket_cap == other.bucket_cap
            && self.max_groups == other.max_groups
            && self.event_max_bytes == other.event_max_bytes
            && self.event_max_lines == other.event_max_lines
            && self.sample_max_bytes == other.sample_max_bytes
            && self.sample_budget_bytes == other.sample_budget_bytes
            && self.fast_half_life_secs == other.fast_half_life_secs
            && self.baseline_half_life_secs == other.baseline_half_life_secs
            && self.new_age_secs == other.new_age_secs
            && self.increasing_min_age_secs == other.increasing_min_age_secs
            && self.increasing_ratio == other.increasing_ratio
            && self.increasing_min_rate == other.increasing_min_rate
            && self.extra_patterns.len() == other.extra_patterns.len()
            && self
                .extra_patterns
                .iter()
                .zip(&other.extra_patterns)
                .all(|(a, b)| a.as_str() == b.as_str())
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            levels: vec![Level::Error],
            timezone: Timezone::Utc,
            similarity: DEFAULT_SIMILARITY,
            bucket_cap: DEFAULT_BUCKET_CAP,
            max_groups: DEFAULT_GROUPS,
            event_max_bytes: DEFAULT_EVENT_BYTES,
            event_max_lines: DEFAULT_EVENT_LINES,
            sample_max_bytes: DEFAULT_SAMPLE_BYTES,
            sample_budget_bytes: DEFAULT_SAMPLE_BUDGET,
            fast_half_life_secs: DEFAULT_FAST_HALF_LIFE_SECS,
            baseline_half_life_secs: DEFAULT_BASELINE_HALF_LIFE_SECS,
            new_age_secs: DEFAULT_NEW_AGE_SECS,
            increasing_min_age_secs: DEFAULT_INCREASING_MIN_AGE_SECS,
            increasing_ratio: DEFAULT_INCREASING_RATIO,
            increasing_min_rate: DEFAULT_INCREASING_MIN_RATE,
            extra_patterns: Vec::new(),
        }
    }
}

pub(super) fn from_table(table: &toml::Table) -> Result<Tuning, String> {
    require_version(table)?;
    reject_unknown(table, "", ROOT_KEYS)?;

    let templates = optional_table(table, "templates")?;
    if let Some(table) = templates {
        reject_unknown(table, "templates", &["similarity", "bucket_cap"])?;
    }
    let groups = optional_table(table, "groups")?;
    if let Some(table) = groups {
        reject_unknown(table, "groups", &["max"])?;
    }
    let event = optional_table(table, "event")?;
    if let Some(table) = event {
        reject_unknown(table, "event", &["max_bytes", "max_lines"])?;
    }
    let sample = optional_table(table, "sample")?;
    if let Some(table) = sample {
        reject_unknown(table, "sample", &["max_bytes", "budget_bytes"])?;
    }
    let rates = optional_table(table, "rates")?;
    if let Some(table) = rates {
        reject_unknown(
            table,
            "rates",
            &[
                "fast_half_life_secs",
                "baseline_half_life_secs",
                "new_age_secs",
                "increasing_min_age_secs",
                "increasing_ratio",
                "increasing_min_rate",
            ],
        )?;
    }
    let redaction = optional_table(table, "redaction")?;
    if let Some(table) = redaction {
        reject_unknown(table, "redaction", &["extra_patterns"])?;
    }

    let tuning = Tuning {
        levels: parse_levels(table)?,
        timezone: parse_timezone(table)?,
        similarity: optional_f64(
            templates,
            "templates.similarity",
            MIN_SIMILARITY,
            MAX_SIMILARITY,
        )?
        .unwrap_or(DEFAULT_SIMILARITY),
        bucket_cap: optional_u32(
            templates,
            "templates.bucket_cap",
            MIN_BUCKET_CAP,
            MAX_BUCKET_CAP,
        )?
        .unwrap_or(DEFAULT_BUCKET_CAP),
        max_groups: optional_u32(groups, "groups.max", MIN_GROUPS, MAX_GROUPS)?
            .unwrap_or(DEFAULT_GROUPS),
        event_max_bytes: optional_u64(event, "event.max_bytes", MIN_EVENT_BYTES, MAX_EVENT_BYTES)?
            .unwrap_or(DEFAULT_EVENT_BYTES),
        event_max_lines: optional_u32(event, "event.max_lines", MIN_EVENT_LINES, MAX_EVENT_LINES)?
            .unwrap_or(DEFAULT_EVENT_LINES),
        sample_max_bytes: optional_u64(
            sample,
            "sample.max_bytes",
            MIN_SAMPLE_BYTES,
            MAX_SAMPLE_BYTES,
        )?
        .unwrap_or(DEFAULT_SAMPLE_BYTES),
        sample_budget_bytes: optional_u64(
            sample,
            "sample.budget_bytes",
            MIN_SAMPLE_BUDGET,
            MAX_SAMPLE_BUDGET,
        )?
        .unwrap_or(DEFAULT_SAMPLE_BUDGET),
        fast_half_life_secs: optional_u32(rates, "rates.fast_half_life_secs", MIN_SECS, MAX_SECS)?
            .unwrap_or(DEFAULT_FAST_HALF_LIFE_SECS),
        baseline_half_life_secs: optional_u32(
            rates,
            "rates.baseline_half_life_secs",
            MIN_SECS,
            MAX_SECS,
        )?
        .unwrap_or(DEFAULT_BASELINE_HALF_LIFE_SECS),
        new_age_secs: optional_u32(rates, "rates.new_age_secs", MIN_SECS, MAX_SECS)?
            .unwrap_or(DEFAULT_NEW_AGE_SECS),
        increasing_min_age_secs: optional_u32(
            rates,
            "rates.increasing_min_age_secs",
            MIN_SECS,
            MAX_SECS,
        )?
        .unwrap_or(DEFAULT_INCREASING_MIN_AGE_SECS),
        increasing_ratio: optional_f64(rates, "rates.increasing_ratio", MIN_RATIO, MAX_RATIO)?
            .unwrap_or(DEFAULT_INCREASING_RATIO),
        increasing_min_rate: optional_f64(rates, "rates.increasing_min_rate", MIN_RATE, MAX_RATE)?
            .unwrap_or(DEFAULT_INCREASING_MIN_RATE),
        extra_patterns: parse_extra_patterns(redaction)?,
    };
    cross_field(&tuning)?;
    Ok(tuning)
}

#[cfg(test)]
pub(super) fn from_toml_str(text: &str) -> Result<Tuning, String> {
    let value: Value = toml::from_str(text).map_err(|err| err.to_string())?;
    match value {
        Value::Table(table) => from_table(&table),
        _ => Err("root must be a table".into()),
    }
}

fn require_version(table: &toml::Table) -> Result<(), String> {
    match table.get("version") {
        None => Err("version is required; expected version = 1".into()),
        Some(Value::Integer(1)) => Ok(()),
        Some(Value::Integer(version)) => Err(format!(
            "unsupported config version {version}; expected version = 1"
        )),
        Some(_) => Err("version must be an integer; expected version = 1".into()),
    }
}

fn cross_field(tuning: &Tuning) -> Result<(), String> {
    if tuning.levels.is_empty() {
        return Err("levels must not be empty".into());
    }
    if tuning.baseline_half_life_secs <= tuning.fast_half_life_secs {
        return Err(
            "rates.baseline_half_life_secs must be greater than rates.fast_half_life_secs".into(),
        );
    }
    if tuning.sample_max_bytes > tuning.sample_budget_bytes {
        return Err("sample.max_bytes must not exceed sample.budget_bytes".into());
    }
    Ok(())
}

fn parse_levels(table: &toml::Table) -> Result<Vec<Level>, String> {
    match table.get("levels") {
        None => Ok(vec![Level::Error]),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err("levels must not be empty".into());
            }
            let mut levels = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let raw = item
                    .as_str()
                    .ok_or_else(|| format!("levels[{index}] must be a string"))?;
                let level = Level::from_str(raw, true)
                    .map_err(|_| format!("levels[{index}] is not a valid level '{raw}'"))?;
                if !levels.contains(&level) {
                    levels.push(level);
                }
            }
            if levels.is_empty() {
                return Err("levels must not be empty".into());
            }
            Ok(levels)
        }
        Some(_) => Err("levels must be an array of strings".into()),
    }
}

fn parse_timezone(table: &toml::Table) -> Result<Timezone, String> {
    match table.get("timezone") {
        None => Ok(Timezone::Utc),
        Some(Value::String(raw)) => Timezone::parse(raw).map_err(|err| err.to_string()),
        Some(_) => Err("timezone must be a string".into()),
    }
}

fn parse_extra_patterns(table: Option<&toml::Table>) -> Result<Vec<Regex>, String> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    match table.get("extra_patterns") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            if items.len() > MAX_EXTRA_PATTERNS {
                return Err(format!(
                    "redaction.extra_patterns must contain at most {MAX_EXTRA_PATTERNS} patterns"
                ));
            }
            let mut patterns = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let raw = item
                    .as_str()
                    .ok_or_else(|| format!("redaction.extra_patterns[{index}] must be a string"))?;
                patterns.push(compile_pattern(index, raw)?);
            }
            Ok(patterns)
        }
        Some(_) => Err("redaction.extra_patterns must be an array of strings".into()),
    }
}

fn compile_pattern(index: usize, pattern: &str) -> Result<Regex, String> {
    let path = format!("redaction.extra_patterns[{index}]");
    if pattern.is_empty() {
        return Err(format!("{path} must not be empty"));
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(format!("{path} must be at most {MAX_PATTERN_BYTES} bytes"));
    }
    Regex::new(pattern).map_err(|err| format!("{path} is not a valid regex: {err}"))
}

fn optional_table<'a>(
    table: &'a toml::Table,
    key: &'static str,
) -> Result<Option<&'a toml::Table>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Table(nested)) => Ok(Some(nested)),
        Some(_) => Err(format!("{key} must be a table")),
    }
}

fn reject_unknown(table: &toml::Table, prefix: &str, known: &[&str]) -> Result<(), String> {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            return Err(format!("unknown field '{path}'"));
        }
    }
    Ok(())
}

fn optional_u32(
    table: Option<&toml::Table>,
    path: &str,
    min: u32,
    max: u32,
) -> Result<Option<u32>, String> {
    let Some(table) = table else {
        return Ok(None);
    };
    let key = path.rsplit('.').next().expect("dotted path");
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => {
            if *value < i64::from(min) || *value > i64::from(max) {
                Err(format!("{path} must be between {min} and {max}"))
            } else {
                Ok(Some(*value as u32))
            }
        }
        Some(_) => Err(format!("{path} must be an integer")),
    }
}

fn optional_u64(
    table: Option<&toml::Table>,
    path: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    let Some(table) = table else {
        return Ok(None);
    };
    let key = path.rsplit('.').next().expect("dotted path");
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => {
            if *value < 0 || (*value as u64) < min || (*value as u64) > max {
                Err(format!("{path} must be between {min} and {max}"))
            } else {
                Ok(Some(*value as u64))
            }
        }
        Some(_) => Err(format!("{path} must be an integer")),
    }
}

fn optional_f64(
    table: Option<&toml::Table>,
    path: &str,
    min: f64,
    max: f64,
) -> Result<Option<f64>, String> {
    let Some(table) = table else {
        return Ok(None);
    };
    let key = path.rsplit('.').next().expect("dotted path");
    let value = match table.get(key) {
        None => return Ok(None),
        Some(Value::Float(value)) => *value,
        Some(Value::Integer(value)) => *value as f64,
        Some(_) => return Err(format!("{path} must be a number")),
    };
    if !value.is_finite() || value < min || value > max {
        Err(format!("{path} must be between {min:.2} and {max:.2}"))
    } else {
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Result<Tuning, String> {
        from_toml_str(body)
    }

    fn parse_v1(body: &str) -> Result<Tuning, String> {
        from_toml_str(&format!("version = 1\n{body}"))
    }

    fn err_v1(body: &str) -> String {
        parse_v1(body).expect_err("expected invalid tuning")
    }

    #[test]
    fn version_is_required_with_exact_diagnostic() {
        let err = parse("timezone = \"utc\"\n").expect_err("missing version");
        assert_eq!(err, "version is required; expected version = 1");
    }

    #[test]
    fn unsupported_version_fails_with_exact_diagnostic() {
        let err = parse("version = 2\n").expect_err("v2");
        assert_eq!(err, "unsupported config version 2; expected version = 1");
        let err = parse("version = 0\n").expect_err("v0");
        assert_eq!(err, "unsupported config version 0; expected version = 1");
        let err = parse("version = \"1\"\n").expect_err("string version");
        assert_eq!(err, "version must be an integer; expected version = 1");
    }

    #[test]
    fn version_only_file_equals_defaults() {
        assert_eq!(parse("version = 1\n").expect("v1"), Tuning::default());
    }

    #[test]
    fn example_toml_is_accepted_and_matches_defaults() {
        let tuning = parse(include_str!("../../aemlog.example.toml")).expect("example");
        assert_eq!(tuning, Tuning::default());
    }

    #[test]
    fn defaults_are_within_every_bound() {
        let tuning = Tuning::default();
        assert!(!tuning.levels.is_empty());
        assert!((MIN_SIMILARITY..=MAX_SIMILARITY).contains(&tuning.similarity));
        assert!((MIN_BUCKET_CAP..=MAX_BUCKET_CAP).contains(&tuning.bucket_cap));
        assert!((MIN_GROUPS..=MAX_GROUPS).contains(&tuning.max_groups));
        assert!((MIN_EVENT_BYTES..=MAX_EVENT_BYTES).contains(&tuning.event_max_bytes));
        assert!((MIN_EVENT_LINES..=MAX_EVENT_LINES).contains(&tuning.event_max_lines));
        assert!((MIN_SAMPLE_BYTES..=MAX_SAMPLE_BYTES).contains(&tuning.sample_max_bytes));
        assert!((MIN_SAMPLE_BUDGET..=MAX_SAMPLE_BUDGET).contains(&tuning.sample_budget_bytes));
        assert!((MIN_SECS..=MAX_SECS).contains(&tuning.fast_half_life_secs));
        assert!((MIN_SECS..=MAX_SECS).contains(&tuning.baseline_half_life_secs));
        assert!(tuning.baseline_half_life_secs > tuning.fast_half_life_secs);
        assert!(tuning.sample_max_bytes <= tuning.sample_budget_bytes);
        assert!((MIN_RATIO..=MAX_RATIO).contains(&tuning.increasing_ratio));
        assert!((MIN_RATE..=MAX_RATE).contains(&tuning.increasing_min_rate));
        assert!(tuning.extra_patterns.len() <= MAX_EXTRA_PATTERNS);
    }

    #[test]
    fn unknown_keys_and_tables_are_rejected() {
        for (body, needle) in [
            ("program_id = \"p1\"\n", "program_id"),
            ("environment_id = \"e1\"\n", "environment_id"),
            ("service = \"author\"\n", "service"),
            ("ims_context = \"ctx\"\n", "ims_context"),
            ("aio = \"/usr/bin/aio\"\n", "aio"),
            ("force_merge = true\n", "force_merge"),
            ("[source]\nprogram_id = \"p1\"\n", "source"),
            ("[mute]\nrules = []\n", "mute"),
            ("[split]\nrules = []\n", "split"),
            ("[templates]\nfoo = 1\n", "templates.foo"),
            ("[groups]\ncapacity = 1\n", "groups.capacity"),
            ("[event]\nunknown = 1\n", "event.unknown"),
            ("[sample]\nunknown = 1\n", "sample.unknown"),
            ("[rates]\nunknown = 1\n", "rates.unknown"),
            ("[redaction]\nunknown = []\n", "redaction.unknown"),
        ] {
            let err = err_v1(body);
            assert!(
                err.contains(&format!("unknown field '{needle}'")),
                "body {body:?} err {err}"
            );
        }
    }

    #[test]
    fn integer_bounds_accept_min_max_and_reject_just_outside() {
        let cases: &[(&str, &str, u64, u64)] = &[
            (
                "templates.bucket_cap",
                "[templates]\nbucket_cap",
                MIN_BUCKET_CAP as u64,
                MAX_BUCKET_CAP as u64,
            ),
            (
                "groups.max",
                "[groups]\nmax",
                MIN_GROUPS as u64,
                MAX_GROUPS as u64,
            ),
            (
                "event.max_bytes",
                "[event]\nmax_bytes",
                MIN_EVENT_BYTES,
                MAX_EVENT_BYTES,
            ),
            (
                "event.max_lines",
                "[event]\nmax_lines",
                MIN_EVENT_LINES as u64,
                MAX_EVENT_LINES as u64,
            ),
            (
                "sample.max_bytes",
                "[sample]\nmax_bytes",
                MIN_SAMPLE_BYTES,
                MAX_SAMPLE_BYTES,
            ),
            (
                "sample.budget_bytes",
                "[sample]\nbudget_bytes",
                DEFAULT_SAMPLE_BYTES,
                MAX_SAMPLE_BUDGET,
            ),
            (
                "rates.fast_half_life_secs",
                "[rates]\nfast_half_life_secs",
                MIN_SECS as u64,
                DEFAULT_BASELINE_HALF_LIFE_SECS as u64 - 1,
            ),
            (
                "rates.baseline_half_life_secs",
                "[rates]\nbaseline_half_life_secs",
                DEFAULT_FAST_HALF_LIFE_SECS as u64 + 1,
                MAX_SECS as u64,
            ),
            (
                "rates.new_age_secs",
                "[rates]\nnew_age_secs",
                MIN_SECS as u64,
                MAX_SECS as u64,
            ),
            (
                "rates.increasing_min_age_secs",
                "[rates]\nincreasing_min_age_secs",
                MIN_SECS as u64,
                MAX_SECS as u64,
            ),
        ];
        for (path, prefix, min, max) in cases {
            parse_v1(&format!("{prefix} = {min}\n"))
                .unwrap_or_else(|err| panic!("{path} min {min} should be accepted: {err}"));
            parse_v1(&format!("{prefix} = {max}\n"))
                .unwrap_or_else(|err| panic!("{path} max {max} should be accepted: {err}"));
            if *min > 0 {
                let err = err_v1(&format!("{prefix} = {}\n", min - 1));
                assert!(err.contains(path), "{path} min-1: {err}");
            }
            let err = err_v1(&format!("{prefix} = {}\n", max + 1));
            assert!(err.contains(path), "{path} max+1: {err}");
        }
    }

    #[test]
    fn companion_fields_allow_absolute_ceilings() {
        parse_v1("[sample]\nmax_bytes = 1\nbudget_bytes = 1\n").expect("sample min pair");
        let err = err_v1("[sample]\nmax_bytes = 1\nbudget_bytes = 0\n");
        assert!(err.contains("sample.budget_bytes must be between"), "{err}");

        parse_v1(&format!(
            "[sample]\nmax_bytes = {MAX_SAMPLE_BYTES}\nbudget_bytes = {MAX_SAMPLE_BUDGET}\n"
        ))
        .expect("sample max pair");
        let err = err_v1(&format!(
            "[sample]\nmax_bytes = {}\nbudget_bytes = {MAX_SAMPLE_BUDGET}\n",
            MAX_SAMPLE_BYTES + 1
        ));
        assert!(err.contains("sample.max_bytes must be between"), "{err}");

        parse_v1(&format!(
            "[rates]\nfast_half_life_secs = {}\nbaseline_half_life_secs = {MAX_SECS}\n",
            MAX_SECS - 1
        ))
        .expect("half-life max pair");
        let err = err_v1(&format!(
            "[rates]\nfast_half_life_secs = {}\nbaseline_half_life_secs = {MAX_SECS}\n",
            u64::from(MAX_SECS) + 1
        ));
        assert!(
            err.contains("rates.fast_half_life_secs must be between"),
            "{err}"
        );
        let err = err_v1("[rates]\nfast_half_life_secs = 1\nbaseline_half_life_secs = 86401\n");
        assert!(
            err.contains("rates.baseline_half_life_secs must be between"),
            "{err}"
        );
    }

    #[test]
    fn float_bounds_accept_min_max_and_reject_just_outside() {
        let ok = parse_v1("[templates]\nsimilarity = 0.50\n").expect("sim min");
        assert_eq!(ok.similarity, 0.50);
        let ok = parse_v1("[templates]\nsimilarity = 1.00\n").expect("sim max");
        assert_eq!(ok.similarity, 1.00);
        let err = err_v1("[templates]\nsimilarity = 0.49\n");
        assert!(err.contains("templates.similarity"), "{err}");
        let err = err_v1("[templates]\nsimilarity = 1.01\n");
        assert!(err.contains("templates.similarity"), "{err}");

        let ok = parse_v1("[rates]\nincreasing_ratio = 1.00\n").expect("ratio min");
        assert_eq!(ok.increasing_ratio, 1.00);
        let ok = parse_v1("[rates]\nincreasing_ratio = 100.00\n").expect("ratio max");
        assert_eq!(ok.increasing_ratio, 100.00);
        let err = err_v1("[rates]\nincreasing_ratio = 0.99\n");
        assert!(err.contains("rates.increasing_ratio"), "{err}");
        let err = err_v1("[rates]\nincreasing_ratio = 100.01\n");
        assert!(err.contains("rates.increasing_ratio"), "{err}");

        let ok = parse_v1("[rates]\nincreasing_min_rate = 0.00\n").expect("rate min");
        assert_eq!(ok.increasing_min_rate, 0.00);
        let ok = parse_v1("[rates]\nincreasing_min_rate = 1000000.00\n").expect("rate max");
        assert_eq!(ok.increasing_min_rate, 1_000_000.00);
        let err = err_v1("[rates]\nincreasing_min_rate = -0.01\n");
        assert!(err.contains("rates.increasing_min_rate"), "{err}");
        let err = err_v1("[rates]\nincreasing_min_rate = 1000000.01\n");
        assert!(err.contains("rates.increasing_min_rate"), "{err}");
    }

    #[test]
    fn cross_field_validation() {
        let err = err_v1("levels = []\n");
        assert!(err.contains("levels must not be empty"), "{err}");

        let err = err_v1("[rates]\nfast_half_life_secs = 10\nbaseline_half_life_secs = 10\n");
        assert!(
            err.contains(
                "rates.baseline_half_life_secs must be greater than rates.fast_half_life_secs"
            ),
            "{err}"
        );
        let err = err_v1("[rates]\nfast_half_life_secs = 300\nbaseline_half_life_secs = 10\n");
        assert!(
            err.contains(
                "rates.baseline_half_life_secs must be greater than rates.fast_half_life_secs"
            ),
            "{err}"
        );

        let err = err_v1("[sample]\nmax_bytes = 1000\nbudget_bytes = 999\n");
        assert!(
            err.contains("sample.max_bytes must not exceed sample.budget_bytes"),
            "{err}"
        );
    }

    #[test]
    fn extra_patterns_compile_with_indexed_errors() {
        let tuning =
            parse_v1("[redaction]\nextra_patterns = [\"secret-[0-9]+\", \"token=[A-Za-z0-9]+\"]\n")
                .expect("valid patterns");
        assert_eq!(tuning.extra_patterns.len(), 2);
        assert_eq!(tuning.extra_patterns[0].as_str(), "secret-[0-9]+");

        let err = err_v1("[redaction]\nextra_patterns = [\"ok\", \"(\"]\n");
        assert!(
            err.contains("redaction.extra_patterns[1] is not a valid regex"),
            "{err}"
        );

        let err = err_v1("[redaction]\nextra_patterns = [\"\"]\n");
        assert!(
            err.contains("redaction.extra_patterns[0] must not be empty"),
            "{err}"
        );

        let too_long = "a".repeat(MAX_PATTERN_BYTES + 1);
        let err = err_v1(&format!("[redaction]\nextra_patterns = [\"{too_long}\"]\n"));
        assert!(
            err.contains("redaction.extra_patterns[0] must be at most 1024 bytes"),
            "{err}"
        );

        let max_len = "a".repeat(MAX_PATTERN_BYTES);
        parse_v1(&format!("[redaction]\nextra_patterns = [\"{max_len}\"]\n"))
            .expect("1024-byte pattern");

        let thirty_two = (0..MAX_EXTRA_PATTERNS)
            .map(|i| format!("\"p{i}\""))
            .collect::<Vec<_>>()
            .join(", ");
        parse_v1(&format!("[redaction]\nextra_patterns = [{thirty_two}]\n")).expect("32 patterns");

        let thirty_three = (0..=MAX_EXTRA_PATTERNS)
            .map(|i| format!("\"p{i}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let err = err_v1(&format!("[redaction]\nextra_patterns = [{thirty_three}]\n"));
        assert!(
            err.contains("redaction.extra_patterns must contain at most 32 patterns"),
            "{err}"
        );
    }

    #[test]
    fn type_errors_use_field_paths() {
        let err = err_v1("levels = \"ERROR\"\n");
        assert!(err.contains("levels must be an array of strings"), "{err}");
        let err = err_v1("levels = [1]\n");
        assert!(err.contains("levels[0] must be a string"), "{err}");
        let err = err_v1("timezone = 1\n");
        assert!(err.contains("timezone must be a string"), "{err}");
        let err = err_v1("[templates]\nsimilarity = \"high\"\n");
        assert!(
            err.contains("templates.similarity must be a number"),
            "{err}"
        );
        let err = err_v1("[templates]\nbucket_cap = 100.5\n");
        assert!(
            err.contains("templates.bucket_cap must be an integer"),
            "{err}"
        );
        let err = err_v1("templates = 1\n");
        assert!(err.contains("templates must be a table"), "{err}");
    }

    #[test]
    fn integer_similarity_one_is_accepted() {
        let tuning = parse_v1("[templates]\nsimilarity = 1\n").expect("int 1");
        assert_eq!(tuning.similarity, 1.00);
    }
}
