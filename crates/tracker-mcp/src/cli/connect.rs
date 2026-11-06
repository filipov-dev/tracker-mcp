//! `tracker-mcp connect` — подключение сервера к MCP-клиенту одной командой.
//!
//! Сценарий: OAuth-приложение пользователя (`ClientID` в профиле; при первом запуске —
//! инструкция и страница создания приложения) → организация → токен по ссылке авторизации
//! приложения, скрытым вводом → проверка `GET /v3/myself` → токен в системное хранилище,
//! настройки в `profiles.toml` → запись сервера в конфигурацию клиента после подтверждения.
//! Токен не проходит через агента и не попадает ни в аргументы, ни в конфигурацию клиента.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use tracker_api::Organization;
use tracker_api::auth::{AuthError, KeyringStore, TokenStore};
use tracker_api::spec::{find_toolset, toolsets};

use crate::browser::{BrowserDecision, BrowserEnv, decide};
use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{ClientKind, DEFAULT_SERVER_NAME, Scope, ServerSpec, codex};
use crate::login::{
    apply_yc_flags, check_iam, check_new_token, org_from_flags, reject_yc_flags_without_iam,
    resolve_iam_org, resolve_org, save_token,
};
use crate::oauth::{ask_client_id, normalize_client_id, prompt_token};
use crate::profiles::{AuthKind, ProfileRepo, ProfileSettings, validate_profile_name};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Tracker tokens come from your own Yandex OAuth application: there is no built-in one. On the \
first run for a profile connect opens the application creation page, explains the settings \
and asks for the ClientID; it is saved in the profile and not asked again.

The token is obtained from the authorization link of your application, read with hidden \
input, checked with GET /v3/myself and stored only in the system keychain under the profile \
name. The client config gets only the command `tracker-mcp serve --profile <profile> ...`.

Links open in the default browser; over SSH, without a graphical session or with \
--no-browser they are only printed.

Use a separate profile and token for every client, so each can be revoked on its own.

With --iam (federated and service accounts of a Yandex Cloud organization) there is no OAuth \
application and no stored token: connect saves the auth type, the Yandex Cloud organization and \
the yc settings in the profile and checks that `yc iam create-token` gives a token that Tracker \
accepts. The server gets IAM tokens from yc when needed and keeps them in memory only.")]
pub struct ConnectArgs {
    /// MCP client to register the server in.
    #[arg(value_enum)]
    pub client: ClientKind,
    /// Profile name. Defaults to the client name.
    #[arg(long)]
    pub profile: Option<String>,
    /// MCP server name in the client config.
    #[arg(long, default_value = DEFAULT_SERVER_NAME)]
    pub name: String,
    /// Where to register the server in the client. Codex supports only `user`.
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Allow tools that create or modify data in Tracker.
    #[arg(long)]
    pub allow_write: bool,
    /// Allow tools that delete data in Tracker (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (passed to `serve --toolsets`). Default: the
    /// default set of `serve`.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Start with only the search, request and toolset tools; the agent loads toolsets on
    /// demand (passed to `serve --dynamic-toolsets`). Needs a client that reloads the tool
    /// list when notified.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    /// ClientID of your Yandex OAuth application (32 hex characters). Saved in the profile.
    #[arg(long, value_name = "ID")]
    pub client_id: Option<String>,
    /// Yandex 360 for Business organization ID (sent as X-Org-ID).
    #[arg(long, value_name = "ID", conflicts_with = "cloud_org_id")]
    pub org_id: Option<String>,
    /// Yandex Cloud Organization ID (sent as X-Cloud-Org-ID).
    #[arg(long, value_name = "ID")]
    pub cloud_org_id: Option<String>,
    /// Do not ask for confirmation: keep a working stored token and change the client config.
    ///
    /// An entry with the same name that does not run tracker-mcp is never replaced with --yes.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Store the token without checking it against the Tracker API.
    #[arg(long)]
    pub skip_check: bool,
    /// Do not open links in a browser; only print them.
    #[arg(long)]
    pub no_browser: bool,
    /// Use Yandex Cloud IAM tokens from yc instead of an OAuth token (federated and service
    /// accounts). Needs a Yandex Cloud organization (--cloud-org-id). A profile already set up
    /// for IAM keeps using it without this flag.
    #[arg(long, conflicts_with_all = ["client_id", "org_id"])]
    pub iam: bool,
    /// IAM: absolute path to the yc CLI. Default: yc from PATH.
    #[arg(long, value_name = "PATH")]
    pub yc_path: Option<PathBuf>,
    /// IAM: yc profile to get tokens with (yc --profile). Default: the active yc profile.
    #[arg(long, value_name = "NAME")]
    pub yc_profile: Option<String>,
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine the path of tracker-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let verifier = (!args.skip_check).then(ApiVerifier::default);
    let mut env = ConnectEnv {
        store: &KeyringStore,
        repo: &ProfileRepo::open_default()?,
        verifier: verifier.as_ref(),
        runner: &mut SystemRunner,
        ui: &mut TerminalUi,
        exe,
        codex_home,
        browser: decide(args.no_browser, &BrowserEnv::current()),
    };
    connect(&mut env, &args).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти и временными каталогами.
pub struct ConnectEnv<'a, V> {
    pub store: &'a dyn TokenStore,
    pub repo: &'a ProfileRepo,
    /// Проверка токена; `None` — `--skip-check`.
    pub verifier: Option<&'a V>,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
    /// Открывать ли ссылки в браузере (решается в `run` по флагу и окружению).
    pub browser: BrowserDecision,
}

