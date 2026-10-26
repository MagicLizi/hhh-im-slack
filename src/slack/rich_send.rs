//! 发出的格式正文 → Slack rich_text；字段与现有真实收信夹具、Block Kit 的 rich_text 结构对齐。
//! 发送与编辑共用；纯文字仍走原来的发送路径。嵌套列表按 Slack 的 indent / offset 表达。

use hhh_im_protocol::content::{RichNode as Node, rich_nodes, rich_outgoing};
use hhh_im_protocol::types::Outgoing;
use serde_json::{Map, Value, json};

use super::parts::{Part, children};
use crate::{Error, ErrorKind, Result};

const NODES_MAX: usize = 2048;
const DEPTH_MAX: usize = 24;
const BODY_MAX_BYTES: usize = 256 * 1024;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidRequest, message)
}

/// 只有单独一份格式正文才返回 blocks；没有格式正文时由旧的文字发送入口处理。
pub(crate) fn outgoing(content: &[Outgoing]) -> Result<Option<(String, String)>> {
    let [Outgoing::Rich { root }] = content else {
        if content
            .iter()
            .any(|item| matches!(item, Outgoing::Rich { .. }))
        {
            return Err(invalid("格式正文须单独成一条消息。"));
        }
        return Ok(None);
    };
    let nodes = rich_nodes(root).map_err(invalid)?;
    let mut count = 0;
    check_budget(&nodes, 0, &mut count)?;
    let mut elements = Vec::new();
    blocks(&nodes, &mut elements)?;
    let text = plain(&nodes);
    if text.trim().is_empty() || text.chars().count() > super::SEND_TEXT_MAX_CHARS {
        return Err(invalid("格式正文为空或超过发送长度上限。"));
    }
    let encoded = json!([{"type":"rich_text", "elements":elements}]).to_string();
    if encoded.len() > BODY_MAX_BYTES {
        return Err(invalid("格式正文超过发送大小上限。"));
    }
    Ok(Some((text, encoded)))
}

fn check_budget(nodes: &[Node], depth: usize, count: &mut usize) -> Result<()> {
    if depth > DEPTH_MAX {
        return Err(invalid("格式正文嵌套过深。"));
    }
    for node in nodes {
        *count += 1;
        if *count > NODES_MAX {
            return Err(invalid("格式正文节点过多。"));
        }
        check_budget(children(node), depth + 1, count)?;
    }
    Ok(())
}

fn blocks(nodes: &[Node], out: &mut Vec<Value>) -> Result<()> {
    for node in nodes {
        match node {
            Node::List { .. } => list(node, 0, out)?,
            Node::Code { value, .. } => out.push(json!({"type":"rich_text_preformatted", "elements":[{"type":"text", "text":value}]})),
            Node::Blockquote { children } => out.push(json!({"type":"rich_text_quote", "elements":inlines(children, &Map::new())?})),
            Node::Paragraph { children } => out.push(section(inlines(children, &Map::new())?)),
            other => out.push(section(inlines(std::slice::from_ref(other), &Map::new())?)),
        }
    }
    Ok(())
}

fn section(elements: Vec<Value>) -> Value {
    // 空段落用换行表达；Slack 不收没有元素的 section。
    json!({"type":"rich_text_section", "elements":if elements.is_empty() { vec![json!({"type":"text", "text":"\n"})] } else { elements }})
}

fn list(node: &Node, indent: usize, out: &mut Vec<Value>) -> Result<()> {
    let Node::List {
        ordered,
        start,
        children: items,
    } = node
    else {
        return Ok(());
    };
    let start = start.unwrap_or(1).max(1);
    for (index, item) in items.iter().enumerate() {
        let mut inline = Vec::new();
        let mut nested = Vec::new();
        for child in children(item) {
            if matches!(child, Node::List { .. }) {
                nested.push(child);
            } else {
                if !inline.is_empty() {
                    inline.push(json!({"type":"text", "text":"\n"}));
                }
                inline.extend(inlines(std::slice::from_ref(child), &Map::new())?);
            }
        }
        let mut value = json!({"type":"rich_text_list", "style":if *ordered {"ordered"} else {"bullet"}, "indent":indent, "elements":[section(inline)]});
        if *ordered {
            value["offset"] = Value::from(u64::from(start - 1) + index as u64);
        }
        out.push(value);
        for child in nested {
            list(child, indent + 1, out)?;
        }
    }
    Ok(())
}

