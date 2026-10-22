//! 插件里的错误：协议的错误分类（protocol-v1 §12）加上给用户看的说明、Slack 的错误名、要不要重新登录与限流等多久。
//! 回给宿主时原样写进协议错误（[`Error::rpc`]）。

use hhh_im_protocol::rpc::RpcError;
pub use hhh_im_protocol::rpc::{ErrorKind, ReauthReason};

/// 一次操作没成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// 分类。
    pub kind: ErrorKind,
    /// 给用户看的说明（中文，不含令牌）。
    pub message: String,
    /// Slack 原样的错误名（只留安全字符）。
    pub platform_code: Option<String>,
    /// 要用户重新登录时的原因。
    pub reauth: Option<ReauthReason>,
    /// 限流时还要等多少秒。
    pub retry_after_secs: Option<u64>,
}

/// 插件里的结果。
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// 一个给定分类与说明的错误。
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            platform_code: None,
            reauth: None,
            retry_after_secs: None,
        }
    }

    /// 网络错误：给用户看的是说明，底层原因（不带请求地址）只进日志。
    pub fn network(error: reqwest::Error, message: impl Into<String>) -> Self {
        let message = message.into();
        let cause = crate::net::network_cause(error);
        tracing::warn!(message, %cause, "IM 网络请求失败");
        Self::new(ErrorKind::Disconnected, message)
    }

    /// 写成协议错误：分类、说明、Slack 错误名、重新登录原因、等多久都带上。
    pub fn rpc(&self) -> RpcError {
        let mut wire = RpcError::from_kind(self.kind, self.message.clone());
        if let Some(data) = wire.data.as_mut() {
            data.platform_code = self.platform_code.clone();
            data.reauth = self.reauth;
            data.retry_after_secs = self.retry_after_secs;
        }
        wire
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(code) = &self.platform_code {
            write!(f, "（错误码：{code}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        error.rpc()
    }
}

impl From<RpcError> for Error {
    fn from(error: RpcError) -> Self {
        match error.data {
            Some(data) => Self {
                kind: data.kind,
                message: data.user_message.unwrap_or(error.message),
                platform_code: data.platform_code,
                reauth: data.reauth,
                retry_after_secs: data.retry_after_secs,
            },
            None => Self::new(ErrorKind::ProtocolError, error.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_carry_their_details_into_the_protocol() {
        let error = Error {
            platform_code: Some("ratelimited".into()),
            retry_after_secs: Some(12),
            ..Error::new(ErrorKind::RateLimited, "Slack 限流，请稍后再试")
        };
        let wire = error.rpc();
        let data = wire.data.unwrap();
        assert_eq!(data.kind, ErrorKind::RateLimited);
        assert_eq!(data.user_message.as_deref(), Some("Slack 限流，请稍后再试"));
        assert_eq!(data.platform_code.as_deref(), Some("ratelimited"));
        assert_eq!(data.retry_after_secs, Some(12));
    }
}
