//! Проверки встроенной OpenAPI-спецификации.

use std::collections::BTreeSet;

use serde_json::Value;

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::from_str(tracker_api::OPENAPI_JSON)
        .expect("спецификация должна быть корректным JSON")
}

#[test]
fn spec_is_openapi_3_1() {
    let spec = spec();
    assert_eq!(spec["openapi"].as_str(), Some("3.1.0"));
}

#[test]
fn every_operation_has_id_and_access_level() {
    let spec = spec();
    let paths = spec["paths"]
        .as_object()
        .expect("paths должен быть объектом");
    let mut count = 0;
    for (path, item) in paths {
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            count += 1;
            assert!(
                op["operationId"].as_str().is_some_and(|id| !id.is_empty()),
                "{method} {path}: нет operationId"
            );
            let level = op["x-access-level"].as_str();
            assert!(
                matches!(level, Some("read" | "write" | "destructive")),
                "{method} {path}: неизвестный x-access-level {level:?}"
            );
        }
    }
    assert!(count > 0, "в спецификации нет операций");
}

#[test]
fn operation_ids_are_unique() {
    let spec = spec();
    let mut seen = BTreeSet::new();
    for (path, method, op) in operations(&spec) {
        let id = op["operationId"].as_str().unwrap_or_default();
        assert!(
            seen.insert(id.to_owned()),
            "{method} {path}: operationId {id} повторяется"
        );
    }
}

#[test]
fn all_refs_resolve() {
    let spec = spec();
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    assert!(!refs.is_empty(), "в спецификации нет $ref");
    for reference in refs {
        let pointer = reference
            .strip_prefix('#')
            .unwrap_or_else(|| panic!("внешняя ссылка {reference}"));
        assert!(
            spec.pointer(pointer).is_some(),
            "ссылка {reference} никуда не ведёт"
        );
    }
}

#[test]
fn path_parameters_match_templates() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        let in_template: BTreeSet<String> = path
            .split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name.to_owned()))
            .collect();
        let path_item = &spec["paths"][path.as_str()];
        let declared: BTreeSet<String> = [&path_item["parameters"], &op["parameters"]]
            .into_iter()
            .filter_map(Value::as_array)
            .flatten()
            .map(|param| resolve(&spec, param))
            .filter(|param| param["in"] == "path")
            .filter_map(|param| param["name"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            in_template, declared,
            "{method} {path}: параметры пути не совпадают с шаблоном"
        );
    }
}

fn operations(spec: &Value) -> Vec<(String, &'static str, &Value)> {
    let mut out = Vec::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                out.push((path.clone(), method, op));
            }
        }
    }
    out
}

fn resolve<'a>(spec: &'a Value, value: &'a Value) -> &'a Value {
    match value["$ref"].as_str().and_then(|r| r.strip_prefix('#')) {
        Some(pointer) => spec
            .pointer(pointer)
            .unwrap_or_else(|| panic!("ссылка #{pointer} никуда не ведёт")),
        None => value,
    }
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                // Примеры ответов — данные Tracker, в них `$ref` не ссылка.
                if key == "example" || key == "examples" {
                    continue;
                }
                if key == "$ref" {
                    if let Some(reference) = child.as_str() {
                        out.push(reference.to_owned());
                    }
                } else {
                    collect_refs(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, out)),
        _ => {}
    }
}
