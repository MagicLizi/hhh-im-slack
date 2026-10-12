//! Slack Web API 的调用层：表单 POST、响应读取上限、`ok:false` 与 429 的统一处理。
//!
//! Slack 的方法一律是 `POST <根地址>/<方法名>`，参数用表单编码，令牌放 `Authorization: Bearer`。
//! 成功与否看正文的 `ok`，不看 HTTP 状态；限流例外，是 HTTP 429 加 `Retry-After`。
//! 响应头 `x-oauth-scopes` 给出这个令牌实际拿到的权限，连接时用它算“还缺哪些权限”。

use super::errors;
use crate::{Error, ErrorKind, Result};
use reqwest::{Response, Url};
use serde_json::Value;
use std::time::Duration;

/// JSON 响应的解压后上限，避免历史或异常响应无界读入。与飞书同一预算。
pub const RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 方法名的长度上限；方法名只收小写字母、数字与点，防止拼进奇怪的路径。
const METHOD_NAME_MAX_BYTES: usize = 64;
/// 查询的单次超时：发送沿用客户端的总超时（[`REQUEST_TIMEOUT`]）。
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// 客户端的总超时：发送、上传用（同 HHH 的 IM 网络客户端）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 空闲连接保留多久。
const POOL_IDLE: Duration = Duration::from_secs(20);
/// TCP 保活间隔。
const TCP_KEEPALIVE: Duration = Duration::from_secs(15);

/// 一次成功调用的结果。
pub(crate) struct Reply {
    /// 响应正文（`ok` 已确认为 true）。
    pub value: Value,
    /// 响应头 `x-oauth-scopes` 里的权限；平台没给时为空。
    pub scopes: Option<Vec<String>>,
}

/// 绑定一个根地址的调用层：统一 TLS（见 `net`），超时与连接池同 HHH 的 IM 网络客户端。
pub(crate) struct Api {
    http: reqwest::Client,
    base: Url,
}

/// 调接口用的 HTTP 客户端：总超时 30 秒、不跟随跳转（同 HHH 的 IM 网络客户端）。
pub(crate) fn http_client() -> Result<reqwest::Client> {
    crate::net::client_builder()
        .timeout(REQUEST_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE)
        .tcp_keepalive(TCP_KEEPALIVE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            let cause = crate::net::network_cause(error);
            tracing::warn!(%cause, "创建 IM 网络客户端失败");
            Error::new(ErrorKind::ProtocolError, "无法创建 IM 网络客户端。")
        })
}

/// 下载资源、头像与表情图用的客户端：不设总超时（大文件慢慢下），重定向不跟随（令牌不带去别的主机）。
pub(crate) fn download_client() -> Result<reqwest::Client> {
    crate::net::download_client_builder()
        .pool_idle_timeout(POOL_IDLE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            let cause = crate::net::network_cause(error);
            tracing::warn!(%cause, "创建 IM 下载客户端失败");
            Error::new(ErrorKind::ProtocolError, "无法创建 IM 下载客户端。")
        })
}

impl Api {
    /// `base` 是 Web API 根地址，例如 `https://slack.com/api`；本机测试可以用 http 回环地址。
    pub(crate) fn new(base: &str) -> Result<Self> {
        let base = parse_base(base)?;
        Ok(Self {
            http: http_client()?,
            base,
        })
    }

    /// 调用一个方法。`token` 为空时不带 `Authorization`（换令牌与续期用）。
    /// `timeout` 是这次请求的单次超时：查询用 10 秒，发送沿用客户端的 30 秒总超时。
    pub(crate) async fn call(
        &self,
        token: Option<&str>,
        method: &str,
        form: &[(&str, &str)],
        timeout: Option<Duration>,
    ) -> Result<Reply> {
        let url = self.method_url(method)?;
        let mut request = self.http.post(url).form(form);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request
            .send()
            .await
            .map_err(|error| Error::network(error, "无法连接 Slack。"))?;
        read_reply(response).await
    }