fn inlines(nodes: &[Node], style: &Map<String, Value>) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let with = |name: &str| {
            let mut next = style.clone();
            next.insert(name.into(), Value::Bool(true));
            next
        };
        let mut leaf = match node {
            Node::Text { value } => json!({"type":"text", "text":value}),
            Node::Break => json!({"type":"text", "text":"\n"}),
            Node::InlineCode { value } => {
                let mut code = with("code");
                code.retain(|_, v| v == &Value::Bool(true));
                json!({"type":"text", "text":value, "style":code})
            }
            Node::Strong { children } => {
                out.extend(inlines(children, &with("bold"))?);
                continue;
            }
            Node::Emphasis { children } => {
                out.extend(inlines(children, &with("italic"))?);
                continue;
            }
            Node::Delete { children } => {
                out.extend(inlines(children, &with("strike"))?);
                continue;
            }
            Node::Link { url, children } => {
                let label = plain(children);
                json!({"type":"link", "url":url, "text":if label.is_empty() {url.clone()} else {label}})
            }
            Node::Emoji { emoji_type } => {
                let (name, tone) = emoji_type
                    .rsplit_once("::skin-tone-")
                    .map_or((emoji_type.as_str(), None), |(name, tone)| {
                        (name, tone.parse::<u8>().ok())
                    });
                if !super::mrkdwn::is_emoji_name(emoji_type) {
                    return Err(invalid("表情编号不合法。"));
                }
                let mut value = json!({"type":"emoji", "name":name});
                if let Some(tone) = tone {
                    value["skin_tone"] = Value::from(tone);
                }
                value
            }
            Node::Mention {
                everyone: true,
                person,
            } => {
                if !super::translate::editable_mention(person.as_ref(), true) {
                    return Err(invalid("这种全体提及不能原样发送。"));
                }
                json!({"type":"broadcast", "range":"channel"})
            }
            Node::Mention {
                person: Some(person),
                ..
            } => {
                if !super::translate::editable_mention(Some(person), false) {
                    return Err(invalid("这种提及不能原样发送。"));
                }
                super::check_id(&person.user_id)?;
                json!({"type":"user", "user_id":person.user_id})
            }
            Node::Mention { .. } => return Err(invalid("提及缺少对象。")),
            Node::Paragraph { children } | Node::ListItem { children } => {
                if index > 0 {
                    out.push(json!({"type":"text", "text":"\n"}));
                }
                out.extend(inlines(children, style)?);
                continue;
            }
            // 宿主按声明降级；这里保留额外节点里的文字，不能静默丢字。
            other if !children(other).is_empty() => {
                out.extend(inlines(children(other), style)?);
                continue;
            }
            other => {
                let text = plain(std::slice::from_ref(other));
                if text.is_empty() {
                    continue;
                }
                json!({"type":"text", "text":text})
            }
        };
        if !style.is_empty() && leaf.get("style").is_none() {
            leaf["style"] = Value::Object(style.clone());
        }
        if leaf.get("text").and_then(Value::as_str) != Some("") {
            out.push(leaf);
        }
    }
    Ok(out)
}

fn plain(nodes: &[Node]) -> String {
    let mut out = String::new();
    for (index, node) in nodes.iter().enumerate() {
        if index > 0
            && matches!(
                node,
                Node::Paragraph { .. }
                    | Node::List { .. }
                    | Node::ListItem { .. }
                    | Node::Blockquote { .. }
                    | Node::Code { .. }
            )
        {
            out.push('\n');
        }
        match node {
            Node::Text { value } | Node::InlineCode { value } | Node::Code { value, .. } => {
                out.push_str(value)
            }
            Node::Break => out.push('\n'),
            Node::Emoji { emoji_type } => out.push_str(&format!(":{emoji_type}:")),
            Node::Mention { everyone: true, .. } => out.push_str("@channel"),
            Node::Mention {
                person: Some(person),
                ..
            }
            | Node::Person { person } => {
                out.push('@');
                out.push_str(person.name.as_deref().unwrap_or(&person.user_id));
            }
            Node::Tag { text, .. } => out.push_str(text),
            Node::Unsupported { label, .. } => out.push_str(label.as_deref().unwrap_or_default()),
            other => out.push_str(&plain(children(other))),
        }
    }
    out
}

