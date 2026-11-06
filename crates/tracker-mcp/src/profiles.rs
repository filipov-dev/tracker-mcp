//! Профили: настройки в `~/.config/tracker-mcp/profiles.toml`, токен — в системном хранилище
//! под именем профиля (OAuth) или от `yc iam create-token` в памяти процесса (IAM).
//!
//! В файле профилей нет токенов и других секретов: только тип авторизации, организация,
//! `ClientID` OAuth-приложения пользователя (он не секретный), путь к `yc` и имя его
//! профиля, настройки доступа по умолчанию.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tracker_api::auth::{AuthError, KeyringStore, TokenStore};
use tracker_api::iam::YcCommand;
use tracker_api::{ApiToken, OrgKind, Organization};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "TRACKER_MCP_CONFIG_DIR";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Профиль по умолчанию.
pub const DEFAULT_PROFILE: &str = "default";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Тип авторизации профиля.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    /// OAuth token from the system keychain.
    #[default]
    Oauth,
    /// Yandex Cloud IAM token from `yc iam create-token`, kept in memory only.
    Iam,
}

impl AuthKind {
    pub fn is_oauth(&self) -> bool {
        *self == Self::Oauth
    }
}

/// Настройки профиля. Токена здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Тип авторизации: OAuth (токен в системном хранилище) или IAM (токен от `yc`, только
    /// в памяти; для федеративных и сервисных аккаунтов). OAuth не записывается в файл.
    #[serde(skip_serializing_if = "AuthKind::is_oauth")]
    pub auth: AuthKind,
    /// IAM: абсолютный путь к `yc`; без него `yc` ищется в `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yc_path: Option<PathBuf>,
    /// IAM: имя профиля `yc` (`yc --profile`); без него — активный профиль `yc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yc_profile: Option<String>,
    /// MCP-клиент, в котором `connect` зарегистрировал профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// `ClientID` OAuth-приложения пользователя, через которое получен токен.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены удаляющие операции (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Наборы инструментов включаются агентом по запросу (`serve --dynamic-toolsets`).
    /// Не записывается, если выключено: файл остаётся читаемым прежними версиями.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_toolsets: bool,
    /// Организация Tracker: тип определяет заголовок (`X-Org-ID` или `X-Cloud-Org-ID`).
    /// Таблица `[profiles.<имя>.org]` с полями `type` и `id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<Organization>,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню: `allow_destructive` включает `allow_write`.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self
    }

    /// Команда `yc` для IAM-токена по настройкам профиля.
    pub fn yc_command(&self) -> anyhow::Result<YcCommand> {
        Ok(YcCommand::new(
            self.yc_path.clone(),
            self.yc_profile.clone(),
        )?)
    }
}

