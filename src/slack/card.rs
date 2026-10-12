//! Slack 的 Block Kit 块与附件 → 协议的卡片（契约 client v11 §3.9、client v4；从 HHH 内置 Slack 原样搬来）。
//!
//! 取样里的机器人消息是 `section` + `rich_text`；网址预览、分享的消息、应用预览放在 `attachments` 里
//! （`title` / `title_link` / `text` / `fields` / `color` / `footer`，应用预览另带 `blocks`）。
//! 能用卡片元素表达的都翻过去；按钮、下拉、输入框这类交互元素翻成“暂不支持”并带 Slack 的叫法。

use super::mrkdwn::{Style, Token, escape_markdown, to_markdown, to_rich, tokenize};
use super::parts::Part;
use super::rich::{array, str_of};
use hhh_im_protocol::content::{Card, CardBlock, CardInline, CardTone};
use serde_json::Value;

/// 一张卡片最多的块数；与飞书同一预算，超过的截掉并在末尾标“暂不支持”。
const CARD_BLOCKS_MAX: usize = 512;

/// 非 `rich_text` 的块 → 一张卡片；第一个 `header` 当标题。没有可显示的内容时为空。
/// 交互元素另外返回成“暂不支持”块，放在卡片后面。
pub(crate) fn from_blocks(blocks: &[&Value]) -> Vec<Part> {
    let mut card = Card {
        title: None,
        subtitle: None,
        tags: Vec::new(),
        wide: false,
        tone: CardTone::Neutral,
        blocks: Vec::new(),
    };
    let mut unsupported = Vec::new();
    for block in blocks {
        match str_of(block, "type") {
            "header" => {
                let title = object_text(block.get("text"));
                if card.title.is_none() && card.blocks.is_empty() {
                    card.title = Some(title);
                } else {
                    push(
                        &mut card,
                        paragraph(format!("**{}**", escape_markdown(&title))),
                    );
                }
            }
            "section" => section(block, &mut card),
            "context" => {
                let mut tokens = Vec::new();
                for part in array(block, "elements")
                    .iter()
                    .filter(|e| str_of(e, "type") != "image")
                    .map(|e| object_tokens(Some(e)))
                    .filter(|t| !t.is_empty())
                {
                    if !tokens.is_empty() {
                        tokens.push(Token::Text(" · ".into()));
                    }
                    tokens.extend(part);
                }
                if !tokens.is_empty() {
                    push(&mut card, text_block(&tokens));
                }
            }
            "divider" => push(&mut card, CardBlock::Divider),
            other => unsupported.push(unsupported_block(other)),
        }
    }
    let mut out = Vec::new();
    if card.title.is_some() || !card.blocks.is_empty() {
        out.push(Part::Card(card));
    }
    out.extend(unsupported);
    out
}

