//! Slack 明确标记的消息转发快照；普通网址、引用与回复预览沿用既有显示。
use super::{
    parts::Part,
    rich::{array, str_of},
    translate,
};
use hhh_im_protocol::{
    budget,
    types::{Block, ForwardedMessage, ForwardedRecord, ForwardedSender, SenderKind},
};
use serde_json::{Value, json};

pub(super) fn attachment(raw: &Value, depth: usize, used: &mut usize) -> Option<Part> {
    if raw.get("is_msg_unfurl").and_then(Value::as_bool) != Some(true)
        || raw.get("is_reply_unfurl").and_then(Value::as_bool) == Some(true)
        || raw.get("is_thread_root_unfurl").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let mut incomplete =
        depth >= budget::FORWARDED_DEPTH_MAX || *used >= budget::FORWARDED_MESSAGES_MAX;
    let entries = array(raw, "message_blocks");
    let total_count = if entries.is_empty() {
        None
    } else {
        u32::try_from(entries.len()).ok()
    };
    let mut messages = Vec::new();
    if !incomplete {
        // 公开结构是 message_blocks[{message,ts,channel,team}]；不把任意 blocks 猜成转发。
        for entry in entries
            .iter()
            .take(budget::FORWARDED_MESSAGES_PER_RECORD_MAX)
        {
            if *used >= budget::FORWARDED_MESSAGES_MAX {
                incomplete = true;
                break;
            }
            *used += 1;
            let Some(message) = entry.get("message").filter(|v| v.is_object()) else {
                incomplete = true;
                continue;
            };
            let parts = translate::body_at(message, depth + 1, used);
            let partial = parts.is_empty() || parts.iter().any(Part::has_unsupported);
            incomplete |= partial;
            let sender = ForwardedSender {
                id: message
                    .get("user")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned),
                id_type: message
                    .get("user")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(|_| "user_id".into()),
                name: message
                    .get("user_profile")
                    .and_then(|p| p.get("display_name"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| message.get("username").and_then(Value::as_str))
                    .map(str::to_owned),
                kind: Some(
                    if message.get("subtype").and_then(Value::as_str) == Some("bot_message") {
                        SenderKind::Bot
                    } else {
                        SenderKind::User
                    },
                ),
                avatar: None,
            };
            let ts = entry
                .get("ts")
                .and_then(Value::as_str)
                .or_else(|| message.get("ts").and_then(Value::as_str));
            messages.push(ForwardedMessage {
                id: ts.map(str::to_owned),
                sender,
                created_at_ms: ts.and_then(translate::ts_millis),
                content: parts.iter().map(Part::block).collect(),
                incomplete: partial,
            });
        }
        incomplete |= messages.len() != entries.len();
        if entries.is_empty() {
            *used += 1;
            let fallback = json!({"text":str_of(raw,"text"),"files":array(raw,"files")});
            let content = translate::body_at(&fallback, depth + 1, used)
                .iter()
                .map(Part::block)
                .collect();
            messages.push(ForwardedMessage {
                id: None,
                sender: ForwardedSender {
                    id: None,
                    id_type: None,
                    kind: None,
                    avatar: None,
                    name: raw
                        .get("author_name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned),
                },
                created_at_ms: raw
                    .get("ts")
                    .and_then(Value::as_str)
                    .and_then(translate::ts_millis),
                content,
                incomplete: true,
            });
            incomplete = true;
        }
    }
    let title = raw
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("转发的消息")
        .to_owned();
    let record = ForwardedRecord {
        title,
        messages,
        total_count,
        incomplete,
    };
    let mut blocks = json!([Block::Forwarded {
        record: Box::new(record)
    }]);
    hhh_im_protocol::forwarded::sanitize_content(&mut blocks);
    let blocks: Vec<Block> = serde_json::from_value(blocks).expect("核对后的转发内容符合协议");
    match blocks.into_iter().next()? {
        Block::Forwarded { record } => Some(Part::Forwarded(record)),
        _ => Some(Part::Unsupported {
            kind: "forwarded-limit".into(),
            label: Some("部分聊天记录无法读取".into()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_unfurls_and_reply_quotes_are_not_forwarded() {
        let mut used = 0;
        assert!(attachment(&json!({"text":"普通网址","blocks":[]}), 0, &mut used).is_none());
        assert!(
            attachment(
                &json!({"is_msg_unfurl":true,"is_reply_unfurl":true,"text":"回复预览"}),
                0,
                &mut used
            )
            .is_none()
        );
    }
    // 合成边界测试不作为线上夹具；真实取样另记验收。
    #[test]
    fn native_snapshots_keep_order_sender_time_and_file_ids() {
        let raw = json!({"is_msg_unfurl":true,"message_blocks":[
            {"ts":"1700000000.000001","message":{"user":"U1","user_profile":{"display_name":"转发时的名字"},"text":"第一条"}},
            {"ts":"1700000001.000001","message":{"user":"U2","text":"第二条","files":[{"id":"F1","name":"说明.txt","mimetype":"text/plain"}]}}
        ]});
        let Part::Forwarded(record) = attachment(&raw, 0, &mut 0).unwrap() else {
            panic!()
        };
        assert_eq!(record.messages.len(), 2);
        assert_eq!(
            record.messages[0].sender.name.as_deref(),
            Some("转发时的名字")
        );
        assert_eq!(record.messages[0].created_at_ms, Some(1700000000000));
        assert!(!record.incomplete);
        assert!(
            record.messages[1]
                .content
                .iter()
                .any(|part| matches!(part,Block::Resource{resource,..} if resource.key=="F1"))
        );
    }
}
