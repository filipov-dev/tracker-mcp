//! Сборка запроса к API из аргументов сгенерированного инструмента.
//!
//! Порядок описан в документации `tracker_api::spec`: неизвестные аргументы отклоняются,
//! обязательные проверяются, значения приводятся по [`ParamType`]. Значение `null` считается
//! непереданным аргументом: некоторые клиенты так передают необязательные параметры.
//!
//! Локальные файлы проходят [`FilePolicy`]: файл для загрузки (параметр [`ParamType::File`])
//! и файл для сохранения двоичного ответа (аргумент [`SAVE_TO_ARG`]) должны лежать внутри
//! разрешённых каталогов.

use std::collections::HashMap;

use rmcp::model::JsonObject;
use serde_json::Value;
use tracker_api::executor::{ApiRequest, FileSource, MultipartPart, RequestBody, render_path};
use tracker_api::spec::{OperationDef, ParamDef, ParamLocation, ParamType, ResponseKind};

use crate::error::ToolError;
use crate::files::FilePolicy;

/// Аргумент инструментов с двоичным ответом: куда сохранить файл.
pub const SAVE_TO_ARG: &str = "save_to";

/// Принимает ли инструмент операции аргумент [`SAVE_TO_ARG`].
pub fn takes_save_to(op: &OperationDef) -> bool {
    op.response == ResponseKind::Binary
}

/// Собирает [`ApiRequest`] для операции `op` из аргументов инструмента.
pub fn build_request(
    op: &OperationDef,
    args: &JsonObject,
    files: &FilePolicy,
) -> Result<ApiRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());

    reject_unknown(op, args)?;
    let mut missing: Vec<String> = op
        .params
        .iter()
        .filter(|param| param.required && !present(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if takes_save_to(op) && !present(SAVE_TO_ARG) {
        missing.push(format!("`{SAVE_TO_ARG}`"));
    }
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }

    let mut path_values: HashMap<&str, String> = HashMap::new();
    let mut query = Vec::new();
    let mut body = RequestBody::Empty;
    let mut parts = Vec::new();

    for param in op.params {
        let Some(value) = args.get(param.name).filter(|value| !value.is_null()) else {
            continue;
        };
        match param.location {
            ParamLocation::Path => {
                path_values.insert(param.name, scalar(param, value)?);
            }
            ParamLocation::Query => {
                for item in query_values(param, value)? {
                    query.push((param.name.to_owned(), item));
                }
            }
            ParamLocation::Body => body = RequestBody::Json(json_body(param, value)?),
            ParamLocation::FormData => parts.push(form_part(param, value, files)?),
        }
    }
    if !parts.is_empty() {
        body = RequestBody::Multipart(parts);
    }

    let path = render_path(op.path, |name| path_values.get(name).cloned())?;
    let save_to = match args.get(SAVE_TO_ARG).filter(|value| !value.is_null()) {
        Some(value) if takes_save_to(op) => {
            let Value::String(target) = value else {
                return Err(ToolError::new(format!(
                    "argument `{SAVE_TO_ARG}` must be a path to a new local file, got {}",
                    kind(value)
                )));
            };
            Some(
                files
                    .check_write(target)
                    .map_err(|err| ToolError::new(format!("argument `{SAVE_TO_ARG}`: {err}")))?,
            )
        }
        _ => None,
    };

    Ok(ApiRequest {
        method: op.method,
        path,
        query,
        body,
        save_to,
    })
}

