//! Ядро MCP-сервера Yandex Tracker: регистрация инструментов, уровни доступа и
//! инструкции для агентов. Не зависит от транспорта (stdio или HTTP).
//!
//! - [`server`] — обработчик `rmcp` ([`TrackerServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`request`] — сборка запроса к API из аргументов инструмента;
//! - [`api_request`] — инструмент запроса к произвольному пути API, [`routes`] — какой
//!   уровень доступа нужен такому запросу;
//! - [`api_search`] — поиск по всем операциям API, в том числе выключенным;
//! - [`response`] — представление ответа API (с постраничной выдачей) в результате инструмента;
//! - [`error`] — ошибки вызова с текстом для агента;
//! - [`redact`] — вырезание значений, похожих на учётные данные, из ответов;
//! - [`files`] — политика доступа к локальным файлам;
//! - [`jq`] — фильтр ответа в стиле jq (аргумент `jq`);
//! - [`custom_tools`] — пользовательские инструменты из `tools.toml`;
//! - [`toolsets`] — наборы инструментов (`--toolsets`) и набор по умолчанию;
//! - [`dynamic_toolsets`] — `list_toolsets`, `enable_toolset`, `disable_toolset` (режим
//!   `--dynamic-toolsets`).
//!
//! Транспорт (stdio, HTTP) подключается в бинарнике `tracker-mcp`. Ядро о транспорте знает
//! две вещи, обе в настройках: работает ли сервер на машине агента
//! ([`ServerOptions::local_machine`]) и откуда брать токен ([`TokenSource`]).

pub mod api_request;
pub mod api_search;
pub mod custom_tools;
pub mod dynamic_toolsets;
pub mod error;
pub mod files;
pub mod jq;
pub mod redact;
pub mod registry;
pub mod request;
pub mod response;
pub mod routes;
pub mod server;
pub mod toolsets;

pub use custom_tools::{CustomTool, CustomToolDef, CustomToolsFile};
pub use error::ToolError;
pub use files::{FilePolicy, FilePolicyError, LOCAL_FILES_DISABLED};
pub use jq::JqRunner;
pub use registry::{RegisteredTool, ToolKind, ToolRegistry, access_name};
pub use server::{RequestAuth, RequestCredentialsFn, SERVER_NAME, TokenSource, TrackerServer};
pub use toolsets::{
    ALL_TOOLSETS, DEFAULT_TOOLSETS, Toolset, available_toolsets, effective_toolsets,
    normalize_toolsets,
};
pub use tracker_api;

use tracker_api::AccessLevel;

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Включённые наборы инструментов (slug, уже проверенные [`normalize_toolsets`]).
    /// `None` или пустой список — набор по умолчанию ([`DEFAULT_TOOLSETS`]). При
    /// `dynamic_toolsets` — наборы, включённые при запуске (`None` — ни одного).
    pub toolsets: Option<Vec<String>>,
    /// Динамические наборы: при запуске регистрируются только `api_search`, `api_request`,
    /// инструменты управления наборами и пользовательские инструменты, остальные агент
    /// включает `enable_toolset` (см. [`dynamic_toolsets`]). Уровень доступа действует как
    /// обычно.
    pub dynamic_toolsets: bool,
    /// Как выполняются фильтры `jq` (см. [`jq`]). По умолчанию — в потоке этого процесса;
    /// `serve` выполняет их в отдельном процессе.
    pub jq: JqRunner,
    /// Пользовательские инструменты (см. [`custom_tools`]). Регистрируются те, чей уровень
    /// доступа не выше `max_access`; наборы инструментов их не ограничивают.
    pub custom_tools: Vec<CustomTool>,
    /// Сервер работает на машине агента (stdio): инструменты читают и пишут локальные файлы
    /// (загрузка вложений, `save_to` для скачивания файлов).
    ///
    /// `false` — сервер удалённый (HTTP-режим): файлы сервера не имеют отношения к агенту,
    /// поэтому инструменты с файловыми аргументами и скачивание файлов не регистрируются, а
    /// любой путь отклоняется ([`FilePolicy::disabled`]).
    pub local_machine: bool,
    /// Профиль токена, с которым запущен сервер; подставляется в подсказки инструкций
    /// (`tracker-mcp auth login --profile …`).
    pub profile: Option<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            dynamic_toolsets: false,
            jq: JqRunner::default(),
            custom_tools: Vec::new(),
            local_machine: true,
            profile: None,
        }
    }
}

/// Уровень доступа по флагам `--allow-write` и `--allow-destructive`: удаление подразумевает
/// запись. Единое правило для `serve`, статуса и профилей.
pub fn access_from_flags(allow_write: bool, allow_destructive: bool) -> AccessLevel {
    if allow_destructive {
        AccessLevel::Destructive
    } else if allow_write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_implies_write() {
        assert_eq!(access_from_flags(false, false), AccessLevel::Read);
        assert_eq!(access_from_flags(true, false), AccessLevel::Write);
        assert_eq!(access_from_flags(false, true), AccessLevel::Destructive);
        assert_eq!(access_from_flags(true, true), AccessLevel::Destructive);
    }
}
