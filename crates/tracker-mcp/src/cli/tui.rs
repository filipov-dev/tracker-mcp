//! `tracker-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки, права и
//! одобрение на macOS, регистрации в Claude Code и Codex, проверка токена в Tracker и
//! инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`TrackerAdapter`]): чтение и запись `profiles.toml`,
//! состояние токена (без самого токена), проверка токена запросом к API, сборка сервера и
//! повторная регистрация без флагов доступа. Сервер для инструментов собирается так же, как
//! `serve` через stdio без флагов: права профиля, урезанные до одобренных.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval;
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};
use tracker_api::auth::{KeyringStore, TokenStore};

use crate::cli::serve::{ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, Scope, ServerSpec, codex};
use crate::profiles::{AuthKind, DEFAULT_PROFILE, ProfileRepo, ProfileSettings};
use crate::ui::CaptureUi;

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the token (never the \
token), the access of each profile and whether it is approved, where the server is registered \
in Claude Code and Codex and whether those registrations agree with the profile, which Tracker \
user the token signs in as, and the tools the profile gives the agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys. The HTTP mode (`serve --http`) has no profiles and is not shown here.";

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let adapter = TrackerAdapter {
        repo: ProfileRepo::open_default()?,
        tokens: &KeyringStore,
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of tracker-mcp: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для Tracker.
pub struct TrackerAdapter<'a> {
    pub repo: ProfileRepo,
    pub tokens: &'a dyn TokenStore,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
}

impl TrackerAdapter<'_> {
    /// Сервер с правами профиля без флагов (как `serve` без флагов), со всеми наборами.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let args = ServeArgs {
            toolsets: vec![tracker_mcp_core::ALL_TOOLSETS.to_owned()],
            ..ServeArgs::local(Some(profile.to_owned()))
        };
        let approvals = approval::platform_store(EXE_STEM);
        let mut setup = Setup::load(&args, self.tokens, &self.repo, approvals.as_deref())?;
        setup.options.dynamic_toolsets = false;
        Ok(setup)
    }

    async fn session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let (auth, token) = match settings.auth {
            AuthKind::Oauth => (
                "OAuth token".to_owned(),
                match self.tokens.contains(name) {
                    Ok(true) => "stored in the system keychain".to_owned(),
                    Ok(false) => {
                        format!("missing: run `tracker-mcp auth login --profile {name}`")
                    }
                    Err(err) => format!("cannot check the keychain: {err}"),
                },
            ),
            AuthKind::Iam => (
                match settings.yc_command() {
                    Ok(command) => format!("Yandex Cloud IAM token from `{}`", command.display()),
                    Err(err) => format!("IAM, invalid yc settings: {err:#}"),
                },
                "not stored; obtained from yc when needed".to_owned(),
            ),
        };
        let org = settings.org.as_ref().map_or_else(
            || format!("not set: run `tracker-mcp auth login --profile {name}`"),
            ToString::to_string,
        );
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        vec![
            Field::new("Auth", auth),
            Field::new("Token", token),
            Field::new("Organization", org),
            Field::new(
                "OAuth app",
                settings
                    .client_id
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), |id| format!("ClientID {id}")),
            ),
            Field::new("Toolsets", toolsets),
            Field::new(
                "Dynamic toolsets",
                if settings.dynamic_toolsets {
                    "on"
                } else {
                    "off"
                },
            ),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for TrackerAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "Yandex Tracker"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            // `serve` не берёт профиль из окружения: такой переменной нет.
            profile_env: "",
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        let session = self.session(profile).await?;
        let arguments = object(json!({
            "method": "GET",
            "path": "/v3/myself",
            "jq": "{login, display}",
        }));
        let user = match session.call("api_request", arguments).await {
            Ok(result) => user_text(&result),
            Err(err) => format!("failed: {err}"),
        };
        session.close().await;
        Ok(account_fields(&settings, user))
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Ответ кабинета: тип авторизации, организация, пользователь токена и где смотреть права.
fn account_fields(settings: &ProfileSettings, user: String) -> Vec<Field> {
    let auth = match settings.auth {
        AuthKind::Oauth => "OAuth token",
        AuthKind::Iam => "Yandex Cloud IAM token (yc)",
    };
    let org = settings
        .org
        .as_ref()
        .map_or_else(|| "not set".to_owned(), ToString::to_string);
    let permissions = match settings.auth {
        AuthKind::Oauth => {
            "Tracker does not report the token's OAuth permissions: see the application at \
             oauth.yandex.ru (tracker:read, tracker:write) and the user's roles in Tracker"
        }
        AuthKind::Iam => {
            "the roles of the Yandex Cloud account in the organization and its rights in Tracker"
        }
    };
    vec![
        Field::new("Auth", auth),
        Field::new("Organization", org),
        Field::new("User", user),
        Field::new("Permissions", permissions),
    ]
}

