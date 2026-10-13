//! 本人授权（访问令牌、刷新令牌、过期时刻、拿到的权限）：插件只在内存里用，保存交给宿主（`credential/save`）。
//! 规则同 HHH 的 `AuthGrant`：令牌不能为空、过长或带空白与控制字符；Debug 永不显示令牌。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, ErrorKind, Result};

/// 给平台长令牌保留空间，同时防止不可信凭据占用无界内存。
const TOKEN_MAX_BYTES: usize = 16 * 1024;

/// 现在的 Unix 秒。
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 一次性随机编号（登录的 state）：24 字节，十六进制。
pub(crate) fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes)
        .map_err(|_| Error::new(ErrorKind::ProtocolError, "无法取得系统随机数。"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// 本人授权。
#[derive(Clone, PartialEq, Eq)]
pub struct AuthGrant {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    scopes: Option<Vec<String>>,
}

impl std::fmt::Debug for AuthGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthGrant")
            .field("credentials", &"[已隐藏]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AuthGrant {
    /// 由令牌、刷新令牌、过期时刻（Unix 秒）与权限拼成。
    ///
    /// # Errors
    /// 令牌为空、过长或带空白与控制字符时返回 [`ErrorKind::InvalidRequest`]。
    pub fn from_parts(
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<u64>,
        scopes: Option<Vec<String>>,
    ) -> Result<Self> {
        for token in std::iter::once(&access_token).chain(refresh_token.iter()) {
            if token.is_empty()
                || token.len() > TOKEN_MAX_BYTES
                || token.chars().any(char::is_whitespace)
                || token.chars().any(char::is_control)
            {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "授权令牌为空、过长或包含非法字符。",
                ));
            }
        }
        Ok(Self {
            access_token,
            refresh_token,
            expires_at,
            scopes,
        })
    }

    /// 访问令牌。
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// 刷新令牌（只能用一次）；没有为空。
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    /// 访问令牌的过期时刻（Unix 秒）；不知道为空。
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    /// 实际拿到的权限；不知道为空。
    pub fn scopes(&self) -> Option<&[String]> {
        self.scopes.as_deref()
    }

    /// 已经过期时报授权失效。
    pub(crate) fn ensure_current(&self) -> Result<()> {
        if self.expires_at.is_some_and(|expires| expires <= now_secs()) {
            return Err(Error::new(
                ErrorKind::AuthExpired,
                "用户授权已过期，请刷新或重新登录。",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_redacted_and_expiry_is_checked() {
        let grant = AuthGrant::from_parts(
            "private_access".into(),
            Some("private_refresh".into()),
            Some(1),
            None,
        )
        .unwrap();
        let shown = format!("{grant:?}");
        assert!(!shown.contains("private"));
        assert_eq!(
            grant.ensure_current().unwrap_err().kind,
            ErrorKind::AuthExpired
        );
        assert!(AuthGrant::from_parts("a b".into(), None, None, None).is_err());
        assert_eq!(random_id().unwrap().len(), 48);
    }
}
