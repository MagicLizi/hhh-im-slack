//! Slack 登录用的公网授权回调中继（从 HHH Friend 的 `im/auth_relay.rs` 搬来 Slack 用到的部分，契约
//! friend/im-auth-relay v1）。
//!
//! Slack 用 PKCE，不要 Client Secret：插件先用授权页里的一次性 state 在中继登记（不要中继代换令牌），成功后才把授权页
//! 交给宿主打开，之后按中继建议的间隔领取。领到的是授权码（或平台给的错误），拼回回调地址后在本机用暗号换令牌。
//! 领取凭证与授权码只在内存，不写日志。协议正本在独立仓库 hhh-im-callback-server 的 README。

use std::time::{Duration, Instant};

use reqwest::{StatusCode, Url};
use serde::Deserialize;

/// 正式中继；Slack 应用后台登记的回调是它下面的 `/hhh-friend-im/slack/callback`。
pub(crate) const DEFAULT_RELAY: &str = "https://hhh-im-auth.magiclizi.com";
/// 在中继上登记、领取用的平台名。
const PLATFORM: &str = "slack";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 中继建议的等待间隔落在这个范围里；没给或给得离谱时按默认 2 秒。
pub(crate) const RETRY_DEFAULT: Duration = Duration::from_secs(2);
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(10);
/// 中继给的中文说明最多显示这么多字符，防止异常响应把提示撑满。
const RELAY_MESSAGE_MAX_CHARS: usize = 120;

/// 一个中继：来源地址与出站客户端（统一 TLS，不跟随跳转）。
#[derive(Clone)]
pub(crate) struct Relay {
    base: Url,
    http: reqwest::Client,
}

/// 在中继登记好的一次登录：state 与领取凭证只在内存；Debug 不显示它们。
pub(crate) struct Flow {
    state: String,
    claim_token: String,
    expires_at: Instant,
}

impl std::fmt::Debug for Flow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Flow([领取凭证已隐藏])")
    }
}

impl Flow {
    /// 中继给的有效期已过（之后再领取只会拿到“已过期”）。
    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// 领取一次的结果。
pub(crate) enum Claim {
    /// 浏览器还没回来；按建议间隔再来。
    Pending { retry_after: Duration },
    /// 平台给了授权码（只能用一次）。
    Code(String),
    /// 平台给了错误（用户拒绝等）。
    Denied {
        error: String,
        description: Option<String>,
    },
}

impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending { retry_after } => write!(f, "Pending({retry_after:?})"),
            Self::Code(_) => f.write_str("Code([已隐藏])"),
            Self::Denied { error, .. } => write!(f, "Denied({error})"),
        }
    }
}

/// 中继出错：`message` 是给用户的中文；`gone` 表示这次登录在中继那边已不存在（过期、已领取、被清理），
/// 再领取也没用；`transient` 表示网络或服务暂时问题，等待中可以下次再试。
#[derive(Debug, Clone)]
pub(crate) struct RelayError {
    pub(crate) message: String,
    pub(crate) gone: bool,
    pub(crate) transient: bool,
}

