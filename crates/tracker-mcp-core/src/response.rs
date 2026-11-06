//! Представление ответа API в результате инструмента.
//!
//! - JSON без постраничной выдачи — компактный JSON ответа как есть;
//! - JSON с заголовками постраничной выдачи — `{"result": <ответ>, "pagination": {...}}`:
//!   агент видит число страниц и записей, `scrollId` и ссылки на соседние страницы, которые
//!   Tracker отдаёт только в заголовках;
//! - текст — как есть (заголовки выдачи — отдельной строкой после текста);
//! - сохранённый файл — `{"saved_to", "bytes", "content_type"}`;
//! - пустое тело — `{"status"}`.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};
use tracker_api::executor::{ApiResponse, Pagination, ResponseBody};

use crate::error::ToolError;

/// Текст успешного ответа (см. описание модуля).
pub fn response_text(response: &ApiResponse) -> String {
    let pagination =
        (!response.pagination.is_empty()).then(|| pagination_json(&response.pagination));
    match (&response.body, pagination) {
        (ResponseBody::Json(value), None) => value.to_string(),
        (ResponseBody::Json(value), Some(pagination)) => {
            json!({"result": value, "pagination": pagination}).to_string()
        }
        (ResponseBody::Text(text), None) => text.clone(),
        (ResponseBody::Text(text), Some(pagination)) => {
            format!("{text}\n\npagination: {pagination}")
        }
        (
            ResponseBody::SavedToFile {
                path,
                bytes,
                content_type,
            },
            _,
        ) => json!({
            "saved_to": path.display().to_string(),
            "bytes": bytes,
            "content_type": content_type,
        })
        .to_string(),
        (ResponseBody::Empty, None) => json!({"status": response.status}).to_string(),
        (ResponseBody::Empty, Some(pagination)) => {
            json!({"status": response.status, "pagination": pagination}).to_string()
        }
    }
}

fn pagination_json(pagination: &Pagination) -> Value {
    // Сериализация `Pagination` не может завершиться ошибкой (только строки и числа).
    serde_json::to_value(pagination).unwrap_or(Value::Null)
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tracker_api::executor::PageLink;

    use super::*;

    fn response(body: ResponseBody, pagination: Pagination) -> ApiResponse {
        ApiResponse {
            status: 204,
            body,
            pagination,
        }
    }

    #[test]
    fn formats_bodies_without_pagination() {
        let none = Pagination::default;
        assert_eq!(
            response_text(&response(ResponseBody::Json(json!({"a": [1, 2]})), none())),
            r#"{"a":[1,2]}"#
        );
        assert_eq!(
            response_text(&response(ResponseBody::Text("plain".into()), none())),
            "plain"
        );
        assert_eq!(
            response_text(&response(ResponseBody::Empty, none())),
            r#"{"status":204}"#
        );
        let saved = response_text(&response(
            ResponseBody::SavedToFile {
                path: PathBuf::from("/tmp/x.bin"),
                bytes: 42,
                content_type: Some("image/png".into()),
            },
            none(),
        ));
        assert_eq!(
            saved,
            r#"{"bytes":42,"content_type":"image/png","saved_to":"/tmp/x.bin"}"#
        );
    }

    #[test]
    fn shows_pagination() {
        let pagination = Pagination {
            total_pages: Some(3),
            total_count: Some(120),
            scroll_id: Some("scroll-1".into()),
            scroll_token: None,
            links: vec![PageLink {
                rel: "next".into(),
                path: "/v3/issues/_search".into(),
                query: vec![("scrollId".into(), "scroll-1".into())],
            }],
        };
        let text = response_text(&response(
            ResponseBody::Json(json!([{"key": "TEST-1"}])),
            pagination.clone(),
        ));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["result"], json!([{"key": "TEST-1"}]));
        assert_eq!(value["pagination"]["total_pages"], 3);
        assert_eq!(value["pagination"]["total_count"], 120);
        assert_eq!(value["pagination"]["scroll_id"], "scroll-1");
        assert_eq!(value["pagination"]["links"][0]["rel"], "next");
        assert!(value["pagination"].get("scroll_token").is_none());

        let text = response_text(&response(ResponseBody::Text("t".into()), pagination));
        assert!(text.starts_with("t\n\npagination: {"), "{text}");
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
