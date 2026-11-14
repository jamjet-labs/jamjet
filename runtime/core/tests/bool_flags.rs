//! The switch values in `sdk/python/tests/fixtures/bool_flags.json` read the
//! same here as in the Python SDK (`sdk/python/tests/test_flags.py` reads the
//! same file).

use jamjet_core::flag::parse_flag;

fn cases(kind: &str) -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/python/tests/fixtures/bool_flags.json");
    let all: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let list: Vec<String> = serde_json::from_value(all[kind].clone()).unwrap();
    assert!(!list.is_empty(), "no {kind} cases");
    list
}

#[test]
fn on_values() {
    for v in cases("on") {
        assert_eq!(parse_flag("X", Some(&v)), Ok(true), "{v:?}");
    }
}

#[test]
fn off_values() {
    for v in cases("off") {
        assert_eq!(parse_flag("X", Some(&v)), Ok(false), "{v:?}");
    }
    assert_eq!(parse_flag("X", None), Ok(false), "unset");
}

#[test]
fn anything_else_is_an_error_naming_the_variable() {
    for v in cases("error") {
        let err = parse_flag("JAMJET_SOME_SWITCH", Some(&v)).unwrap_err();
        assert!(err.contains("JAMJET_SOME_SWITCH"), "{v:?}: {err}");
    }
}
