//! `tracker-mcp auth` — токены и организации профилей.
//!
//! OAuth: токен принимается только скрытым вводом или из stdin (`--token-stdin`), хранится
//! только в системном хранилище и нигде не печатается. Команды, которая выводит значение
//! токена, нет и быть не должно.
//!
//! IAM (`--auth iam`): токена в хранилище нет; в профиль пишутся тип, организация Yandex
//! Cloud и настройки `yc`, а проверка получает токен через `yc` и сразу его забывает.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use tracker_api::Organization;
use tracker_api::auth::{AuthError, KEYRING_SERVICE, KeyringStore, TokenStore};
use tracker_api::executor::AuthScheme;

use crate::login::{
    apply_yc_flags, check_iam, check_new_token, org_from_flags, reject_yc_flags_without_iam,
    resolve_iam_org, resolve_org, save_token, validate_token,
};
use crate::oauth::{AUTHORIZE_URL, authorize_url};
use crate::profiles::{
    AuthKind, DEFAULT_PROFILE, ProfileRepo, ProfileSettings, check_iam_org, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a Tracker OAuth token and the organization for a profile, or set the profile up
    /// for Yandex Cloud IAM tokens (--auth iam).
    ///
    /// OAuth: the token is read from a hidden prompt (or from stdin with --token-stdin),
    /// checked with GET /v3/myself and saved in the system keychain. It is never written to
    /// profiles.toml. Replaces the stored token if there is one.
    ///
    /// IAM (federated and service accounts of a Yandex Cloud organization): no token is
    /// stored. The profile gets the auth type, the Yandex Cloud organization and the yc
    /// settings; the check gets a token with `yc iam create-token` and calls GET /v3/myself.
    /// The server then gets IAM tokens from yc when needed and keeps them in memory only.
    Login(LoginArgs),
    /// Show the profile, its auth type, organization and the Tracker user of the token.
    ///
    /// The token value is never printed.
    Status(StatusArgs),
    /// Remove the stored token of the profile from the system keychain.
    Logout(LogoutArgs),
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Yandex 360 for Business organization ID (sent as X-Org-ID).
    #[arg(long, value_name = "ID", conflicts_with = "cloud_org_id")]
    pub org_id: Option<String>,
    /// Yandex Cloud Organization ID (sent as X-Cloud-Org-ID).
    #[arg(long, value_name = "ID")]
    pub cloud_org_id: Option<String>,
    /// Read the token from stdin instead of a hidden prompt.
    ///
    /// The organization is not asked interactively in this mode: pass --org-id or
    /// --cloud-org-id unless the profile already has one.
    #[arg(long)]
    pub token_stdin: bool,
    /// Store the token without checking it against the Tracker API (with IAM: without
    /// running yc).
    #[arg(long)]
    pub skip_check: bool,
    /// Auth type: `oauth` (a token in the system keychain) or `iam` (a Yandex Cloud IAM token
    /// from yc, for federated and service accounts). Default: the type saved in the
    /// profile, otherwise oauth.
    #[arg(long, value_enum, value_name = "TYPE")]
    pub auth: Option<AuthKind>,
    /// Same as --auth iam.
    #[arg(long, conflicts_with = "auth")]
    pub iam: bool,
    /// IAM: absolute path to the yc CLI. Default: yc from PATH.
    #[arg(long, value_name = "PATH")]
    pub yc_path: Option<PathBuf>,
    /// IAM: yc profile to get tokens with (yc --profile). Default: the active yc profile.
    #[arg(long, value_name = "NAME")]
    pub yc_profile: Option<String>,
}

