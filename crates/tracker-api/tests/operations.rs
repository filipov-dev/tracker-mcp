//! Инварианты таблицы операций, сгенерированной `build.rs`, и снапшоты характерных операций.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use tracker_api::spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamLocation, ParamType, ResponseKind, find,
    find_toolset, operations, toolsets,
};

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::from_str(tracker_api::OPENAPI_JSON).expect("спецификация должна быть JSON")
}

/// Операции спецификации по `operationId`.
fn spec_operations(spec: &Value) -> BTreeMap<String, (String, &'static str, Value)> {
    let mut out = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                let id = op["operationId"].as_str().expect("operationId").to_owned();
                out.insert(id, (path.clone(), method, op.clone()));
            }
        }
    }
    out
}

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn placeholders(path: &str) -> BTreeSet<&str> {
    path.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect()
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

#[test]
fn every_spec_operation_is_generated() {
    let spec = spec();
    let expected = spec_operations(&spec);
    assert!(
        (150..=300).contains(&expected.len()),
        "неожиданное число операций в спецификации: {}",
        expected.len()
    );
    assert_eq!(operations().len(), expected.len());
    let generated: BTreeSet<&str> = operations().iter().map(|op| op.operation_id).collect();
    let in_spec: BTreeSet<&str> = expected.keys().map(String::as_str).collect();
    assert_eq!(generated, in_spec);

    for op in operations() {
        let (path, method, raw) = &expected[op.operation_id];
        assert_eq!(op.path, path, "{}", op.tool_name);
        assert_eq!(
            op.method.as_str(),
            method.to_uppercase(),
            "{}",
            op.tool_name
        );
        assert_eq!(
            Some(op.access.as_str()),
            raw["x-access-level"].as_str(),
            "{}: уровень доступа должен совпадать с x-access-level",
            op.tool_name
        );
        assert_eq!(
            Some(op.docs_url),
            raw["externalDocs"]["url"].as_str(),
            "{}",
            op.tool_name
        );
        assert!(!op.summary.is_empty(), "{}: нет summary", op.tool_name);
        let tag = raw["tags"][0].as_str().expect("тег");
        assert_eq!(
            find_toolset(op.toolset).map(|t| t.tag),
            Some(tag),
            "{}: набор не соответствует тегу",
            op.tool_name
        );
    }
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "имена должны быть уникальны и отсортированы");
    for name in names {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимое имя инструмента {name}"
        );
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("no_such_tool").is_none());
    assert_eq!(op("get_issue").operation_id, "getIssue");
    assert_eq!(
        op("view_autoaction_logs2").operation_id,
        "viewAutoactionLogs2"
    );
}

