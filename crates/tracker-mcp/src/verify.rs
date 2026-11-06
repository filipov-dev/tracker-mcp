//! Проверка токена и организации запросом текущего пользователя (`GET /v3/myself`).
//!
//! Это небольшой собственный вызов `reqwest` только для `auth`: заголовки авторизации и
//! организации формируются здесь же. Когда появится общий исполнитель запросов, проверку
//! стоит перевести на него.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use reqwest::header::{AUTHORIZATION, HeaderValue, USER_AGENT};
use serde_json::Value;
use tracker_api::{ApiToken, Organization};

/// Адрес Tracker API.
pub const TRACKER_API_URL: &str = "https://api.tracker.yandex.net";

/// Путь запроса текущего пользователя.
pub const MYSELF_PATH: &str = "/v3/myself";

/// Предел длины сообщения об ошибке из ответа Tracker.
const MAX_DETAILS_LEN: usize = 300;

/// Пользователь, которому принадлежит токен. Только то, что нужно показать: логин и имя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    pub login: String,
    pub display: Option<String>,
}

impl fmt::Display for CurrentUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.display {
            Some(display) if display != &self.login => write!(f, "{} ({display})", self.login),
            _ => f.write_str(&self.login),
        }
    }
}

/// Проверка токена. В тестах подменяется, чтобы не ходить в сеть.
pub trait TokenVerifier {
    /// Текущий пользователь для токена в организации `org`.
    async fn current_user(
        &self,
        token: &ApiToken,
        org: &Organization,
    ) -> anyhow::Result<CurrentUser>;
}

/// Проверка через Tracker API.
#[derive(Debug, Clone)]
pub struct ApiVerifier {
    base_url: String,
}

impl Default for ApiVerifier {
    fn default() -> Self {
        Self::with_base_url(TRACKER_API_URL)
    }
}

impl ApiVerifier {
    /// Проверка через API по другому адресу (тесты).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    fn url(&self) -> String {
        format!("{}{MYSELF_PATH}", self.base_url.trim_end_matches('/'))
    }
}

/// Заголовок `Authorization` для OAuth-токена. Помечен как чувствительный, чтобы `reqwest`
/// не выводил его значение; ошибка не содержит токена.
fn authorization(token: &ApiToken) -> anyhow::Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("OAuth {}", token.expose())).map_err(|_| {
        anyhow!("the token contains characters that are not allowed in HTTP headers")
    })?;
    value.set_sensitive(true);
    Ok(value)
}