/// 一个附件 → 一张卡片。附件自带 `blocks`（应用预览）时按块翻译；分享的消息（`message_blocks`）
/// 与网址预览按标题、正文、字段显示。颜色条决定色调。
pub(crate) fn from_attachment(attachment: &Value) -> Vec<Part> {
    let blocks: Vec<&Value> = array(attachment, "blocks").iter().collect();
    if !blocks.is_empty() {
        let mut out = from_blocks(&blocks);
        if let Some(Part::Card(card)) = out.first_mut() {
            card.tone = tone_of(str_of(attachment, "color"));
        }
        return out;
    }
    let mut card = Card {
        title: None,
        subtitle: None,
        tags: Vec::new(),
        wide: false,
        tone: tone_of(str_of(attachment, "color")),
        blocks: Vec::new(),
    };
    // 与 Slack 客户端的排法一致：来源（网站名或作者）在最上面当标题，下面是带链接的标题。
    card.title = first_text(attachment, &["service_name", "author_name"]);
    let title = first_text(attachment, &["title"]);
    let link = first_text(attachment, &["title_link", "from_url", "original_url"])
        .filter(|link| safe_link(link));
    match (title, link) {
        (Some(title), Some(link)) => push(
            &mut card,
            paragraph(format!("[{}](<{link}>)", escape_markdown(&title))),
        ),
        (Some(title), None) if card.title.is_none() => card.title = Some(title),
        (Some(title), None) => push(
            &mut card,
            paragraph(format!("**{}**", escape_markdown(&title))),
        ),
        (None, Some(link)) => push(&mut card, paragraph(format!("<{link}>"))),
        (None, None) => {}
    }
    let pretext = str_of(attachment, "pretext");
    if !pretext.is_empty() {
        push(&mut card, text_block(&tokenize(pretext)));
    }
    let text = str_of(attachment, "text");
    if !text.is_empty() {
        push(&mut card, text_block(&tokenize(text)));
    }
    let fields = array(attachment, "fields");
    if !fields.is_empty() {
        let columns = fields
            .iter()
            .map(|field| {
                let title = str_of(field, "title");
                let mut tokens = Vec::new();
                if !title.is_empty() {
                    tokens.extend([
                        Token::Style(Style::Bold),
                        Token::Text(title.to_owned()),
                        Token::Style(Style::Bold),
                        Token::Newline,
                    ]);
                }
                tokens.extend(tokenize(str_of(field, "value")));
                vec![text_block(&tokens)]
            })
            .collect();
        push(&mut card, CardBlock::Columns { columns });
    }
    let footer = str_of(attachment, "footer");
    if !footer.is_empty() {
        push(&mut card, text_block(&tokenize(footer)));
    }
    if card.title.is_none() && card.blocks.is_empty() {
        // 只剩兜底文字（`fallback`）时显示它，至少不丢信息。
        let fallback = str_of(attachment, "fallback");
        if fallback.is_empty() {
            return Vec::new();
        }
        push(&mut card, paragraph(escape_markdown(fallback)));
    }
    vec![Part::Card(card)]
}

/// `section`：正文一段；`fields` 两两并排成多栏；配件（`accessory`）是按钮等交互元素时不显示。
fn section(block: &Value, card: &mut Card) {
    let text = object_tokens(block.get("text"));
    if !text.is_empty() {
        push(card, text_block(&text));
    }
    let fields = array(block, "fields");
    if !fields.is_empty() {
        let columns = fields
            .iter()
            .map(|field| vec![text_block(&object_tokens(Some(field)))])
            .collect();
        push(card, CardBlock::Columns { columns });
    }
}

/// Block Kit 的文字对象：`mrkdwn` 按 Slack 格式解析，`plain_text` 当纯文字。
fn object_tokens(object: Option<&Value>) -> Vec<Token> {
    let Some(object) = object else {
        return Vec::new();
    };
    let text = str_of(object, "text");
    match str_of(object, "type") {
        "mrkdwn" => tokenize(text),
        _ if text.is_empty() => Vec::new(),
        _ => vec![Token::Text(text.to_owned())],
    }
}

/// 一段卡片文字：提到了人（`<@U…>`）时翻成格式正文，提及是能按编号显示名字的提及节点
/// （契约 client v23 §7，取样：转来的消息末尾 `from <@U…>’s thread`）；其余照旧是一段 Markdown。
fn text_block(tokens: &[Token]) -> CardBlock {
    if tokens.iter().any(|t| matches!(t, Token::User(_))) {
        CardBlock::Rich {
            children: to_rich(tokens),
        }
    } else {
        paragraph(to_markdown(tokens))
    }
}

/// 文字对象的原文（标题用，不转义）。
fn object_text(object: Option<&Value>) -> String {
    object
        .map(|o| str_of(o, "text").to_owned())
        .unwrap_or_default()
}

fn paragraph(text: String) -> CardBlock {
    CardBlock::Paragraph {
        parts: vec![CardInline::Markdown { text }],
    }
}

fn push(card: &mut Card, block: CardBlock) {
    if card.blocks.len() < CARD_BLOCKS_MAX {
        card.blocks.push(block);
    }
}