/// Пользователь токена из ответа `GET /v3/myself` (после фильтра `{login, display}`) или
/// первая строка ошибки.
fn user_text(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        let first = text.lines().next().unwrap_or("error");
        let first: String = first.chars().take(160).collect();
        return format!("check failed: {first}");
    }
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    match (field("login"), field("display")) {
        (Some(login), Some(display)) if display != login => format!("{login} ({display})"),
        (Some(login), _) => login,
        (None, Some(display)) => display,
        (None, None) => "signed in (no login in the response)".to_owned(),
    }
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

/// Флаги доступа в аргументах регистрации: без них `serve` берёт права профиля.
const ACCESS_FLAGS: [&str; 2] = ["--allow-write", "--allow-destructive"];
/// Флаг доступа со значением.
const ALLOW_PATH_FLAG: &str = "--allow-path";

/// Аргументы регистрации без флагов доступа; остальное (профиль, наборы, `--tools-file`,
/// таймауты) остаётся как было.
fn without_access_flags(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if ACCESS_FLAGS.contains(&arg.as_str()) || arg.starts_with(&format!("{ALLOW_PATH_FLAG}=")) {
            continue;
        }
        if arg == ALLOW_PATH_FLAG {
            iter.next();
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Регистрирует запись заново: та же команда `serve`, но без флагов доступа, с путём к
/// текущему бинарнику. Запись проекта (Claude Code) — только из каталога проекта.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    if serve.subcommand.as_deref() != Some("serve") || serve.http {
        return Err(format!(
            "`{}` does not run `tracker-mcp serve` over stdio; change it by hand",
            registration.name
        ));
    }
    // Проверки имени и пути — как у `connect`; аргументы — прежние без флагов доступа.
    let mut spec = ServerSpec::new(
        &registration.name,
        exe,
        &serve.profile,
        &ProfileSettings::default(),
    )
    .map_err(|err| format!("{err:#}"))?;
    spec.args = without_access_flags(&registration.args);
    let mut ui = CaptureUi::default();
    let done = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                other => {
                    return Err(format!(
                        "`{}` is registered for {other}; run `tracker-mcp connect claude-code \
                         --scope project --name {} --profile {}` in that project",
                        registration.name, registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, true)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, true)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    if !done {
        return Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into()));
    }
    Ok(format!(
        "{} now runs `{}` as `{}`; restart the client to pick it up",
        registration.client.display_name(),
        spec.display(),
        spec.name
    ))
}

#[cfg(test)]
mod tests {
    use tracker_api::auth::MemoryStore;
    use tracker_api::{ApiToken, OrgKind, Organization};

    use super::*;
    use crate::clients::TEST_EXE;