/// 只把能无损发送的格式放进编辑原文；附件、用户组、频道链接及不支持的样式继续不提供编辑。
pub(crate) fn edit_draft(parts: &[Part]) -> Option<Vec<Outgoing>> {
    fn supported(nodes: &[Node]) -> bool {
        nodes.iter().all(|node| match node {
            Node::Text { .. } | Node::Break | Node::Emoji { .. } | Node::InlineCode { .. } => true,
            Node::Code { lang, .. } => lang.as_ref().is_none_or(String::is_empty),
            Node::Mention { person, everyone } => {
                super::translate::editable_mention(person.as_ref(), *everyone)
            }
            Node::Paragraph { children }
            | Node::Strong { children }
            | Node::Emphasis { children }
            | Node::Delete { children }
            | Node::Blockquote { children }
            | Node::List { children, .. }
            | Node::ListItem { children } => supported(children),
            Node::Link { url, children } => {
                (url.starts_with("https://") || url.starts_with("http://"))
                    && children.iter().all(|n| matches!(n, Node::Text { .. }))
            }
            _ => false,
        })
    }
    let mut nodes = Vec::new();
    for part in parts {
        match part {
            Part::Text(value) => nodes.push(Node::Paragraph {
                children: vec![Node::Text {
                    value: value.clone(),
                }],
            }),
            Part::Mention { person, everyone }
                if super::translate::editable_mention(person.as_ref(), *everyone) =>
            {
                nodes.push(Node::Paragraph {
                    children: vec![Node::Mention {
                        person: person.clone(),
                        everyone: *everyone,
                    }],
                })
            }
            Part::Rich(children) if supported(children) => nodes.extend(children.clone()),
            _ => return None,
        }
    }
    (!nodes.is_empty()).then(|| vec![rich_outgoing(nodes)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_styles_lists_and_quotes_round_trip_through_sender() {
        for sample in [
            include_str!("../../tests/fixtures/slack/response_styles_link.json"),
            include_str!("../../tests/fixtures/slack/response_list_quote_code.json"),
        ] {
            let raw: Value = serde_json::from_str(sample).unwrap();
            let before = super::super::rich::blocks(
                &raw["messages"][0]["blocks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let (_, encoded) = outgoing(&[rich_outgoing(before.clone())]).unwrap().unwrap();
            let sent: Vec<Value> = serde_json::from_str(&encoded).unwrap();
            let after = super::super::rich::blocks(&sent.iter().collect::<Vec<_>>()).unwrap();
            assert_eq!(after, before);
        }
    }

    #[test]
    fn nested_list_keeps_order_start_and_inline_emoji() {
        let paragraph = |text: &str| Node::Paragraph {
            children: vec![Node::Text { value: text.into() }],
        };
        let nodes = vec![Node::List {
            ordered: true,
            start: Some(3),
            children: vec![
                Node::ListItem {
                    children: vec![
                        paragraph("甲"),
                        Node::List {
                            ordered: false,
                            start: None,
                            children: vec![Node::ListItem {
                                children: vec![paragraph("乙")],
                            }],
                        },
                    ],
                },
                Node::ListItem {
                    children: vec![Node::Paragraph {
                        children: vec![Node::Emoji {
                            emoji_type: "wave::skin-tone-3".into(),
                        }],
                    }],
                },
            ],
        }];
        let (_, encoded) = outgoing(&[rich_outgoing(nodes.clone())]).unwrap().unwrap();
        let sent: Vec<Value> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            super::super::rich::blocks(&sent.iter().collect::<Vec<_>>()).unwrap(),
            nodes
        );
    }

    #[test]
    fn plain_path_and_invalid_messages_stay_separate() {
        assert!(
            outgoing(&[Outgoing::Text {
                text: "**字**".into()
            }])
            .unwrap()
            .is_none()
        );
        assert!(outgoing(&[rich_outgoing(vec![])]).is_err());
        assert!(outgoing(&[rich_outgoing(vec![]), Outgoing::Text { text: "字".into() }]).is_err());
    }
}