impl RelayError {
    fn unreachable(cause: String) -> Self {
        Self {
            message: format!("授权回调服务暂时连不上（{cause}），请稍后再试。"),
            gone: false,
            transient: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    state: String,
    claim_token: String,
    callback_url: String,
    expires_in: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Claimed {
    status: String,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    result: Option<ClaimedResult>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClaimedResult {
    Code {
        code: String,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        error: String,
        #[serde(default)]
        error_description: Option<String>,
    },
    /// 代换令牌的结果（飞书用）：Slack 登记时不要代换，领到这些说明中继不对。
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RelayFailure {
    error: RelayFailureBody,
}

#[derive(Deserialize)]
struct RelayFailureBody {
    code: String,
    message: String,
}

/// 中继来源的规则：只要来源（不带路径、参数、账号）；https，或只在本机回环上的 http（测试中继）。
fn parse_relay_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim())
        .map_err(|_| "中继地址要是完整的网址，以 https:// 开头。".to_owned())?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return Err("中继地址要用 https（本机测试可以用 http://127.0.0.1）。".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("中继地址只填网址本身，不带路径或参数。".into());
    }
    Ok(url)
}

impl Relay {
    /// 一个中继（地址已核对过）。
    pub(crate) fn new(raw: &str) -> Result<Self, String> {
        let base = parse_relay_url(raw)?;
        let http = crate::net::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                let cause = crate::net::network_cause(error);
                tracing::error!(%cause, "IM 授权中继客户端建立失败");
                format!("授权回调服务客户端建立失败：{cause}")
            })?;
        Ok(Self { base, http })
    }

    /// 登记在 Slack 应用后台的回调地址。
    pub(crate) fn callback_url(&self) -> String {
        format!(
            "{}/hhh-friend-im/{PLATFORM}/callback",
            self.base.as_str().trim_end_matches('/')
        )
    }

    fn endpoint(&self, path: &str) -> Url {
        // 基址只有来源、路径固定由这里拼，join 不会出错。
        self.base.join(path).expect("中继来源拼固定路径")
    }

    /// 用授权页里的 state 登记一次登录（不要中继代换令牌）。中继回的 state 与回调地址必须与本机一致，否则说明中继的
    /// 公网地址配置不符，登记作废（不能把用户带去一个回不来的地址）。
    pub(crate) async fn register(&self, state: &str) -> Result<Flow, RelayError> {
        let started = Instant::now();
        let body = serde_json::json!({ "platform": PLATFORM, "state": state });
        let response = self
            .http
            .post(self.endpoint("/v1/flows"))
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                let cause = crate::net::network_cause(error);
                tracing::warn!(platform = PLATFORM, %cause, "IM 授权中继登记失败：连不上");
                RelayError::unreachable(cause)
            })?;
        let status = response.status();
        if status != StatusCode::CREATED {
            return Err(failure("登记", status, response).await);
        }
        let created: Created = response.json().await.map_err(|error| {
            let cause = crate::net::network_cause(error);
            tracing::warn!(platform = PLATFORM, %cause, "IM 授权中继登记回执读不懂");
            RelayError {
                message: format!("授权回调服务的回执读不懂（{cause}），请稍后再试。"),
                gone: false,
                transient: true,
            }
        })?;
        if created.state != state || created.callback_url != self.callback_url() {
            tracing::error!(
                platform = PLATFORM,
                "IM 授权中继回的回调地址或 state 与本机不一致"
            );
            return Err(RelayError {
                message: "授权回调服务的地址配置与 Friend 不一致，暂时不能登录。".into(),
                gone: false,
                transient: false,
            });
        }
        tracing::info!(
            platform = PLATFORM,
            exchange = false,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "IM 授权已在中继登记"
        );
        Ok(Flow {
            state: created.state,
            claim_token: created.claim_token,
            expires_at: Instant::now() + Duration::from_secs(created.expires_in),
        })
    }

    /// 领取一次。授权码只出现在返回值里，不进日志。
    pub(crate) async fn claim(&self, flow: &Flow) -> Result<Claim, RelayError> {
        let path = format!("/v1/flows/{}/claim", flow.state);
        let response = self
            .http
            .post(self.endpoint(&path))
            .bearer_auth(&flow.claim_token)
            .send()
            .await
            .map_err(|error| {
                let cause = crate::net::network_cause(error);
                tracing::warn!(platform = PLATFORM, %cause, "IM 授权中继领取失败：连不上");
                RelayError::unreachable(cause)
            })?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(failure("领取", status, response).await);
        }
        let claimed: Claimed = response.json().await.map_err(|error| {
            let cause = crate::net::network_cause(error);
            tracing::warn!(platform = PLATFORM, %cause, "IM 授权中继领取回执读不懂");
            RelayError {
                message: format!("授权回调服务的回执读不懂（{cause}），请重新登录。"),
                gone: false,
                transient: true,
            }
        })?;
        let mismatch = || {
            tracing::error!(
                platform = PLATFORM,
                "IM 授权中继领取回执的状态不认识或 state 不符"
            );
            RelayError {
                message: "授权回调服务给的结果对不上这次登录，请重新登录。".into(),
                gone: true,
                transient: false,
            }
        };
        match (claimed.status.as_str(), claimed.result) {
            ("pending", _) => Ok(Claim::Pending {
                retry_after: claimed
                    .retry_after_ms
                    .map(Duration::from_millis)
                    .map_or(RETRY_DEFAULT, |wait| wait.clamp(RETRY_MIN, RETRY_MAX)),
            }),
            ("completed", Some(result)) if claimed.state.as_deref() == Some(&flow.state) => {
                match result {
                    ClaimedResult::Code { code } => Ok(Claim::Code(code)),
                    ClaimedResult::Error {
                        error,
                        error_description,
                    } => Ok(Claim::Denied {
                        error,
                        description: error_description,
                    }),
                    ClaimedResult::Other => Err(mismatch()),
                }
            }
            _ => Err(mismatch()),
        }
    }

    /// 把领到的结果拼回登记过的回调地址（带 state），交给 PKCE 换码时核对。
    pub(crate) fn callback_with(&self, flow: &Flow, claim: &Claim) -> String {
        let mut url = Url::parse(&self.callback_url()).expect("回调地址由合法来源拼成");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("state", &flow.state);
            match claim {
                Claim::Code(code) => {
                    query.append_pair("code", code);
                }
                Claim::Denied { error, description } => {
                    query.append_pair("error", error);
                    if let Some(description) = description {
                        query.append_pair("error_description", description);
                    }
                }
                Claim::Pending { .. } => {}
            }
        }
        url.into()
    }
}

