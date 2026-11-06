//! Сквозная проверка: сервер в процессе, клиент `rmcp` через in-memory транспорт,
//! Tracker API подменён `wiremock`. Реальный API, keychain и сеть не используются.

use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tracker_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use tracker_api::{AccessLevel, ApiToken};
use tracker_mcp_core::{FilePolicy, ServerOptions, TrackerServer};
use wiremock::matchers::{body_json, header, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";
const TEST_ORG: &str = "test-org";

fn client(api: &MockServer) -> ApiClient {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new(TEST_TOKEN))),
        Some(OrgId::Cloud(TEST_ORG.into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

async fn start(
    api: &MockServer,
    options: ServerOptions,
    files: FilePolicy,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let server = TrackerServer::new(client(api), options, files).unwrap();
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let handle = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();
    (client, handle)
}

fn options(max_access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access,
        toolsets: Some(vec!["all".into()]),
        ..ServerOptions::default()
    }
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

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(value)))
        .await
        .unwrap()
}

#[tokio::test]
async fn read_only_server_end_to_end() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/issues/TEST-1"))
        .and(query_param("expand", "attachments"))
        .and(header(
            "authorization",
            format!("OAuth {TEST_TOKEN}").as_str(),
        ))
        .and(header("x-cloud-org-id", TEST_ORG))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "TEST-1",
            "summary": "Bug"
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/issues/MISSING-1"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errors": {},
            "errorMessages": ["Задача не существует."],
            "statusCode": 404
        })))
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "tracker-mcp");
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("read-only"), "{instructions}");

    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "get_issue"));
    assert!(tools.iter().all(|t| {
        t.annotations
            .as_ref()
            .is_some_and(|a| a.read_only_hint == Some(true))
    }));
    assert!(!tools.iter().any(|t| t.name == "create_issue"));
    assert!(!tools.iter().any(|t| t.name == "delete_queue"));

    // Успешный вызов: путь, query, Authorization и организацию проверяет mock.
    let result = call(
        &client,
        "get_issue",
        json!({"issueId": "TEST-1", "expand": "attachments"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"key":"TEST-1","summary":"Bug"}"#);

    // 404 Tracker — управляемая ошибка, не ошибка протокола.
    let result = call(&client, "get_issue", json!({"issueId": "MISSING-1"})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 404"), "{message}");
    assert!(message.contains("Задача не существует."), "{message}");
    assert!(!message.contains(TEST_TOKEN));

    // Инструмент записи не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = call(
        &client,
        "create_issue",
        json!({"body": {"queue": "TEST", "summary": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    // Ошибка аргументов не доходит до API.
    let result = call(&client, "get_issue", json!({"issue": "TEST-1"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument(s) `issue`"),
        "{}",
        text(&result)
    );
    let result = call(&client, "get_issue", json!({})).await;
    assert!(text(&result).contains("missing required argument(s): `issueId`"));

    // Неизвестный инструмент — ошибка протокола, сервер продолжает работать.
    assert!(
        client
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .is_err()
    );
    assert!(!client.list_all_tools().await.unwrap().is_empty());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn pagination_headers_are_returned() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .and(query_param("perPage", "2"))
        .and(body_json(json!({"queue": "TEST"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Total-Pages", "3")
                .insert_header("X-Total-Count", "5")
                .set_body_json(json!([{"key": "TEST-1"}, {"key": "TEST-2"}])),
        )
        .expect(1)
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &client,
        "search_issues",
        json!({"perPage": 2, "body": {"queue": "TEST"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"][1]["key"], "TEST-2");
    assert_eq!(
        value["pagination"],
        json!({"total_pages": 3, "total_count": 5})
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn credentials_are_redacted() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/queues/TEST/triggers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 1,
            "actions": [{"type": "Webhook", "authContext": {"type": "basic", "password": "p@ss"}}],
            "echo": TEST_TOKEN
        }])))
        .mount(&api)
        .await;
    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(&client, "get_triggers", json!({"queueId": "TEST"})).await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(false), "{body}");
    assert!(!body.contains("p@ss"), "{body}");
    assert!(!body.contains(TEST_TOKEN), "{body}");
    assert!(
        body.contains("[redacted]") && body.contains("password"),
        "{body}"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

/// Значение, похожее на OAuth-токен Яндекса по форме (не настоящее).
const LOOKS_LIKE_TOKEN: &str = "y0_AgAAAAAexampleexampleexampleexampleexampleexample";

#[tokio::test]
async fn secrets_are_redacted_in_responses_and_errors() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/queues/TEST/triggers/5/webhooks/log"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "request": {
                "headers": {"X-Hook-Key": "hook-key-1", "Content-Type": "application/json"},
                "webhookAuthContext": {"type": "header", "headerValue": "hook-value-1"}
            }
        }])))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/issues/TEST-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "TEST-1",
            "description": format!("bot uses OAuth {LOOKS_LIKE_TOKEN}"),
        })))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errors": {"password": "pw-in-error"},
            "errorMessages": [format!("bad filter near {LOOKS_LIKE_TOKEN}")],
            "statusCode": 400
        })))
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;

    let result = call(
        &client,
        "view_trigger_logs",
        json!({"queueId": "TEST", "triggerId": "5"}),
    )
    .await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(false), "{body}");
    assert!(
        !body.contains("hook-key-1") && !body.contains("hook-value-1"),
        "{body}"
    );
    assert!(body.contains("application/json"), "{body}");
    assert!(body.contains("X-Hook-Key, headerValue"), "{body}");

    // Тот же ответ через `api_request` скрывается так же.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/v3/queues/TEST/triggers/5/webhooks/log"}),
    )
    .await;
    assert!(!text(&result).contains("hook-key-1"), "{}", text(&result));

    let result = call(&client, "get_issue", json!({"issueId": "TEST-1"})).await;
    let body = text(&result);
    assert!(!body.contains(LOOKS_LIKE_TOKEN), "{body}");
    assert!(body.contains("bot uses OAuth [redacted]"), "{body}");
    assert!(body.contains("1 value(s) that look like tokens"), "{body}");

    // Ошибка Tracker: значения скрыты и в сообщениях, и в полях, с пометкой.
    let result = call(
        &client,
        "search_issues",
        json!({"body": {"query": "Queue: TEST"}}),
    )
    .await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(true));
    assert!(body.contains("HTTP 400"), "{body}");
    assert!(
        !body.contains("pw-in-error") && !body.contains(LOOKS_LIKE_TOKEN),
        "{body}"
    );
    assert!(body.contains("password: [redacted]"), "{body}");
    assert!(body.contains("replaced with \"[redacted]\""), "{body}");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn files_outside_allowed_directories_are_never_used() {
    let api = MockServer::start().await;
    // Ни загрузка, ни скачивание до API не доходят.
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"content".to_vec()))
        .expect(0)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root))
        .unwrap()
        .with_base_dir(&root);
    let (client, server) = start(&api, options(AccessLevel::Destructive), files).await;

    // Загрузка: файл вне корня, ссылка на него и путь через ссылку на каталог.
    for file in [
        outside.join("secret.txt").display().to_string(),
        root.join("link.txt").display().to_string(),
        "out/secret.txt".to_owned(),
        "../outside/secret.txt".to_owned(),
    ] {
        let result = call(
            &client,
            "post_attachment",
            json!({"issueId": "TEST-1", "file": file}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{file}");
        assert!(
            text(&result).contains("outside the allowed directories"),
            "{file}: {}",
            text(&result)
        );
    }

    // Скачивание: каталог назначения через ссылку наружу и существующая ссылка.
    for target in [
        root.join("out").join("a.png").display().to_string(),
        outside.join("a.png").display().to_string(),
        "out/a.png".to_owned(),
    ] {
        for (tool, args) in [
            (
                "issues_get_attachment",
                json!({"issueId": "TEST-1", "fileId": "4", "fileName": "a.png", "save_to": target}),
            ),
            (
                "api_request",
                json!({"method": "GET", "path": "/v3/issues/TEST-1/attachments/4/a.png", "save_to": target}),
            ),
        ] {
            let result = call(&client, tool, args).await;
            assert_eq!(result.is_error, Some(true), "{tool} {target}");
            assert!(
                text(&result).contains("outside the allowed directories"),
                "{tool} {target}: {}",
                text(&result)
            );
        }
    }
    let result = call(
        &client,
        "issues_get_attachment",
        json!({"issueId": "TEST-1", "fileId": "4", "fileName": "a.png", "save_to": "link.txt"}),
    )
    .await;
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert!(!outside.join("a.png").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "secret"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn binary_download_is_saved_to_file() {
    let api = MockServer::start().await;
    let content: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3];
    Mock::given(method("GET"))
        .and(path("/v3/issues/TEST-1/attachments/4/a.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(content.clone()),
        )
        .expect(1)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
    let (client, server) = start(&api, options(AccessLevel::Read), files).await;

    let tools = client.list_all_tools().await.unwrap();
    let download = tools
        .iter()
        .find(|t| t.name == "issues_get_attachment")
        .unwrap();
    let required = download.input_schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("save_to")));

    let target = root.join("a.png");
    let result = call(
        &client,
        "issues_get_attachment",
        json!({
            "issueId": "TEST-1", "fileId": "4", "fileName": "a.png",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["saved_to"], target.display().to_string());
    assert_eq!(value["bytes"], content.len());
    assert_eq!(value["content_type"], "image/png");
    assert_eq!(std::fs::read(&target).unwrap(), content);

    // Повтор с тем же путём: файл не перезаписывается, запрос не отправляется (expect(1)).
    let result = call(
        &client,
        "issues_get_attachment",
        json!({
            "issueId": "TEST-1", "fileId": "4", "fileName": "a.png",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn multipart_upload_from_allowed_file() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/TEST-1/attachments"))
        .and(query_param("filename", "renamed.txt"))
        .and(header_regex(
            "content-type",
            "^multipart/form-data; boundary=",
        ))
        .and(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains("name=\"file\"")
                && body.contains("filename=\"report.txt\"")
                && body.contains("report content")
        })
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"id": "7", "name": "renamed.txt"})),
        )
        .expect(1)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = root.join("report.txt");
    std::fs::write(&file, "report content").unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
    let (client, server) = start(&api, options(AccessLevel::Write), files).await;

    let result = call(
        &client,
        "post_attachment",
        json!({"issueId": "TEST-1", "filename": "renamed.txt", "file": file.display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":"7","name":"renamed.txt"}"#);

    // Файл вне разрешённых каталогов не читается.
    let other = tempfile::tempdir().unwrap();
    let outside = other.path().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    let result = call(
        &client,
        "post_attachment",
        json!({"issueId": "TEST-1", "file": outside.display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("outside the allowed directories"),
        "{}",
        text(&result)
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn access_levels_change_the_tool_list() {
    let api = MockServer::start().await;
    let files = || FilePolicy::new(&[]).unwrap();

    let (write, server) = start(&api, options(AccessLevel::Write), files()).await;
    let tools = write.list_all_tools().await.unwrap();
    let create = tools.iter().find(|t| t.name == "create_issue").unwrap();
    let annotations = create.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert!(!tools.iter().any(|t| t.name == "delete_queue"));
    write.cancel().await.unwrap();
    server.await.unwrap();

    let (all, server) = start(&api, options(AccessLevel::Destructive), files()).await;
    let tools = all.list_all_tools().await.unwrap();
    let delete = tools.iter().find(|t| t.name == "delete_queue").unwrap();
    assert_eq!(
        delete.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    // Все операции плюс `api_request` и `api_search`.
    assert_eq!(tools.len(), tracker_api::spec::operations().len() + 2);
    let request = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(
        request.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    all.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_request_respects_the_access_level() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .and(query_param("perPage", "1"))
        .and(body_json(json!({"filter": {"queue": "TEST"}})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Total-Count", "7")
                .set_body_json(json!([{"key": "TEST-1"}])),
        )
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/issues/TEST-1/worklog"))
        .and(query_param("expand", "a"))
        .and(query_param("expand", "b"))
        .and(header(
            "authorization",
            format!("OAuth {TEST_TOKEN}").as_str(),
        ))
        .and(header("x-cloud-org-id", TEST_ORG))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 1}])))
        .expect(1)
        .mount(&api)
        .await;
    // Изменяющие запросы до API не доходят.
    Mock::given(method("POST"))
        .and(path("/v3/issues"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&api)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&api)
        .await;

    // Набор `gaps` не мешает `api_request` и `api_search`.
    let (client, server) = start(
        &api,
        ServerOptions {
            toolsets: Some(vec!["gaps".into()]),
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let tools = client.list_all_tools().await.unwrap();
    let request = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(
        request.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tools.iter().any(|t| t.name == "api_search"));

    // Поиск через POST — чтение.
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "POST",
            "path": "/v3/issues/_search",
            "query": {"perPage": 1},
            "body": {"filter": {"queue": "TEST"}}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"][0]["key"], "TEST-1");
    assert_eq!(value["pagination"]["total_count"], 7);

    // Путь v2 и повтор ключа в query.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/v2/issues/TEST-1/worklog", "query": {"expand": ["a", "b"]}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));

    // Запись и удаление: понятный отказ с нужным флагом.
    let result = call(
        &client,
        "api_request",
        json!({"method": "POST", "path": "/v3/issues", "body": {"queue": "TEST", "summary": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("needs write access"), "{message}");
    assert!(message.contains("--allow-write"), "{message}");
    for path in ["/v3/queues/TEST", "/v3/some/unknown/thing"] {
        let result = call(
            &client,
            "api_request",
            json!({"method": "DELETE", "path": path}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let message = text(&result);
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("--allow-destructive"), "{message}");
    }
    // Заголовки и полные URL не принимаются.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/v3/myself", "headers": {"Authorization": "OAuth x"}}),
    )
    .await;
    assert!(text(&result).contains("headers cannot be set"));
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "https://example.test/v3/myself"}),
    )
    .await;
    assert!(text(&result).contains("full URLs"));

    // `api_search` находит выключенные операции и называет флаги.
    let result = call(&client, "api_search", json!({"query": "доска"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    let names: Vec<&str> = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["tool_name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"get_board") && names.contains(&"post_board"),
        "{names:?}"
    );
    let post = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool_name"] == "post_board")
        .unwrap();
    assert_eq!(post["enable_with"], "--allow-write --toolsets boards");

    let result = call(&client, "api_search", json!({"query": "worklog"})).await;
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    let first = &value["results"][0];
    assert!(
        first["tool_name"].as_str().unwrap().contains("worklog"),
        "{first}"
    );
    assert!(
        first["reference"].as_str().unwrap().starts_with("https://"),
        "{first}"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_request_writes_with_flags() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/TEST-1/worklog"))
        .and(body_json(json!({"duration": "PT1H"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 9})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v3/issues/TEST-1/worklog/9"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&api)
        .await;

    let (write, server) = start(
        &api,
        options(AccessLevel::Write),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &write,
        "api_request",
        json!({"method": "POST", "path": "/v3/issues/TEST-1/worklog", "body": {"duration": "PT1H"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":9}"#);
    let result = call(
        &write,
        "api_request",
        json!({"method": "DELETE", "path": "/v3/issues/TEST-1/worklog/9"}),
    )
    .await;
    assert!(text(&result).contains("--allow-destructive"));
    write.cancel().await.unwrap();
    server.await.unwrap();

    let (all, server) = start(
        &api,
        options(AccessLevel::Destructive),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &all,
        "api_request",
        json!({"method": "DELETE", "path": "/v3/issues/TEST-1/worklog/9"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"status":204}"#);
    all.cancel().await.unwrap();
    server.await.unwrap();
}
