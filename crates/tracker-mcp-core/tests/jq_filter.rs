//! Фильтр ответа `jq`: инструменты операций и `api_request`, постраничная выдача, вырезание
//! учётных данных до фильтра, ошибки до и после запроса. Tracker API подменён `wiremock`.

use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use tracker_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use tracker_api::{AccessLevel, ApiToken};
use tracker_mcp_core::{FilePolicy, ServerOptions, TrackerServer};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";

fn server(api: &MockServer, max_access: AccessLevel, max_response_bytes: usize) -> TrackerServer {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new(TEST_TOKEN))),
        Some(OrgId::Cloud("test-org".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.max_response_bytes = max_response_bytes;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    TrackerServer::new(
        ApiClient::new(config).unwrap(),
        ServerOptions {
            max_access,
            toolsets: Some(vec!["all".into()]),
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap()
}

fn args(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call(server: &TrackerServer, name: &str, value: Value) -> CallToolResult {
    server.call(name, args(value)).await.unwrap()
}

fn issues() -> Value {
    json!([
        {"key": "TEST-1", "summary": "Bug", "description": "long text", "status": {"key": "open"}},
        {"key": "TEST-2", "summary": "Task", "description": "more text", "status": {"key": "closed"}}
    ])
}

/// Главный сценарий: `.[] | {key, summary}` на поиске задач возвращает только эти поля, а
/// заголовки постраничной выдачи остаются рядом с результатом.
#[tokio::test]
async fn filters_issue_search_and_keeps_pagination() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .and(query_param("perPage", "2"))
        .and(body_json(json!({"query": "Queue: TEST"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(issues())
                .insert_header("X-Total-Pages", "3")
                .insert_header("X-Total-Count", "6"),
        )
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    let tool = server.registry().get("search_issues").unwrap().tool.clone();
    assert!(tool.input_schema["properties"].get("jq").is_some());

    let result = call(
        &server,
        "search_issues",
        json!({
            "body": {"query": "Queue: TEST"},
            "perPage": 2,
            "jq": ".[] | {key, summary}"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(
        value["result"],
        json!([
            {"key": "TEST-1", "summary": "Bug"},
            {"key": "TEST-2", "summary": "Task"}
        ])
    );
    assert_eq!(value["pagination"]["total_pages"], 3);
    assert_eq!(value["pagination"]["total_count"], 6);
}

#[tokio::test]
async fn filters_plain_responses_and_api_request() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/issues/TEST-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues()[0].clone()))
        .expect(3)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);

    let result = call(
        &server,
        "get_issue",
        json!({"issueId": "TEST-1", "jq": "{key, status: .status.key}"}),
    )
    .await;
    assert_eq!(text(&result), r#"{"key":"TEST-1","status":"open"}"#);

    // Ни одного результата — пустой массив, `null` — как отсутствие фильтра.
    let result = call(
        &server,
        "get_issue",
        json!({"issueId": "TEST-1", "jq": "select(.key == \"NOPE\")"}),
    )
    .await;
    assert_eq!(text(&result), "[]");

    let result = call(
        &server,
        "api_request",
        json!({"method": "GET", "path": "/v3/issues/TEST-1", "jq": ".summary"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#""Bug""#);
    let schema = server.registry().get("api_request").unwrap().tool.clone();
    assert!(schema.input_schema["properties"].get("jq").is_some());
}

/// Сломанный фильтр и фильтр, читающий окружение, отклоняются до запроса к API.
#[tokio::test]
async fn broken_filter_is_reported_before_the_request() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"key": "TEST-3"})))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, 64 * 1024);
    for (filter, needle) in [
        (json!(".key |"), "syntax error"),
        (json!("env"), "undefined filter `env/0`"),
        (json!("$ENV"), "undefined variable `$ENV`"),
        (json!("input"), "undefined filter `input/0`"),
        (json!(1), "must be a string"),
    ] {
        let result = call(
            &server,
            "create_issue",
            json!({"body": {"queue": "TEST", "summary": "x"}, "jq": filter}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains(needle),
            "{needle}: {}",
            text(&result)
        );
    }
    let result = call(
        &server,
        "api_request",
        json!({"method": "POST", "path": "/v3/issues", "body": {}, "jq": "{"}),
    )
    .await;
    assert!(text(&result).contains("argument `jq`"), "{}", text(&result));
}

/// Ошибка выполнения фильтра после записи: агент узнаёт, что запрос выполнен.
#[tokio::test]
async fn runtime_error_says_the_request_succeeded() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"key": "TEST-3"})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, 64 * 1024);
    let result = call(
        &server,
        "create_issue",
        json!({"body": {"queue": "TEST", "summary": "x"}, "jq": ".key[]"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("the API request succeeded (HTTP 201)"),
        "{message}"
    );
    assert!(message.contains("`jq` filter failed"), "{message}");
    assert!(message.contains("apply the change again"), "{message}");
}

/// Лимит ответа действует на результат фильтра: большой ответ можно сузить фильтром, а
/// слишком большой результат — ошибка с подсказкой.
#[tokio::test]
async fn size_limit_applies_to_the_filtered_result() {
    let api = MockServer::start().await;
    let big: Vec<Value> = (0..200)
        .map(|i| json!({"key": format!("TEST-{i}"), "description": "x".repeat(100)}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v3/queues/TEST/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(big)))
        .mount(&api)
        .await;
    // Ответ около 25 КБ при лимите 4 КБ.
    let server = server(&api, AccessLevel::Read, 4 * 1024);
    let request = json!({"method": "GET", "path": "/v3/queues/TEST/issues"});

    let unfiltered = call(&server, "api_request", request.clone()).await;
    assert_eq!(unfiltered.is_error, Some(true), "{}", text(&unfiltered));

    let mut narrowed = request.clone();
    narrowed["jq"] = json!("[.[] | .key] | length");
    let result = call(&server, "api_request", narrowed).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), "200");

    let mut too_big = request;
    too_big["jq"] = json!(".[] | .description");
    let result = call(&server, "api_request", too_big).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("larger than 4096 bytes"), "{message}");
    assert!(message.contains("HTTP 200"), "{message}");
}