/// Проверяет значение аргумента операции заранее, без сборки запроса: так пользовательский
/// инструмент с фиксированными аргументами (`tools.toml`) отклоняется при загрузке, а не при
/// первом вызове. Файловые аргументы и `save_to` фиксировать нельзя: пути должен проверять
/// [`FilePolicy`] при каждом вызове.
pub fn check_fixed_argument(op: &OperationDef, name: &str, value: &Value) -> Result<(), ToolError> {
    if name == SAVE_TO_ARG && takes_save_to(op) {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}` cannot be fixed: the agent passes a new file path on every \
             call"
        )));
    }
    let Some(param) = op.params.iter().find(|param| param.name == name) else {
        let names: Vec<String> = op
            .params
            .iter()
            .map(|param| format!("`{}`", param.name))
            .collect();
        return Err(ToolError::new(format!(
            "operation `{}` has no argument `{name}`; its arguments: {}",
            op.tool_name,
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        )));
    };
    if value.is_null() {
        return Err(ToolError::new(format!(
            "argument `{name}` cannot be fixed to null; remove it instead"
        )));
    }
    match param.location {
        ParamLocation::Path => scalar(param, value).map(|_| ()),
        ParamLocation::Query => query_values(param, value).map(|_| ()),
        ParamLocation::Body => json_body(param, value).map(|_| ()),
        ParamLocation::FormData if param.value_type == ParamType::File => {
            Err(ToolError::new(format!(
                "file argument `{name}` cannot be fixed: local paths are checked on every call"
            )))
        }
        ParamLocation::FormData => scalar(param, value).map(|_| ()),
    }
}

fn reject_unknown(op: &OperationDef, args: &JsonObject) -> Result<(), ToolError> {
    let allowed = |key: &str| {
        op.params.iter().any(|param| param.name == key) || (takes_save_to(op) && key == SAVE_TO_ARG)
    };
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !allowed(key))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let mut names: Vec<String> = op
        .params
        .iter()
        .map(|param| format!("`{}`", param.name))
        .collect();
    if takes_save_to(op) {
        names.push(format!("`{SAVE_TO_ARG}`"));
    }
    let names = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {names}",
        unknown.join(", ")
    )))
}

/// Значение path-, query- или текстового multipart-параметра строкой.
fn scalar(param: &ParamDef, value: &Value) -> Result<String, ToolError> {
    let invalid = |expected: &str| {
        ToolError::new(format!(
            "argument `{}` must be {expected}, got {}",
            param.name,
            kind(value)
        ))
    };
    match param.value_type {
        ParamType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
            Value::String(s) if is_integer(s.trim()) => Ok(s.trim().to_owned()),
            _ => Err(invalid("an integer (a number or a string of digits)")),
        },
        ParamType::Number => match value {
            Value::Number(n) => Ok(n.to_string()),
            Value::String(s) if s.trim().parse::<f64>().is_ok_and(f64::is_finite) => {
                Ok(s.trim().to_owned())
            }
            _ => Err(invalid("a number")),
        },
        ParamType::Boolean => match value {
            Value::Bool(b) => Ok(b.to_string()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Ok("true".to_owned()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Ok("false".to_owned()),
            _ => Err(invalid("a boolean")),
        },
        // Строки, а также элементы массивов: числа и логические значения принимаем как их
        // текстовую запись — ключи и ID в Tracker бывают и числовыми.
        ParamType::String | ParamType::Array => match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(invalid("a string")),
        },
        ParamType::Object | ParamType::File => Err(invalid("a string")),
    }
}

/// Значения query-параметра: массив передаётся повторением ключа.
fn query_values(param: &ParamDef, value: &Value) -> Result<Vec<String>, ToolError> {
    match (param.value_type, value) {
        (ParamType::Array, Value::Array(items)) => {
            items.iter().map(|item| scalar(param, item)).collect()
        }
        _ => scalar(param, value).map(|item| vec![item]),
    }
}

/// JSON-тело. Строка с JSON допускается: исполнитель разберёт её и отправит объектом
/// (некоторые клиенты передают тело строкой).
fn json_body(param: &ParamDef, value: &Value) -> Result<Value, ToolError> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    let ok = match (param.value_type, &parsed) {
        (ParamType::Object, Some(Value::Object(_))) => true,
        (ParamType::Array, Some(Value::Array(_))) => true,
        (ParamType::Object | ParamType::Array, _) => false,
        _ => parsed.is_some(),
    };
    if !ok {
        let expected = match param.value_type {
            ParamType::Array => "a JSON array",
            _ => "a JSON object",
        };
        return Err(ToolError::new(format!(
            "argument `{}` must be {expected} (the request body), got {}",
            param.name,
            kind(value)
        )));
    }
    Ok(value.clone())
}

fn form_part(
    param: &ParamDef,
    value: &Value,
    files: &FilePolicy,
) -> Result<MultipartPart, ToolError> {
    if param.value_type == ParamType::File {
        let Value::String(path) = value else {
            return Err(ToolError::new(format!(
                "argument `{}` must be a path to a local file, got {}",
                param.name,
                kind(value)
            )));
        };
        let path = files
            .check_read(path)
            .map_err(|err| ToolError::new(format!("argument `{}`: {err}", param.name)))?;
        return Ok(MultipartPart::File {
            name: param.name.to_owned(),
            source: FileSource::Path(path),
            file_name: None,
            content_type: None,
        });
    }
    Ok(MultipartPart::Text {
        name: param.name.to_owned(),
        value: scalar(param, value)?,
    })
}

fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tracker_api::HttpMethod;
    use tracker_api::spec::{find, operations};

    use super::*;

    fn op(name: &str) -> &'static OperationDef {
        find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn build(name: &str, value: Value) -> Result<ApiRequest, ToolError> {
        build_request(op(name), &args(value), &policy())
    }

    /// Первая операция с параметром в `location` типа `value_type`.
    fn op_with(
        location: ParamLocation,
        value_type: ParamType,
    ) -> (&'static OperationDef, &'static ParamDef) {
        operations()
            .iter()
            .find_map(|op| {
                op.params
                    .iter()
                    .find(|p| p.location == location && p.value_type == value_type)
                    .map(|p| (op, p))
            })
            .unwrap_or_else(|| panic!("нет параметра {location:?} {value_type:?}"))
    }

    #[test]
    fn path_and_query() {
        let request = build(
            "get_issue",
            json!({"issueId": "TEST-1", "expand": "attachments", "fields": null}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/v3/issues/TEST-1");
        assert_eq!(request.query, vec![("expand".into(), "attachments".into())]);
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());
    }

    #[test]
    fn path_values_are_single_segments() {
        let request = build("get_issue", json!({"issueId": "a/b c"})).unwrap();
        assert_eq!(request.path, "/v3/issues/a%2Fb%20c");
        let err = build("get_issue", json!({"issueId": ".."})).unwrap_err();
        assert!(err.message().contains("valid API path"), "{err}");
        let err = build("get_issue", json!({"issueId": ""})).unwrap_err();
        assert!(err.message().contains("missing path argument"), "{err}");
        let err = build("get_issue", json!({"issueId": {"a": 1}})).unwrap_err();
        assert!(
            err.message().contains("must be a string, got an object"),
            "{err}"
        );
    }

    #[test]
    fn missing_and_unknown_arguments() {
        let err = build("get_issue", json!({})).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `issueId`");
        // `null` — то же, что непереданный аргумент.
        let err = build("get_issue", json!({"issueId": null})).unwrap_err();
        assert!(err.message().contains("`issueId`"), "{err}");

        let err = build(
            "get_issue",
            json!({"issueId": "T-1", "issue_id": "x", "token": "t"}),
        )
        .unwrap_err();
        assert!(
            err.message()
                .starts_with("unknown argument(s) `issue_id`, `token`"),
            "{err}"
        );
        assert!(err.message().contains("`expand`"), "{err}");
        // `save_to` есть только у операций с двоичным ответом.
        let err = build("get_issue", json!({"issueId": "T-1", "save_to": "/tmp/x"})).unwrap_err();
        assert!(
            err.message().contains("unknown argument(s) `save_to`"),
            "{err}"
        );
    }

    #[test]
    fn scalar_types() {
        let (_, integer) = op_with(ParamLocation::Query, ParamType::Integer);
        assert_eq!(scalar(integer, &json!(5)).unwrap(), "5");
        assert_eq!(scalar(integer, &json!(" 7 ")).unwrap(), "7");
        for bad in [json!(1.5), json!("ten"), json!(true)] {
            let err = scalar(integer, &bad).unwrap_err();
            assert!(err.message().contains("must be an integer"), "{err}");
        }

        let (_, number) = op_with(ParamLocation::Query, ParamType::Number);
        assert_eq!(scalar(number, &json!(1.5)).unwrap(), "1.5");
        assert_eq!(scalar(number, &json!("2")).unwrap(), "2");
        assert!(scalar(number, &json!("NaN")).is_err());
        assert!(scalar(number, &json!([1])).is_err());

        let (_, boolean) = op_with(ParamLocation::Query, ParamType::Boolean);
        assert_eq!(scalar(boolean, &json!(true)).unwrap(), "true");
        assert_eq!(scalar(boolean, &json!("FALSE")).unwrap(), "false");
        assert!(scalar(boolean, &json!(1)).is_err());

        let (_, string) = op_with(ParamLocation::Query, ParamType::String);
        assert_eq!(scalar(string, &json!(42)).unwrap(), "42");
    }

    #[test]
    fn query_arrays_repeat_the_key() {
        let (op, param) = op_with(ParamLocation::Query, ParamType::Array);
        let mut value = serde_json::Map::new();
        for required in op.params.iter().filter(|p| p.required) {
            value.insert(required.name.to_owned(), json!("x"));
        }
        value.insert(param.name.to_owned(), json!(["a", 2]));
        let request = build_request(op, &value, &policy()).unwrap();
        let repeated: Vec<&str> = request
            .query
            .iter()
            .filter(|(k, _)| k == param.name)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(repeated, ["a", "2"]);
    }

    #[test]
    fn json_body() {
        let request = build(
            "create_issue",
            json!({"body": {"queue": "TEST", "summary": "S"}}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/v3/issues");
        assert!(
            matches!(request.body, RequestBody::Json(ref v) if v == &json!({"queue": "TEST", "summary": "S"}))
        );
        // Тело строкой с JSON допустимо, исполнитель отправит его объектом.
        let request = build("create_issue", json!({"body": "{\"queue\": \"TEST\"}"})).unwrap();
        assert!(matches!(request.body, RequestBody::Json(Value::String(_))));
        for bad in [json!("not json"), json!([1]), json!(5)] {
            let err = build("create_issue", json!({ "body": bad })).unwrap_err();
            assert!(err.message().contains("must be a JSON object"), "{err}");
        }
    }

    #[test]
    fn multipart_with_file_policy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("report.txt");
        std::fs::write(&file, b"report").unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();

        let request = build_request(
            op("post_attachment"),
            &args(json!({
                "issueId": "TEST-1",
                "filename": "renamed.txt",
                "file": file.display().to_string()
            })),
            &policy,
        )
        .unwrap();
        assert_eq!(request.path, "/v3/issues/TEST-1/attachments");
        assert_eq!(
            request.query,
            vec![("filename".into(), "renamed.txt".into())]
        );
        let RequestBody::Multipart(parts) = request.body else {
            panic!("ожидался multipart");
        };
        assert!(
            matches!(&parts[..], [MultipartPart::File { name, source: FileSource::Path(path), .. }]
                if name == "file" && path == &file),
            "{parts:?}"
        );

        // Существующий файл вне корня.
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("hosts");
        std::fs::write(&outside, b"x").unwrap();
        let outside = outside.display().to_string();
        let err = build_request(
            op("post_attachment"),
            &args(json!({"issueId": "TEST-1", "file": outside})),
            &policy,
        )
        .unwrap_err();
        assert!(
            err.message().contains(&format!(
                "argument `file`: `{outside}` is outside the allowed directories"
            )),
            "{err}"
        );
        let err = build_request(
            op("post_attachment"),
            &args(json!({"issueId": "TEST-1", "file": 1})),
            &policy,
        )
        .unwrap_err();
        assert!(
            err.message().contains("must be a path to a local file"),
            "{err}"
        );

        let err = build_request(
            op("post_attachment"),
            &args(json!({"issueId": "TEST-1", "file": file.display().to_string()})),
            &FilePolicy::disabled(),
        )
        .unwrap_err();
        assert!(err.message().contains("HTTP mode"), "{err}");
    }

    #[test]
    fn binary_response_needs_save_to_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let download = op("issues_get_attachment");
        assert!(takes_save_to(download));
        let base = json!({"issueId": "TEST-1", "fileId": "4", "fileName": "a.png"});

        let err = build_request(download, &args(base.clone()), &policy).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `save_to`");

        let mut with_target = args(base.clone());
        let target = root.join("a.png");
        with_target.insert(SAVE_TO_ARG.into(), json!(target.display().to_string()));
        let request = build_request(download, &with_target, &policy).unwrap();
        assert_eq!(request.path, "/v3/issues/TEST-1/attachments/4/a.png");
        assert_eq!(request.save_to, Some(target.clone()));

        // Существующий файл не перезаписывается, каталог вне корней отклоняется.
        std::fs::write(&target, b"old").unwrap();
        let err = build_request(download, &with_target, &policy).unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        let other = tempfile::tempdir().unwrap();
        let mut outside = args(base);
        outside.insert(
            SAVE_TO_ARG.into(),
            json!(other.path().join("x.png").display().to_string()),
        );
        let err = build_request(download, &outside, &policy).unwrap_err();
        assert!(err.message().contains("argument `save_to`"), "{err}");
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
    }

    #[test]
    fn no_operation_parameter_clashes_with_save_to() {
        for op in operations() {
            assert!(op.param(SAVE_TO_ARG).is_none(), "{}", op.tool_name);
        }
    }
}
