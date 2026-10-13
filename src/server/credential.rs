//! 插件自己的凭据（protocol-v1 §6.2）：宿主只管存和还，内容由插件定。重新上线要的全在里面——本人授权（访问令牌、
//! 刷新令牌、过期时刻、权限），以及发出这份授权的 Slack 应用（续期要用同一个 Client ID；新登录一律是 HHH 的应用，
//! 不记）。另记上次核验的账号与工作区名字：账号只用来核对，工作区名字卡片上要写（不必每次上线都读一遍）。
//!
//! 附录 D 的交接（`hhh-builtin-v1`）也在这里换成插件的凭据：HHH 内置 Slack 的授权与连接设置那一栏
//! （`clientId`、`accountName`、`workspaceName`…）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AuthGrant, Result};

/// 凭据格式的版本。
const VERSION: u32 = 1;
/// 离过期不足 5 分钟就换新（同 Friend 的 `REFRESH_MARGIN_SECS`：覆盖一次查询的最长耗时，又不会每次请求都续）。
pub(crate) const REFRESH_MARGIN_SECS: u64 = 300;

/// 上次核验的账号（只用来核对与写日志）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KnownAccount {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

/// 插件的凭据。不实现给人看的 Debug：令牌不进日志。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Credential {
    version: u32,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// 访问令牌的过期时刻（Unix 秒，同内置时代）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
    /// 发出这份授权的 Slack 应用：旧版本里用户自己填的 Client ID（交接来的）；HHH 的应用不写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<KnownAccount>,
    /// 工作区名字（卡片写“名字 · 工作区”）；读不到时不写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("tokens", &"[已隐藏]")
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl Credential {
    /// 用一份授权拼凭据；`client_id` 为空是 HHH 的应用。
    pub(crate) fn new(grant: &AuthGrant, client_id: Option<String>) -> Self {
        Self {
            version: VERSION,
            access_token: grant.access_token().to_owned(),
            refresh_token: grant.refresh_token().map(str::to_owned),
            expires_at: grant.expires_at(),
            scopes: grant.scopes().map(<[String]>::to_vec),
            client_id,
            account: None,
            workspace: None,
        }
    }

    /// 读宿主还回来的凭据；读不懂时为空。
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let credential: Self = serde_json::from_str(text).ok()?;
        (credential.version == VERSION).then_some(credential)
    }

    /// 写成交给宿主的文字。
    pub(crate) fn to_text(&self) -> String {
        serde_json::to_string(self).expect("凭据总能写成 JSON")
    }

    /// 里面的本人授权。
    pub(crate) fn grant(&self) -> Result<AuthGrant> {
        AuthGrant::from_parts(
            self.access_token.clone(),
            self.refresh_token.clone(),
            self.expires_at,
            self.scopes.clone(),
        )
    }

    /// 换了一份授权（续期）：应用、账号与工作区照旧。
    pub(crate) fn with_grant(&self, grant: &AuthGrant) -> Self {
        Self {
            account: self.account.clone(),
            workspace: self.workspace.clone(),
            ..Self::new(grant, self.client_id.clone())
        }
    }

    /// 访问令牌（认“是不是同一份授权”用）。
    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    /// 下次该续期的时间（毫秒）：过期前 5 分钟；没有刷新令牌、不知道过期时间时不排续期。
    pub(crate) fn renew_at_ms(&self) -> Option<i64> {
        self.refresh_token.as_ref()?;
        let at = self.expires_at?.saturating_sub(REFRESH_MARGIN_SECS);
        i64::try_from(at).ok()?.checked_mul(1000)
    }
}

/// 授权在使用前的状况（同 Friend 的 `grant::freshness`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// 有效且不临近过期；过期时间未知时也交给平台判定。
    Current,
    /// 仍有效但临近过期：尽量换新，临时失败时继续用旧的。
    RefreshSoon,
    /// 已过期：必须换新才能使用。
    MustRefresh,
    /// 已过期且没有刷新令牌：只能重新登录。
    Unrenewable,
}

/// 按过期时间与有没有刷新令牌判断要不要换新。
pub(crate) fn freshness(grant: &AuthGrant, now: u64) -> Freshness {
    let Some(expires_at) = grant.expires_at() else {
        return Freshness::Current;
    };
    let renewable = grant.refresh_token().is_some();
    if expires_at <= now {
        if renewable {
            Freshness::MustRefresh
        } else {
            Freshness::Unrenewable
        }
    } else if renewable && expires_at <= now.saturating_add(REFRESH_MARGIN_SECS) {
        Freshness::RefreshSoon
    } else {
        Freshness::Current
    }
}

/// 交接来的内置授权（附录 D 的 `credential`）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyGrant {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    account: Option<LegacyAccount>,
}

#[derive(Deserialize)]
struct LegacyAccount {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

/// HHH 内置 Slack 在连接设置里的那一栏（附录 D 的 `settings`）：发出授权的 Client ID、账号名与工作区名字。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacySettings {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    workspace_name: Option<String>,
}