impl LoginArgs {
    /// Тип авторизации: флаги, иначе сохранённый в профиле.
    fn auth_kind(&self, settings: &ProfileSettings) -> AuthKind {
        if self.iam {
            AuthKind::Iam
        } else {
            self.auth.unwrap_or(settings.auth)
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Do not query the Tracker API for the current user.
    #[arg(long)]
    pub skip_check: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LogoutArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Also remove the profile settings from profiles.toml.
    #[arg(long)]
    pub forget: bool,
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    let mut ui = TerminalUi;
    match command {
        AuthCommand::Login(args) => {
            let verifier = (!args.skip_check).then(ApiVerifier::default);
            login(&store, &repo, verifier.as_ref(), &mut ui, &args).await
        }
        AuthCommand::Status(args) => {
            let verifier = (!args.skip_check).then(ApiVerifier::default);
            status(&store, &repo, verifier.as_ref(), &mut ui, &args.profile).await
        }
        AuthCommand::Logout(args) => logout(&store, &repo, &mut ui, &args.profile, args.forget),
    }
}

/// Подсказка, где взять токен.
fn token_hint(settings: &ProfileSettings) -> String {
    match &settings.client_id {
        Some(client_id) => format!(
            "Get a token from your OAuth application: {}",
            authorize_url(client_id)
        ),
        None => format!(
            "Get an OAuth token from your own Yandex OAuth application with Tracker access: \
             {AUTHORIZE_URL}?response_type=token&client_id=<ClientID> \
             (`tracker-mcp connect` helps to create the application)"
        ),
    }
}

pub async fn login<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
) -> anyhow::Result<()> {
    let profile = args.profile.as_str();
    validate_profile_name(profile)?;
    let mut settings = repo.get(profile)?.unwrap_or_default();
    if args.auth_kind(&settings) == AuthKind::Iam {
        return login_iam(store, repo, verifier, ui, args, settings).await;
    }
    reject_yc_flags_without_iam(args.yc_path.as_deref(), args.yc_profile.as_deref())?;

    let from_flags = org_from_flags(args.org_id.as_deref(), args.cloud_org_id.as_deref())?;
    let cannot_ask = args.token_stdin.then(|| {
        format!(
            "profile `{profile}` has no organization: pass --org-id <ID> (Yandex 360) or \
             --cloud-org-id <ID> (Yandex Cloud) together with --token-stdin"
        )
    });
    let org = resolve_org(ui, from_flags, settings.org.clone(), cannot_ask)?;

    let token = if args.token_stdin {
        ui.read_token_stdin()
            .context("failed to read the token from stdin")?
    } else {
        ui.say(&token_hint(&settings));
        ui.read_token(&format!(
            "Paste the token for profile `{profile}` (input is hidden): "
        ))
        .context("failed to read the token")?
    };
    validate_token(&token)?;
    check_new_token(ui, verifier, &token, &org).await?;
    save_token(ui, store, profile, &token)?;

    // Профиль, который был на IAM, переходит на OAuth: настройки yc больше не нужны.
    settings.auth = AuthKind::Oauth;
    settings.yc_path = None;
    settings.yc_profile = None;
    settings.org = Some(org);
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    Ok(())
}

/// `auth login --auth iam`: тип, организация Yandex Cloud и настройки `yc` в профиль, без
/// токена. Системное хранилище не нужно (сервисный аккаунт на сервере без Secret Service).
async fn login_iam<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
    mut settings: ProfileSettings,
) -> anyhow::Result<()> {
    let profile = args.profile.as_str();
    if args.token_stdin {
        bail!("--token-stdin is not used with IAM tokens: tracker-mcp gets them from yc");
    }
    let command = apply_yc_flags(
        &mut settings,
        args.yc_path.as_deref(),
        args.yc_profile.as_deref(),
    )?;
    let org = resolve_iam_org(
        ui,
        profile,
        args.org_id.as_deref(),
        args.cloud_org_id.as_deref(),
        settings.org.clone(),
    )?;
    check_iam(ui, verifier, &command, &org).await?;

    settings.auth = AuthKind::Iam;
    settings.org = Some(org);
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}: IAM tokens from `{}`, no token is stored.",
        repo.path().display(),
        command.display()
    ));
    // Ошибка хранилища здесь не важна: для IAM оно не нужно.
    if matches!(store.contains(profile), Ok(true)) {
        ui.say(&format!(
            "An OAuth token of profile `{profile}` is still in the system keychain; it is not \
             used with IAM. Remove it with `tracker-mcp auth logout --profile {profile}`."
        ));
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub async fn status<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let settings = repo.get(profile)?;
    let org = settings.as_ref().and_then(|settings| settings.org.clone());
    let kind = settings.as_ref().map_or(AuthKind::Oauth, |s| s.auth);

    ui.say(&format!("Profile:   {profile}"));
    let user = match kind {
        AuthKind::Oauth => status_oauth(store, verifier, ui, profile, org.as_ref()).await?,
        AuthKind::Iam => {
            let settings = settings.clone().unwrap_or_default();
            status_iam(verifier, ui, profile, &settings, org.as_ref()).await
        }
    };
    ui.say(&format!(
        "Org:       {}",
        org.as_ref()
            .map_or_else(|| "not set".to_owned(), Organization::to_string)
    ));
    ui.say(&format!("User:      {user}"));
    match &settings {
        Some(settings) => {
            ui.say(&format!(
                "Client ID: {}",
                settings.client_id.as_deref().unwrap_or("-")
            ));
            ui.say(&format!(
                "Client:    {}",
                settings.client.as_deref().unwrap_or("-")
            ));
            // Действующий уровень, как у `serve`: удаление подразумевает запись.
            ui.say(&format!("Write:     {}", yes_no(settings.writes_allowed())));
            ui.say(&format!(
                "Delete:    {}",
                yes_no(settings.allow_destructive)
            ));
            let toolsets = if settings.toolsets.is_empty() {
                "default".to_owned()
            } else {
                settings.toolsets.join(",")
            };
            ui.say(&format!("Toolsets:  {toolsets}"));
        }
        None => ui.say("Settings:  none (read-only defaults)"),
    }
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    Ok(())
}

