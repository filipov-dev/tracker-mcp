//! Тесты исполнителя запросов на `wiremock`, без реальной сети.

use std::time::{Duration, Instant};

use serde_json::json;
use tracker_api::ApiToken;
use tracker_api::HttpMethod;
use tracker_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, Credentials, FileSource, MultipartPart, OrgId,
    PageLink, RequestBody, ResponseBody, RetryPolicy,
};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Узнаваемое фиктивное значение, чтобы искать утечки в текстах ошибок.
const TOKEN: &str = "test-token-LEAKCHECK-7f3a9";
/// Фиктивный идентификатор организации.
const ORG: &str = "0000001";

fn config(base: &str) -> ClientConfig {
    ClientConfig {
        base_url: url::Url::parse(base).unwrap(),
        credentials: Some(Credentials::oauth(ApiToken::new(TOKEN))),
        iam: None,
        org: Some(OrgId::Yandex360(ORG.into())),
        timeout: Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 2,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(5),
        },
    }
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(config(&server.uri())).unwrap()
}

fn get(path: &str) -> ApiRequest {
    ApiRequest::new(HttpMethod::Get, path)
}

fn http_error(err: ApiError) -> tracker_api::executor::HttpError {
    match err {
        ApiError::Http(e) => *e,
        other => panic!("ожидалась Http, получено {other:?}"),
    }
}

#[tokio::test]
async fn sends_oauth_org_and_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/myself"))
        .and(header("authorization", format!("OAuth {TOKEN}").as_str()))
        .and(header("x-org-id", ORG))
        .and(header(
            "user-agent",
            concat!("tracker-mcp/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me"})))
        .expect(1)
        .mount(&server)
        .await;

    let response = client(&server).execute(get("/v3/myself")).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, ResponseBody::Json(json!({"login": "me"})));
    assert!(response.pagination.is_empty());
}

