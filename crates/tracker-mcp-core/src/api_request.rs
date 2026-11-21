//! Инструмент `api_request`: запрос к произвольному пути Tracker API.
//!
//! Это запасной выход для операций, которых нет среди сгенерированных инструментов или
//! которые отключены наборами (`--toolsets` его не ограничивают). Границы безопасности
//! те же, что у остальных инструментов:
//!
//! - путь только относительный, `/v2/…` или `/v3/…`, и проходит `validate_relative_path`;
//!   query передаётся отдельно, заголовки задать нельзя, поэтому `Authorization`,
//!   `X-Org-ID` и `X-Cloud-Org-ID` не переопределяются, а запрос не уходит на другой хост;
//! - требуемый уровень доступа определяется сопоставлением с операциями ([`crate::routes`]),
//!   а без совпадения — по методу; выше `max_access` запрос отклоняется до отправки;
//! - `save_to` принимается только для операций с двоичным ответом (скачивание файлов) и
//!   проходит [`FilePolicy`]. JSON-ответ в файл не сохраняется: там вырезание учётных данных
//!   не применялось бы, а агент мог бы прочитать файл.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use tracker_api::executor::{ApiRequest, RequestBody, validate_relative_path};
use tracker_api::spec::{AccessLevel, HttpMethod, OperationDef, ResponseKind};

use crate::error::ToolError;
use crate::files::{FilePolicy, LOCAL_FILES_DISABLED};
use crate::jq::add_jq_argument;
use crate::registry::{access_hint, access_name};
use crate::request::SAVE_TO_ARG;
use crate::routes::{AccessDecision, default_access, required_access};

/// Имя инструмента.
pub const API_REQUEST_TOOL: &str = "api_request";

const ARGUMENTS: &[&str] = &["method", "path", "query", "body", SAVE_TO_ARG];

const METHODS: [HttpMethod; 5] = [
    HttpMethod::Get,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
];

/// Подготовленный запрос и самая точная совпавшая операция.
#[derive(Debug)]
pub struct PreparedRequest {
    pub request: ApiRequest,
    pub operation: Option<&'static OperationDef>,
    /// Уровень доступа, который нужен запросу.
    pub access: AccessLevel,
}

/// Описание инструмента; аннотации соответствуют максимальному уровню доступа сервера.
///
/// Без локальных файлов (`local_files == false`, HTTP-режим) аргумента `save_to` в схеме нет.
pub fn api_request_tool(max_access: AccessLevel, local_files: bool) -> Tool {
    let mut properties = json!({
        "method": {
            "type": "string",
            "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
            "description": "HTTP method."
        },
        "path": {
            "type": "string",
            "description": "API path with the version, starting with `/v3/` (or `/v2/` for \
                methods that exist only there), e.g. `/v3/issues/TEST-1/comments`. Full URLs, \
                `?` and `#` are rejected: pass query parameters in `query`. Percent-encode \
                special characters inside a segment."
        },
        "query": {
            "type": "object",
            "description": "Query parameters. Values may be strings, numbers, booleans or \
                arrays of them; an array repeats the key (`key=a&key=b`).",
            "additionalProperties": {
                "anyOf": [
                    {"type": ["string", "number", "boolean"]},
                    {"type": "array", "items": {"type": ["string", "number", "boolean"]}}
                ]
            }
        },
        "body": {
            "description": "JSON request body (any JSON value), sent as application/json. \
                Not allowed with GET."
        }
    });
    if local_files && let Value::Object(map) = &mut properties {
        map.insert(
            SAVE_TO_ARG.to_owned(),
            json!({
                "type": "string",
                "description": "For file downloads only (operations that return a file, such \
                    as attachment content): path of a new local file to save the content to \
                    (absolute, or relative to the server's working directory) inside the \
                    directories allowed by the server. Existing files are never overwritten."
            }),
        );
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "required": ["method", "path"],
        "additionalProperties": false
    });
    let mut schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    add_jq_argument(&mut schema);

    let destructive_hint = access_hint(AccessLevel::Destructive, local_files);
    let allowed = match max_access {
        AccessLevel::Read => {
            "This server is read-only, so only requests that read data are allowed (GET and \
             read-only operations such as POST /v3/issues/_search); others are refused with \
             what the user would need to allow."
                .to_owned()
        }
        AccessLevel::Write => format!(
            "This server allows reading and changing data; deleting (and other destructive \
             operations) needs `destructive` access: ask the user to {destructive_hint}."
        ),
        AccessLevel::Destructive => {
            "This server allows reading, changing and deleting data; confirm destructive \
             requests with the user."
                .to_owned()
        }
    };
    let description = format!(
        "Send a request to any Yandex Tracker REST API path. Use it only when no dedicated tool \
         covers the operation (or its toolset is not enabled): dedicated tools validate \
         arguments and document the schema. Use `api_search` first to find the operation, its \
         dedicated tool and its reference page.\n\n\
         The path includes the API version (`/v3/...`, `/v2/...`). Headers cannot be set: the \
         token and the organization are added by the server.\n\n\
         Access: a request that matches a known API operation needs that operation's access \
         level; otherwise GET is read, POST/PUT/PATCH are write and DELETE is destructive. \
         {allowed} Toolsets do not restrict this tool.\n\n\
         The response is returned like other tools' responses (paging headers in \
         `pagination`, credentials redacted); pass `jq` to filter the JSON body."
    );

    let base = ToolAnnotations::with_title("Custom Tracker API request").open_world(true);
    let annotations = match max_access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => base.read_only(false).destructive(false),
        AccessLevel::Destructive => base.read_only(false).destructive(true),
    };
    let mut tool = Tool::new(API_REQUEST_TOOL, description, Arc::new(schema));
    tool.title = Some("Custom Tracker API request".to_owned());
    tool.annotations = Some(annotations);
    tool
}

