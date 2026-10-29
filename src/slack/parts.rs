//! 一条消息翻好的内容，写成协议的内容块之前的样子：格式正文与卡片还是类型化的节点，读消息时补名字（`names`）、
//! 算“有没有翻不了的内容”与编辑原文都在这一步，交给宿主前再写成协议的内容块（[`Part::block`]）。

use hhh_im_protocol::content::{Card, CardBlock, PersonRef, RichNode, card_block, rich_block};
use hhh_im_protocol::types::{Block, Resource};

/// 一个内容块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    /// 纯文字。
    Text(String),
    /// 提及：一个人、一个用户组，或所有人（带 Slack 的原词）。
    Mention {
        person: Option<PersonRef>,
        everyone: bool,
    },
    /// 格式正文（若干块级节点）。
    Rich(Vec<RichNode>),
    /// 卡片。
    Card(Card),
    /// 原生消息转发的只读快照。
    Forwarded(Box<hhh_im_protocol::types::ForwardedRecord>),
    /// 图片或文件。
    Resource {
        resource: Resource,
        name: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
    /// 暂时翻译不了的内容。
    Unsupported { kind: String, label: Option<String> },
}

impl Part {
    /// 写成协议的内容块。
    pub(crate) fn block(&self) -> Block {
        match self {
            Self::Text(text) => Block::Text { text: text.clone() },
            Self::Mention { person, everyone } => Block::Mention {
                user_id: person.as_ref().map(|p| p.user_id.clone()),
                id_type: person.as_ref().and_then(|p| p.id_type.clone()),
                kind: person.as_ref().and_then(|p| p.kind),
                name: person.as_ref().and_then(|p| p.name.clone()),
                everyone: *everyone,
            },
            Self::Rich(nodes) => rich_block(nodes.clone()),
            Self::Card(card) => card_block(card),
            Self::Forwarded(record) => Block::Forwarded {
                record: record.clone(),
            },
            Self::Resource {
                resource,
                name,
                width,
                height,
            } => Block::Resource {
                resource: resource.clone(),
                name: name.clone(),
                width: *width,
                height: *height,
            },
            Self::Unsupported { kind, label } => Block::Unsupported {
                kind: kind.clone(),
                label: label.clone(),
            },
        }
    }

    /// 这一块里有没有翻不了的内容（格式正文与卡片里的“暂不支持”节点也算，同 HHH 的 `has_unsupported`）。
    pub(crate) fn has_unsupported(&self) -> bool {
        let mut found = false;
        let mut look = |node: &RichNode| found |= matches!(node, RichNode::Unsupported { .. });
        match self {
            Self::Unsupported { .. } => return true,
            Self::Forwarded(record) => {
                return record.incomplete
                    || record.messages.iter().any(|message| message.incomplete);
            }
            Self::Rich(nodes) => walk(nodes, &mut look),
            Self::Card(card) => walk_card(&card.blocks, &mut look),
            _ => {}
        }
        found
    }
}

/// 一个节点的子节点（没有子节点的为空）。
pub(crate) fn children(node: &RichNode) -> &[RichNode] {
    match node {
        RichNode::Paragraph { children }
        | RichNode::Heading { children, .. }
        | RichNode::Blockquote { children }
        | RichNode::List { children, .. }
        | RichNode::ListItem { children }
        | RichNode::Table { children, .. }
        | RichNode::TableRow { children }
        | RichNode::TableCell { children }
        | RichNode::Strong { children }
        | RichNode::Emphasis { children }
        | RichNode::Delete { children }
        | RichNode::Underline { children }
        | RichNode::Link { children, .. }
        | RichNode::Color { children, .. } => children,
        _ => &[],
    }
}

/// 同 [`children`]，可改。
pub(crate) fn children_mut(node: &mut RichNode) -> Option<&mut Vec<RichNode>> {
    match node {
        RichNode::Paragraph { children }
        | RichNode::Heading { children, .. }
        | RichNode::Blockquote { children }
        | RichNode::List { children, .. }
        | RichNode::ListItem { children }
        | RichNode::Table { children, .. }
        | RichNode::TableRow { children }
        | RichNode::TableCell { children }
        | RichNode::Strong { children }
        | RichNode::Emphasis { children }
        | RichNode::Delete { children }
        | RichNode::Underline { children }
        | RichNode::Link { children, .. }
        | RichNode::Color { children, .. } => Some(children),
        _ => None,
    }
}

/// 先序走遍每个节点。
pub(crate) fn walk(nodes: &[RichNode], f: &mut impl FnMut(&RichNode)) {
    for node in nodes {
        f(node);
        walk(children(node), f);
    }
}

/// 先序走遍每个节点，可改（先改这个节点，再走它改过之后的子节点）。
pub(crate) fn walk_mut(nodes: &mut [RichNode], f: &mut impl FnMut(&mut RichNode)) {
    for node in nodes {
        f(node);
        if let Some(children) = children_mut(node) {
            walk_mut(children, f);
        }
    }
}

/// 卡片里每一段格式正文的每个节点（含栏与折叠面板里的，同 HHH 的 `walk_rich`）。
fn walk_card(blocks: &[CardBlock], f: &mut impl FnMut(&RichNode)) {
    for block in blocks {
        match block {
            CardBlock::Rich { children } => walk(children, f),
            CardBlock::Columns { columns } => {
                for column in columns {
                    walk_card(column, f);
                }
            }
            CardBlock::Panel { title, blocks, .. } => {
                walk(title, f);
                walk_card(blocks, f);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhh_im_protocol::content::CardTone;

    #[test]
    fn unsupported_nodes_anywhere_mark_the_part() {
        let rich = Part::Rich(vec![RichNode::Blockquote {
            children: vec![RichNode::Unsupported {
                kind: "x".into(),
                label: None,
            }],
        }]);
        assert!(rich.has_unsupported());
        let card = Part::Card(Card {
            tone: CardTone::Neutral,
            blocks: vec![CardBlock::Columns {
                columns: vec![vec![CardBlock::Rich {
                    children: vec![RichNode::Unsupported {
                        kind: "y".into(),
                        label: None,
                    }],
                }]],
            }],
            ..Card::default()
        });
        assert!(card.has_unsupported());
        assert!(!Part::Text("hi".into()).has_unsupported());
    }
}
