//! 本人的阅读水位，不表示他人读过消息。缺少 last_read 时保持未知。
use super::*;
use hhh_im_protocol::reading::{ReadState, ReadSyncParams, valid_read_ids, valid_read_scope};

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
    pub(crate) async fn reading_sync(&self, params: &ReadSyncParams) -> Result<ReadState> {
        if !valid_read_scope(&params.scope) || !valid_read_ids(&params.message_ids) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "阅读查询范围不正确。",
            ));
        }
        check_id(&params.scope.conversation_id)?;
        if params.scope.thread_root.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "尚未核实本授权的独立话题阅读位置。",
            ));
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
        let through = match channel.get("last_read") {
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
}
