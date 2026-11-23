//! Общие шаги `auth login` и `connect`: организация, проверка формы токена, проверка
//! запросом `/myself` и сохранение в системное хранилище; для IAM — организация Yandex
//! Cloud, настройки `yc` и проверка, что `yc` выдаёт токен, который принимает Tracker.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tracker_api::auth::{KEYRING_SERVICE, TokenStore};
use tracker_api::executor::AuthScheme;
use tracker_api::iam::YcCommand;
use tracker_api::{ApiToken, OrgKind, Organization};

use crate::profiles::ProfileSettings;
use crate::ui::Ui;
use crate::verify::TokenVerifier;

/// Предел длины токена: OAuth-токен Tracker — несколько десятков символов.
const MAX_TOKEN_LEN: usize = 4096;

/// Организация из флагов `--org-id` / `--cloud-org-id`.
pub fn org_from_flags(
    org_id: Option<&str>,
    cloud_org_id: Option<&str>,
) -> anyhow::Result<Option<Organization>> {
    let (kind, id) = match (org_id, cloud_org_id) {
        (Some(_), Some(_)) => bail!("--org-id and --cloud-org-id cannot be used together"),
        (Some(id), None) => (OrgKind::Yandex360, id),
        (None, Some(id)) => (OrgKind::Cloud, id),
        (None, None) => return Ok(None),
    };
    Ok(Some(Organization::new(kind, id)?))
}

/// Спрашивает тип и ID организации.
pub fn ask_org(ui: &mut dyn Ui) -> anyhow::Result<Organization> {
    ui.say("Tracker needs the organization of every request:");
    ui.say("  1) Yandex 360 for Business (sent as X-Org-ID)");
    ui.say("  2) Yandex Cloud Organization (sent as X-Cloud-Org-ID)");
    ui.say("The ID is shown in Tracker on the Administration -> Organizations page.");
    let answer = ui
        .ask("Organization type [1/2]: ")
        .context("failed to read the organization type")?;
    let kind = match answer.to_ascii_lowercase().as_str() {
        "1" | "360" | "yandex360" => OrgKind::Yandex360,
        "2" | "cloud" => OrgKind::Cloud,
        _ => {
            bail!("unknown organization type `{answer}`: answer 1 (Yandex 360) or 2 (Yandex Cloud)")
        }
    };
    let id = ui
        .ask("Organization ID: ")
        .context("failed to read the organization ID")?;
    Ok(Organization::new(kind, id)?)
}

/// Организация профиля: из флагов, иначе из профиля, иначе спрашивается у пользователя.
///
/// `cannot_ask` — текст ошибки, если спрашивать нельзя (ввод не интерактивный).
pub fn resolve_org(
    ui: &mut dyn Ui,
    from_flags: Option<Organization>,
    stored: Option<Organization>,
    cannot_ask: Option<String>,
) -> anyhow::Result<Organization> {
    match (from_flags, stored) {
        (Some(org), _) => Ok(org),
        (None, Some(org)) => {
            ui.say(&format!("Organization: {org} (from the profile)."));
            Ok(org)
        }
        (None, None) => match cannot_ask {
            Some(message) => bail!(message),
            None => ask_org(ui),
        },
    }
}

/// Проверяет форму токена до сохранения. Значение токена в ошибку не попадает.
pub fn validate_token(token: &ApiToken) -> anyhow::Result<()> {
    let value = token.expose();
    if value.is_empty() {
        bail!("empty token; nothing was stored");
    }
    if value.len() > MAX_TOKEN_LEN || !value.chars().all(|c| c.is_ascii_graphic()) {
        bail!(
            "this does not look like a Tracker OAuth token: expected a single line of printable \
             ASCII characters without spaces; nothing was stored"
        );
    }
    Ok(())
}

/// Проверяет новый токен запросом `/myself`; без проверяющего (`--skip-check`) только сообщает
/// об этом. Ошибка проверки означает, что ничего не сохранено.
pub async fn check_new_token<V: TokenVerifier>(
    ui: &mut dyn Ui,
    verifier: Option<&V>,
    token: &ApiToken,
    org: &Organization,
) -> anyhow::Result<()> {
    match verifier {
        Some(verifier) => {
            let user = verifier.current_user(token, org).await.map_err(|err| {
                anyhow!(
                    "{err:#} Nothing was stored. Re-run with --skip-check to store the token \
                     without verification."
                )
            })?;
            ui.say(&format!(
                "Token is valid: signed in to Tracker as `{user}` in {org}."
            ));
        }
        None => ui.say("Skipping the token check (--skip-check)."),
    }
    Ok(())
}