impl TokenVerifier for ApiVerifier {
    async fn current_user(
        &self,
        token: &ApiToken,
        org: &Organization,
    ) -> anyhow::Result<CurrentUser> {
        // Клиент создаётся только при проверке: без корневых сертификатов системы HTTPS-клиент
        // не создаётся, а `--skip-check` должен работать и там.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("could not create the HTTPS client")?;
        let (org_header, org_id) = org.header();
        let response = client
            .get(self.url())
            .header(AUTHORIZATION, authorization(token)?)
            .header(org_header, org_id)
            .header(
                USER_AGENT,
                concat!("tracker-mcp/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .map_err(|err| anyhow!("could not reach Tracker: {err}"))?;
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap_or_default();
        let json: Option<Value> = serde_json::from_slice(&body).ok();
        if !(200..300).contains(&status) {
            return Err(status_error(status, json.as_ref(), token, org));
        }
        match json.as_ref().and_then(current_user) {
            Some(user) => Ok(user),
            None => bail!("unexpected response from Tracker {MYSELF_PATH} (HTTP {status})"),
        }
    }
}

/// Понятная ошибка по коду ответа. Из тела берутся только `errorMessages` Tracker, без
/// значения токена.
fn status_error(
    status: u16,
    body: Option<&Value>,
    token: &ApiToken,
    org: &Organization,
) -> anyhow::Error {
    let details = body.and_then(error_messages).map(|text| {
        let text = if token.expose().is_empty() {
            text
        } else {
            text.replace(token.expose(), "***")
        };
        let mut short: String = text.chars().take(MAX_DETAILS_LEN).collect();
        if text.chars().count() > MAX_DETAILS_LEN {
            short.push('…');
        }
        format!(" Tracker says: {short}")
    });
    let details = details.unwrap_or_default();
    let header = org.kind().header_name();
    match status {
        401 => anyhow!(
            "Tracker rejected the token (HTTP 401). Make sure it was copied completely and has \
             not expired or been revoked.{details}"
        ),
        403 => anyhow!(
            "Tracker denied access (HTTP 403). Check the organization ID and type ({header}) \
             and that the token has Tracker access in this organization.{details}"
        ),
        _ => anyhow!("Tracker returned HTTP {status} for {MYSELF_PATH}.{details}"),
    }
}

/// `errorMessages` из ответа Tracker с ошибкой.
fn error_messages(body: &Value) -> Option<String> {
    let messages: Vec<&str> = body
        .get("errorMessages")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .collect();
    (!messages.is_empty()).then(|| messages.join("; "))
}

/// Логин и имя из ответа `GET /v3/myself`.
fn current_user(body: &Value) -> Option<CurrentUser> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let display = text("display");
    let login = text("login")
        .or_else(|| body.get("uid").map(|uid| uid.to_string()))
        .or_else(|| display.clone())?;
    Some(CurrentUser { login, display })
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Проверка без сети: принимает только заданное значение токена.
    #[derive(Debug)]
    pub struct FakeVerifier {
        pub valid_token: String,
    }

    impl TokenVerifier for FakeVerifier {
        async fn current_user(
            &self,
            token: &ApiToken,
            org: &Organization,
        ) -> anyhow::Result<CurrentUser> {
            if token.expose() == self.valid_token {
                Ok(CurrentUser {
                    login: "test-user".to_owned(),
                    display: Some("Test User".to_owned()),
                })
            } else {
                Err(status_error(401, None, token, org))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tracker_api::OrgKind;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn org360() -> Organization {
        Organization::new(OrgKind::Yandex360, "12345").unwrap()
    }

    #[tokio::test]
    async fn sends_oauth_and_org_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "OAuth good-token"))
            .and(header("x-org-id", "12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "login": "jdoe",
                "display": "John Doe",
                "email": "jdoe@example.test"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());
        let user = verifier
            .current_user(&ApiToken::new("good-token"), &org360())
            .await
            .unwrap();
        assert_eq!(user.to_string(), "jdoe (John Doe)");
    }

    #[tokio::test]
    async fn cloud_org_uses_cloud_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "jdoe"})))
            .expect(1)
            .mount(&server)
            .await;
        let org = Organization::new(OrgKind::Cloud, "cloud-org").unwrap();
        let user = ApiVerifier::with_base_url(format!("{}/", server.uri()))
            .current_user(&ApiToken::new("t"), &org)
            .await
            .unwrap();
        assert_eq!(user.to_string(), "jdoe");
    }

    #[tokio::test]
    async fn maps_rejections_without_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "OAuth bad-token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errorMessages": ["Token bad-token is invalid"],
                "statusCode": 401
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "OAuth other-org"))
            .respond_with(ResponseTemplate::new(403).set_body_string("not json"))
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());

        let err = verifier
            .current_user(&ApiToken::new("bad-token"), &org360())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rejected the token"), "{err}");
        assert!(err.contains("Token *** is invalid"), "{err}");
        assert!(!err.contains("bad-token"), "{err}");

        let err = verifier
            .current_user(&ApiToken::new("other-org"), &org360())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("X-Org-ID"), "{err}");
    }

    #[tokio::test]
    async fn transport_errors_and_bad_tokens() {
        // Порт 9 (discard) на localhost закрыт: соединение не устанавливается.
        let verifier = ApiVerifier::with_base_url("http://127.0.0.1:9");
        let err = verifier
            .current_user(&ApiToken::new("t"), &org360())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not reach Tracker"), "{err}");

        let err = verifier
            .current_user(&ApiToken::new("bad\nvalue"), &org360())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        assert!(!err.to_string().contains("bad"), "{err}");
    }

    #[test]
    fn extracts_user() {
        assert_eq!(
            current_user(&json!({"login": "a", "display": "a"})),
            Some(CurrentUser {
                login: "a".into(),
                display: Some("a".into())
            })
        );
        assert_eq!(current_user(&json!({"uid": 7})).unwrap().login, "7");
        assert_eq!(current_user(&json!({})), None);
        assert_eq!(
            CurrentUser {
                login: "a".into(),
                display: Some("a".into())
            }
            .to_string(),
            "a"
        );
    }

    #[test]
    fn long_details_are_truncated() {
        let body = json!({"errorMessages": ["x".repeat(1000)]});
        let err = status_error(500, Some(&body), &ApiToken::new("t"), &org360()).to_string();
        assert!(err.contains("HTTP 500"), "{err}");
        assert!(err.len() < 500, "{err}");
    }
}