/// Наборы инструментов из флага: без пустых и повторов, только известные slug.
fn normalize_toolsets(values: &[String]) -> anyhow::Result<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        let slug = value.trim();
        if slug.is_empty() || result.iter().any(|known| known == slug) {
            continue;
        }
        if find_toolset(slug).is_none() {
            let known: Vec<&str> = toolsets().iter().map(|toolset| toolset.slug).collect();
            bail!("unknown toolset `{slug}`; available: {}", known.join(", "));
        }
        result.push(slug.to_owned());
    }
    Ok(result)
}

pub async fn connect<V: TokenVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
) -> anyhow::Result<()> {
    let client = args.client;
    let profile = args
        .profile
        .clone()
        .unwrap_or_else(|| client.name().to_owned());
    validate_profile_name(&profile)?;

    // Всё, что можно проверить без вопросов, — до первого вопроса пользователю.
    let toolsets = normalize_toolsets(&args.toolsets)?;
    let org_flags = org_from_flags(args.org_id.as_deref(), args.cloud_org_id.as_deref())?;
    let client_id_flag = args
        .client_id
        .as_deref()
        .map(normalize_client_id)
        .transpose()?;
    let mut settings = env.repo.get(&profile)?.unwrap_or_default();
    let access = ProfileSettings {
        allow_write: args.allow_write,
        allow_destructive: args.allow_destructive,
        ..Default::default()
    }
    .normalized();
    let spec = ServerSpec::new(
        &args.name,
        &env.exe,
        &profile,
        &ProfileSettings {
            toolsets: toolsets.clone(),
            dynamic_toolsets: args.dynamic_toolsets,
            ..access.clone()
        },
    )?;
    let iam = args.iam || settings.auth == AuthKind::Iam;
    let yc_command = if iam {
        Some(apply_yc_flags(
            &mut settings,
            args.yc_path.as_deref(),
            args.yc_profile.as_deref(),
        )?)
    } else {
        reject_yc_flags_without_iam(args.yc_path.as_deref(), args.yc_profile.as_deref())?;
        None
    };
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }

    if let Some(command) = yc_command {
        // IAM: ни OAuth-приложения, ни токена в хранилище; организация — только Yandex Cloud.
        let org = resolve_iam_org(
            env.ui,
            &profile,
            args.org_id.as_deref(),
            args.cloud_org_id.as_deref(),
            settings.org.clone(),
        )?;
        check_iam(env.ui, env.verifier, &command, &org).await?;
        settings.auth = AuthKind::Iam;
        settings.org = Some(org);
    } else {
        let flags = OAuthFlags {
            org: org_flags,
            client_id: client_id_flag,
            allow_write: access.allow_write,
        };
        let org = oauth_token(env, args, &profile, &mut settings, flags).await?;
        settings.org = Some(org);
    }

    settings.client = Some(client.name().to_owned());
    settings.allow_write = access.allow_write;
    settings.allow_destructive = access.allow_destructive;
    settings.toolsets = toolsets;
    settings.dynamic_toolsets = args.dynamic_toolsets;
    env.repo.upsert(&profile, settings)?;
    env.ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        env.repo.path().display()
    ));

    let registered = match client {
        ClientKind::ClaudeCode => {
            claude::register(env.runner, env.ui, args.scope, &spec, args.yes)?
        }
        ClientKind::Codex => codex::register(env.ui, &env.codex_home, &spec, args.yes)?,
    };
    if registered {
        env.ui.say(&format!(
            "Done: {} now runs `{}` as MCP server `{}`. Restart the client to pick it up.",
            client.display_name(),
            spec.display(),
            spec.name
        ));
    } else {
        env.ui.say(&format!(
            "{} config was not changed. The token and profile are kept; re-run \
             `tracker-mcp connect {}` to register later.",
            client.display_name(),
            client.name()
        ));
    }
    Ok(())
}