/// IAM-токены выдаются только для организации Yandex Cloud (`X-Cloud-Org-ID`).
pub fn check_iam_org(profile: &str, org: &Organization) -> anyhow::Result<()> {
    if org.kind() != OrgKind::Cloud {
        bail!(
            "profile `{profile}` uses IAM tokens, which work only with a Yandex Cloud \
             organization, but its organization is {org}; set it with \
             `tracker-mcp auth login --profile {profile} --auth iam --cloud-org-id <ID>`"
        );
    }
    Ok(())
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Создаёт или заменяет профиль. Флаги доступа сохраняются нормализованными
    /// (см. [`ProfileSettings::normalized`]).
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document
            .profiles
            .insert(name.to_owned(), settings.normalized());
        self.save(&document)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `TRACKER_MCP_CONFIG_DIR`, иначе `~/.config/tracker-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("tracker-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, ключи TOML и имя
/// записи в keychain, поэтому допускаются только `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Откуда профиль берёт токен.
#[derive(Debug, Clone)]
pub enum ProfileAuth {
    /// OAuth-токен из системного хранилища.
    OAuth(ApiToken),
    /// IAM-токен от `yc`: получается при запросах и держится в памяти.
    Iam(YcCommand),
}

/// Профиль, готовый к работе: настройки, организация и источник токена.
#[derive(Debug, Clone)]
pub struct Profile {
    pub settings: ProfileSettings,
    pub org: Organization,
    pub auth: ProfileAuth,
}

/// Загружает профиль: настройки из `profiles.toml` и токен из системного хранилища (OAuth)
/// или команду `yc` (IAM: хранилище не используется).
///
/// Отсутствие токена или организации — понятная пользователю ошибка с подсказкой, как их
/// сохранить.
// `serve` загружает профиль через `load_profile_with` с явным хранилищем.
#[allow(dead_code)]
pub fn load_profile(name: &str) -> anyhow::Result<Profile> {
    load_profile_with(&ProfileRepo::open_default()?, &KeyringStore, name)
}

/// То же, что [`load_profile`], с явными файлом профилей и хранилищем токенов.
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn TokenStore,
    name: &str,
) -> anyhow::Result<Profile> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    if settings.auth == AuthKind::Iam {
        let Some(org) = settings.org.clone() else {
            bail!(
                "profile `{name}` has no Tracker organization; set it with \
                 `tracker-mcp auth login --profile {name} --auth iam --cloud-org-id <ID>`"
            );
        };
        check_iam_org(name, &org)?;
        let command = settings
            .yc_command()
            .with_context(|| format!("profile `{name}` has invalid yc settings"))?;
        return Ok(Profile {
            settings,
            org,
            auth: ProfileAuth::Iam(command),
        });
    }
    let token = match store.get(name) {
        Ok(token) => token,
        Err(err @ AuthError::NotFound { .. }) => return Err(err.into()),
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("cannot load the token of profile `{name}`")));
        }
    };
    let Some(org) = settings.org.clone() else {
        bail!(
            "profile `{name}` has no Tracker organization; set it with \
             `tracker-mcp auth login --profile {name} --org-id <ID>` (Yandex 360) or \
             `--cloud-org-id <ID>` (Yandex Cloud)"
        );
    };
    Ok(Profile {
        settings,
        org,
        auth: ProfileAuth::OAuth(token),
    })
}

#[cfg(test)]
mod tests {
    use tracker_api::OrgKind;
    use tracker_api::auth::MemoryStore;