#[tokio::test]
async fn sends_iam_bearer_and_cloud_org() {
    let server = MockServer::start().await;
    Mock::given(path("/v2/myself"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("x-cloud-org-id", "bpf-cloud-org"))
        .respond_with(|req: &Request| {
            // Заголовок другой схемы организации не отправляется.
            assert!(req.headers.get("x-org-id").is_none());
            ResponseTemplate::new(200).set_body_json(json!({"ok": true}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = Some(Credentials::iam(ApiToken::new(TOKEN)));
    cfg.org = Some(OrgId::Cloud("bpf-cloud-org".into()));
    let response = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v2/myself"))
        .await
        .unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
}

#[tokio::test]
async fn missing_token_or_org_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = None;
    let err = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v3/myself"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::MissingToken), "{err:?}");

    let mut cfg = config(&server.uri());
    cfg.org = None;
    let no_org = ApiClient::new(cfg).unwrap();
    let err = no_org.execute(get("/v3/myself")).await.unwrap_err();
    assert!(matches!(err, ApiError::MissingOrg), "{err:?}");

    // Организацию можно задать копии клиента.
    let with_org = no_org.with_org(OrgId::Yandex360("1".into())).unwrap();
    assert!(with_org.config().org.is_some());
    for bad in ["", "1 2", "1\n", "один"] {
        let err = no_org.with_org(OrgId::Yandex360(bad.into())).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidConfig(_)),
            "{bad:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn encodes_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/issues"))
        .and(query_param("filter", "a b&c=d"))
        .and(query_param("perPage", "10"))
        .and(query_param("page", "2"))
        .and(query_param("fields", "key,summary"))
        .and(query_param("expand", "transitions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = get("/v3/issues");
    request.query = vec![
        ("filter".into(), "a b&c=d".into()),
        ("perPage".into(), "10".into()),
        ("page".into(), "2".into()),
        ("fields".into(), "key,summary".into()),
        ("expand".into(), "transitions".into()),
    ];
    let response = client(&server).execute(request).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!([])));
}

#[tokio::test]
async fn keeps_base_url_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(path("/proxy/v3/myself"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(2)
        .mount(&server)
        .await;

    for base in [
        format!("{}/proxy", server.uri()),
        format!("{}/proxy/", server.uri()),
    ] {
        let client = ApiClient::new(config(&base)).unwrap();
        let response = client.execute(get("/v3/myself")).await.unwrap();
        assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    }
}

#[tokio::test]
async fn json_body_is_sent_as_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"queue": "TEST", "summary": "s"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"key": "TEST-1"})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/issues/");
    request.body = RequestBody::Json(json!({"queue": "TEST", "summary": "s"}));
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(response.status, 201);

    // Регрессия: тело, переданное строкой с JSON, уходит объектом, а не строкой.
    request.body = RequestBody::Json(json!(r#"{"queue": "TEST", "summary": "s"}"#));
    client.execute(request).await.unwrap();
}

#[tokio::test]
async fn multipart_sends_text_and_file_parts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/TEST-1/attachments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "1"})))
        .expect(2)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("report.txt");
    std::fs::write(&file, "file-content-123").unwrap();

    let client = client(&server);
    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/issues/TEST-1/attachments");
    request.query = vec![("filename".into(), "отчёт.log".into())];
    request.body = RequestBody::Multipart(vec![
        MultipartPart::Text {
            name: "comment".into(),
            value: "hello-text".into(),
        },
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Path(file.clone()),
            file_name: Some("отчёт.log".into()),
            content_type: Some("text/x-log".into()),
        },
    ]);
    client.execute(request).await.unwrap();

    // Файл из памяти: тип угадывается по имени.
    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/issues/TEST-1/attachments");
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Bytes(b"\x89PNG-bytes".to_vec()),
        file_name: Some("pic.png".into()),
        content_type: None,
    }]);
    client.execute(request).await.unwrap();

    let received = server.received_requests().await.unwrap();
    let ct = received[0]
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    let body = String::from_utf8_lossy(&received[0].body);
    assert!(body.contains("name=\"comment\""));
    assert!(body.contains("hello-text"));
    assert!(
        body.contains("name=\"file\"; filename=\"отчёт.log\""),
        "{body}"
    );
    assert!(body.contains("Content-Type: text/x-log"));
    assert!(body.contains("file-content-123"));

    let body = String::from_utf8_lossy(&received[1].body);
    assert!(body.contains("filename=\"pic.png\""), "{body}");
    assert!(body.contains("Content-Type: image/png"), "{body}");
    assert!(body.contains("PNG-bytes"));

    // Ошибки до отправки: нет имени для файла из памяти, плохой тип, нет файла.
    for part in [
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Bytes(vec![1]),
            file_name: None,
            content_type: None,
        },
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Path(file),
            file_name: None,
            content_type: Some("not a mime".into()),
        },
    ] {
        let mut request = ApiRequest::new(HttpMethod::Post, "/v3/attachments");
        request.body = RequestBody::Multipart(vec![part]);
        let err = client.execute(request).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }
    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/attachments");
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path(base.join("missing.bin")),
        file_name: None,
        content_type: None,
    }]);
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::Io(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn returns_page_and_scroll_headers() {
    let server = MockServer::start().await;
    let host = server.uri();
    Mock::given(path("/v3/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"key": "TEST-1"}]))
                .insert_header("x-total-pages", "7")
                .insert_header("x-total-count", "321"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/issues/_search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([]))
                .insert_header("x-scroll-id", "scroll-abc")
                .insert_header("x-scroll-token", "scroll-token-xyz")
                .insert_header("x-total-count", "15000")
                .insert_header(
                    "link",
                    format!(
                        "<{host}/v3/issues/_search?scrollId=scroll-abc&perScroll=100>; rel=\"next\", \
                         <{host}/v3/issues/_search?scrollType=sorted&perScroll=100>; rel=\"first\", \
                         <https://evil.example/v3/issues>; rel=\"prev\""
                    )
                    .as_str(),
                ),
        )
        .mount(&server)
        .await;

    let client = client(&server);
    let page = client.execute(get("/v3/issues")).await.unwrap().pagination;
    assert_eq!(page.total_pages, Some(7));
    assert_eq!(page.total_count, Some(321));
    assert!(page.scroll_id.is_none() && page.links.is_empty());

    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/issues/_search");
    request.query = vec![
        ("scrollType".into(), "sorted".into()),
        ("perScroll".into(), "100".into()),
        ("scrollTTLMillis".into(), "60000".into()),
    ];
    request.body = RequestBody::Json(json!({"filter": {"queue": "TEST"}}));
    let scroll = client.execute(request).await.unwrap().pagination;
    assert_eq!(scroll.scroll_id.as_deref(), Some("scroll-abc"));
    assert_eq!(scroll.scroll_token.as_deref(), Some("scroll-token-xyz"));
    assert_eq!(scroll.total_count, Some(15000));
    // Ссылка на чужой хост отброшена, остальные приведены к относительным путям.
    assert_eq!(scroll.links.len(), 2, "{:?}", scroll.links);
    assert_eq!(
        scroll.next(),
        Some(&PageLink {
            rel: "next".into(),
            path: "/v3/issues/_search".into(),
            query: vec![
                ("scrollId".into(), "scroll-abc".into()),
                ("perScroll".into(), "100".into()),
            ],
        })
    );
    let serialized = serde_json::to_value(&scroll).unwrap();
    assert_eq!(serialized["scroll_id"], "scroll-abc");
    assert!(serialized.get("total_pages").is_none());
}

#[tokio::test]
async fn tracker_errors_are_parsed() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/issues/NOPE-1"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errors": {},
            "errorMessages": ["Задача не существует."],
            "statusCode": 404
        })))
        .mount(&server)
        .await;
    Mock::given(path("/v3/issues/"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "errors": {"summary": "Обязательное поле"},
            "errorMessages": [],
            "statusCode": 422
        })))
        .mount(&server)
        .await;
    Mock::given(path("/v3/huge-error"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;

    let client = client(&server);
    let err = client.execute(get("/v3/issues/NOPE-1")).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    let text = err.to_string();
    assert!(
        text.contains("404") && text.contains("Задача не существует."),
        "{text}"
    );
    let e = http_error(err);
    assert_eq!(e.error_messages, vec!["Задача не существует."]);
    assert!(e.errors.is_empty());

    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/issues/");
    request.body = RequestBody::Json(json!({}));
    let err = client.execute(request).await.unwrap_err();
    assert!(
        err.to_string().contains("summary: Обязательное поле"),
        "{err}"
    );
    let e = http_error(err);
    assert_eq!(e.status, 422);
    assert_eq!(e.errors["summary"], "Обязательное поле");

    let e = http_error(client.execute(get("/v3/huge-error")).await.unwrap_err());
    assert_eq!(e.status, 500);
    assert!(
        e.body.len() < 9000,
        "тело ошибки должно обрезаться: {}",
        e.body.len()
    );
    assert!(e.body.ends_with("(обрезано)"));
}

#[tokio::test]
async fn retries_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/myself"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v3/myself"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let started = Instant::now();
    let response = client(&server).execute(get("/v3/myself")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "пауза Retry-After не выдержана: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn retry_limits() {
    let server = MockServer::start().await;
    // Без Retry-After: пауза по умолчанию, после исчерпания попыток — ошибка 429.
    Mock::given(path("/v3/always-429"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "errorMessages": ["Too many requests"]
        })))
        .expect(3)
        .mount(&server)
        .await;
    // Retry-After больше max_delay: не ждём, сразу возвращаем ошибку с подсказкой.
    Mock::given(path("/v3/long-wait"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .expect(1)
        .mount(&server)
        .await;
    // 503 повторяется для GET, но не для POST.
    Mock::given(method("GET"))
        .and(path("/v3/unavailable"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/unavailable"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    // Прочие ошибки не повторяются.
    Mock::given(path("/v3/server-error"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(client.execute(get("/v3/always-429")).await.unwrap_err());
    assert_eq!(e.status, 429);
    assert_eq!(e.error_messages, vec!["Too many requests"]);
    assert_eq!(e.retry_after, None);

    let err = client.execute(get("/v3/long-wait")).await.unwrap_err();
    assert!(err.to_string().contains("3600"), "{err}");
    assert_eq!(http_error(err).retry_after, Some(3600));

    let e = http_error(client.execute(get("/v3/unavailable")).await.unwrap_err());
    assert_eq!(e.status, 503);
    let post = ApiRequest::new(HttpMethod::Post, "/v3/unavailable");
    assert_eq!(client.execute(post).await.unwrap_err().status(), Some(503));
    assert_eq!(
        client
            .execute(get("/v3/server-error"))
            .await
            .unwrap_err()
            .status(),
        Some(500)
    );
}

#[tokio::test]
async fn retry_resends_multipart_file() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/attachments"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v3/attachments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "9"})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("a.txt");
    std::fs::write(&file, "retry-body-42").unwrap();
    let mut request = ApiRequest::new(HttpMethod::Post, "/v3/attachments");
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path(file),
        file_name: None,
        content_type: None,
    }]);
    client(&server).execute(request).await.unwrap();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    for req in &received {
        assert!(String::from_utf8_lossy(&req.body).contains("retry-body-42"));
    }
}

#[tokio::test]
async fn large_response_is_rejected_with_hint() {
    let server = MockServer::start().await;
    let big = format!("{{\"data\": \"{}\"}}", "a".repeat(10_000));
    Mock::given(path("/v3/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big, "application/json"))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.max_response_bytes = 1000;
    let client = ApiClient::new(cfg).unwrap();

    let err = client.execute(get("/v3/big")).await.unwrap_err();
    assert!(
        matches!(err, ApiError::ResponseTooLarge { limit: 1000 }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("fields") && text.contains("perPage"),
        "{text}"
    );

    // Лимит отдельного запроса заменяет лимит конфигурации.
    let response = client
        .execute_with_limit(get("/v3/big"), 64 * 1024)
        .await
        .unwrap();
    assert!(matches!(response.body, ResponseBody::Json(_)));
}

#[tokio::test]
async fn text_and_untyped_json_responses() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/text"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("hello", "text/plain"))
        .mount(&server)
        .await;
    Mock::given(path("/v3/untyped"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{\"a\":1}".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/v3/empty"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = client(&server);
    assert_eq!(
        client.execute(get("/v3/text")).await.unwrap().body,
        ResponseBody::Text("hello".into())
    );
    assert_eq!(
        client.execute(get("/v3/untyped")).await.unwrap().body,
        ResponseBody::Json(json!({"a": 1}))
    );
    assert_eq!(
        client.execute(get("/v3/empty")).await.unwrap().body,
        ResponseBody::Empty
    );
}

#[tokio::test]
async fn binary_response_without_save_to_returns_note() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/blob"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0u8, 159, 146, 150], "application/octet-stream"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v3/sniffed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8, 159, 146, 150]))
        .mount(&server)
        .await;

    let client = client(&server);
    for p in ["/v3/blob", "/v3/sniffed"] {
        match client.execute(get(p)).await.unwrap().body {
            ResponseBody::Text(note) => {
                assert!(note.contains("двоичный"), "{note}");
                assert!(note.contains("save_to"), "{note}");
                assert!(note.contains("4 байт"), "{note}");
            }
            other => panic!("ожидалось пояснение, получено {other:?}"),
        }
    }
}

#[tokio::test]
async fn save_to_writes_file_and_refuses_overwrite() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(path("/v3/issues/TEST-1/thumbnails/5"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(payload.clone(), "image/png"))
        .mount(&server)
        .await;
    Mock::given(path("/v3/fail"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let target = base.join("thumb.png");
    let mut cfg = config(&server.uri());
    // Лимит ответа не распространяется на сохранение в файл.
    cfg.max_response_bytes = 10;
    let client = ApiClient::new(cfg).unwrap();

    let request = ApiRequest {
        save_to: Some(target.clone()),
        ..get("/v3/issues/TEST-1/thumbnails/5")
    };
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::SavedToFile {
            path: target.clone(),
            bytes: payload.len() as u64,
            content_type: Some("image/png".into()),
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Повторная запись в существующий файл запрещена.
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Каталог должен существовать, путь — быть абсолютным.
    for bad in [
        base.join("no-such-dir").join("x.bin"),
        std::path::PathBuf::from("relative.bin"),
    ] {
        let err = client
            .execute(ApiRequest {
                save_to: Some(bad),
                ..get("/v3/issues/TEST-1/thumbnails/5")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }

    // При ошибке API файл не создаётся.
    let failed = base.join("failed.bin");
    let err = client
        .execute(ApiRequest {
            save_to: Some(failed.clone()),
            ..get("/v3/fail")
        })
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert!(!failed.exists());
}

#[tokio::test]
async fn invalid_paths_never_reach_server() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = client(&server);
    for p in [
        "https://evil.example/v3/issues",
        "http://127.0.0.1/v3/issues",
        "//evil.example/v3/issues",
        "/v3/issues/../../admin",
        "/v3/issues/%2e%2e/admin",
        "/v3/issues?x=1",
        "/v1/issues",
        "/admin",
        "v3/issues",
    ] {
        let err = client.execute(get(p)).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{p}: {err:?}");
    }
}

#[tokio::test]
async fn cross_origin_redirect_is_not_followed() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(path("/v3/external"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal?sig=secret", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v3/internal"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/v3/myself"))
        .mount(&server)
        .await;
    Mock::given(path("/v3/myself"))
        .and(header("authorization", format!("OAuth {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me"})))
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(client.execute(get("/v3/external")).await.unwrap_err());
    assert_eq!(e.status, 302);
    assert!(!e.body.contains("sig=secret"), "{}", e.body);
    let response = client.execute(get("/v3/internal")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"login": "me"})));
}

#[tokio::test]
async fn with_credentials_uses_its_own_token_per_copy() {
    let server = MockServer::start().await;
    for token in ["token-agent-a", "token-agent-b"] {
        Mock::given(path("/v3/myself"))
            .and(header("authorization", format!("OAuth {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": token})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(path("/v3/echo"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(403).set_body_string(format!("denied: {auth}"))
        })
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = None;
    let base = ApiClient::new(cfg).unwrap();
    let a = base
        .with_credentials(Credentials::oauth(ApiToken::new("token-agent-a")))
        .unwrap();
    let b = base
        .with_credentials(Credentials::oauth(ApiToken::new("token-agent-b")))
        .unwrap();
    a.execute(get("/v3/myself")).await.unwrap();
    b.execute(get("/v3/myself")).await.unwrap();
    // Исходный клиент токен не получил.
    assert!(matches!(
        base.execute(get("/v3/myself")).await.unwrap_err(),
        ApiError::MissingToken
    ));

    // Токен копии вырезается из ошибок и из текстов через scrub_token.
    let err = a.execute(get("/v3/echo")).await.unwrap_err();
    assert!(!format!("{err}").contains("token-agent-a"), "{err}");
    assert_eq!(a.scrub_token("x token-agent-a y".into()), "x *** y");

    // Недопустимые значения отклоняются без отражения в ошибке.
    for bad in ["", "  ", "bad\ntoken"] {
        let err = base
            .with_credentials(Credentials::oauth(ApiToken::new(bad)))
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{err:?}");
        assert!(!format!("{err:?}").contains("bad\ntoken"));
    }
}

#[tokio::test]
async fn token_never_leaks() {
    // Конфигурация и клиент в Debug не показывают токен и org id.
    let cfg = config("https://api.example");
    let debug = format!("{cfg:?}");
    assert!(!debug.contains(TOKEN) && !debug.contains(ORG), "{debug}");
    let client = ApiClient::new(cfg).unwrap();
    let debug = format!("{client:?}");
    assert!(!debug.contains(TOKEN) && !debug.contains(ORG), "{debug}");

    // Сервер отражает заголовок Authorization в теле ошибки (в том числе в errorMessages
    // и errors) и в успешном ответе.
    let server = MockServer::start().await;
    Mock::given(path("/v3/echo-error"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401).set_body_json(json!({
                "errors": {"authorization": auth},
                "errorMessages": [format!("bad credentials: {auth}")],
                "statusCode": 401
            }))
        })
        .mount(&server)
        .await;
    Mock::given(path("/v3/echo-text"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_raw(format!("you sent {auth}"), "text/plain")
        })
        .mount(&server)
        .await;

    let client = ApiClient::new(config(&server.uri())).unwrap();
    let mut outputs = Vec::new();

    let err = client.execute(get("/v3/echo-error")).await.unwrap_err();
    outputs.push(format!("{err}"));
    outputs.push(format!("{err:?}"));
    let ok = client.execute(get("/v3/echo-text")).await.unwrap();
    outputs.push(format!("{ok:?}"));

    // Ошибки пути, параметров, ввода-вывода.
    let mut file_request = ApiRequest::new(HttpMethod::Post, "/v3/attachments");
    file_request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path("/nonexistent/file".into()),
        file_name: None,
        content_type: None,
    }]);
    for e in [
        client.execute(get("/v3/../x")).await.unwrap_err(),
        client.execute(file_request).await.unwrap_err(),
    ] {
        outputs.push(format!("{e}"));
        outputs.push(format!("{e:?}"));
    }

    // Порт, на котором никто не слушает: соединение отклоняется.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let unreachable = ApiClient::new(config(&format!("http://127.0.0.1:{closed}"))).unwrap();
    let transport = unreachable.execute(get("/v3/myself")).await.unwrap_err();
    assert!(matches!(transport, ApiError::Transport(_)), "{transport:?}");
    outputs.push(format!("{transport}"));
    outputs.push(format!("{transport:?}"));

    for out in &outputs {
        assert!(!out.contains(TOKEN), "токен утёк: {out}");
    }
    assert!(outputs[0].contains("***"), "{}", outputs[0]);
}

#[tokio::test]
async fn timeout_maps_to_transport_error() {
    let server = MockServer::start().await;
    Mock::given(path("/v3/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(200);
    let client = ApiClient::new(cfg).unwrap();
    match client.execute(get("/v3/slow")).await.unwrap_err() {
        ApiError::Transport(text) => assert!(text.contains("таймаут"), "{text}"),
        other => panic!("ожидалась Transport, получено {other:?}"),
    }
}

#[test]
fn default_config_targets_tracker() {
    let cfg = ClientConfig::new(None, None).unwrap();
    assert_eq!(cfg.base_url.as_str(), "https://api.tracker.yandex.net/");
    assert!(ApiClient::new(cfg).is_ok());
}

#[test]
fn rejects_insecure_or_invalid_config() {
    for base in [
        "http://api.tracker.yandex.net",
        "https://user:pw@api.example",
        "ftp://api.example",
    ] {
        let err = ApiClient::new(config(base)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{base}: {err:?}");
    }
    let mut cfg = config("https://api.example");
    cfg.credentials = Some(Credentials::oauth(ApiToken::new("bad\ntoken")));
    let err = ApiClient::new(cfg).unwrap_err();
    assert!(matches!(err, ApiError::InvalidConfig(_)));
    assert!(!format!("{err:?}").contains("bad\ntoken"));

    let mut cfg = config("https://api.example");
    cfg.org = Some(OrgId::Cloud("bad org\n".into()));
    let err = ApiClient::new(cfg).unwrap_err();
    assert!(matches!(err, ApiError::InvalidConfig(_)));
    assert!(!format!("{err}").contains("bad org"));
}
