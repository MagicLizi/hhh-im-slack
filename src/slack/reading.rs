//! 本人的阅读水位，不表示他人读过消息。缺少 last_read 时保持未知。
use super::*;
use hhh_im_protocol::reading::{
    MarkReadParams, MarkReadResult, ReadState, ReadSyncParams, compare_read_positions,
    valid_read_ids, valid_read_scope,
};

pub(crate) fn position(ts: &str) -> Option<String> {
    if ts == "0" {
        return Some("0".into());
    }
    let (seconds, micros) = ts.split_once('.')?;
    if seconds.is_empty()
        || seconds.len() > 20
        || micros.len() != 6
        || !seconds
            .bytes()
            .chain(micros.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let value = format!("{seconds}{micros}");
    let trimmed = value.trim_start_matches('0');
    Some(if trimmed.is_empty() { "0" } else { trimmed }.into())
}

impl SlackClient {
    pub(crate) async fn reading_mark(&self, params: &MarkReadParams) -> Result<MarkReadResult> {
        if !valid_read_scope(&params.scope) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "阅读写回范围不正确。",
            ));
        }
        check_id(&params.scope.conversation_id)?;
        if params.scope.thread_root.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "Slack 官方接口尚未提供可核实的独立话题阅读写回。",
            ));
        }
        let target = position(&params.message_id)
            .filter(|p| p != "0")
            .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "消息阅读位置格式不正确。"))?;
        if params.position.as_deref().is_some_and(|p| p != target) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "消息与阅读位置不一致。",
            ));
        }
        let current = self
            .reading_sync(&ReadSyncParams {
                scope: params.scope.clone(),
                message_ids: Vec::new(),
            })
            .await?;
        if current
            .through
            .as_deref()
            .is_some_and(|p| compare_read_positions(p, &target).is_ge())
        {
            return Ok(MarkReadResult::Applied);
        }
        let page = self
            .query(
                "conversations.history",
                &[
                    ("channel", &params.scope.conversation_id),
                    ("oldest", &params.message_id),
                    ("latest", &params.message_id),
                    ("inclusive", "true"),
                    ("limit", "1"),
                ],
            )
            .await?;
        let exact = page["messages"].as_array().is_some_and(|messages| {
            messages.len() == 1
                && messages[0]["ts"].as_str() == Some(params.message_id.as_str())
                && messages[0]
                    .get("thread_ts")
                    .and_then(Value::as_str)
                    .is_none_or(|root| root == params.message_id)
        });
        if !exact {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "无法确认这条消息属于当前会话，未写入。",
            ));
        }
        match self
            .write(
                "conversations.mark",
                &[
                    ("channel", &params.scope.conversation_id),
                    ("ts", &params.message_id),
                ],
            )
            .await
        {
            Ok(_) => {}
            Err(error)
                if error.kind == ErrorKind::Disconnected
                    || error.kind == ErrorKind::ProtocolError =>
            {
                return Ok(MarkReadResult::Unknown);
            }
            Err(error) => return Err(error),
        }
        // 写入只发一次。后续查询失败或未确认时保留 unknown，不重放写入。
        Ok(
            match self
                .reading_sync(&ReadSyncParams {
                    scope: params.scope.clone(),
                    message_ids: Vec::new(),
                })
                .await
            {
                Ok(state)
                    if state
                        .through
                        .as_deref()
                        .is_some_and(|p| compare_read_positions(p, &target).is_ge()) =>
                {
                    MarkReadResult::Applied
                }
                _ => MarkReadResult::Unknown,
            },
        )
    }

    pub(crate) async fn reading_sync(&self, params: &ReadSyncParams) -> Result<ReadState> {
        if !valid_read_scope(&params.scope) || !valid_read_ids(&params.message_ids) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "阅读查询范围不正确。",
            ));
        }
        check_id(&params.scope.conversation_id)?;
        if let Some(root) = params.scope.thread_root.as_deref() {
            if position(root).is_none_or(|value| value == "0") {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "话题根消息格式不正确。",
                ));
            }
            // 公开 conversations.replies 的根消息含本人的 last_read；缺省保持未知。
            let page = self
                .query(
                    "conversations.replies",
                    &[
                        ("channel", &params.scope.conversation_id),
                        ("ts", root),
                        ("limit", "1"),
                    ],
                )
                .await?;
            let message = page["messages"]
                .as_array()
                .and_then(|messages| {
                    messages
                        .iter()
                        .find(|message| message["ts"].as_str() == Some(root))
                })
                .filter(|message| {
                    message
                        .get("thread_ts")
                        .and_then(Value::as_str)
                        .is_none_or(|id| id == root)
                })
                .ok_or_else(|| {
                    Error::new(ErrorKind::ProtocolError, "阅读信息不属于查询的话题。")
                })?;
            return read_state(params, message.get("last_read"));
        }
        let value = self
            .query(
                "conversations.info",
                &[("channel", &params.scope.conversation_id)],
            )
            .await?;
        let channel = &value["channel"];
        if channel["id"].as_str() != Some(&params.scope.conversation_id) {
            return Err(Error::new(
                ErrorKind::ProtocolError,
                "阅读信息不属于查询的会话。",
            ));
        }
        read_state(params, channel.get("last_read"))
    }
}

fn read_state(params: &ReadSyncParams, cursor: Option<&Value>) -> Result<ReadState> {
    let through = match cursor {
        None | Some(Value::Null) => None,
        Some(Value::String(ts)) => Some(
            position(ts)
                .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "阅读位置格式不正确。"))?,
        ),
        _ => return Err(Error::new(ErrorKind::ProtocolError, "阅读位置格式不正确。")),
    };
    Ok(ReadState {
        scope: params.scope.clone(),
        through,
        read_message_ids: Vec::new(),
    })
}