    fn adapter<'a>(dir: &Path, tokens: &'a MemoryStore) -> TrackerAdapter<'a> {
        TrackerAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            tokens,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
        }
    }

    #[test]
    fn shows_profiles_without_the_token_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens
            .set("work", &ApiToken::new("tui-secret-token"))
            .unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    allow_write: true,
                    org: Some(Organization::new(OrgKind::Yandex360, "42").unwrap()),
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert(
                "fed",
                ProfileSettings {
                    auth: AuthKind::Iam,
                    yc_profile: Some("corp".into()),
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 3);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(text.contains("stored in the system keychain"), "{text}");
        assert!(text.contains("X-Org-ID"), "{text}");
        assert!(!text.contains("tui-secret-token"), "{text}");
        let fed = profiles.iter().find(|p| p.name == "fed").unwrap();
        let text = format!("{:?}", fed.fields);
        assert!(
            text.contains("yc iam create-token --profile corp"),
            "{text}"
        );
        assert!(text.contains("not stored"), "{text}");
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        assert!(format!("{:?}", empty.fields).contains("auth login --profile empty"));

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert!(saved.org.is_some());
    }

    #[test]
    fn user_and_account_fields_are_short() {
        let ok = CallToolResult::success(vec![ContentBlock::text(
            r#"{"login": "robot", "display": "Robot R."}"#,
        )]);
        assert_eq!(user_text(&ok), "robot (Robot R.)");
        let same = CallToolResult::success(vec![ContentBlock::text(
            r#"{"login": "robot", "display": "robot"}"#,
        )]);
        assert_eq!(user_text(&same), "robot");
        let err = CallToolResult::error(vec![ContentBlock::text(
            "Tracker returned 401 Unauthorized\nmore",
        )]);
        assert_eq!(
            user_text(&err),
            "check failed: Tracker returned 401 Unauthorized"
        );
        let fields = account_fields(
            &ProfileSettings {
                org: Some(Organization::new(OrgKind::Cloud, "c1").unwrap()),
                ..ProfileSettings::default()
            },
            "robot".into(),
        );
        let text = format!("{fields:?}");
        assert!(text.contains("OAuth token"), "{text}");
        assert!(text.contains("X-Cloud-Org-ID"), "{text}");
        assert!(text.contains("tracker:write"), "{text}");
    }

    /// Запрос `check_account` через настоящий сервер с правами профиля, API — wiremock.
    #[tokio::test]
    async fn account_request_shows_the_tokens_user() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/myself"))
            .and(header("authorization", "OAuth tui-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "login": "robot",
                "display": "Robot",
                "email": "robot@example.com"
            })))
            .expect(1)
            .mount(&api)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens.set("work", &ApiToken::new("tui-token")).unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    org: Some(Organization::new(OrgKind::Yandex360, "42").unwrap()),
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        // Сервер как у `check_account`, но с адресом wiremock и `jq` в процессе.
        let mut setup = Setup::load(
            &ServeArgs {
                toolsets: vec!["all".into()],
                ..ServeArgs::local(Some("work".into()))
            },
            &tokens,
            &adapter.repo,
            None,
        )
        .unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        setup.options.jq = tracker_mcp_core::JqRunner::in_process();
        let session = Session::start(setup.server().unwrap()).await.unwrap();
        let result = session
            .call(
                "api_request",
                object(json!({"method": "GET", "path": "/v3/myself", "jq": "{login, display}"})),
            )
            .await
            .unwrap();
        session.close().await;
        assert_eq!(user_text(&result), "robot (Robot)");
    }

    fn registration(
        client: registrations::Client,
        scope: registrations::Scope,
        args: &[&str],
    ) -> Registration {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        Registration {
            client,
            scope,
            name: "tracker".into(),
            command: PathBuf::from(TEST_EXE),
            serve: registrations::parse_serve(&args, None, DEFAULT_PROFILE),
            args,
            env_names: vec![],
            enabled: true,
        }
    }

    #[test]
    fn access_flags_are_removed_and_the_rest_is_kept() {
        let args: Vec<String> = [
            "serve",
            "--profile",
            "work",
            "--allow-write",
            "--allow-path",
            "/data",
            "--allow-path=/more",
            "--allow-destructive",
            "--tools-file",
            "/t.toml",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            without_access_flags(&args),
            ["serve", "--profile", "work", "--tools-file", "/t.toml"]
        );
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.tracker]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\", \"--toolsets\", \"issues\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &[
                "serve",
                "--profile",
                "work",
                "--allow-write",
                "--allow-destructive",
                "--toolsets",
                "issues",
            ],
        );
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow"), "{config}");
        assert!(
            config.contains(r#"args = ["serve", "--profile", "work", "--toolsets", "issues"]"#),
            "{config}"
        );
    }

    #[test]
    fn project_registration_elsewhere_and_http_are_explained() {
        let here = if cfg!(windows) { r"C:\here" } else { "/here" };
        let elsewhere = if cfg!(windows) {
            r"C:\elsewhere"
        } else {
            "/elsewhere"
        };
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(PathBuf::from(elsewhere)),
            &["serve", "--profile", "work"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(PathBuf::from(here)),
            &["serve", "--profile", "work"],
        );
        assert!(reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).is_err());
        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &["serve", "--http", "--allow-write"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, None).unwrap_err();
        assert!(err.contains("by hand"), "{err}");
    }

    #[test]
    fn identity_matches_own_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        let adapter = adapter(dir.path(), &tokens);
        let found = registrations::codex_toml(
            &format!(
                "[mcp_servers.tracker]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"codex\", \"--allow-write\"]\n\n[mcp_servers.other]\ncommand = \"/usr/bin/node\"\nargs = []\n"
            ),
            &adapter.identity(),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].serve.profile, "codex");
        assert!(found[0].serve.has_access_flags());
    }
}