/// Флаги OAuth, проверенные до первого вопроса пользователю.
struct OAuthFlags {
    org: Option<Organization>,
    client_id: Option<String>,
    allow_write: bool,
}

/// OAuth: приложение пользователя, организация и токен профиля (рабочий сохранённый можно
/// оставить). Возвращает организацию; токен сохранён в системном хранилище.
async fn oauth_token<V: TokenVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
    profile: &str,
    settings: &mut ProfileSettings,
    flags: OAuthFlags,
) -> anyhow::Result<Organization> {
    let OAuthFlags {
        org: org_flags,
        client_id: client_id_flag,
        allow_write,
    } = flags;

    // OAuth-приложение пользователя. `ClientID` сохраняется сразу, чтобы повторный запуск
    // после ошибки на следующих шагах не спрашивал его снова.
    let client_id = match (client_id_flag, settings.client_id.clone()) {
        (Some(client_id), _) => client_id,
        (None, Some(stored)) => {
            let client_id = normalize_client_id(&stored).with_context(|| {
                format!(
                    "profile `{profile}` in {} has an invalid client_id; pass --client-id",
                    env.repo.path().display()
                )
            })?;
            env.ui.say(&format!(
                "OAuth application: ClientID {client_id} (from the profile)."
            ));
            client_id
        }
        (None, None) => ask_client_id(env.ui, profile, allow_write, env.browser)?,
    };
    if settings.client_id.as_deref() != Some(client_id.as_str()) {
        settings.client_id = Some(client_id.clone());
        env.repo.upsert(profile, settings.clone())?;
        env.ui.say(&format!(
            "ClientID saved to profile `{profile}` in {}.",
            env.repo.path().display()
        ));
    }
    if allow_write {
        env.ui
            .say("Writes are enabled: the OAuth application needs the tracker:write permission.");
    }

    let org = resolve_org(env.ui, org_flags, settings.org.clone(), None)?;

    // Токен: рабочий сохранённый можно оставить, иначе — новый по ссылке авторизации.
    let stored_token = match env.store.get(profile) {
        Ok(token) => Some(token),
        Err(AuthError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };
    let keep = match (&stored_token, env.verifier) {
        (None, _) => false,
        (Some(token), Some(verifier)) => match verifier.current_user(token, &org).await {
            Ok(user) => {
                env.ui.say(&format!(
                    "Profile `{profile}` already has a working token: signed in to Tracker as \
                     `{user}` in {org}."
                ));
                args.yes || env.ui.confirm("Keep the stored token?", true)?
            }
            Err(err) => {
                env.ui.say(&format!(
                    "The stored token of profile `{profile}` does not work ({err:#}); a new \
                     token is needed."
                ));
                false
            }
        },
        (Some(_), None) => {
            env.ui.say(&format!(
                "Profile `{profile}` already has a stored token (not checked: --skip-check)."
            ));
            args.yes || env.ui.confirm("Keep the stored token?", true)?
        }
    };
    if keep {
        env.ui.say("Keeping the stored token.");
    } else {
        let token = prompt_token(env.ui, profile, &client_id, env.browser)?;
        check_new_token(env.ui, env.verifier, &token, &org).await?;
        save_token(env.ui, env.store, profile, &token)?;
    }
    Ok(org)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tracker_api::ApiToken;
    use tracker_api::auth::MemoryStore;

    use super::*;
    use crate::browser::SkipReason;
    use crate::clients::TEST_EXE;
    use crate::clients::claude::testing::FakeRunner;
    use crate::oauth::{CLIENT_NEW_URL, authorize_url};
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

    struct Fixture {
        _dir: tempfile::TempDir,
        store: MemoryStore,
        repo: ProfileRepo,
        verifier: FakeVerifier,
        skip_check: bool,
        runner: FakeRunner,
        ui: ScriptedUi,
        codex_home: PathBuf,
        browser: BrowserDecision,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                store: MemoryStore::new(),
                repo: ProfileRepo::in_dir(dir.path().join("config")),
                verifier: FakeVerifier {
                    valid_token: "good-token".into(),
                },
                skip_check: false,
                runner: FakeRunner::default(),
                ui: ScriptedUi::default(),
                codex_home: dir.path().join("codex"),
                browser: BrowserDecision::Skip(SkipReason::Flag),
                _dir: dir,
            }
        }

        /// Профиль, уже подключённый раньше: `ClientID` и организация.
        fn with_profile(self, profile: &str) -> Self {
            self.repo
                .upsert(
                    profile,
                    ProfileSettings {
                        client_id: Some(CLIENT_ID.into()),
                        org: Some(
                            tracker_api::Organization::new(tracker_api::OrgKind::Yandex360, "42")
                                .unwrap(),
                        ),
                        ..Default::default()
                    },
                )
                .unwrap();
            self
        }

        fn answers(&mut self, values: &[&str]) {
            self.ui.answers = values.iter().map(|value| (*value).to_owned()).collect();
        }

        fn tokens(&mut self, values: &[&str]) {
            self.ui.tokens = values.iter().map(|value| (*value).to_owned()).collect();
        }

        async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
            let mut env = ConnectEnv {
                store: &self.store,
                repo: &self.repo,
                verifier: (!self.skip_check).then_some(&self.verifier),
                runner: &mut self.runner,
                ui: &mut self.ui,
                exe: PathBuf::from(TEST_EXE),
                codex_home: self.codex_home.clone(),
                browser: self.browser,
            };
            connect(&mut env, args).await
        }

        fn codex_config(&self) -> Option<String> {
            fs::read_to_string(codex::config_path(&self.codex_home)).ok()
        }
    }

    fn args(client: ClientKind) -> ConnectArgs {
        ConnectArgs {
            client,
            profile: None,
            name: DEFAULT_SERVER_NAME.into(),
            scope: Scope::User,
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            dynamic_toolsets: false,
            client_id: None,
            org_id: None,
            cloud_org_id: None,
            yes: false,
            skip_check: false,
            no_browser: false,
            iam: false,
            yc_path: None,
            yc_profile: None,
        }
    }

    /// Аргументы `serve` в TOML-записи Codex.
    fn toml_args(args: &[&str]) -> String {
        format!("args = {args:?}")
    }

    #[tokio::test]
    async fn first_codex_run_creates_app_profile_token_and_config() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Open;
        // ClientID, тип и ID организации, подтверждение записи конфига.
        fx.answers(&[CLIENT_ID, "1", "42", "y"]);
        fx.tokens(&[" good-token \n"]);
        let mut args = args(ClientKind::Codex);
        args.allow_write = true;
        args.toolsets = vec!["issues".into(), " ".into(), "issues".into()];
        args.dynamic_toolsets = true;

        fx.connect(&args).await.unwrap();

        assert_eq!(fx.store.get("codex").unwrap().expose(), "good-token");
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.client.as_deref(), Some("codex"));
        assert_eq!(settings.client_id.as_deref(), Some(CLIENT_ID));
        assert_eq!(settings.org.unwrap().header(), ("X-Org-ID", "42"));
        assert!(settings.allow_write);
        assert!(!settings.allow_destructive);
        assert_eq!(settings.toolsets, ["issues"]);
        assert!(settings.dynamic_toolsets);

        let config = fx.codex_config().unwrap();
        assert!(config.contains("[mcp_servers.tracker]"), "{config}");
        assert!(
            config.contains(&toml_args(&[
                "serve",
                "--profile",
                "codex",
                "--allow-write",
                "--toolsets",
                "issues",
                "--dynamic-toolsets"
            ])),
            "{config}"
        );
        // Сначала страница создания приложения, затем ссылка авторизации этого приложения.
        assert_eq!(
            fx.ui.opened,
            [CLIENT_NEW_URL.to_owned(), authorize_url(CLIENT_ID)]
        );
        let output = fx.ui.text();
        assert!(output.contains("tracker:write"), "{output}");
        assert!(output.contains("verification_code"), "{output}");
        assert!(output.contains("test-user"), "{output}");
        // Токен не выводится и не попадает ни в конфиг клиента, ни в профили.
        assert!(!output.contains("good-token"));
        assert!(!config.contains("good-token"));
        let profiles = fs::read_to_string(fx.repo.path()).unwrap();
        assert!(!profiles.contains("good-token"), "{profiles}");
        assert!(fx.runner.calls.is_empty());
    }

    #[tokio::test]
    async fn rerun_does_not_ask_client_id_and_keeps_working_token() {
        let mut fx = Fixture::new().with_profile("codex");
        fx.answers(&["y"]);
        fx.tokens(&["good-token"]);
        fx.connect(&args(ClientKind::Codex)).await.unwrap();
        // Первый запуск: токена ещё нет — спрашивается только подтверждение записи.
        assert_eq!(fx.ui.questions, ["Write the Codex config?"]);

        let mut fx2_ui = ScriptedUi {
            // «Оставить токен?» — по умолчанию да; затем запись конфига (без изменений).
            answers: ["".to_owned(), "y".to_owned()].into(),
            ..Default::default()
        };
        std::mem::swap(&mut fx.ui, &mut fx2_ui);
        fx.connect(&args(ClientKind::Codex)).await.unwrap();
        assert_eq!(
            fx.ui.questions,
            ["Keep the stored token?", "Write the Codex config?"]
        );
        assert!(fx.ui.tokens.is_empty());
        let text = fx.ui.text();
        assert!(text.contains("from the profile"), "{text}");
        assert!(text.contains("already has a working token"), "{text}");
        assert!(text.contains("already up to date"), "{text}");
        assert!(!text.contains(CLIENT_NEW_URL), "{text}");
    }

    #[tokio::test]
    async fn stored_token_can_be_replaced_or_is_replaced_when_broken() {
        // Пользователь отказался оставить рабочий токен.
        let mut fx = Fixture::new().with_profile("codex");
        fx.verifier.valid_token = "new-token".into();
        fx.store.set("codex", &ApiToken::new("new-token")).unwrap();
        fx.answers(&["n"]);
        fx.tokens(&["new-token"]);
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        // `--yes` оставляет рабочий токен без вопроса.
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.questions.is_empty());
        assert_eq!(fx.ui.tokens.len(), 1);
        args.yes = false;
        fx.answers(&["n", "y"]);
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.tokens.is_empty());
        assert!(fx.ui.text().contains("Token replaced"), "{}", fx.ui.text());

        // Неработающий токен заменяется без вопроса.
        let mut fx = Fixture::new().with_profile("codex");
        fx.store.set("codex", &ApiToken::new("stale")).unwrap();
        fx.tokens(&["good-token"]);
        fx.answers(&["y"]);
        fx.connect(&args).await.unwrap();
        assert_eq!(fx.store.get("codex").unwrap().expose(), "good-token");
        assert_eq!(fx.ui.questions, ["Write the Codex config?"]);
        let text = fx.ui.text();
        assert!(text.contains("does not work"), "{text}");
        assert!(!text.contains("stale"), "{text}");
    }

    #[tokio::test]
    async fn access_flag_combinations() {
        let cases: [(bool, bool, &[&str]); 4] = [
            (false, false, &[]),
            (true, false, &["--allow-write"]),
            (false, true, &["--allow-write", "--allow-destructive"]),
            (true, true, &["--allow-write", "--allow-destructive"]),
        ];
        for (write, destructive, flags) in cases {
            let mut fx = Fixture::new().with_profile("codex");
            fx.tokens(&["good-token"]);
            let mut args = args(ClientKind::Codex);
            args.allow_write = write;
            args.allow_destructive = destructive;
            args.yes = true;
            fx.connect(&args).await.unwrap();

            let settings = fx.repo.get("codex").unwrap().unwrap();
            assert_eq!(settings.allow_write, write || destructive);
            assert_eq!(settings.allow_destructive, destructive);

            let mut expected = vec!["serve", "--profile", "codex"];
            expected.extend_from_slice(flags);
            let config = fx.codex_config().unwrap();
            assert!(config.contains(&toml_args(&expected)), "{config}");
        }
    }

    #[tokio::test]
    async fn ssh_session_only_prints_links() {
        let mut fx = Fixture::new();
        fx.browser = BrowserDecision::Skip(SkipReason::Ssh);
        fx.answers(&[CLIENT_ID, "2", "cloud-org"]);
        fx.tokens(&["good-token"]);
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.opened.is_empty());
        let text = fx.ui.text();
        assert!(text.contains(CLIENT_NEW_URL), "{text}");
        assert!(text.contains(&authorize_url(CLIENT_ID)), "{text}");
        assert_eq!(text.matches("SSH session").count(), 2, "{text}");
        let org = fx.repo.get("codex").unwrap().unwrap().org.unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-ID", "cloud-org"));
    }

    #[tokio::test]
    async fn browser_failure_only_prints_link() {
        let mut fx = Fixture::new().with_profile("codex");
        fx.browser = BrowserDecision::Open;
        fx.ui.open_fails = true;
        fx.tokens(&["good-token"]);
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let text = fx.ui.text();
        assert!(text.contains("Could not open a browser"), "{text}");
        assert!(text.contains(&authorize_url(CLIENT_ID)), "{text}");
        assert!(fx.store.contains("codex").unwrap());
    }

    #[tokio::test]
    async fn declined_config_write_keeps_token_and_profile() {
        let mut fx = Fixture::new().with_profile("codex");
        fx.tokens(&["good-token"]);
        fx.answers(&["n"]);
        fx.connect(&args(ClientKind::Codex)).await.unwrap();
        assert!(fx.codex_config().is_none());
        assert!(fx.store.contains("codex").unwrap());
        assert_eq!(
            fx.repo.get("codex").unwrap().unwrap().client.as_deref(),
            Some("codex")
        );
        assert!(fx.ui.text().contains("config was not changed"));

        // Без ответа (EOF) конфиг тоже не пишется.
        let mut fx = Fixture::new().with_profile("codex");
        fx.tokens(&["good-token"]);
        assert!(fx.connect(&args(ClientKind::Codex)).await.is_err());
        assert!(fx.codex_config().is_none());
    }

    #[tokio::test]
    async fn existing_codex_config_is_preserved() {
        let mut fx = Fixture::new().with_profile("codex");
        fs::create_dir_all(&fx.codex_home).unwrap();
        let existing = "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.yandex_tracker_mcp]\nurl = \"http://localhost:9903/mcp\"  # legacy\n";
        fs::write(codex::config_path(&fx.codex_home), existing).unwrap();
        fx.tokens(&["good-token"]);
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        let config = fx.codex_config().unwrap();
        assert!(config.starts_with(existing), "{config}");
        assert!(config.contains("[mcp_servers.tracker]"), "{config}");
    }

    #[tokio::test]
    async fn invalid_token_is_not_stored_but_client_id_is_kept() {
        let mut fx = Fixture::new();
        fx.answers(&[CLIENT_ID, "1", "42"]);
        fx.tokens(&["bad-token"]);
        let err = fx.connect(&args(ClientKind::Codex)).await.unwrap_err();
        assert!(err.to_string().contains("rejected the token"), "{err}");
        assert!(err.to_string().contains("Nothing was stored"), "{err}");
        assert!(!fx.store.contains("codex").unwrap());
        assert!(fx.codex_config().is_none());
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.client_id.as_deref(), Some(CLIENT_ID));
        assert_eq!(settings.org, None);

        // Повторный запуск не спрашивает ClientID.
        fx.answers(&["1", "42"]);
        fx.tokens(&["good-token"]);
        fx.ui.questions.clear();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert_eq!(
            fx.ui.questions,
            ["Organization type [1/2]: ", "Organization ID: "]
        );
    }

    #[tokio::test]
    async fn malformed_tokens_are_rejected() {
        for (value, expected) in [("  ", "empty token"), ("two words", "does not look like")] {
            let mut fx = Fixture::new().with_profile("codex");
            fx.tokens(&[value]);
            let err = fx.connect(&args(ClientKind::Codex)).await.unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            assert!(!fx.store.contains("codex").unwrap());
        }
    }

    #[tokio::test]
    async fn skip_check_stores_unverified_token() {
        let mut fx = Fixture::new().with_profile("codex-ci");
        fx.skip_check = true;
        fx.tokens(&["unchecked-token"]);
        let mut args = args(ClientKind::Codex);
        args.skip_check = true;
        args.yes = true;
        args.profile = Some("codex-ci".into());
        fx.connect(&args).await.unwrap();
        assert_eq!(
            fx.store.get("codex-ci").unwrap().expose(),
            "unchecked-token"
        );
        assert!(fx.ui.questions.is_empty());
        assert!(fx.ui.text().contains("--skip-check"));
        let config = fx.codex_config().unwrap();
        assert!(
            config.contains(&toml_args(&["serve", "--profile", "codex-ci"])),
            "{config}"
        );
    }

    #[tokio::test]
    async fn flags_replace_questions() {
        let mut fx = Fixture::new();
        fx.tokens(&["good-token"]);
        let mut args = args(ClientKind::Codex);
        args.client_id = Some(CLIENT_ID.to_uppercase());
        args.cloud_org_id = Some("cloud-org".into());
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.ui.questions.is_empty());
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.client_id.as_deref(), Some(CLIENT_ID));
        assert_eq!(
            settings.org.unwrap().header(),
            ("X-Cloud-Org-ID", "cloud-org")
        );
    }

    #[tokio::test]
    async fn claude_flow_with_custom_name_and_project_scope() {
        let mut fx = Fixture::new().with_profile("claude-code");
        fx.store
            .set("claude-code", &ApiToken::new("good-token"))
            .unwrap();
        fx.answers(&["", "y"]);
        let mut args = args(ClientKind::ClaudeCode);
        args.scope = Scope::Project;
        args.name = "tracker-work".into();
        fx.connect(&args).await.unwrap();

        assert!(fx.ui.tokens.is_empty());
        assert_eq!(
            fx.runner.calls,
            [
                vec!["--version".to_owned()],
                claude::add_args(
                    Scope::Project,
                    &ServerSpec {
                        name: "tracker-work".into(),
                        command: TEST_EXE.into(),
                        args: vec!["serve".into(), "--profile".into(), "claude-code".into()],
                    }
                )
            ]
        );
        let settings = fx.repo.get("claude-code").unwrap().unwrap();
        assert_eq!(settings.client.as_deref(), Some("claude-code"));
        assert!(fx.codex_config().is_none());
    }

    /// `connect --iam`: без OAuth-приложения и токена в хранилище; `yc` — подменный.
    #[cfg(unix)]
    #[tokio::test]
    async fn iam_connect_saves_profile_without_token() {
        let yc = crate::login::testing::FakeYc::new();
        let mut fx = Fixture::new();
        fx.verifier.valid_token = "iam-token-1".into();
        let mut args = args(ClientKind::Codex);
        args.iam = true;
        args.yc_path = Some(yc.path());
        args.cloud_org_id = Some("cloud-org".into());
        args.yes = true;
        fx.connect(&args).await.unwrap();

        assert_eq!(yc.calls(), 1);
        assert!(!fx.store.contains("codex").unwrap());
        assert!(fx.ui.questions.is_empty());
        assert!(fx.ui.opened.is_empty());
        let settings = fx.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.auth, AuthKind::Iam);
        assert_eq!(settings.client_id, None);
        assert_eq!(settings.yc_path, Some(yc.path()));
        assert_eq!(
            settings.org.unwrap().header(),
            ("X-Cloud-Org-ID", "cloud-org")
        );
        let config = fx.codex_config().unwrap();
        assert!(
            config.contains(&toml_args(&["serve", "--profile", "codex"])),
            "{config}"
        );
        let text = fx.ui.text();
        assert!(text.contains("IAM token works"), "{text}");
        assert!(!text.contains("iam-token-1"), "{text}");
        assert!(!config.contains("iam-token"), "{config}");

        // Повторный запуск без --iam: профиль остаётся на IAM, организация из профиля.
        let mut args = args.clone();
        args.iam = false;
        args.yc_path = None;
        args.cloud_org_id = None;
        fx.verifier.valid_token = "iam-token-2".into();
        fx.connect(&args).await.unwrap();
        assert_eq!(yc.calls(), 2);
        assert!(
            fx.ui.text().contains("(from the profile)"),
            "{}",
            fx.ui.text()
        );
    }

    #[tokio::test]
    async fn iam_connect_validates_flags_before_questions() {
        let mut fx = Fixture::new();
        let mut yc_without_iam = args(ClientKind::Codex);
        yc_without_iam.yc_profile = Some("work".into());
        let err = fx.connect(&yc_without_iam).await.unwrap_err();
        assert!(err.to_string().contains("--auth iam"), "{err}");

        let mut bad_profile = args(ClientKind::Codex);
        bad_profile.iam = true;
        bad_profile.yc_profile = Some("-x".into());
        let err = fx.connect(&bad_profile).await.unwrap_err();
        assert!(err.to_string().contains("yc profile"), "{err}");

        let mut org360 = args(ClientKind::Codex);
        org360.iam = true;
        org360.org_id = Some("42".into());
        let err = fx.connect(&org360).await.unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");

        assert!(fx.ui.output.is_empty());
        assert!(fx.ui.questions.is_empty());
        assert_eq!(fx.repo.get("codex").unwrap(), None);
    }

    #[tokio::test]
    async fn claude_missing_cli_fails_before_questions() {
        let mut fx = Fixture::new();
        fx.runner.results = [claude::CommandOutput {
            success: false,
            stderr: "broken".into(),
            ..Default::default()
        }]
        .into();
        let err = fx.connect(&args(ClientKind::ClaudeCode)).await.unwrap_err();
        assert!(err.to_string().contains("broken"), "{err}");
        assert!(fx.ui.output.is_empty());
        assert!(fx.ui.questions.is_empty());
    }

    #[tokio::test]
    async fn invalid_input_fails_before_questions() {
        let mut cases = Vec::new();
        let mut codex_project = args(ClientKind::Codex);
        codex_project.scope = Scope::Project;
        cases.push((codex_project, "--scope user"));
        let mut toolset = args(ClientKind::Codex);
        toolset.toolsets = vec!["nope".into()];
        cases.push((toolset, "unknown toolset `nope`"));
        let mut client_id = args(ClientKind::Codex);
        client_id.client_id = Some("abc".into());
        cases.push((client_id, "invalid ClientID"));
        let mut profile = args(ClientKind::Codex);
        profile.profile = Some("--evil".into());
        cases.push((profile, "invalid profile name"));
        let mut name = args(ClientKind::Codex);
        name.name = "a b".into();
        cases.push((name, "invalid server name"));
        let mut org = args(ClientKind::Codex);
        org.org_id = Some("a b".into());
        cases.push((org, "organization ID"));

        for (args, expected) in cases {
            let mut fx = Fixture::new();
            let err = fx.connect(&args).await.unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            assert!(fx.ui.output.is_empty());
            assert_eq!(fx.repo.get("codex").unwrap(), None);
        }
    }
}
