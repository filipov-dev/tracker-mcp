//! Пользовательские инструменты из `tools.toml`: видны агенту в `tools/list`, отправляют
//! фиксированные аргументы, применяют свой фильтр `jq`, не регистрируются выше уровня
//! доступа. Tracker API подменён `wiremock`.

use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use serde_json::{Value, json};
use tracker_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use tracker_api::{AccessLevel, ApiToken};
use tracker_mcp_core::{CustomTool, CustomToolsFile, FilePolicy, ServerOptions, TrackerServer};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Те же поля, что в `tools.toml` (разбор TOML проверяют тесты бинарника).
fn custom_tools() -> Vec<CustomTool> {
    let file: CustomToolsFile = serde_json::from_value(json!({
        "tools": [
            {
                "name": "my_open_issues",
                "description": "Open issues assigned to me",
                "operation": "search_issues",
                "jq": ".[] | {key, summary}",
                "args": {
                    "perPage": 2,
                    "body": {"query": "Assignee: me() Resolution: empty()"}
                }
            },
            {
                "name": "comment_test_1",
                "description": "Comment on TEST-1",
                "operation": "issues_add_comment",
                "args": {"issueId": "TEST-1"}
            }
        ]
    }))
    .unwrap();
    file.tools
        .into_iter()
        .map(|def| CustomTool::compile(def, "tools.toml").unwrap())
        .collect()
}

fn server(api: &MockServer, max_access: AccessLevel) -> TrackerServer {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new("test-token-value"))),
        Some(OrgId::Yandex360("12345".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let client = ApiClient::new(config).unwrap();
    let options = ServerOptions {
        max_access,
        custom_tools: custom_tools(),
        ..ServerOptions::default()
    };
    TrackerServer::new(client, options, FilePolicy::new(&[]).unwrap()).unwrap()
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

#[tokio::test]
async fn custom_tool_is_listed_and_calls_operation() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .and(query_param("perPage", "2"))
        .and(body_json(
            json!({"query": "Assignee: me() Resolution: empty()"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "2")
                .insert_header("X-Total-Pages", "1")
                .set_body_json(json!([
                    {"key": "TEST-1", "summary": "One", "description": "long"},
                    {"key": "TEST-2", "summary": "Two", "description": "long"}
                ])),
        )
        .expect(1)
        .mount(&api)
        .await;

    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let server = server(&api, AccessLevel::Read);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "my_open_issues")
        .expect("пользовательский инструмент в tools/list");
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.starts_with("Open issues assigned to me\n"),
        "{description}"
    );
    assert!(description.contains("`search_issues`"), "{description}");
    let properties = tool.input_schema.get("properties").unwrap();
    // Фиксированные аргументы убраны из схемы, дополнительный фильтр остаётся.
    assert!(properties.get("perPage").is_none(), "{properties}");
    assert!(properties.get("body").is_none(), "{properties}");
    assert!(properties.get("jq").is_some(), "{properties}");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    // Инструмент записи при уровне «чтение» не зарегистрирован.
    assert!(!tools.iter().any(|t| t.name == "comment_test_1"));
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains("Custom tools"), "{instructions}");

    let result = client
        .call_tool(CallToolRequestParams::new("my_open_issues").with_arguments(JsonObject::new()))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(
        value["result"],
        json!([
            {"key": "TEST-1", "summary": "One"},
            {"key": "TEST-2", "summary": "Two"}
        ])
    );
    assert_eq!(value["pagination"]["total_count"], 2);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn agent_filter_is_applied_after_tool_filter() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"key": "TEST-1", "summary": "One"},
            {"key": "TEST-2", "summary": "Two"}
        ])))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read);
    let result = server
        .call("my_open_issues", args(json!({"jq": ".key"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"["TEST-1","TEST-2"]"#);
}

#[tokio::test]
async fn fixed_arguments_cannot_be_overridden() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read);
    let result = server
        .call("my_open_issues", args(json!({"perPage": 100})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("perPage"), "{}", text(&result));
}

#[tokio::test]
async fn write_tool_needs_write_access() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/TEST-1/comments"))
        .and(body_json(json!({"text": "Done"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1, "text": "Done"})))
        .expect(1)
        .mount(&api)
        .await;

    let read_only = server(&api, AccessLevel::Read);
    assert!(read_only.registry().get("comment_test_1").is_none());
    let result = read_only
        .call("comment_test_1", args(json!({"body": {"text": "Done"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("allow `write` access"),
        "{}",
        text(&result)
    );

    let writer = server(&api, AccessLevel::Write);
    let registry = writer.registry();
    let tool = registry.get("comment_test_1").unwrap();
    assert_eq!(
        tool.tool.annotations.as_ref().unwrap().read_only_hint,
        Some(false)
    );
    let result = writer
        .call("comment_test_1", args(json!({"body": {"text": "Done"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
}