/// Строки `Auth` и `Token` профиля с OAuth; возвращает строку `User`.
async fn status_oauth<V: TokenVerifier>(
    store: &dyn TokenStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    org: Option<&Organization>,
) -> anyhow::Result<String> {
    let token = match store.get(profile) {
        Ok(token) => Some(token),
        Err(AuthError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };
    ui.say("Auth:      OAuth");
    ui.say(&format!(
        "Token:     {}",
        if token.is_some() {
            format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
        } else {
            "not found".to_owned()
        }
    ));
    Ok(match (&token, org, verifier) {
        (Some(token), Some(org), Some(verifier)) => match verifier.current_user(token, org).await {
            Ok(user) => user.to_string(),
            Err(err) => format!("check failed: {err:#}"),
        },
        (Some(_), Some(_), None) => "not checked (--skip-check)".to_owned(),
        _ => "-".to_owned(),
    })
}

/// Строки `Auth` и `Token` профиля с IAM; возвращает строку `User`. Системное хранилище не
/// читается; с проверкой токен получается через `yc` и сразу забывается.
async fn status_iam<V: TokenVerifier>(
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    settings: &ProfileSettings,
    org: Option<&Organization>,
) -> String {
    let command = settings.yc_command();
    match &command {
        Ok(command) => ui.say(&format!(
            "Auth:      IAM (Yandex Cloud IAM token from `{}`)",
            command.display()
        )),
        Err(err) => ui.say(&format!("Auth:      IAM (invalid yc settings: {err:#})")),
    }
    ui.say("Token:     not stored; obtained from yc when needed and kept in memory only");
    let (Ok(command), Some(org)) = (command, org) else {
        return "-".to_owned();
    };
    if let Err(err) = check_iam_org(profile, org) {
        return format!("check failed: {err:#}");
    }
    let Some(verifier) = verifier else {
        return "not checked (--skip-check)".to_owned();
    };
    let issued = match command.run().await {
        Ok(issued) => issued,
        Err(err) => return format!("check failed: {err}"),
    };
    match verifier
        .current_user_as(AuthScheme::Iam, &issued.token, org)
        .await
    {
        Ok(user) => user.to_string(),
        Err(err) => format!("check failed: {err:#}"),
    }
}

pub fn logout(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let iam = repo
        .get(profile)?
        .is_some_and(|settings| settings.auth == AuthKind::Iam);
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "Token of profile `{profile}` removed from the system keychain. If it is no longer \
             needed anywhere, also revoke it in your Yandex ID account."
        )),
        Err(AuthError::NotFound { .. }) => {
            ui.say(&format!("No token stored for profile `{profile}`."));
        }
        // Профилю с IAM хранилище не нужно: его недоступность — не ошибка.
        Err(AuthError::Unavailable(_)) if iam => {}
        Err(err) => return Err(err.into()),
    }
    if iam {
        ui.say(&format!(
            "Profile `{profile}` uses IAM tokens from yc, which tracker-mcp never stores. To stop \
             using it, remove the profile with --forget or the MCP server from the client."
        ));
    }
    if forget {
        if repo.remove(profile)? {
            ui.say(&format!(
                "Profile `{profile}` removed from {}.",
                repo.path().display()
            ));
        } else {
            ui.say(&format!("Profile `{profile}` has no saved settings."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tracker_api::auth::MemoryStore;
    use tracker_api::{ApiToken, OrgKind};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::MYSELF_PATH;
    use crate::verify::testing::FakeVerifier;

    fn verifier() -> FakeVerifier {
        FakeVerifier {
            valid_token: "good-token".into(),
        }
    }

    fn args(profile: &str) -> LoginArgs {
        LoginArgs {
            profile: profile.into(),
            org_id: None,
            cloud_org_id: None,
            token_stdin: false,
            skip_check: false,
            auth: None,
            iam: false,
            yc_path: None,
            yc_profile: None,
        }
    }

    fn setup() -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        (dir, repo, MemoryStore::new())
    }

    #[tokio::test]
    async fn login_status_logout() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        assert!(ui.text().contains("Token:     not found"), "{}", ui.text());
        assert!(ui.text().contains("Org:       not set"), "{}", ui.text());

        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            ..Default::default()
        };
        let login_args = LoginArgs {
            org_id: Some("12345".into()),
            ..args("ci")
        };
        login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
            .await
            .unwrap();
        assert!(ui.questions.is_empty());
        assert!(
            ui.text()
                .contains("signed in to Tracker as `test-user (Test User)`")
        );
        assert!(ui.text().contains("Token saved"), "{}", ui.text());
        assert_eq!(store.get("ci").unwrap().expose(), "good-token");
        let saved = repo.get("ci").unwrap().unwrap();
        assert_eq!(saved.org.unwrap().header(), ("X-Org-ID", "12345"));
        assert!(!saved.allow_write);
        // Токен в файл профилей не пишется.
        let file = std::fs::read_to_string(repo.path()).unwrap();
        assert!(!file.contains("good-token"), "{file}");

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Token:     stored"), "{text}");
        assert!(
            text.contains("Org:       Yandex 360 `12345` (X-Org-ID)"),
            "{text}"
        );
        assert!(text.contains("User:      test-user (Test User)"), "{text}");
        assert!(text.contains("Write:     no"), "{text}");
        assert!(!text.contains("good-token"), "{text}");

        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "ci", false).unwrap();
        assert!(!store.contains("ci").unwrap());
        assert!(repo.get("ci").unwrap().is_some());

        // Повторный logout — не ошибка; --forget удаляет настройки.
        logout(&store, &repo, &mut ui, "ci", true).unwrap();
        assert!(ui.text().contains("No token stored"));
        assert_eq!(repo.get("ci").unwrap(), None);
    }

    #[tokio::test]
    async fn login_asks_org_interactively() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            answers: ["2".to_owned(), " cloud-org ".to_owned()].into(),
            ..Default::default()
        };
        login(
            &store,
            &repo,
            Some(&verifier()),
            &mut ui,
            &args(DEFAULT_PROFILE),
        )
        .await
        .unwrap();
        assert_eq!(ui.questions.len(), 2);
        let org = repo.get(DEFAULT_PROFILE).unwrap().unwrap().org.unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-ID", "cloud-org"));

        // Неизвестный тип — ошибка, ничего не сохраняется.
        let mut ui = ScriptedUi {
            answers: ["3".to_owned()].into(),
            ..Default::default()
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &args("other"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown organization type"),
            "{err}"
        );
        assert!(!store.contains("other").unwrap());
    }

    #[tokio::test]
    async fn token_stdin_keeps_settings_and_replaces_token() {
        let (_dir, repo, store) = setup();
        let settings = ProfileSettings {
            client_id: Some("abc123".into()),
            allow_write: true,
            toolsets: vec!["issues".into()],
            org: Some(Organization::new(OrgKind::Yandex360, "1").unwrap()),
            ..Default::default()
        };
        repo.upsert("codex", settings.clone()).unwrap();
        store.set("codex", &ApiToken::new("old")).unwrap();

        let mut ui = ScriptedUi {
            stdin_token: Some("new-token\n".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            token_stdin: true,
            skip_check: true,
            ..args("codex")
        };
        login::<FakeVerifier>(&store, &repo, None, &mut ui, &login_args)
            .await
            .unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "new-token");
        assert_eq!(repo.get("codex").unwrap(), Some(settings));
        let text = ui.text();
        assert!(text.contains("Token replaced"), "{text}");
        assert!(text.contains("--skip-check"), "{text}");
        assert!(ui.questions.is_empty());

        // Флаг меняет организацию сохранённого профиля.
        let mut ui = ScriptedUi {
            stdin_token: Some("new-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("moved".into()),
            ..login_args
        };
        login::<FakeVerifier>(&store, &repo, None, &mut ui, &login_args)
            .await
            .unwrap();
        let org = repo.get("codex").unwrap().unwrap().org.unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-ID", "moved"));
    }

    #[tokio::test]
    async fn token_stdin_requires_org() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi {
            stdin_token: Some("good-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            token_stdin: true,
            ..args("ci")
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--org-id"), "{err}");
        assert!(!store.contains("ci").unwrap());
    }

    #[tokio::test]
    async fn login_rejects_invalid_tokens() {
        let (_dir, repo, store) = setup();
        let login_args = LoginArgs {
            org_id: Some("1".into()),
            ..args("ci")
        };
        for (value, expected) in [
            ("bad", "rejected the token"),
            ("", "empty token"),
            ("two words", "does not look like"),
        ] {
            let mut ui = ScriptedUi {
                tokens: [value.to_owned()].into(),
                ..Default::default()
            };
            let err = login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            if !value.is_empty() {
                assert!(!err.to_string().contains(value), "{err}");
            }
        }
        assert!(!store.contains("ci").unwrap());
        assert_eq!(repo.get("ci").unwrap(), None);

        // Неверный ID организации отклоняется до чтения токена.
        let mut ui = ScriptedUi::default();
        let bad_org = LoginArgs {
            org_id: Some("a b".into()),
            ..args("ci")
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &bad_org)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("organization ID"), "{err}");
    }

    #[tokio::test]
    async fn status_reports_failed_check_and_effective_access() {
        let (_dir, repo, store) = setup();
        std::fs::write(
            repo.path(),
            "[profiles.old]\nallow_destructive = true\n\n[profiles.old.org]\ntype = \"cloud\"\nid = \"o\"\n",
        )
        .unwrap();
        store.set("old", &ApiToken::new("stale-value")).unwrap();
        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "old")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("User:      check failed"), "{text}");
        assert!(text.contains("Write:     yes"), "{text}");
        assert!(text.contains("Delete:    yes"), "{text}");
        assert!(!text.contains("stale-value"), "{text}");

        let mut ui = ScriptedUi::default();
        status::<FakeVerifier>(&store, &repo, None, &mut ui, "old")
            .await
            .unwrap();
        assert!(ui.text().contains("not checked (--skip-check)"));
    }

    /// Полный сценарий с настоящим HTTP-клиентом против локального wiremock.
    #[tokio::test]
    async fn login_and_status_against_mock_tracker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "OAuth mock-token"))
            .and(header("x-cloud-org-id", "mock-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "login": "jdoe",
                "display": "John Doe"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());
        let (_dir, repo, store) = setup();

        let mut ui = ScriptedUi {
            stdin_token: Some("mock-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("mock-org".into()),
            token_stdin: true,
            ..args(DEFAULT_PROFILE)
        };
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        assert!(ui.text().contains("`jdoe (John Doe)`"), "{}", ui.text());

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier), &mut ui, DEFAULT_PROFILE)
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("User:      jdoe (John Doe)"), "{text}");
        assert!(text.contains("Auth:      OAuth"), "{text}");
        assert!(!text.contains("mock-token"), "{text}");
    }

    #[cfg(unix)]
    use crate::login::testing::FakeYc;

    #[cfg(unix)]
    fn iam_args(profile: &str, yc: &FakeYc) -> LoginArgs {
        LoginArgs {
            iam: true,
            yc_path: Some(yc.path()),
            yc_profile: Some("work".into()),
            cloud_org_id: Some("cloud-org".into()),
            ..args(profile)
        }
    }

    /// `auth login --iam` с подменным `yc` и wiremock: токен не сохраняется, в профиль
    /// пишутся тип, организация и настройки yc; `auth status` показывает тип и пользователя.
    #[cfg(unix)]
    #[tokio::test]
    async fn iam_login_and_status_against_fake_yc() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "Bearer iam-token-1"))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "fed-user"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(MYSELF_PATH))
            .and(header("authorization", "Bearer iam-token-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "fed-user"})))
            .expect(1)
            .mount(&server)
            .await;
        let api = ApiVerifier::with_base_url(server.uri());
        let (_dir, repo, store) = setup();
        let yc = FakeYc::new();

        let mut ui = ScriptedUi::default();
        login(&store, &repo, Some(&api), &mut ui, &iam_args("fed", &yc))
            .await
            .unwrap();
        assert_eq!(yc.calls(), 1);
        assert_eq!(
            yc.args().trim(),
            "iam create-token --profile work --format json"
        );
        let text = ui.text();
        assert!(
            text.contains("signed in to Tracker as `fed-user`"),
            "{text}"
        );
        assert!(!text.contains("iam-token"), "{text}");
        assert!(ui.questions.is_empty());
        assert!(!store.contains("fed").unwrap());
        let saved = repo.get("fed").unwrap().unwrap();
        assert_eq!(saved.auth, AuthKind::Iam);
        assert_eq!(saved.yc_profile.as_deref(), Some("work"));
        assert_eq!(saved.yc_path, Some(yc.path()));
        assert_eq!(saved.org.unwrap().header(), ("X-Cloud-Org-ID", "cloud-org"));
        let file = std::fs::read_to_string(repo.path()).unwrap();
        assert!(!file.contains("iam-token"), "{file}");

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&api), &mut ui, "fed")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Auth:      IAM"), "{text}");
        assert!(text.contains("--profile work`"), "{text}");
        assert!(text.contains("Token:     not stored"), "{text}");
        assert!(text.contains("User:      fed-user"), "{text}");
        assert!(!text.contains("iam-token"), "{text}");
        assert_eq!(yc.calls(), 2);

        // Без проверки yc не запускается.
        let mut ui = ScriptedUi::default();
        status::<ApiVerifier>(&store, &repo, None, &mut ui, "fed")
            .await
            .unwrap();
        assert!(ui.text().contains("not checked"), "{}", ui.text());
        assert_eq!(yc.calls(), 2);

        // Повторный login без флагов остаётся на IAM (тип из профиля) и ничего не спрашивает.
        let mut ui = ScriptedUi::default();
        let rerun = LoginArgs {
            skip_check: true,
            ..args("fed")
        };
        login::<ApiVerifier>(&store, &repo, None, &mut ui, &rerun)
            .await
            .unwrap();
        assert!(ui.questions.is_empty());
        assert!(ui.tokens.is_empty());
        assert_eq!(repo.get("fed").unwrap().unwrap().auth, AuthKind::Iam);

        // `--auth oauth` переводит профиль обратно на OAuth и убирает настройки yc.
        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            ..Default::default()
        };
        let back = LoginArgs {
            auth: Some(AuthKind::Oauth),
            ..args("fed")
        };
        login(&store, &repo, Some(&verifier()), &mut ui, &back)
            .await
            .unwrap();
        let saved = repo.get("fed").unwrap().unwrap();
        assert_eq!(saved.auth, AuthKind::Oauth);
        assert_eq!(saved.yc_profile, None);
        assert_eq!(saved.yc_path, None);
        assert_eq!(store.get("fed").unwrap().expose(), "good-token");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn iam_login_failures_save_nothing() {
        let (_dir, repo, store) = setup();
        let yc = FakeYc::new();
        let verifier = verifier();

        // yc падает: ничего не сохраняется, stderr yc в ошибке замаскирован.
        yc.fail();
        let mut ui = ScriptedUi::default();
        let err = login(
            &store,
            &repo,
            Some(&verifier),
            &mut ui,
            &iam_args("fed", &yc),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Nothing was saved"), "{err}");
        assert!(err.contains("denied ***"), "{err}");
        assert!(!err.contains("LeakedSecret"), "{err}");
        assert_eq!(repo.get("fed").unwrap(), None);

        // Tracker не принимает токен: ничего не сохраняется, значения токена в ошибке нет.
        let yc = FakeYc::new();
        let mut ui = ScriptedUi::default();
        let err = login(
            &store,
            &repo,
            Some(&verifier),
            &mut ui,
            &iam_args("fed", &yc),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("rejected the IAM token"), "{err}");
        assert!(!err.contains("iam-token"), "{err}");
        assert_eq!(repo.get("fed").unwrap(), None);
    }

    #[tokio::test]
    async fn iam_login_requires_cloud_org() {
        let (_dir, repo, store) = setup();
        let iam = LoginArgs {
            iam: true,
            skip_check: true,
            ..args("fed")
        };
        // --org-id (Yandex 360) с IAM — ошибка до запуска yc.
        let mut ui = ScriptedUi::default();
        let err = login::<FakeVerifier>(
            &store,
            &repo,
            None,
            &mut ui,
            &LoginArgs {
                org_id: Some("42".into()),
                ..iam.clone()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");

        // Организация Yandex 360 в профиле — тоже ошибка.
        repo.upsert(
            "fed",
            ProfileSettings {
                org: Some(Organization::new(OrgKind::Yandex360, "42").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = login::<FakeVerifier>(&store, &repo, None, &mut ui, &iam)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Yandex Cloud organization"),
            "{err}"
        );

        // Без организации спрашивается только ID организации Yandex Cloud.
        let mut ui = ScriptedUi {
            answers: ["cloud-org".to_owned()].into(),
            ..Default::default()
        };
        login::<FakeVerifier>(
            &store,
            &repo,
            None,
            &mut ui,
            &LoginArgs {
                profile: "fresh".into(),
                ..iam.clone()
            },
        )
        .await
        .unwrap();
        assert_eq!(ui.questions, ["Yandex Cloud Organization ID: "]);
        let saved = repo.get("fresh").unwrap().unwrap();
        assert_eq!(saved.auth, AuthKind::Iam);
        assert_eq!(saved.org.unwrap().header(), ("X-Cloud-Org-ID", "cloud-org"));
        assert!(ui.text().contains("--skip-check"), "{}", ui.text());

        // --token-stdin и флаги yc без IAM — ошибки.
        let err = login::<FakeVerifier>(
            &store,
            &repo,
            None,
            &mut ui,
            &LoginArgs {
                token_stdin: true,
                ..iam.clone()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--token-stdin"), "{err}");
        let err = login::<FakeVerifier>(
            &store,
            &repo,
            None,
            &mut ui,
            &LoginArgs {
                yc_profile: Some("work".into()),
                org_id: Some("1".into()),
                ..args("other")
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--auth iam"), "{err}");
        let err = login::<FakeVerifier>(
            &store,
            &repo,
            None,
            &mut ui,
            &LoginArgs {
                yc_profile: Some("--format".into()),
                ..iam
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("yc profile"), "{err}");
    }

    #[tokio::test]
    async fn logout_of_iam_profile_explains_that_nothing_is_stored() {
        let (_dir, repo, store) = setup();
        repo.upsert(
            "fed",
            ProfileSettings {
                auth: AuthKind::Iam,
                org: Some(Organization::new(OrgKind::Cloud, "o").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "fed", false).unwrap();
        assert!(ui.text().contains("uses IAM tokens"), "{}", ui.text());
        assert!(repo.get("fed").unwrap().is_some());
    }
}
