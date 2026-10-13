//! Slack `rich_text` 块 → 协议的格式正文节点（契约 client v11 §3.9；从 HHH 内置 Slack 原样搬来）。
//!
//! 人发的消息都带 `rich_text`（取样全部如此），它是结构化的，比解析文字可靠：
//! - 块：`rich_text_section`（段）、`rich_text_list`（列表，`indent` 表示缩进层级）、
//!   `rich_text_preformatted`（代码块）、`rich_text_quote`（引用）；
//! - 行内：`text`（样式 bold / italic / strike / code / underline）、`link`、`user`、`usergroup`、
//!   `channel`、`emoji`、`broadcast`（@here 等）、`date`、`color`。
//!
//! 段里的换行写在文字里（`\n`），这里按行成段，与飞书富文本按行成段一致。

use super::mrkdwn::{channel_link, everyone_mention, group_mention, user_mention};
use hhh_im_protocol::content::RichNode;
use serde_json::Value;

/// 一棵格式正文最多的节点数与嵌套深度；与飞书同一预算，超过的整块记为暂不支持。
pub(crate) const RICH_NODES_MAX: usize = 2048;
pub(crate) const RICH_DEPTH_MAX: usize = 24;

/// 超出预算。
#[derive(Debug)]
pub(crate) struct OverBudget;

