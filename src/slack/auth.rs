//! Slack 的 PKCE 授权与续期（契约 client v11 §3.1；从 HHH 内置 Slack 原样搬来）。
//!
//! PKCE（RFC 7636）让桌面程序不用 Client Secret 也能安全地换令牌：发起授权时自己生成一段一次性暗号
//! （code_verifier），只把它的 SHA-256 摘要（code_challenge）放进授权链接；换令牌时再交出暗号原文，
//! Slack 核对两者对得上，才给令牌。暗号只留在本机内存里，截到授权码的人换不走令牌。
//!
//! 取样事实（回溯 v11 §4）：
//! - 用授权码换令牌时，本人令牌在 `authed_user` 下（`access_token`、`refresh_token`、`expires_in`、`scope`）；
//! - 续期时令牌在顶层，`token_type` 为 `user`；
//! - 令牌轮换打开后，访问令牌 12 小时（43200 秒）过期，每次续期都换一个新的刷新令牌。

use super::SlackConfig;
use super::api::{Api, QUERY_TIMEOUT, split_scopes};
use crate::grant::{now_secs, random_id};
use crate::{AuthGrant, Error, ErrorKind, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 一次授权从打开授权页到回调的最长等待；与 Slack 授权码的有效期一致（10 分钟）。
const LOGIN_TTL_SECS: u64 = 600;
/// 一次性暗号的随机字节数：32 字节编码后是 43 个字符，落在 RFC 7636 要求的 43–128 之间。
const VERIFIER_BYTES: usize = 32;
/// Client ID 的长度上限；Slack 的 Client ID 形如 `123456789.123456789`。
const CLIENT_ID_MAX_BYTES: usize = 64;

/// 本次登录要的回调与权限。
#[derive(Debug, Clone)]
pub struct LoginRequest {
    /// 已在应用后台登记的回调地址。
    pub redirect_uri: String,
    /// 本次申请的本人权限。
    pub scopes: Vec<String>,
}

/// 已登记到 Slack 的应用，只需要 Client ID（PKCE 下不需要 Client Secret）。
pub struct SlackAuth {
    config: SlackConfig,
    client_id: String,
    api: Api,
}

/// 一次性登录上下文；以值传递消费，不能复制后重复换码。Debug 永不显示暗号与 state。
pub struct SlackPendingLogin {
    url: Url,
    redirect: Url,
    state: String,
    verifier: String,
    created_at: u64,
}
impl SlackPendingLogin {
    /// 交给用户浏览器打开的授权地址。
    pub fn authorization_url(&self) -> &str {
        self.url.as_str()
    }

    /// 本次登录的一次性 state（已写在授权地址里），给宿主在公网回调中继登记用；暗号不外露。
    ///
    pub fn state(&self) -> &str {
        &self.state
    }
}
impl std::fmt::Debug for SlackPendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SlackPendingLogin([一次性状态已隐藏])")
    }
}

/// 授权完成的结果：本人授权与它属于哪个工作区、哪个人。
#[derive(Debug, Clone)]
pub struct SlackLogin {
    /// 本人授权（访问令牌、刷新令牌、过期时刻、实际拿到的权限）。
    pub grant: AuthGrant,
    /// 工作区编号，`T` 开头。
    pub team_id: String,
    /// 本人编号，`U` 或 `W` 开头。
    pub user_id: String,
}

