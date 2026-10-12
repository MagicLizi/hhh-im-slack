//! Slack 的错误名 → 协议的错误分类（契约 client v11 §3.12、v17 §5；从 HHH 内置 Slack 原样搬来）。
//!
//! Slack 失败时 HTTP 多半仍是 200，正文 `{"ok":false,"error":"<错误名>"}`；限流是 HTTP 429 加
//! `Retry-After`（秒）。错误名是平台原样的英文词，记进错误码；授权失败时给出重新授权原因。

use crate::{Error, ErrorKind, ReauthReason};

/// 把 Slack 的错误名翻成统一错误。`retry_after` 只在限流时有意义。
pub(crate) fn from_slack(error: &str, status: u16, retry_after: Option<u64>) -> Error {
    let (kind, reason) = classify(error, status);
    let name = safe_name(error);
    tracing::warn!(?kind, error = name, status, "Slack 请求未完成");
    Error {
        kind,
        message: reason.to_owned(),
        platform_code: Some(name.to_owned()),
        reauth: reauth_of(name, kind),
        retry_after_secs: if kind == ErrorKind::RateLimited {
            retry_after
        } else {
            None
        },
    }
}

/// 授权失败时要用户做什么：应用编号或密钥不对是应用凭据无效（协议的 `credentialInvalid`），其余授权失败是过期。
fn reauth_of(error: &str, kind: ErrorKind) -> Option<ReauthReason> {
    match error {
        "invalid_client_id" | "bad_client_secret" => Some(ReauthReason::CredentialInvalid),
        _ if kind == ErrorKind::AuthExpired => Some(ReauthReason::Expired),
        _ => None,
    }
}

/// 只保留错误名里的安全字符再写进错误码，避免把异常响应的任意文本带给界面或日志。
fn safe_name(error: &str) -> &str {
    let valid = !error.is_empty()
        && error.len() <= 64
        && error
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid { error } else { "unknown_error" }
}

/// 错误名 → 分类与中文原因。没列出的按 HTTP 状态兜底：5xx 算暂时断开，其余算平台拒绝。
fn classify(error: &str, status: u16) -> (ErrorKind, &'static str) {
    match error {
        "ratelimited" | "rate_limited" => (ErrorKind::RateLimited, "Slack 限流，请稍后再试"),
        "not_authed"
        | "invalid_auth"
        | "token_expired"
        | "token_revoked"
        | "account_inactive"
        | "invalid_refresh_token"
        | "invalid_grant" => (ErrorKind::AuthExpired, "Slack 登录已失效，需要重新登录"),
        "missing_scope"
        | "no_permission"
        | "not_allowed_token_type"
        | "ekm_access_denied"
        | "access_denied"
        | "team_access_not_granted" => (
            ErrorKind::PermissionDenied,
            "Slack 登录缺少这项权限，需要重新登录",
        ),
        "channel_not_found" | "message_not_found" | "thread_not_found" | "user_not_found"
        | "bot_not_found" | "file_not_found" | "no_reaction" | "not_in_channel" => {
            (ErrorKind::NotFound, "Slack 里找不到它，或你不在这个会话里")
        }
        "cant_update_message" | "edit_window_closed" => {
            (ErrorKind::PermissionDenied, "Slack 不允许编辑这条消息")
        }
        "cant_delete_message" | "compliance_exports_prevent_deletion" => {
            (ErrorKind::PermissionDenied, "Slack 不允许撤回这条消息")
        }
        "is_archived"
        | "restricted_action"
        | "restricted_action_read_only_channel"
        | "restricted_action_thread_only_channel"
        | "restricted_action_non_threadable_channel"
        | "cannot_reply_to_message" => (ErrorKind::PermissionDenied, "这个会话不允许这样操作"),
        "already_reacted" => (ErrorKind::InvalidRequest, "已经点过这个表情"),
        "invalid_name" | "too_many_emoji" | "too_many_reactions" => {
            (ErrorKind::InvalidRequest, "Slack 不接受这个表情")
        }
        "msg_too_long" | "too_many_attachments" | "no_text" | "invalid_blocks" => {
            (ErrorKind::InvalidRequest, "Slack 不接受这条消息的内容")
        }
        "invalid_code"
        | "code_already_used"
        | "invalid_code_verifier"
        | "bad_redirect_uri"
        | "oauth_authorization_url_mismatch"
        | "invalid_client_id"
        | "bad_client_secret" => (ErrorKind::InvalidRequest, "Slack 授权码无效，请重新登录"),
        "invalid_cursor" | "invalid_ts_latest" | "invalid_ts_oldest" | "invalid_arguments"
        | "invalid_arg_name" | "invalid_array_arg" | "invalid_charset" | "invalid_form_data"
        | "invalid_post_type" | "missing_post_type" => {
            (ErrorKind::InvalidRequest, "请求参数不被 Slack 接受")
        }
        "fatal_error" | "internal_error" | "service_unavailable" | "request_timeout" => {
            (ErrorKind::Disconnected, "Slack 暂时不可用，请稍后再试")
        }
        _ if status == 429 => (ErrorKind::RateLimited, "Slack 限流，请稍后再试"),
        _ if status >= 500 => (ErrorKind::Disconnected, "Slack 暂时不可用，请稍后再试"),
        _ => (ErrorKind::ProtocolError, "Slack 拒绝了这个请求"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_permission_and_rate_limit_errors_map_to_their_kinds() {
        assert_eq!(
            from_slack("invalid_auth", 200, None).kind,
            ErrorKind::AuthExpired
        );
        assert_eq!(
            from_slack("token_expired", 200, None).kind,
            ErrorKind::AuthExpired
        );
        let missing = from_slack("missing_scope", 200, None);
        assert_eq!(missing.kind, ErrorKind::PermissionDenied);
        assert_eq!(missing.platform_code.as_deref(), Some("missing_scope"));
        assert_eq!(
            from_slack("token_revoked", 200, None).reauth,
            Some(ReauthReason::Expired)
        );
        assert_eq!(
            from_slack("invalid_client_id", 200, None).reauth,
            Some(ReauthReason::CredentialInvalid)
        );
        assert_eq!(missing.reauth, None);
        let limited = from_slack("ratelimited", 429, Some(12));
        assert_eq!(limited.kind, ErrorKind::RateLimited);
        assert_eq!(limited.retry_after_secs, Some(12));
        // 限流以外的错误不带等待秒数，免得上层误以为要等。
        assert_eq!(
            from_slack("channel_not_found", 200, Some(9)).retry_after_secs,
            None
        );
    }

    #[test]
    fn unknown_errors_fall_back_on_http_status_and_never_echo_unsafe_text() {
        assert_eq!(
            from_slack("something_new", 503, None).kind,
            ErrorKind::Disconnected
        );
        assert_eq!(
            from_slack("something_new", 429, Some(3)).kind,
            ErrorKind::RateLimited
        );
        assert_eq!(
            from_slack("something_new", 200, None).kind,
            ErrorKind::ProtocolError
        );
        let odd = from_slack("<script>alert(1)</script>", 200, None);
        assert_eq!(odd.platform_code.as_deref(), Some("unknown_error"));
        assert!(!odd.to_string().contains("script"));
    }

    #[test]
    fn edit_and_recall_refusals_explain_themselves() {
        assert!(
            from_slack("cant_update_message", 200, None)
                .message
                .contains("编辑")
        );
        assert!(
            from_slack("cant_delete_message", 200, None)
                .message
                .contains("撤回")
        );
        assert!(
            from_slack("edit_window_closed", 200, None)
                .message
                .contains("编辑")
        );
    }
}