    /// 本调用层的 HTTP 客户端；上传文件内容时向 Slack 给的上传地址直接发字节。
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// `url` 与接口根地址是不是同一个来源（协议、主机、端口都相同）。
    pub(crate) fn same_origin(&self, url: &Url) -> bool {
        url.origin() == self.base.origin()
    }

    fn method_url(&self, method: &str) -> Result<Url> {
        let valid = !method.is_empty()
            && method.len() <= METHOD_NAME_MAX_BYTES
            && method.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b.is_ascii_uppercase()
            });
        if !valid {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack 方法名不合法。",
            ));
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "Slack 服务地址格式不正确。"))?
            .pop_if_empty()
            .push(method);
        Ok(url)
    }
}

/// 地址只收 https（本机回环可用 http），不能带凭据（同 HHH 的 IM 地址校验）。
pub(crate) fn validate_url(url: &Url) -> Result<()> {
    let local = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !url.username().is_empty()
        || url.password().is_some()
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
    {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "地址必须使用 HTTPS；仅本机测试或回调可使用 HTTP，地址中不能包含凭据。",
        ));
    }
    Ok(())
}

/// 校验根地址：只收 https（本机回环可用 http），不能带凭据、查询参数或片段。
pub(crate) fn parse_base(base: &str) -> Result<Url> {
    let url = Url::parse(base)
        .map_err(|_| Error::new(ErrorKind::InvalidRequest, "Slack 服务地址格式不正确。"))?;
    validate_url(&url)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "服务端点不能包含查询参数或片段。",
        ));
    }
    Ok(url)
}

/// 读完响应并判定成败：先按上限读正文，再看 429、`ok` 与 `error`。
async fn read_reply(response: Response) -> Result<Reply> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse().ok());
    let scopes = response
        .headers()
        .get("x-oauth-scopes")
        .and_then(|h| h.to_str().ok())
        .map(split_scopes);
    let bytes = read_limited(response, RESPONSE_MAX_BYTES).await?;
    if status == 429 {
        return Err(errors::from_slack("ratelimited", status, retry_after));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if (500..600).contains(&status) {
            errors::from_slack("service_unavailable", status, None)
        } else {
            Error::new(ErrorKind::ProtocolError, "Slack 未返回有效的 JSON 响应。")
        }
    })?;
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(Reply { value, scopes }),
        Some(false) => {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            Err(errors::from_slack(error, status, retry_after))
        }
        None => Err(Error::new(
            ErrorKind::ProtocolError,
            "Slack 响应缺少成功状态。",
        )),
    }
}

/// 按上限逐块读正文；超过上限直接放弃，不把整包留在内存里。
pub(crate) async fn read_limited(mut response: Response, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::network(error, "读取 Slack 响应时连接中断。"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "Slack 响应超过读取预算。",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// `x-oauth-scopes` 是逗号分隔的权限名。
pub(crate) fn split_scopes(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_urls_append_the_method_and_reject_odd_names() {
        let api = Api::new("https://slack.com/api").unwrap();
        assert_eq!(
            api.method_url("conversations.history").unwrap().as_str(),
            "https://slack.com/api/conversations.history"
        );
        let local = Api::new("http://127.0.0.1:9/api/").unwrap();
        assert_eq!(
            local.method_url("auth.test").unwrap().as_str(),
            "http://127.0.0.1:9/api/auth.test"
        );
        for bad in ["", "../auth.test", "auth test", "a/b", &"x".repeat(65)] {
            assert!(api.method_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn base_must_be_https_or_loopback_without_query() {
        assert!(parse_base("https://slack.com/api").is_ok());
        assert!(parse_base("http://localhost:1/api").is_ok());
        assert!(parse_base("http://slack.com/api").is_err());
        assert!(parse_base("https://slack.com/api?x=1").is_err());
        assert!(parse_base("https://user:pw@slack.com/api").is_err());
    }

    #[test]
    fn scope_headers_split_on_commas() {
        assert_eq!(
            split_scopes("channels:history, im:read,,chat:write"),
            vec!["channels:history", "im:read", "chat:write"]
        );
    }
}