impl SlackAuth {
    /// 用 Client ID 建授权入口；地址按 `config` 校验。
    pub fn new(config: SlackConfig, client_id: impl Into<String>) -> Result<Self> {
        config.validate()?;
        let client_id = client_id.into();
        let valid = !client_id.is_empty()
            && client_id.len() <= CLIENT_ID_MAX_BYTES
            && client_id.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        if !valid {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack Client ID 格式不正确，应形如 123456789.123456789。",
            ));
        }
        let api = Api::new(&config.api_base)?;
        Ok(Self {
            config,
            client_id,
            api,
        })
    }

    /// 生成授权页地址。`redirect_uri` 必须是已登记到 Slack 后台的地址：http 只接受主机名
    /// `localhost`（Slack 只把它当本机回调），其余一律 https；端口要与登记的一致。
    pub fn begin_login(&self, request: LoginRequest) -> Result<SlackPendingLogin> {
        let redirect = Url::parse(&request.redirect_uri)
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "授权回调地址格式不正确。"))?;
        let local = redirect.scheme() == "http" && redirect.host_str() == Some("localhost");
        if !(local || redirect.scheme() == "https")
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack 回调地址必须是 https，或主机名为 localhost 的 http，且不能带参数。",
            ));
        }
        let scopes: Vec<_> = request
            .scopes
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if scopes.is_empty()
            || scopes.iter().any(|s| {
                !s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b':' || b == b'.' || b == b'_')
            })
        {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack 权限名为空或格式不正确。",
            ));
        }
        let verifier = random_verifier()?;
        let state = random_id()?;
        let mut url = Url::parse(&self.config.authorize_url)
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "Slack 授权地址格式不正确。"))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("user_scope", &scopes.join(","))
            .append_pair("redirect_uri", redirect.as_str())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge_of(&verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(SlackPendingLogin {
            url,
            redirect,
            state,
            verifier,
            created_at: now_secs(),
        })
    }

    /// 核对完整回调地址（地址、state、重复参数、用户是否拒绝），再用授权码与暗号换本人令牌。
    /// 授权码只能用一次，失败不自动重试。
    pub async fn finish_login(
        &self,
        pending: SlackPendingLogin,
        callback_url: &str,
    ) -> Result<SlackLogin> {
        if now_secs().saturating_sub(pending.created_at) > LOGIN_TTL_SECS {
            return Err(Error::new(
                ErrorKind::AuthExpired,
                "本次授权等待已超时，请重新发起。",
            ));
        }
        let callback = Url::parse(callback_url)
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "授权回调地址无效。"))?;
        let mut target = callback.clone();
        target.set_query(None);
        if target != pending.redirect {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "授权回调地址与本次登录不匹配。",
            ));
        }
        let fields: Vec<_> = callback.query_pairs().collect();
        let single = |key: &str| -> Result<Option<String>> {
            let values: Vec<_> = fields.iter().filter(|(k, _)| k == key).collect();
            if values.len() > 1 {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "授权回调包含重复参数。",
                ));
            }
            Ok(values.first().map(|(_, value)| value.to_string()))
        };
        if single("state")?.as_deref() != Some(&pending.state) {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "授权回调的 state 不匹配，已拒绝换取令牌。",
            ));
        }
        if single("error")?.is_some() {
            return Err(Error::new(ErrorKind::Cancelled, "用户未完成 Slack 授权。"));
        }
        let code = single("code")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "授权回调缺少授权码。"))?;
        let reply = self
            .api
            .call(
                None,
                "oauth.v2.access",
                &[
                    ("client_id", &self.client_id),
                    ("code", &code),
                    ("code_verifier", &pending.verifier),
                    ("redirect_uri", pending.redirect.as_str()),
                ],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        let login = login_of(&reply.value)?;
        tracing::info!("Slack 授权完成");
        Ok(login)
    }

    /// 用刷新令牌续期；PKCE 下不带 Client Secret。刷新令牌只能用一次，续期成功后要先保存新授权再用。
    pub async fn refresh(&self, grant: &AuthGrant) -> Result<AuthGrant> {
        let refresh = grant.refresh_token().ok_or_else(|| {
            Error::new(
                ErrorKind::AuthExpired,
                "Slack 授权没有刷新令牌，需要重新登录。",
            )
        })?;
        let reply = self
            .api
            .call(
                None,
                "oauth.v2.access",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                    ("client_id", &self.client_id),
                ],
                Some(QUERY_TIMEOUT),
            )
            .await?;
        // 续期的令牌在顶层；个别情况下平台也可能照授权码那样放进 authed_user，两种都认。
        let value = &reply.value;
        let body = value
            .get("authed_user")
            .filter(|u| u.get("access_token").is_some())
            .unwrap_or(value);
        let grant = grant_of(body)?;
        tracing::info!("Slack 授权已续期");
        Ok(grant)
    }
}

/// 已授权的权限里还缺哪些；顺序跟 `needed` 一致。宿主据此提示“重新授权”。
///
pub fn missing_scopes(granted: &[String], needed: &[&str]) -> Vec<String> {
    needed
        .iter()
        .filter(|need| !granted.iter().any(|have| have == *need))
        .map(|need| (*need).to_owned())
        .collect()
}