/// Проверяет аргументы, уровень доступа и файлы и собирает запрос.
pub fn prepare(
    args: &JsonObject,
    max_access: AccessLevel,
    files: &FilePolicy,
) -> Result<PreparedRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());
    reject_unknown(args)?;
    if files.is_disabled() && present(SAVE_TO_ARG) {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}`: {LOCAL_FILES_DISABLED}"
        )));
    }

    let method = parse_method(args.get("method"))?;
    let path = parse_path(args.get("path"))?;

    // Уровень доступа проверяется раньше всего остального: в отказе не раскрываются
    // подробности о локальных файлах.
    let decision = required_access(method, &path);
    check_access(method, &path, &decision, max_access, !files.is_disabled())?;

    if method == HttpMethod::Get && present("body") {
        return Err(ToolError::new(
            "GET requests cannot have a body; pass parameters in `query`",
        ));
    }
    let query = parse_query(args.get("query"))?;
    let body = match args.get("body").filter(|value| !value.is_null()) {
        Some(value) => RequestBody::Json(value.clone()),
        None => RequestBody::Empty,
    };

    let save_to = match args.get(SAVE_TO_ARG).filter(|value| !value.is_null()) {
        Some(value) => {
            check_download(method, &path, &decision)?;
            let target = string_arg(SAVE_TO_ARG, value)?;
            Some(
                files
                    .check_write(target)
                    .map_err(|err| ToolError::new(format!("argument `{SAVE_TO_ARG}`: {err}")))?,
            )
        }
        None => None,
    };

    Ok(PreparedRequest {
        operation: decision.operation(),
        access: decision.required,
        request: ApiRequest {
            method,
            path,
            query,
            body,
            save_to,
        },
    })
}

/// `save_to` — только для операций, которые возвращают файл.
fn check_download(
    method: HttpMethod,
    path: &str,
    decision: &AccessDecision,
) -> Result<(), ToolError> {
    if decision
        .matches
        .iter()
        .any(|m| m.op.response == ResponseKind::Binary)
    {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "`{SAVE_TO_ARG}` is only for file downloads, and {} {path} matches no API operation \
         that returns a file. Call it without `{SAVE_TO_ARG}`: the JSON response is returned \
         in the result",
        method.as_str()
    )))
}

/// Уровень доступа запроса против уровня сервера.
fn check_access(
    method: HttpMethod,
    path: &str,
    decision: &AccessDecision,
    max_access: AccessLevel,
    local_machine: bool,
) -> Result<(), ToolError> {
    if max_access.allows(decision.required) {
        return Ok(());
    }
    let reason = match decision.strictest() {
        Some(op) => format!(
            "it matches the `{}` operation ({})",
            op.tool_name,
            op.summary.trim()
        ),
        None => format!(
            "{} requests to paths that match no known operation default to {} access",
            method.as_str(),
            access_name(default_access(method))
        ),
    };
    Err(ToolError::new(format!(
        "{} {path} needs {} access ({reason}), but the server runs with {} access. Ask the user \
         to {} if this change is intended",
        method.as_str(),
        access_name(decision.required),
        access_name(max_access),
        access_hint(decision.required, local_machine),
    )))
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGUMENTS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed: Vec<String> = ARGUMENTS.iter().map(|name| format!("`{name}`")).collect();
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {} (headers cannot be set: the token and \
         the organization are added by the server)",
        unknown.join(", "),
        allowed.join(", ")
    )))
}

fn parse_method(value: Option<&Value>) -> Result<HttpMethod, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `method`"));
    };
    let method = string_arg("method", value)?.trim();
    METHODS
        .into_iter()
        .find(|candidate| candidate.as_str().eq_ignore_ascii_case(method))
        .ok_or_else(|| {
            ToolError::new(format!(
                "argument `method` must be one of GET, POST, PUT, PATCH, DELETE; got `{method}`"
            ))
        })
}

fn parse_path(value: Option<&Value>) -> Result<String, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `path`"));
    };
    let path = string_arg("path", value)?.trim();
    if path.contains("://") || path.starts_with("//") {
        return Err(ToolError::new(
            "argument `path` must be a path of the Tracker API, e.g. `/v3/issues/TEST-1`; full \
             URLs are not accepted",
        ));
    }
    if path.contains(['?', '#']) {
        return Err(ToolError::new(
            "argument `path` must not contain `?` or `#`; pass query parameters in `query`",
        ));
    }
    let versioned = tracker_api::path::API_VERSIONS.iter().any(|version| {
        path.strip_prefix('/')
            .and_then(|rest| rest.strip_prefix(version))
            .is_some_and(|rest| rest.starts_with('/'))
    });
    if !versioned {
        return Err(ToolError::new(
            "argument `path` must start with the API version, `/v3/` or `/v2/`, e.g. \
             `/v3/issues/TEST-1`",
        ));
    }
    validate_relative_path(path).map_err(|_| {
        ToolError::new(
            "argument `path` is not a valid API path: name a resource after the version, use \
             only URL path characters (percent-encode others as %XX), and no empty, `.` or `..` \
             segments",
        )
    })?;
    Ok(path.to_owned())
}

