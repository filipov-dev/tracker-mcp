//! Описание операций Tracker API.
//!
//! Таблицы [`operations`] и [`toolsets`] генерируются `build.rs` из
//! `spec/tracker.openapi.json`. Типы этого модуля — контракт между генератором,
//! исполнителем запросов и MCP-сервером.
//!
//! Как MCP-сервер строит запрос из аргументов инструмента:
//!
//! 1. Проверить, что переданы все параметры с `required: true`.
//! 2. Подставить [`ParamLocation::Path`] в шаблон [`OperationDef::path`] (значения
//!    кодируются как сегмент пути), [`ParamLocation::Query`] передать в строке запроса
//!    (массивы — повторением ключа).
//! 3. Тело выбрать по [`OperationDef::body`]: для [`BodyKind::Json`] аргумент `body`
//!    отправляется JSON-телом, для [`BodyKind::Multipart`] параметры
//!    [`ParamLocation::FormData`] собираются в multipart/form-data (параметры
//!    [`ParamType::File`] содержат путь к локальному файлу).
//! 4. Ответ разобрать по [`OperationDef::response`]: JSON или бинарное содержимое файла.
//!
//! Заголовки `Authorization`, `X-Org-ID` и `X-Cloud-Org-ID` в параметры не входят:
//! их добавляет исполнитель.

/// HTTP-метод операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Уровень доступа операции (`x-access-level` спецификации).
/// Уровни упорядочены: `Read < Write < Destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessLevel {
    Read,
    Write,
    Destructive,
}

impl AccessLevel {
    /// Разрешает ли текущий уровень операцию с уровнем `required`.
    pub fn allows(self, required: AccessLevel) -> bool {
        required <= self
    }

    /// Значение `x-access-level`: `read`, `write` или `destructive`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// Где передаётся параметр операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// Подставляется в шаблон пути (`{name}`).
    Path,
    /// Передаётся в строке запроса.
    Query,
    /// JSON-тело запроса; у операции не больше одного такого параметра, его имя — `body`.
    Body,
    /// Поле multipart/form-data тела.
    FormData,
}

/// Тип значения параметра в аргументах инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    /// Массив; в строке запроса передаётся повторением ключа (`keys=a&keys=b`).
    Array,
    /// JSON-объект (тело запроса).
    Object,
    /// Путь к локальному файлу, содержимое которого отправляется полем multipart.
    File,
}

/// Вид тела запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `application/json`: аргумент `body` ([`ParamLocation::Body`]).
    Json,
    /// `multipart/form-data`: параметры [`ParamLocation::FormData`].
    Multipart,
}

/// Вид успешного ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// JSON (или пустой ответ, например `204`).
    Json,
    /// Содержимое файла (`application/octet-stream`).
    Binary,
}

/// Параметр операции.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Имя параметра: ключ в аргументах инструмента, имя в пути, строке запроса или поле multipart.
    pub name: &'static str,
    pub location: ParamLocation,
    /// Обязателен ли параметр. Параметры пути обязательны всегда.
    pub required: bool,
    pub description: &'static str,
    pub value_type: ParamType,
}

/// Набор инструментов — раздел справочника (тег спецификации).
#[derive(Debug, Clone, Copy)]
pub struct ToolsetDef {
    /// Короткое английское имя для `--toolsets`, например `issues`.
    pub slug: &'static str,
    /// Тег спецификации (название раздела справочника).
    pub tag: &'static str,
    /// Описание раздела из спецификации.
    pub description: &'static str,
}

/// Операция Tracker API, доступная как MCP-инструмент.
#[derive(Debug, Clone, Copy)]
pub struct OperationDef {
    /// Имя MCP-инструмента (snake_case из `operationId`), уникально.
    pub tool_name: &'static str,
    /// Исходный `operationId` из спецификации.
    pub operation_id: &'static str,
    /// Slug набора инструментов ([`ToolsetDef::slug`]).
    pub toolset: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    /// Страница справочника с описанием операции.
    pub docs_url: &'static str,
    pub method: HttpMethod,
    /// Шаблон пути с версией API, например `/v3/issues/{issueId}`.
    pub path: &'static str,
    pub access: AccessLevel,
    pub params: &'static [ParamDef],
    /// Вид тела запроса; `None` — тела нет.
    pub body: Option<BodyKind>,
    pub response: ResponseKind,
    /// JSON Schema аргументов инструмента в виде JSON-текста.
    ///
    /// Объект с `type: object` и `additionalProperties: false`: параметры пути и запроса,
    /// `body` (JSON-тело с раскрытыми `$ref`) или поля multipart. Заголовков авторизации
    /// и организации в схеме нет.
    pub input_schema: &'static str,
}

impl OperationDef {
    /// Параметры в расположении `location`.
    pub fn params_in(&self, location: ParamLocation) -> impl Iterator<Item = &'static ParamDef> {
        self.params
            .iter()
            .filter(move |param| param.location == location)
    }

    /// Параметр по имени.
    pub fn param(&self, name: &str) -> Option<&'static ParamDef> {
        self.params.iter().find(|param| param.name == name)
    }
}

include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// Все операции API, отсортированные по имени инструмента.
pub fn operations() -> &'static [OperationDef] {
    OPERATIONS
}

/// Операция по имени MCP-инструмента.
pub fn find(tool_name: &str) -> Option<&'static OperationDef> {
    OPERATIONS
        .binary_search_by(|op| op.tool_name.cmp(tool_name))
        .ok()
        .and_then(|index| OPERATIONS.get(index))
}

/// Наборы инструментов в порядке разделов справочника.
pub fn toolsets() -> &'static [ToolsetDef] {
    TOOLSETS
}

/// Набор инструментов по slug.
pub fn find_toolset(slug: &str) -> Option<&'static ToolsetDef> {
    TOOLSETS.iter().find(|toolset| toolset.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_are_ordered() {
        assert!(AccessLevel::Destructive.allows(AccessLevel::Write));
        assert!(AccessLevel::Write.allows(AccessLevel::Read));
        assert!(!AccessLevel::Read.allows(AccessLevel::Write));
        assert!(!AccessLevel::Write.allows(AccessLevel::Destructive));
    }

    #[test]
    fn find_by_name() {
        let op = find("get_issue").expect("операция get_issue");
        assert_eq!(op.path, "/v3/issues/{issueId}");
        assert_eq!(
            op.param("issueId").map(|p| p.location),
            Some(ParamLocation::Path)
        );
        assert!(find("no_such_tool").is_none());
        assert_eq!(find_toolset("issues").map(|t| t.tag), Some("Задачи"));
        assert!(find_toolset("Задачи").is_none());
    }
}