/// Фильтр видит только очищенный ответ: ни поиском строк, ни преобразованием нельзя
/// достать вырезанные значения и токен клиента.
#[tokio::test]
async fn filter_sees_only_redacted_values() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/webhooks/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "password": "super-secret-password",
            "echo": format!("token is {TEST_TOKEN}"),
            "endpoint": "https://user:pa55word@hooks.example.com/x"
        })))
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    for filter in ["[.. | strings]", "[.. | strings | @base64]", "tojson"] {
        let result = call(
            &server,
            "api_request",
            json!({"method": "GET", "path": "/v3/webhooks/1", "jq": filter}),
        )
        .await;
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        let out = text(&result);
        for secret in ["super-secret-password", TEST_TOKEN, "pa55word"] {
            assert!(!out.contains(secret), "{filter}: {out}");
            let encoded = base64_encode(secret);
            assert!(!out.contains(&encoded), "{filter}: {out}");
        }
        assert!(out.contains("[redacted]"), "{filter}: {out}");
    }
}

/// Не-JSON ответ возвращается без фильтра, с пояснением.
#[tokio::test]
async fn non_json_response_is_returned_unfiltered() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/issues/TEST-1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("plain text", "text/plain"))
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    let result = call(
        &server,
        "api_request",
        json!({"method": "GET", "path": "/v3/issues/TEST-1/comments", "jq": ".a"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let out = text(&result);
    assert!(out.starts_with("plain text"), "{out}");
    assert!(out.contains("filter was not applied"), "{out}");
}

/// Операции с двоичным ответом сохраняют файл: аргумента `jq` у них нет.
#[test]
fn binary_downloads_have_no_jq() {
    let api_uri = "http://127.0.0.1:9";
    let mut config = ClientConfig::new(None, None).unwrap();
    config.base_url = api_uri.parse().unwrap();
    let server = TrackerServer::new(
        ApiClient::new(config).unwrap(),
        ServerOptions {
            toolsets: Some(vec!["issues".into()]),
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap();
    let registry = server.registry();
    let download = &registry.get("issues_get_attachment").unwrap().tool;
    assert!(download.input_schema["properties"].get("jq").is_none());
    assert!(download.input_schema["properties"].get("save_to").is_some());
}

/// Простой base64 без зависимостей: проверяем, что фильтр не вернул закодированный секрет.
fn base64_encode(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