fn parse_query(value: Option<&Value>) -> Result<Vec<(String, String)>, ToolError> {
    let map = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(ToolError::new(
                "argument `query` must be an object of parameter names to values",
            ));
        }
    };
    let mut query = Vec::new();
    for (key, value) in map {
        if key.trim().is_empty() {
            return Err(ToolError::new(
                "argument `query` has an empty parameter name",
            ));
        }
        match value {
            Value::Null => {}
            Value::Array(items) => {
                for item in items {
                    query.push((key.clone(), query_scalar(key, item)?));
                }
            }
            other => query.push((key.clone(), query_scalar(key, other)?)),
        }
    }
    Ok(query)
}

fn query_scalar(key: &str, value: &Value) -> Result<String, ToolError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ToolError::new(format!(
            "query parameter `{key}` must be a string, number, boolean or an array of them"
        ))),
    }
}

fn string_arg<'a>(name: &str, value: &'a Value) -> Result<&'a str, ToolError> {
    value
        .as_str()
        .ok_or_else(|| ToolError::new(format!("argument `{name}` must be a string")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn prep(value: Value, max_access: AccessLevel) -> Result<PreparedRequest, ToolError> {
        prepare(&args(value), max_access, &policy())
    }

    fn err(value: Value, max_access: AccessLevel) -> String {
        prep(value, max_access).unwrap_err().message().to_owned()
    }

    #[test]
    fn get_with_query() {
        let prepared = prep(
            json!({
                "method": "get",
                "path": "/v3/issues/TEST-1/comments",
                "query": {"perPage": 5, "expand": ["attachments", "html"], "flag": true, "id": null}
            }),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(prepared.access, AccessLevel::Read);
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("get_comments")
        );
        let request = prepared.request;
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/v3/issues/TEST-1/comments");
        let mut query = request.query.clone();
        query.sort();
        assert_eq!(
            query,
            vec![
                ("expand".into(), "attachments".into()),
                ("expand".into(), "html".into()),
                ("flag".into(), "true".into()),
                ("perPage".into(), "5".into()),
            ]
        );
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());
    }

    #[test]
    fn unknown_path_has_no_operation() {
        let prepared = prep(
            json!({"method": "GET", "path": "/v2/some/new/endpoint"}),
            AccessLevel::Read,
        )
        .unwrap();
        assert!(prepared.operation.is_none());
        assert_eq!(prepared.access, AccessLevel::Read);
    }

    #[test]
    fn validation_errors() {
        let read = AccessLevel::Read;
        assert!(err(json!({"path": "/v3/myself"}), read).contains("`method`"));
        assert!(err(json!({"method": "GET"}), read).contains("`path`"));
        assert!(
            err(json!({"method": "HEAD", "path": "/v3/myself"}), read).contains("must be one of")
        );
        assert!(err(json!({"method": 1, "path": "/v3/myself"}), read).contains("must be a string"));
        for url in [
            "https://evil.example/v3/myself",
            "//evil.example/v3/myself",
            "/v3/http://evil.example",
        ] {
            assert!(
                err(json!({"method": "GET", "path": url}), read).contains("full URLs"),
                "{url}"
            );
        }
        assert!(err(json!({"method": "GET", "path": "/v3/myself?x=1"}), read).contains("`query`"));
        assert!(err(json!({"method": "GET", "path": "/v3/myself#x"}), read).contains("`query`"));
        for path in ["v3/myself", "/myself", "/v1/myself", "/v3", "/v3x/myself"] {
            assert!(
                err(json!({"method": "GET", "path": path}), read).contains("API version"),
                "{path}"
            );
        }
        for path in [
            "/v3/issues/../myself",
            "/v3/issues/%2e%2e/myself",
            "/v3/a b",
            "/v3/",
            "/v3//issues",
        ] {
            assert!(
                err(json!({"method": "GET", "path": path}), read).contains("valid API path"),
                "{path}"
            );
        }

        let unknown = err(
            json!({"method": "GET", "path": "/v3/myself", "headers": {"Authorization": "x"}}),
            read,
        );
        assert!(
            unknown.contains("unknown argument(s) `headers`"),
            "{unknown}"
        );
        assert!(unknown.contains("headers cannot be set"), "{unknown}");
        let unknown = err(
            json!({"method": "GET", "path": "/v3/myself", "X-Org-ID": "1"}),
            read,
        );
        assert!(unknown.contains("`X-Org-ID`"), "{unknown}");

        assert!(
            err(
                json!({"method": "GET", "path": "/v3/myself", "query": "a=1"}),
                read
            )
            .contains("must be an object")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/v3/myself", "query": {"a": {"b": 1}}}),
                read
            )
            .contains("query parameter `a`")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/v3/myself", "query": {"a": [[1]]}}),
                read
            )
            .contains("query parameter `a`")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/v3/myself", "body": {}}),
                read
            )
            .contains("GET requests cannot have a body")
        );
    }

    #[test]
    fn read_only_server_refuses_writes_and_deletes() {
        let read = AccessLevel::Read;
        let message = err(
            json!({"method": "POST", "path": "/v3/issues", "body": {"queue": "TEST"}}),
            read,
        );
        assert!(message.contains("needs write access"), "{message}");
        assert!(message.contains("`create_issue`"), "{message}");
        assert!(message.contains("read-only access"), "{message}");
        assert!(message.contains("allow `write` access"), "{message}");
        assert!(message.contains("tracker-mcp ui"), "{message}");

        let message = err(json!({"method": "DELETE", "path": "/v3/queues/TEST"}), read);
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("`delete_queue`"), "{message}");
        assert!(message.contains("allow `destructive` access"), "{message}");

        // Неизвестный путь — по методу.
        let message = err(json!({"method": "PATCH", "path": "/v3/unknown/1"}), read);
        assert!(message.contains("match no known operation"), "{message}");
        assert!(message.contains("allow `write` access"), "{message}");
        let message = err(
            json!({"method": "DELETE", "path": "/v3/unknown/1"}),
            AccessLevel::Write,
        );
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("allow `destructive` access"), "{message}");

        // Отказ по доступу раньше проверки файлов: путь вне корней не раскрывается.
        let message = err(
            json!({"method": "DELETE", "path": "/v3/queues/TEST", "save_to": "/etc/x"}),
            read,
        );
        assert!(message.contains("allow `destructive` access"), "{message}");

        // HTTP-режим: уровень задают флаги сервера.
        let message = prepare(
            &args(json!({"method": "DELETE", "path": "/v3/queues/TEST"})),
            read,
            &FilePolicy::disabled(),
        )
        .unwrap_err()
        .message()
        .to_owned();
        assert!(message.contains("--allow-destructive"), "{message}");

        // Поиск через POST — чтение.
        for path in [
            "/v3/issues/_search",
            "/v3/issues/_count",
            "/v2/issues/_search",
        ] {
            let prepared = prep(
                json!({"method": "POST", "path": path, "body": {"queue": "TEST"}}),
                read,
            )
            .unwrap();
            assert_eq!(prepared.access, AccessLevel::Read, "{path}");
            assert!(matches!(prepared.request.body, RequestBody::Json(_)));
        }

        // С правами записи — можно, удаление — нет.
        assert!(
            prep(
                json!({"method": "POST", "path": "/v3/issues/TEST-1/worklog", "body": {"duration": "PT1H"}}),
                AccessLevel::Write
            )
            .is_ok()
        );
        assert!(
            prep(
                json!({"method": "DELETE", "path": "/v3/issues/TEST-1/worklog/5"}),
                AccessLevel::Destructive
            )
            .is_ok()
        );
    }

    #[test]
    fn save_to_only_for_downloads_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let target = root.join("a.png");

        let prepared = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v3/issues/TEST-1/attachments/4/a.png",
                "save_to": target.display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap();
        assert_eq!(prepared.request.save_to, Some(target.clone()));
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("issues_get_attachment")
        );

        // JSON-ответ в файл не сохраняется.
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v3/queues/TEST/triggers",
                "save_to": root.join("t.json").display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message.message().contains("only for file downloads"),
            "{message}"
        );
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v3/unknown/file",
                "save_to": root.join("u.bin").display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message.message().contains("only for file downloads"),
            "{message}"
        );

        // Каталог вне корней.
        let other = tempfile::tempdir().unwrap();
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v3/issues/TEST-1/thumbnails/4",
                "save_to": other.path().join("x.png").display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message.message().contains("argument `save_to`")
                && message
                    .message()
                    .contains("outside the allowed directories"),
            "{message}"
        );
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v3/issues/TEST-1/thumbnails/4",
                "save_to": 5
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(message.message().contains("must be a string"), "{message}");
    }

    #[test]
    fn tool_annotations_follow_max_access() {
        let read = api_request_tool(AccessLevel::Read, true);
        let a = read.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(a.destructive_hint, None);
        let write = api_request_tool(AccessLevel::Write, true);
        let a = write.annotations.as_ref().unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(false))
        );
        let all = api_request_tool(AccessLevel::Destructive, true);
        let a = all.annotations.as_ref().unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(true))
        );
        let description = read.description.unwrap_or_default();
        assert!(description.contains("api_search"));
        assert!(description.contains("Toolsets do not restrict"));
        assert!(description.contains("read-only"));
        assert_eq!(read.input_schema["additionalProperties"], false);
        assert!(read.input_schema["properties"].get(SAVE_TO_ARG).is_some());
    }

    #[test]
    fn save_to_is_rejected_without_local_files() {
        let files = FilePolicy::disabled();
        let request = args(json!({
            "method": "GET",
            "path": "/v3/issues/TEST-1/attachments/4/a.png",
            "save_to": "a.png"
        }));
        let err = prepare(&request, AccessLevel::Destructive, &files).unwrap_err();
        assert!(err.message().contains("HTTP mode"), "{err}");
        let request = args(json!({"method": "GET", "path": "/v3/myself"}));
        assert!(prepare(&request, AccessLevel::Read, &files).is_ok());

        let tool = api_request_tool(AccessLevel::Read, false);
        let schema = Value::Object((*tool.input_schema).clone()).to_string();
        assert!(!schema.contains(SAVE_TO_ARG), "{schema}");
        assert!(
            !tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("local file")
        );
    }
}