#[test]
fn every_tag_has_toolset() {
    let spec = spec();
    let tags: Vec<&str> = spec["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    let toolset_tags: Vec<&str> = toolsets().iter().map(|t| t.tag).collect();
    assert_eq!(
        toolset_tags, tags,
        "наборы должны повторять tags спецификации"
    );

    let slugs: BTreeSet<&str> = toolsets().iter().map(|t| t.slug).collect();
    assert_eq!(slugs.len(), toolsets().len(), "slug повторяются");
    for toolset in toolsets() {
        assert!(
            toolset
                .slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимый slug {}",
            toolset.slug
        );
        assert!(
            operations().iter().any(|op| op.toolset == toolset.slug),
            "пустой набор {}",
            toolset.slug
        );
    }
    for op in operations() {
        assert!(slugs.contains(op.toolset), "{}", op.tool_name);
    }
    for slug in [
        "issues",
        "queues",
        "boards",
        "entities",
        "projects",
        "filters",
        "dashboards",
        "macros",
        "bulkchange",
        "import",
        "users",
        "gaps",
        "admin",
    ] {
        assert!(find_toolset(slug).is_some(), "нет набора {slug}");
    }
}

#[test]
fn input_schemas_are_objects_with_all_params() {
    for op in operations() {
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        assert!(
            !op.input_schema.contains("$ref"),
            "{}: $ref не раскрыт",
            op.tool_name
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: нет properties", op.tool_name));
        let param_names: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        let property_names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(param_names, property_names, "{}", op.tool_name);
        assert_eq!(
            param_names.len(),
            op.params.len(),
            "{}: повтор параметра",
            op.tool_name
        );
        for header in ["Authorization", "X-Org-ID", "X-Cloud-Org-ID"] {
            assert!(
                !property_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(header)),
                "{}: заголовок {header} попал в аргументы",
                op.tool_name
            );
        }

        for param in op.params {
            assert!(
                !param.description.is_empty(),
                "{}: {}",
                op.tool_name,
                param.name
            );
            assert_eq!(
                properties[param.name]["description"], param.description,
                "{}: описание {} в схеме и ParamDef различается",
                op.tool_name, param.name
            );
        }

        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expected: BTreeSet<&str> = op
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();
        assert_eq!(required, expected, "{}", op.tool_name);

        check_schema(op.tool_name, "", &schema);
    }
}

/// Проверяет, что схема понятна MCP-клиентам: известные типы, имена свойств по шаблону
/// `^[a-zA-Z0-9_.-]{1,64}$` (ограничение API Claude), непустой `required` из имён свойств.
fn check_schema(tool: &str, at: &str, schema: &Value) {
    const TYPES: [&str; 7] = [
        "string", "number", "integer", "boolean", "array", "object", "null",
    ];
    let object = schema
        .as_object()
        .unwrap_or_else(|| panic!("{tool}{at}: схема не объект"));
    for key in object.keys() {
        assert!(
            !key.starts_with("x-") && key != "example" && key != "examples" && key != "$ref",
            "{tool}{at}: лишний ключ {key}"
        );
    }
    match object.get("type") {
        None => {}
        Some(Value::String(kind)) => {
            assert!(TYPES.contains(&kind.as_str()), "{tool}{at}: тип {kind}")
        }
        Some(Value::Array(kinds)) => {
            for kind in kinds {
                assert!(
                    kind.as_str().is_some_and(|k| TYPES.contains(&k)),
                    "{tool}{at}: тип {kind}"
                );
            }
        }
        Some(other) => panic!("{tool}{at}: тип {other}"),
    }
    let is_array = match object.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k == "array"),
        _ => false,
    };
    assert!(
        !is_array || object.contains_key("items"),
        "{tool}{at}: массив без items"
    );
    let properties = object.get("properties").and_then(Value::as_object);
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .unwrap_or_else(|| panic!("{tool}{at}: required не массив"));
        assert!(!required.is_empty(), "{tool}{at}: пустой required");
        for name in required {
            let name = name.as_str().expect("имя в required");
            assert!(
                properties.is_some_and(|p| p.contains_key(name)),
                "{tool}{at}: required {name} нет в properties"
            );
        }
    }
    for (name, property) in properties.into_iter().flatten() {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "{tool}{at}: недопустимое имя свойства {name}"
        );
        check_schema(tool, &format!("{at}.{name}"), property);
    }
    if let Some(items) = object.get("items") {
        check_schema(tool, &format!("{at}[]"), items);
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional.is_object()
    {
        check_schema(tool, &format!("{at}{{}}"), additional);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = object.get(key) {
            for variant in variants.as_array().expect("варианты") {
                check_schema(tool, &format!("{at}|"), variant);
            }
        }
    }
}

#[test]
fn path_params_match_templates() {
    for op in operations() {
        let declared: BTreeSet<&str> = op.params_in(ParamLocation::Path).map(|p| p.name).collect();
        assert_eq!(placeholders(op.path), declared, "{}", op.tool_name);
        assert!(
            op.params_in(ParamLocation::Path).all(|p| p.required),
            "{}: path-параметр необязателен",
            op.tool_name
        );
        assert!(
            op.path.starts_with("/v2/") || op.path.starts_with("/v3/"),
            "{}: {}",
            op.tool_name,
            op.path
        );
    }
}

#[test]
fn body_kind_matches_params() {
    for op in operations() {
        let bodies: Vec<_> = op.params_in(ParamLocation::Body).collect();
        let form: Vec<_> = op.params_in(ParamLocation::FormData).collect();
        match op.body {
            None => assert!(bodies.is_empty() && form.is_empty(), "{}", op.tool_name),
            Some(BodyKind::Json) => {
                assert!(form.is_empty(), "{}", op.tool_name);
                assert!(
                    matches!(bodies.as_slice(), [body] if body.name == "body" && body.value_type == ParamType::Object),
                    "{}",
                    op.tool_name
                );
            }
            Some(BodyKind::Multipart) => {
                assert!(bodies.is_empty(), "{}", op.tool_name);
                assert!(!form.is_empty(), "{}", op.tool_name);
            }
        }
        for param in op.params {
            assert_eq!(
                param.value_type == ParamType::File,
                param.location == ParamLocation::FormData,
                "{}: {}",
                op.tool_name,
                param.name
            );
        }
    }
}

