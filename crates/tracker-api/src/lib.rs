//! Клиент Yandex Tracker REST API.
//!
//! Операции описаны в OpenAPI-спецификации `spec/tracker.openapi.json`, которая
//! собрана из справочника API скриптом `spec/build_openapi.py`.
//!
//! - [`spec`] — операции API, сгенерированные из спецификации при сборке.
//! - [`executor`] — выполнение HTTP-запросов к API.
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`auth`] — хранение токенов в системном хранилище (Keychain и аналоги).
//! - [`iam`] — IAM-токены Yandex Cloud через `yc iam create-token`, только в памяти.
//! - [`org`] — организация Tracker и заголовок, в котором передаётся её ID.
//! - [`token`] — обёртка токена, которая не попадает в логи и `Debug`.

pub mod auth;
pub mod executor;
pub mod iam;
pub mod org;
pub mod path;
pub mod spec;
pub mod token;

pub use executor::DEFAULT_BASE_URL;
pub use org::{OrgKind, Organization};
pub use spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamDef, ParamLocation, ParamType,
    ResponseKind, ToolsetDef,
};
pub use token::ApiToken;

/// OpenAPI-спецификация Tracker API в виде JSON.
pub const OPENAPI_JSON: &str = include_str!("../spec/tracker.openapi.json");