/// Сохраняет токен профиля в системное хранилище, заменяя прежний.
pub fn save_token(
    ui: &mut dyn Ui,
    store: &dyn TokenStore,
    profile: &str,
    token: &ApiToken,
) -> anyhow::Result<()> {
    let replaced = store.contains(profile)?;
    store.set(profile, token)?;
    ui.say(&format!(
        "Token {} in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(())
}

/// Таймаут `yc` в интерактивных командах (`auth login`, `connect`): для федеративного
/// аккаунта `yc` может открыть браузер и ждать входа через SSO.
pub const INTERACTIVE_YC_TIMEOUT: Duration = Duration::from_secs(180);

/// Организация профиля с IAM: только Yandex Cloud. Из `--cloud-org-id`, иначе из профиля,
/// иначе спрашивается ID (тип не спрашивается: он может быть только один).
pub fn resolve_iam_org(
    ui: &mut dyn Ui,
    profile: &str,
    org_id: Option<&str>,
    cloud_org_id: Option<&str>,
    stored: Option<Organization>,
) -> anyhow::Result<Organization> {
    if org_id.is_some() {
        bail!(
            "IAM tokens work only with a Yandex Cloud organization: use --cloud-org-id instead \
             of --org-id"
        );
    }
    if let Some(id) = cloud_org_id {
        return Ok(Organization::new(OrgKind::Cloud, id)?);
    }
    match stored {
        Some(org) if org.kind() == OrgKind::Cloud => {
            ui.say(&format!("Organization: {org} (from the profile)."));
            Ok(org)
        }
        Some(org) => bail!(
            "profile `{profile}` has the organization {org}, but IAM tokens work only with a \
             Yandex Cloud organization: pass --cloud-org-id <ID>"
        ),
        None => {
            ui.say("IAM tokens work with a Yandex Cloud Organization (sent as X-Cloud-Org-ID).");
            ui.say("The ID is shown in Tracker on the Administration -> Organizations page.");
            let id = ui
                .ask("Yandex Cloud Organization ID: ")
                .context("failed to read the organization ID")?;
            Ok(Organization::new(OrgKind::Cloud, id)?)
        }
    }
}

/// Применяет `--yc-path` и `--yc-profile` к настройкам профиля (флаги важнее сохранённого)
/// и возвращает команду `yc`. Ошибка в настройках — до запуска `yc`.
pub fn apply_yc_flags(
    settings: &mut ProfileSettings,
    yc_path: Option<&Path>,
    yc_profile: Option<&str>,
) -> anyhow::Result<YcCommand> {
    if let Some(path) = yc_path {
        settings.yc_path = Some(path.to_owned());
    }
    if let Some(name) = yc_profile {
        settings.yc_profile = Some(name.trim().to_owned());
    }
    settings.yc_command()
}

/// `--yc-path` и `--yc-profile` имеют смысл только для IAM.
pub fn reject_yc_flags_without_iam(
    yc_path: Option<&Path>,
    yc_profile: Option<&str>,
) -> anyhow::Result<()> {
    if yc_path.is_some() || yc_profile.is_some() {
        bail!("--yc-path and --yc-profile are used only with IAM tokens (--auth iam)");
    }
    Ok(())
}

/// Проверяет IAM: `yc` выдаёт токен, и Tracker принимает его (`GET /v3/myself`). Токен
/// нигде не сохраняется и не выводится; без проверяющего (`--skip-check`) `yc` не
/// запускается. Ошибка означает, что ничего не сохранено.
pub async fn check_iam<V: TokenVerifier>(
    ui: &mut dyn Ui,
    verifier: Option<&V>,
    command: &YcCommand,
    org: &Organization,
) -> anyhow::Result<()> {
    let Some(verifier) = verifier else {
        ui.say("Skipping the yc and token check (--skip-check).");
        return Ok(());
    };
    ui.say(&format!(
        "Getting an IAM token with `{}` (it is not shown or stored)...",
        command.display()
    ));
    let issued = command
        .clone()
        .with_timeout(INTERACTIVE_YC_TIMEOUT)
        .run()
        .await
        .map_err(|err| {
            anyhow!(
                "{err}. Nothing was saved. Re-run with --skip-check to save the profile without \
                 the check."
            )
        })?;
    let user = verifier
        .current_user_as(AuthScheme::Iam, &issued.token, org)
        .await
        .map_err(|err| {
            anyhow!(
                "{err:#} Nothing was saved. Re-run with --skip-check to save the profile \
                 without the check."
            )
        })?;
    ui.say(&format!(
        "IAM token works: signed in to Tracker as `{user}` in {org}. tracker-mcp does not store \
         it and gets a new one from yc when needed."
    ));
    Ok(())
}

#[cfg(all(test, unix))]
pub mod testing {
    use std::path::PathBuf;

    /// Пишет исполняемый скрипт через отдельный процесс `sh`: если тест держит
    /// открытый на запись дескриптор, параллельный тест может унаследовать его
    /// при fork, и запуск скрипта падает с «Text file busy» (ETXTBSY).
    pub fn write_script(path: &std::path::Path, content: &str) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(content.as_bytes()).unwrap();
        drop(stdin);
        assert!(child.wait().unwrap().success());
    }

    /// Подменный `yc` во временном каталоге: печатает `iam-token-<номер вызова>`, считает
    /// вызовы и запоминает аргументы; после [`FakeYc::fail`] падает с «секретом» в stderr.
    /// Настоящий `yc` в тестах не запускается.
    pub struct FakeYc {
        dir: tempfile::TempDir,
    }

    impl FakeYc {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("yc");
            write_script(
                &script,
                "#!/bin/sh\ndir=$(dirname \"$0\")\nn=$(cat \"$dir/count\" 2>/dev/null || echo 0)\n\
                 n=$((n + 1))\necho \"$n\" > \"$dir/count\"\necho \"$@\" > \"$dir/args\"\n\
                 if [ -f \"$dir/fail\" ]; then echo 'denied t1.LeakedSecretValue0123456' >&2; exit 1; fi\n\
                 echo \"iam-token-$n\"\n",
            );
            Self { dir }
        }

        pub fn path(&self) -> PathBuf {
            self.dir.path().join("yc")
        }

        pub fn calls(&self) -> u32 {
            std::fs::read_to_string(self.dir.path().join("count"))
                .map(|text| text.trim().parse().unwrap())
                .unwrap_or(0)
        }

        pub fn args(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("args")).unwrap()
        }

        pub fn fail(&self) {
            std::fs::write(self.dir.path().join("fail"), "").unwrap();
        }
    }
}