    use super::*;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            auth: AuthKind::Oauth,
            yc_path: None,
            yc_profile: None,
            client: Some("codex".into()),
            client_id: Some("0123456789abcdef".into()),
            allow_write: true,
            allow_destructive: false,
            toolsets: vec!["issues".into(), "queues".into()],
            dynamic_toolsets: true,
            org: Some(Organization::new(OrgKind::Cloud, "test-org").unwrap()),
        }
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("codex").unwrap(), None);

        repo.upsert("codex", sample()).unwrap();
        repo.upsert(DEFAULT_PROFILE, ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("codex").unwrap(), Some(sample()));
        assert_eq!(
            repo.get(DEFAULT_PROFILE).unwrap(),
            Some(ProfileSettings::default())
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.codex]"), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");
        assert!(text.contains("[profiles.codex.org]"), "{text}");
        assert!(text.contains("type = \"cloud\""), "{text}");
        assert!(text.contains("dynamic_toolsets = true"), "{text}");
        // Пустые необязательные поля, OAuth по умолчанию и выключенные динамические
        // наборы не записываются.
        assert_eq!(text.matches("client_id").count(), 1, "{text}");
        assert_eq!(text.matches("dynamic_toolsets").count(), 1, "{text}");
        assert!(!text.contains("auth"), "{text}");
        assert!(!text.contains("yc_"), "{text}");

        assert!(repo.remove("codex").unwrap());
        assert!(!repo.remove("codex").unwrap());
        assert_eq!(repo.get("codex").unwrap(), None);
        assert!(repo.get(DEFAULT_PROFILE).unwrap().is_some());
    }

    #[test]
    fn parses_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(
            repo.path(),
            "# comment\n[profiles.ci]\nallow_write = true\n\n[profiles.ci.org]\ntype = \"yandex360\"\nid = \"42\"\n\n[profiles.empty]\n",
        )
        .unwrap();
        let ci = repo.get("ci").unwrap().unwrap();
        assert!(ci.allow_write);
        assert!(!ci.allow_destructive);
        assert!(ci.toolsets.is_empty());
        assert!(!ci.dynamic_toolsets);
        assert_eq!(ci.client_id, None);
        let org = ci.org.unwrap();
        assert_eq!(org.header(), ("X-Org-ID", "42"));
        assert_eq!(repo.get("empty").unwrap(), Some(ProfileSettings::default()));
    }

    #[test]
    fn destructive_implies_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        // Файл, написанный вручную: удаление без записи.
        fs::write(repo.path(), "[profiles.old]\nallow_destructive = true\n").unwrap();
        let old = repo.get("old").unwrap().unwrap();
        assert!(!old.allow_write);
        assert!(old.writes_allowed());

        let settings = ProfileSettings {
            allow_destructive: true,
            ..ProfileSettings::default()
        };
        repo.upsert("new", settings).unwrap();
        let saved = repo.get("new").unwrap().unwrap();
        assert!(saved.allow_write);
        assert!(saved.allow_destructive);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_org() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(repo.path(), "[profiles.ci]\ntoken = \"x\"\n").unwrap();
        assert!(repo.get("ci").is_err());
        fs::write(
            repo.path(),
            "[profiles.ci.org]\ntype = \"yandex360\"\nid = \"a b\"\n",
        )
        .unwrap();
        assert!(repo.get("ci").is_err());
        fs::write(
            repo.path(),
            "[profiles.ci.org]\ntype = \"other\"\nid = \"1\"\n",
        )
        .unwrap();
        assert!(repo.get("ci").is_err());
    }

    #[test]
    fn validates_profile_names() {
        for good in ["default", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn load_profile_requires_token_and_org() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        repo.upsert("codex", sample()).unwrap();

        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile codex"),
            "{err}"
        );

        store.set("codex", &ApiToken::new("t")).unwrap();
        let profile = load_profile_with(&repo, &store, "codex").unwrap();
        assert_eq!(profile.settings, sample());
        assert_eq!(profile.org.header(), ("X-Cloud-Org-ID", "test-org"));
        assert!(matches!(&profile.auth, ProfileAuth::OAuth(token) if token.expose() == "t"));

        // Токен без организации — ошибка с подсказкой.
        store.set("bare", &ApiToken::new("t2")).unwrap();
        let err = load_profile_with(&repo, &store, "bare").unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");
        assert!(!err.to_string().contains("t2"));
    }

    #[test]
    fn iam_profile_needs_cloud_org_but_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let iam = ProfileSettings {
            auth: AuthKind::Iam,
            yc_profile: Some("work".into()),
            yc_path: Some(dir.path().join("yc")),
            org: Some(Organization::new(OrgKind::Cloud, "cloud-org").unwrap()),
            ..ProfileSettings::default()
        };
        repo.upsert("fed", iam.clone()).unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("auth = \"iam\""), "{text}");
        assert!(text.contains("yc_profile = \"work\""), "{text}");
        assert_eq!(repo.get("fed").unwrap(), Some(iam.clone()));

        // Токен в хранилище не нужен и не читается.
        let profile = load_profile_with(&repo, &store, "fed").unwrap();
        let ProfileAuth::Iam(command) = &profile.auth else {
            panic!("ожидался IAM: {:?}", profile.auth);
        };
        assert_eq!(command.profile(), Some("work"));
        assert_eq!(command.program(), dir.path().join("yc"));

        // Организация Yandex 360 с IAM — ошибка.
        let wrong = ProfileSettings {
            org: Some(Organization::new(OrgKind::Yandex360, "1").unwrap()),
            ..iam.clone()
        };
        repo.upsert("fed", wrong).unwrap();
        let err = load_profile_with(&repo, &store, "fed").unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");

        // Без организации и с неверными настройками yc — ошибки с подсказкой.
        let no_org = ProfileSettings {
            org: None,
            ..iam.clone()
        };
        repo.upsert("fed", no_org).unwrap();
        let err = load_profile_with(&repo, &store, "fed").unwrap_err();
        assert!(err.to_string().contains("--auth iam"), "{err}");
        let relative = ProfileSettings {
            yc_path: Some(PathBuf::from("relative/yc")),
            ..iam
        };
        repo.upsert("fed", relative).unwrap();
        let err = load_profile_with(&repo, &store, "fed").unwrap_err();
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");

        fs::write(repo.path(), "[profiles.x]\nauth = \"password\"\n").unwrap();
        assert!(repo.get("x").is_err());
    }
}