fn first_text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .map(|key| str_of(value, key))
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 卡片里只放 http(s) 链接，地址里不能有会截断 Markdown 的字符。
fn safe_link(link: &str) -> bool {
    (link.starts_with("https://") || link.starts_with("http://"))
        && !link.contains(['<', '>', '\n', '\r', ' '])
}

/// 交互元素与 HHH 没有的块 → 暂不支持，带 Slack 的叫法。
fn unsupported_block(kind: &str) -> Part {
    let label = match kind {
        "actions" => "按钮",
        "input" => "输入框",
        "image" => "图片",
        "video" => "视频",
        "file" => "文件",
        "call" => "通话",
        "table" => "表格",
        "markdown" => "Markdown",
        _ => "卡片内容",
    };
    Part::Unsupported {
        kind: kind.to_owned(),
        label: Some(label.to_owned()),
    }
}

/// 附件颜色条 → 色调。Slack 允许 `good` / `warning` / `danger` 或十六进制颜色。
fn tone_of(color: &str) -> CardTone {
    match color {
        "" => CardTone::Neutral,
        "good" => CardTone::Green,
        "warning" => CardTone::Yellow,
        "danger" => CardTone::Red,
        hex => hex_tone(hex.trim_start_matches('#')).unwrap_or(CardTone::Neutral),
    }
}

