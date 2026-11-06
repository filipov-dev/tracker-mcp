//! Общие шаги `auth login` и `connect`: организация, проверка формы токена, проверка
//! запросом `/myself` и сохранение в системное хранилище.

use anyhow::{Context, anyhow, bail};
use tracker_api::auth::{KEYRING_SERVICE, TokenStore};
use tracker_api::{ApiToken, OrgKind, Organization};

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