/// 翻译若干 `rich_text` 块的 `elements`，按顺序拼成一串块级节点。
pub(crate) fn blocks(rich_texts: &[&Value]) -> Result<Vec<RichNode>, OverBudget> {
    let mut budget = Budget::default();
    let mut out = Vec::new();
    for block in rich_texts {
        for element in array(block, "elements") {
            budget.spend(1)?;
            match str_of(element, "type") {
                "rich_text_section" => section(element, &mut out, &mut budget)?,
                "rich_text_preformatted" => out.push(RichNode::Code {
                    lang: None,
                    value: plain_text(array(element, "elements"))
                        .trim_end_matches('\n')
                        .to_owned(),
                }),
                "rich_text_quote" => {
                    let mut children = Vec::new();
                    section(element, &mut children, &mut budget)?;
                    out.push(RichNode::Blockquote { children });
                }
                "rich_text_list" => list(element, &mut out, &mut budget)?,
                other => out.push(RichNode::Unsupported {
                    kind: other.to_owned(),
                    label: None,
                }),
            }
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Budget {
    nodes: usize,
}
impl Budget {
    fn spend(&mut self, n: usize) -> Result<(), OverBudget> {
        self.nodes += n;
        if self.nodes > RICH_NODES_MAX {
            Err(OverBudget)
        } else {
            Ok(())
        }
    }
}

/// 一段行内元素按换行切成若干段落，追加到 `out`。
fn section(
    element: &Value,
    out: &mut Vec<RichNode>,
    budget: &mut Budget,
) -> Result<(), OverBudget> {
    let mut line: Vec<RichNode> = Vec::new();
    for item in array(element, "elements") {
        budget.spend(1)?;
        if str_of(item, "type") == "text" {
            let text = str_of(item, "text");
            let mut pieces = text.split('\n').peekable();
            while let Some(piece) = pieces.next() {
                if !piece.is_empty() {
                    line.push(styled(item, vec![text_or_code(item, piece)]));
                }
                if pieces.peek().is_some() {
                    flush(&mut line, out);
                }
            }
        } else if let Some(node) = inline(item) {
            line.push(styled(item, vec![node]));
        }
    }
    flush(&mut line, out);
    Ok(())
}

fn flush(line: &mut Vec<RichNode>, out: &mut Vec<RichNode>) {
    if !line.is_empty() {
        out.push(RichNode::Paragraph {
            children: std::mem::take(line),
        });
    }
}

/// 带 `code` 样式的文字是行内代码，其余是普通文字。
fn text_or_code(item: &Value, text: &str) -> RichNode {
    if style(item, "code") {
        RichNode::InlineCode {
            value: text.to_owned(),
        }
    } else {
        RichNode::Text {
            value: text.to_owned(),
        }
    }
}

/// 非文字的行内元素。
fn inline(item: &Value) -> Option<RichNode> {
    Some(match str_of(item, "type") {
        "link" => {
            let url = str_of(item, "url").to_owned();
            let label = item
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or(&url)
                .to_owned();
            RichNode::Link {
                url,
                children: vec![text_or_code(item, &label)],
            }
        }
        "user" => user_mention(str_of(item, "user_id")),
        "usergroup" => group_mention(str_of(item, "usergroup_id"), None),
        "channel" => channel_link(str_of(item, "channel_id"), None),
        "broadcast" => everyone_mention(match str_of(item, "range") {
            "" => "channel",
            range => range,
        }),
        "emoji" => {
            let name = str_of(item, "name");
            let tone = item.get("skin_tone").and_then(Value::as_u64);
            RichNode::Emoji {
                emoji_type: match tone {
                    Some(tone) => format!("{name}::skin-tone-{tone}"),
                    None => name.to_owned(),
                },
            }
        }
        // 日期：显示 Slack 给的兜底文字（按对方时区格式化的时间要客户端算，这里不做）。
        "date" => RichNode::Text {
            value: item
                .get("fallback")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "color" => RichNode::Text {
            value: str_of(item, "value").to_owned(),
        },
        other => RichNode::Unsupported {
            kind: other.to_owned(),
            label: None,
        },
    })
}

/// 按样式由内到外包：删除线、斜体、加粗、下划线（code 已在叶子上处理）。
fn styled(item: &Value, mut children: Vec<RichNode>) -> RichNode {
    if style(item, "strike") {
        children = vec![RichNode::Delete { children }];
    }
    if style(item, "italic") {
        children = vec![RichNode::Emphasis { children }];
    }
    if style(item, "bold") {
        children = vec![RichNode::Strong { children }];
    }
    if style(item, "underline") {
        children = vec![RichNode::Underline { children }];
    }
    children.pop().unwrap_or(RichNode::Text {
        value: String::new(),
    })
}

fn style(item: &Value, key: &str) -> bool {
    item.get("style")
        .and_then(|s| s.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Slack 的列表是扁平的：每个 `rich_text_list` 带一个缩进层级，连续的几个组成嵌套列表。
/// 这里用栈把它们还原成嵌套：缩进更深的挂到上一层最后一项的后面。
fn list(element: &Value, out: &mut Vec<RichNode>, budget: &mut Budget) -> Result<(), OverBudget> {
    let indent = element.get("indent").and_then(Value::as_u64).unwrap_or(0) as usize;
    let ordered = str_of(element, "style") == "ordered";
    let offset = element.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32;
    if indent + 2 > RICH_DEPTH_MAX {
        return Err(OverBudget);
    }
    let mut items = Vec::new();
    for item in array(element, "elements") {
        budget.spend(1)?;
        let mut children = Vec::new();
        section(item, &mut children, budget)?;
        items.push(RichNode::ListItem { children });
    }
    let list = RichNode::List {
        ordered,
        start: (ordered && offset > 0).then_some(offset + 1),
        children: items,
    };
    // 找到要挂的位置：从 out 最后一个列表开始，沿“最后一项”往下走 indent 层。
    if indent > 0
        && let Some(target) = nest_target(out, indent)
    {
        target.push(list);
        return Ok(());
    }
    // 同一层、同一种的列表接在上一个后面，保持编号连续。
    if indent == 0
        && let Some(RichNode::List {
            ordered: last_ordered,
            children,
            ..
        }) = out.last_mut()
        && *last_ordered == ordered
        && let RichNode::List { children: more, .. } = list
    {
        children.extend(more);
        return Ok(());
    }
    out.push(list);
    Ok(())
}

/// 第 `indent` 层列表应挂进的子节点表：上一层列表最后一项的子节点。
fn nest_target(out: &mut [RichNode], indent: usize) -> Option<&mut Vec<RichNode>> {
    let mut nodes: &mut Vec<RichNode> = match out.last_mut()? {
        RichNode::List { children, .. } => children,
        _ => return None,
    };
    for depth in 0..indent {
        let item_children = match nodes.last_mut()? {
            RichNode::ListItem { children } => children,
            _ => return None,
        };
        if depth + 1 == indent {
            return Some(item_children);
        }
        nodes = match item_children.last_mut()? {
            RichNode::List { children, .. } => children,
            _ => return None,
        };
    }
    None
}

/// 代码块里的元素按原样拼成文字：链接显示地址，提及显示编号。
fn plain_text(elements: &[Value]) -> String {
    let mut out = String::new();
    for item in elements {
        match str_of(item, "type") {
            "text" => out.push_str(str_of(item, "text")),
            "link" => out.push_str(
                item.get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(str_of(item, "url")),
            ),
            "user" => {
                out.push('@');
                out.push_str(str_of(item, "user_id"));
            }
            "channel" => {
                out.push('#');
                out.push_str(str_of(item, "channel_id"));
            }
            "emoji" => {
                out.push(':');
                out.push_str(str_of(item, "name"));
                out.push(':');
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

pub(crate) fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(value: &str) -> RichNode {
        RichNode::Text {
            value: value.into(),
        }
    }
    fn para(children: Vec<RichNode>) -> RichNode {
        RichNode::Paragraph { children }
    }

    #[test]
    fn sections_split_on_newlines_and_keep_styles() {
        let block = json!({"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
            {"type": "text", "text": "第一行\n"},
            {"type": "text", "text": "粗", "style": {"bold": true}},
            {"type": "text", "text": "码", "style": {"code": true}},
            {"type": "link", "url": "https://a.example", "text": "链接", "style": {"bold": true}},
            {"type": "emoji", "name": "wave", "skin_tone": 3},
            {"type": "user", "user_id": "U01"}
        ]}]});
        let nodes = blocks(&[&block]).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0], para(vec![text("第一行")]));
        assert_eq!(
            nodes[1],
            para(vec![
                RichNode::Strong {
                    children: vec![text("粗")]
                },
                RichNode::InlineCode {
                    value: "码".into()
                },
                RichNode::Strong {
                    children: vec![RichNode::Link {
                        url: "https://a.example".into(),
                        children: vec![text("链接")]
                    }]
                },
                RichNode::Emoji {
                    emoji_type: "wave::skin-tone-3".into()
                },
                user_mention("U01"),
            ])
        );
    }

    #[test]
    fn flat_lists_with_indent_become_nested_lists() {
        let block = json!({"type": "rich_text", "elements": [
            {"type": "rich_text_list", "style": "bullet", "indent": 0, "elements": [
                {"type": "rich_text_section", "elements": [{"type": "text", "text": "a"}]}
            ]},
            {"type": "rich_text_list", "style": "ordered", "indent": 1, "elements": [
                {"type": "rich_text_section", "elements": [{"type": "text", "text": "a1"}]}
            ]},
            {"type": "rich_text_list", "style": "bullet", "indent": 0, "elements": [
                {"type": "rich_text_section", "elements": [{"type": "text", "text": "b"}]}
            ]}
        ]});
        assert_eq!(
            blocks(&[&block]).unwrap(),
            vec![RichNode::List {
                ordered: false,
                start: None,
                children: vec![
                    RichNode::ListItem {
                        children: vec![
                            para(vec![text("a")]),
                            RichNode::List {
                                ordered: true,
                                start: None,
                                children: vec![RichNode::ListItem {
                                    children: vec![para(vec![text("a1")])]
                                }],
                            },
                        ]
                    },
                    RichNode::ListItem {
                        children: vec![para(vec![text("b")])]
                    },
                ]
            }]
        );
    }

    #[test]
    fn code_blocks_quotes_and_unknown_elements() {
        let block = json!({"type": "rich_text", "elements": [
            {"type": "rich_text_preformatted", "elements": [{"type": "text", "text": "let a = 1;\n"}]},
            {"type": "rich_text_quote", "elements": [{"type": "text", "text": "引用"}]},
            {"type": "rich_text_future", "elements": []}
        ]});
        assert_eq!(
            blocks(&[&block]).unwrap(),
            vec![
                RichNode::Code {
                    lang: None,
                    value: "let a = 1;".into()
                },
                RichNode::Blockquote {
                    children: vec![para(vec![text("引用")])]
                },
                RichNode::Unsupported {
                    kind: "rich_text_future".into(),
                    label: None
                },
            ]
        );
    }

    #[test]
    fn huge_trees_are_over_budget() {
        let many: Vec<_> = (0..RICH_NODES_MAX + 1)
            .map(|_| json!({"type": "text", "text": "x"}))
            .collect();
        let block = json!({"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": many}]});
        assert!(blocks(&[&block]).is_err());
    }
}