/// 交接换成插件的凭据；内置那份用不了（读不懂、令牌不合规）时返回给用户看的原因。`builtin_client_id` 是 HHH 的应用：
/// 交接来的授权出自它时不记 Client ID（同新登录）。
pub(crate) fn from_legacy(
    credential: &str,
    settings: &Value,
    builtin_client_id: &str,
) -> std::result::Result<Credential, String> {
    let grant: LegacyGrant = serde_json::from_str(credential)
        .map_err(|_| "交接来的 Slack 授权读不懂，请重新登录。".to_owned())?;
    let settings: LegacySettings = serde_json::from_value(settings.clone())
        .map_err(|_| "交接来的 Slack 连接设置读不懂，请重新登录。".to_owned())?;
    let blank =
        |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let checked = AuthGrant::from_parts(
        grant.access_token,
        grant.refresh_token,
        grant.expires_at,
        grant.scopes,
    )
    .map_err(|error| error.message)?;
    let client_id = blank(settings.client_id).filter(|id| id != builtin_client_id);
    let mut out = Credential::new(&checked, client_id);
    out.account = grant.account.map(|account| KnownAccount {
        id: account.id,
        name: account
            .name
            .or_else(|| blank(settings.account_name.clone())),
    });
    out.workspace = blank(settings.workspace_name);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BUILTIN: &str = "1000000001.2000000000002";

    #[test]
    fn credentials_round_trip_and_hide_their_tokens() {
        let grant = AuthGrant::from_parts(
            "xoxe.xoxp-access".into(),
            Some("xoxe-refresh".into()),
            Some(10_000),
            Some(vec!["chat:write".into()]),
        )
        .unwrap();
        let mut credential = Credential::new(&grant, Some("123.456".into()));
        credential.workspace = Some("示例工作区".into());
        let text = credential.to_text();
        let back = Credential::parse(&text).unwrap();
        assert_eq!(back.grant().unwrap().refresh_token(), Some("xoxe-refresh"));
        assert_eq!(back.client_id.as_deref(), Some("123.456"));
        assert_eq!(back.workspace.as_deref(), Some("示例工作区"));
        assert_eq!(back.renew_at_ms(), Some((10_000 - 300) * 1000));
        let renewed = back
            .with_grant(&AuthGrant::from_parts("xoxe.xoxp-new".into(), None, None, None).unwrap());
        assert_eq!(
            renewed.client_id.as_deref(),
            Some("123.456"),
            "续期照旧用那个应用"
        );
        assert_eq!(renewed.workspace.as_deref(), Some("示例工作区"));
        assert_eq!(renewed.renew_at_ms(), None, "没有刷新令牌不排续期");
        let shown = format!("{credential:?}");
        assert!(!shown.contains("xoxe.xoxp-access") && !shown.contains("xoxe-refresh"));
        assert!(Credential::parse("{\"version\":9}").is_none());
        assert!(
            !Credential::new(&grant, None).to_text().contains("clientId"),
            "HHH 的应用不记 Client ID"
        );
    }

    // 附录 D：内置 Slack 那一栏的 Client ID 是 HHH 的应用时不记（同新登录），是旧版本自填的就记下、续期照旧用它。
    #[test]
    fn legacy_handovers_keep_the_app_that_issued_the_grant() {
        let legacy = json!({"accessToken": "xoxe.xoxp-1-a", "refreshToken": "xoxe-1-r",
            "expiresAt": 1_791_090_000u64, "scopes": ["chat:write"],
            "account": {"id": "U000000001", "name": "示例账号"}})
        .to_string();
        let builtin = from_legacy(
            &legacy,
            &json!({"clientId": BUILTIN, "accountName": "示例账号", "workspaceName": "示例工作区",
                "missingScopes": ["reactions:read"]}),
            BUILTIN,
        )
        .unwrap();
        assert_eq!(builtin.client_id, None);
        assert_eq!(builtin.workspace.as_deref(), Some("示例工作区"));
        assert_eq!(builtin.account.as_ref().unwrap().id, "U000000001");
        assert_eq!(builtin.grant().unwrap().expires_at(), Some(1_791_090_000));
        let own = from_legacy(&legacy, &json!({"clientId": "123.456"}), BUILTIN).unwrap();
        assert_eq!(own.client_id.as_deref(), Some("123.456"));
        assert_eq!(own.workspace, None);
        assert!(from_legacy("{}", &json!({}), BUILTIN).is_err());
        assert!(
            from_legacy(
                &json!({"accessToken": "a b"}).to_string(),
                &json!({}),
                BUILTIN
            )
            .is_err()
        );
    }

    #[test]
    fn freshness_follows_friend() {
        let grant = |refresh: bool, expires: u64| {
            AuthGrant::from_parts("a".into(), refresh.then(|| "r".into()), Some(expires), None)
                .unwrap()
        };
        assert_eq!(freshness(&grant(true, 10_000), 1_000), Freshness::Current);
        assert_eq!(
            freshness(&grant(true, 1_200), 1_000),
            Freshness::RefreshSoon
        );
        assert_eq!(freshness(&grant(false, 1_200), 1_000), Freshness::Current);
        assert_eq!(freshness(&grant(true, 900), 1_000), Freshness::MustRefresh);
        assert_eq!(freshness(&grant(false, 900), 1_000), Freshness::Unrenewable);
    }
}
