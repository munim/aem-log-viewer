use std::fs;
use std::path::PathBuf;

fn schema() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema/aemlog-v1.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn validator() -> jsonschema::Validator {
    jsonschema::draft202012::new(&schema()).expect("schema compiles")
}

fn load_dir(kind: &str) -> Vec<(String, serde_json::Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ndjson")
        .join(kind);
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let value = serde_json::from_str(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            (name, value)
        })
        .collect()
}

#[test]
fn schema_is_draft_2020_12() {
    assert!(jsonschema::draft202012::meta::is_valid(&schema()));
}

#[test]
fn valid_fixtures_conform() {
    let validator = validator();
    let fixtures = load_dir("valid");
    assert_eq!(fixtures.len(), 6, "expected one fixture per event type");
    for (name, value) in fixtures {
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|err| err.to_string())
            .collect();
        assert!(errors.is_empty(), "{name} should be valid: {errors:?}");
    }
}

#[test]
fn invalid_fixtures_are_rejected() {
    let validator = validator();
    let fixtures = load_dir("invalid");
    assert_eq!(fixtures.len(), 5);
    for (name, value) in fixtures {
        assert!(
            !validator.is_valid(&value),
            "{name} should be rejected by aemlog-v1"
        );
    }
}

#[test]
fn non_finite_rate_is_rejected() {
    let validator = validator();
    let mut value = load_dir("valid")
        .into_iter()
        .find(|(name, _)| name == "group_updated.json")
        .expect("valid update")
        .1;
    value["fast_rate"] = serde_json::json!(f64::INFINITY);
    assert!(!validator.is_valid(&value));
    value["fast_rate"] = serde_json::json!(f64::NAN);
    assert!(!validator.is_valid(&value));
}