#[test]
fn access_levels_and_kinds() {
    for op in operations() {
        if op.method == HttpMethod::Get {
            assert_eq!(op.access, AccessLevel::Read, "{}", op.tool_name);
        }
        if op.method == HttpMethod::Delete {
            assert_eq!(op.access, AccessLevel::Destructive, "{}", op.tool_name);
        }
    }
    assert_eq!(op("get_issue").access, AccessLevel::Read);
    assert_eq!(op("search_issues").access, AccessLevel::Read);
    assert_eq!(op("create_issue").access, AccessLevel::Write);
    assert_eq!(op("delete_tag").access, AccessLevel::Destructive);
    assert_eq!(op("manage_access").access, AccessLevel::Destructive);

    let binary: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.response == ResponseKind::Binary)
        .map(|op| op.tool_name)
        .collect();
    assert_eq!(
        binary,
        BTreeSet::from(["get_attachment_preview", "issues_get_attachment"])
    );

    let multipart: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.body == Some(BodyKind::Multipart))
        .map(|op| op.tool_name)
        .collect();
    assert_eq!(
        multipart,
        BTreeSet::from(["import_attachments", "post_attachment", "temp_attachment"])
    );
    for name in multipart {
        let file = op(name)
            .param("file")
            .unwrap_or_else(|| panic!("{name}: нет параметра file"));
        assert_eq!(file.location, ParamLocation::FormData, "{name}");
        assert_eq!(file.value_type, ParamType::File, "{name}");
        assert!(file.required, "{name}");
    }
}

/// Снапшот операции. Ключи сортируются, чтобы порядок не зависел от фичи
/// `preserve_order` у `serde_json`, которую может включить любая зависимость.
macro_rules! snapshot {
    ($name:expr) => {
        insta::with_settings!({ sort_maps => true }, {
            insta::assert_json_snapshot!(snapshot_view(op($name)));
        })
    };
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "value_type": format!("{:?}", p.value_type),
                "description": p.description,
            })
        })
        .collect();
    json!({
        "tool_name": op.tool_name,
        "operation_id": op.operation_id,
        "toolset": op.toolset,
        "summary": op.summary,
        "description": op.description,
        "docs_url": op.docs_url,
        "method": op.method.as_str(),
        "path": op.path,
        "access": format!("{:?}", op.access),
        "params": params,
        "body": op.body.map(|kind| format!("{kind:?}")),
        "response": format!("{:?}", op.response),
        "input_schema": schema(op),
    })
}

#[test]
fn snapshot_get_issue() {
    snapshot!("get_issue");
}

#[test]
fn snapshot_create_issue() {
    // JSON-тело с полями-объединениями (`anyOf`).
    snapshot!("create_issue");
}

#[test]
fn snapshot_search_issues() {
    // POST, который только читает.
    snapshot!("search_issues");
}

#[test]
fn snapshot_post_attachment() {
    // multipart-загрузка файла.
    snapshot!("post_attachment");
}

#[test]
fn snapshot_issues_get_attachment() {
    // Скачивание файла: бинарный ответ.
    snapshot!("issues_get_attachment");
}

#[test]
fn snapshot_delete_queue() {
    snapshot!("delete_queue");
}

#[test]
fn snapshot_delete_filter() {
    // Операция API v2.
    snapshot!("delete_filter");
}

#[test]
fn snapshot_patch_sprint() {
    // Версия спринта — обязательный параметр запроса version, хотя справочник
    // передаёт её заголовком If-Match.
    snapshot!("patch_sprint");
}

#[test]
fn object_versions_are_query_params() {
    // Справочник передаёт версию заголовком If-Match (спринты, колонки), не описывает её
    // (доска) или описывает в таблице «Ресурс» вместо параметров запроса (остальные).
    for name in [
        "patch_sprint",
        "start_sprint",
        "archive_sprint",
        "patch_board",
        "post_column",
        "patch_column",
        "delete_column",
        "patch_workflow",
        "patch_workflow_action",
        "change_trigger",
        "patch_component",
        "patch_issue_field_value",
    ] {
        let version = op(name)
            .param("version")
            .unwrap_or_else(|| panic!("{name}: нет параметра version"));
        assert_eq!(version.location, ParamLocation::Query, "{name}");
    }
    assert!(
        op("patch_sprint")
            .param("version")
            .is_some_and(|p| p.required)
    );
}