/// 用授权码换令牌的响应 → 本人授权。本人令牌只在 `authed_user` 下。
fn login_of(value: &Value) -> Result<SlackLogin> {
    let user = value.get("authed_user").ok_or_else(|| {
        Error::new(
            ErrorKind::ProtocolError,
            "Slack 没有给出本人令牌：授权链接要申请用户权限（user_scope）。",
        )
    })?;
    let grant = grant_of(user)?;
    let user_id = text(user, "id")
        .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 授权响应缺少本人编号。"))?;
    let team_id = value
        .get("team")
        .and_then(|team| text(team, "id"))
        .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 授权响应缺少工作区编号。"))?;
    Ok(SlackLogin {
        grant,
        team_id,
        user_id,
    })
}

/// 令牌、刷新令牌、有效期与权限 → 授权。访问令牌必须是本人令牌（`xoxp-`，轮换后为 `xoxe.xoxp-`）。
fn grant_of(body: &Value) -> Result<AuthGrant> {
    let access = text(body, "access_token")
        .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 授权响应缺少访问令牌。"))?;
    if !(access.starts_with("xoxp-") || access.starts_with("xoxe.xoxp-")) {
        return Err(Error::new(
            ErrorKind::ProtocolError,
            "Slack 给出的不是本人令牌。",
        ));
    }
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .map(|v| now_secs().saturating_add(v));
    let scopes = text(body, "scope").map(|s| split_scopes(&s));
    AuthGrant::from_parts(access, text(body, "refresh_token"), expires_at, scopes)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn random_verifier() -> Result<String> {
    let mut bytes = [0_u8; VERIFIER_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|_| Error::new(ErrorKind::ProtocolError, "无法取得系统随机数。"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// RFC 7636 的 S256：暗号原文的 SHA-256，再做不带补位的 base64url。
fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth() -> SlackAuth {
        SlackAuth::new(SlackConfig::default(), "1000000001.2000000000002").unwrap()
    }

    fn request(redirect: &str) -> LoginRequest {
        LoginRequest {
            redirect_uri: redirect.into(),
            scopes: vec!["channels:history".into(), "chat:write".into()],
        }
    }

    #[test]
    fn challenge_follows_rfc_7636_appendix_b() {
        // RFC 7636 附录 B 的例子：暗号原文与它的 S256 摘要。
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let verifier = random_verifier().unwrap();
        assert_eq!(verifier.len(), 43);
        assert!(
            verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn authorization_url_carries_user_scopes_state_and_challenge_but_never_the_verifier() {
        let pending = auth()
            .begin_login(request(
                "http://localhost:8766/hhh-friend-im/slack/callback",
            ))
            .unwrap();
        let url = Url::parse(pending.authorization_url()).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("slack.com"));
        assert_eq!(pairs["client_id"], "1000000001.2000000000002");
        assert_eq!(pairs["user_scope"], "channels:history,chat:write");
        assert_eq!(
            pairs["redirect_uri"],
            "http://localhost:8766/hhh-friend-im/slack/callback"
        );
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], challenge_of(&pending.verifier));
        assert_eq!(pairs["state"], pending.state);
        assert!(!pending.authorization_url().contains(&pending.verifier));
        // 只申请本人权限，不带机器人权限 scope（PKCE 本机回调不许申请机器人权限）。
        assert!(!pairs.contains_key("scope"));
        assert!(!format!("{pending:?}").contains(&pending.state));
    }

    #[test]
    fn redirects_must_be_https_or_localhost_http() {
        let auth = auth();
        assert!(auth.begin_login(request("https://example.com/cb")).is_ok());
        for bad in [
            "http://127.0.0.1:8766/cb",
            "http://example.com/cb",
            "http://localhost:8766/cb?x=1",
            "myapp://callback",
        ] {
            assert!(auth.begin_login(request(bad)).is_err(), "{bad}");
        }
        let mut odd = request("http://localhost:1/cb");
        odd.scopes = vec!["chat write".into()];
        assert!(auth.begin_login(odd).is_err());
    }

    #[test]
    fn client_ids_are_digits_and_dots() {
        assert!(SlackAuth::new(SlackConfig::default(), "123.456").is_ok());
        assert!(SlackAuth::new(SlackConfig::default(), "").is_err());
        assert!(SlackAuth::new(SlackConfig::default(), "abc").is_err());
    }

    #[tokio::test]
    async fn callbacks_are_checked_before_any_network_call() {
        let auth = auth();
        let redirect = "http://localhost:8766/hhh-friend-im/slack/callback";
        let pending = auth.begin_login(request(redirect)).unwrap();
        let state = pending.state.clone();
        let wrong_path = format!("http://localhost:8766/other?code=a&state={state}");
        assert_eq!(
            auth.finish_login(pending, &wrong_path)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidRequest
        );
        let pending = auth.begin_login(request(redirect)).unwrap();
        let wrong_state = format!("{redirect}?code=a&state=nope");
        assert_eq!(
            auth.finish_login(pending, &wrong_state)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::PermissionDenied
        );
        let pending = auth.begin_login(request(redirect)).unwrap();
        let state = pending.state.clone();
        let doubled = format!("{redirect}?code=a&code=b&state={state}");
        assert_eq!(
            auth.finish_login(pending, &doubled).await.unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
        let pending = auth.begin_login(request(redirect)).unwrap();
        let state = pending.state.clone();
        let denied = format!("{redirect}?error=access_denied&state={state}");
        assert_eq!(
            auth.finish_login(pending, &denied).await.unwrap_err().kind,
            ErrorKind::Cancelled
        );
        let mut pending = auth.begin_login(request(redirect)).unwrap();
        pending.created_at = now_secs() - LOGIN_TTL_SECS - 1;
        let state = pending.state.clone();
        let late = format!("{redirect}?code=a&state={state}");
        assert_eq!(
            auth.finish_login(pending, &late).await.unwrap_err().kind,
            ErrorKind::AuthExpired
        );
    }

    #[test]
    fn exchange_and_refresh_shapes_both_become_user_grants() {
        // 形状取自真实取样（去掉令牌）：换令牌时在 authed_user 下，续期时在顶层。
        let exchange = json!({
            "ok": true, "app_id": "A01", "authed_user": {
                "id": "U01", "scope": "channels:history,chat:write",
                "access_token": "xoxe.xoxp-1-abc", "token_type": "user",
                "refresh_token": "xoxe-1-def", "expires_in": 43200
            },
            "team": {"id": "T01"}, "enterprise": null, "is_enterprise_install": false
        });
        let login = login_of(&exchange).unwrap();
        assert_eq!(login.team_id, "T01");
        assert_eq!(login.user_id, "U01");
        assert_eq!(login.grant.refresh_token(), Some("xoxe-1-def"));
        assert_eq!(
            login.grant.scopes().unwrap(),
            ["channels:history".to_string(), "chat:write".to_string()]
        );
        let expires = login.grant.expires_at().unwrap();
        assert!(expires > now_secs() + 43000 && expires <= now_secs() + 43200);

        let refreshed = json!({
            "ok": true, "access_token": "xoxe.xoxp-1-new", "expires_in": 43200,
            "refresh_token": "xoxe-1-new", "token_type": "user", "scope": "chat:write",
            "user_id": "U01", "team": {"id": "T01"}
        });
        let grant = grant_of(&refreshed).unwrap();
        assert_eq!(grant.access_token(), "xoxe.xoxp-1-new");

        // 没申请到本人令牌（只拿到机器人令牌）时明确报错，不把机器人当本人。
        let bot_only = json!({"ok": true, "access_token": "xoxb-1", "team": {"id": "T01"}});
        assert_eq!(
            login_of(&bot_only).unwrap_err().kind,
            ErrorKind::ProtocolError
        );
        assert_eq!(
            grant_of(&bot_only).unwrap_err().kind,
            ErrorKind::ProtocolError
        );
    }

    #[test]
    fn missing_scopes_keep_the_needed_order() {
        let granted = vec!["b".to_string(), "c".to_string()];
        assert_eq!(missing_scopes(&granted, &["a", "b", "d"]), vec!["a", "d"]);
        assert!(missing_scopes(&granted, &["c"]).is_empty());
    }
}