/// 中继给的中文说明：去掉控制字符、限制长度，防止异常响应把提示撑满。
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(RELAY_MESSAGE_MAX_CHARS)
        .collect()
}

/// 中继给的错误：读它的中文说明；404/410 表示这次登录在中继那边已不存在。日志只记编号与状态码。
async fn failure(
    step: &'static str,
    status: StatusCode,
    response: reqwest::Response,
) -> RelayError {
    let body = response.json::<RelayFailure>().await.ok();
    let code = body.as_ref().map(|body| body.error.code.as_str());
    tracing::warn!(
        platform = PLATFORM,
        step,
        status = status.as_u16(),
        code,
        "IM 授权中继拒绝了请求"
    );
    let gone = matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE);
    let transient = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
    let message = match body {
        Some(body) if gone => {
            tracing::debug!(code = %body.error.code, "中继说这次登录已不存在");
            "这次登录已失效，请重新登录。".to_owned()
        }
        Some(body) => format!("授权回调服务：{}", clean(&body.error.message)),
        None if gone => "这次登录已失效，请重新登录。".to_owned(),
        None => format!(
            "授权回调服务暂时不可用（HTTP {}），请稍后再试。",
            status.as_u16()
        ),
    };
    RelayError {
        message,
        gone,
        transient,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_origins_accept_https_and_loopback_http_only() {
        assert!(parse_relay_url(DEFAULT_RELAY).is_ok());
        assert!(parse_relay_url("http://127.0.0.1:18080").is_ok());
        for raw in [
            "http://relay.example",
            "https://relay.example/path",
            "https://u:p@relay.example",
            "https://relay.example/?x=1",
            "not a url",
        ] {
            assert!(parse_relay_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn the_callback_is_fixed_and_carries_the_claim() {
        let relay = Relay::new(DEFAULT_RELAY).unwrap();
        assert_eq!(
            relay.callback_url(),
            "https://hhh-im-auth.magiclizi.com/hhh-friend-im/slack/callback"
        );
        let flow = Flow {
            state: "s1".into(),
            claim_token: "t".into(),
            expires_at: Instant::now(),
        };
        assert_eq!(
            relay.callback_with(&flow, &Claim::Code("c 1".into())),
            "https://hhh-im-auth.magiclizi.com/hhh-friend-im/slack/callback?state=s1&code=c+1"
        );
        assert!(
            relay
                .callback_with(
                    &flow,
                    &Claim::Denied {
                        error: "access_denied".into(),
                        description: None
                    }
                )
                .ends_with("?state=s1&error=access_denied")
        );
        assert!(!format!("{:?}", Claim::Code("secret".into())).contains("secret"));
    }
}