/// 十六进制颜色按色相归到最近的色调；太灰的算中性。
fn hex_tone(hex: &str) -> Option<CardTone> {
    if hex.len() != 6 {
        return None;
    }
    let channel = |i: usize| {
        u8::from_str_radix(hex.get(i..i + 2)?, 16)
            .ok()
            .map(f32::from)
    };
    let (r, g, b) = (channel(0)?, channel(2)?, channel(4)?);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min < 40.0 {
        return Some(CardTone::Neutral);
    }
    let hue = if max == r {
        60.0 * (((g - b) / (max - min)) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / (max - min) + 2.0)
    } else {
        60.0 * ((r - g) / (max - min) + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    Some(match hue {
        h if !(15.0..330.0).contains(&h) => CardTone::Red,
        h if h < 40.0 => CardTone::Orange,
        h if h < 70.0 => CardTone::Yellow,
        h if h < 170.0 => CardTone::Green,
        h if h < 260.0 => CardTone::Blue,
        _ => CardTone::Purple,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 转来的消息末尾 `from <@U…>’s thread`（2026-10-02 真实取样的 context 块）：提及翻成提及节点，名字按人名册
    // 显示（契约 client v23 §7）；没提到人的照旧是一段 Markdown。
    #[test]
    fn mentions_inside_cards_become_mention_nodes() {
        let blocks = [
            json!({"type": "section", "text": {"type": "mrkdwn", "text": "收到通知：0.1.10 已处于 Pending 状态。"}}),
            json!({"type": "context", "elements": [{"type": "mrkdwn",
                "text": "from <@U02AYQYFU>’s thread <https://ampcode.com/threads/T-1|lise · taptap>"}]}),
        ];
        let refs: Vec<&Value> = blocks.iter().collect();
        let out = from_blocks(&refs);
        let [Part::Card(card)] = out.as_slice() else {
            panic!("应是一张卡片");
        };
        assert!(matches!(card.blocks[0], CardBlock::Paragraph { .. }));
        let CardBlock::Rich { children } = &card.blocks[1] else {
            panic!("提到了人的一段是格式正文");
        };
        let mut mentioned = Vec::new();
        super::super::parts::walk(children, &mut |node| {
            if let hhh_im_protocol::content::RichNode::Mention {
                person: Some(person),
                ..
            } = node
            {
                mentioned.push(person.user_id.clone());
            }
        });
        assert_eq!(mentioned, ["U02AYQYFU"]);
        // 附件的页脚同样。
        let shared = from_attachment(&json!({"text": "原文", "footer": "来自 <@W01ABCDEF2>"}));
        let [Part::Card(card)] = shared.as_slice() else {
            panic!("应是一张卡片");
        };
        assert!(matches!(card.blocks.last(), Some(CardBlock::Rich { .. })));
    }

    fn md(text: &str) -> CardBlock {
        paragraph(text.into())
    }

    #[test]
    fn header_section_fields_context_and_divider() {
        let blocks = [
            json!({"type": "header", "text": {"type": "plain_text", "text": "发布结果"}}),
            json!({"type": "section", "text": {"type": "mrkdwn", "text": "*成功* 3 项"},
                   "fields": [{"type": "mrkdwn", "text": "*环境*\n线上"}, {"type": "plain_text", "text": "a_b"}],
                   "accessory": {"type": "button", "text": {"type": "plain_text", "text": "查看"}}}),
            json!({"type": "divider"}),
            json!({"type": "context", "elements": [{"type": "mrkdwn", "text": "来自 CI"}, {"type": "image", "image_url": "https://x/y.png"}]}),
            json!({"type": "actions", "elements": []}),
        ];
        let refs: Vec<_> = blocks.iter().collect();
        assert_eq!(
            from_blocks(&refs),
            vec![
                Part::Card(Card {
                    title: Some("发布结果".into()),
                    subtitle: None,
                    tags: Vec::new(),
                    wide: false,
                    tone: CardTone::Neutral,
                    blocks: vec![
                        md("**成功** 3 项"),
                        CardBlock::Columns {
                            columns: vec![vec![md("**环境**\n线上")], vec![md("a\\_b")]]
                        },
                        CardBlock::Divider,
                        md("来自 CI"),
                    ],
                }),
                Part::Unsupported {
                    kind: "actions".into(),
                    label: Some("按钮".into())
                },
            ]
        );
    }

    #[test]
    fn link_unfurls_become_cards_with_a_linked_title() {
        let attachment = json!({
            "service_name": "GitHub", "title": "修复 #12", "title_link": "https://github.com/a/b/pull/12",
            "text": "改了 `x`", "color": "36a64f", "fallback": "GitHub: 修复 #12"
        });
        assert_eq!(
            from_attachment(&attachment),
            vec![Part::Card(Card {
                title: Some("GitHub".into()),
                subtitle: None,
                tags: Vec::new(),
                wide: false,
                tone: CardTone::Green,
                blocks: vec![
                    md("[修复 \\#12](<https://github.com/a/b/pull/12>)"),
                    md("改了 `x`")
                ],
            })]
        );
        // 只有标题、没有来源与链接时，标题当卡片标题。
        assert_eq!(
            from_attachment(&json!({"title": "周报"})),
            vec![Part::Card(Card {
                title: Some("周报".into()),
                subtitle: None,
                tags: Vec::new(),
                wide: false,
                tone: CardTone::Neutral,
                blocks: vec![],
            })]
        );
        // 只有兜底文字时也不丢。
        let bare = json!({"fallback": "[已分享] 一条消息"});
        assert_eq!(
            from_attachment(&bare),
            vec![Part::Card(Card {
                title: None,
                subtitle: None,
                tags: Vec::new(),
                wide: false,
                tone: CardTone::Neutral,
                blocks: vec![md("\\[已分享\\] 一条消息")],
            })]
        );
        assert!(from_attachment(&json!({})).is_empty());
    }

    #[test]
    fn colors_map_to_tones() {
        assert_eq!(tone_of("danger"), CardTone::Red);
        assert_eq!(tone_of("#2eb67d"), CardTone::Green);
        assert_eq!(tone_of("1d9bd1"), CardTone::Blue);
        assert_eq!(tone_of("#e01e5a"), CardTone::Red);
        assert_eq!(tone_of("ecb22e"), CardTone::Yellow);
        assert_eq!(tone_of("#dddddd"), CardTone::Neutral);
        assert_eq!(tone_of("zzz"), CardTone::Neutral);
    }
}
