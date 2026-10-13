//! Slack 文本格式（mrkdwn）的解析：机器人消息、卡片段落与没有 `rich_text` 的旧消息用它（从 HHH 内置 Slack 原样搬来，
//! 输出改成协议的格式正文节点）。
//!
//! mrkdwn 不是标准 Markdown：`*粗*`、`_斜_`、`~删~`、`` `码` ``、```` ``` ```` 代码块、`>` 引用；
//! 尖括号里是特殊记号——`<网址|文字>` 链接、`<@U…>` 提及、`<#C…|名字>` 频道、`<!here>` 等；
//! 正文里的 `&`、`<`、`>` 被写成 `&amp;`、`&lt;`、`&gt;`。
//!
//! 解析成一串记号（[`Token`]），再由两种输出使用：
//! - [`to_rich`]：协议的格式正文节点（没有 `rich_text` 的消息）；
//! - [`to_markdown`]：标准 Markdown 文字（卡片段落，契约 client v4：纯文字要转义）。

use hhh_im_protocol::content::{PersonRef, RichNode};

/// 提及里人的编号种类：用户（与内置 Slack 时代相同，本机旧记录里的人认得出）。
pub(crate) const USER_ID_TYPE: &str = "user_id";
/// 用户组的编号种类。
pub(crate) const GROUP_ID_TYPE: &str = "usergroup_id";
/// 提及所有人（`@here` 等）的编号种类：编号与名字都是 Slack 的原词。
pub(crate) const BROADCAST_ID_TYPE: &str = "broadcast";

/// 纯文字放进 Markdown 段落前转义：每个 ASCII 标点前加反斜杠，保证原样显示、不被当成格式或链接（同 HHH 的卡片规则）。
pub(crate) fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 一段 mrkdwn 的最大字节数；超过的部分不解析，按原文截断，防止病态输入拖慢翻译。
pub(crate) const MRKDWN_MAX_BYTES: usize = 64 * 1024;

/// 行内记号。样式用开关记号表示，渲染时再配对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Text(String),
    /// 行内代码。
    Code(String),
    /// 代码块。
    Pre(String),
    Link {
        url: String,
        label: Option<String>,
    },
    User(String),
    /// 用户组提及，带 Slack 给的显示名（`@组名`）。
    Group {
        id: String,
        label: Option<String>,
    },
    Channel {
        id: String,
        label: Option<String>,
    },
    /// `@here`、`@channel`、`@everyone`。
    Everyone(String),
    /// 表情名，如 `+1`、`smile::skin-tone-2`。
    Emoji(String),
    Style(Style),
    Newline,
}

/// 可以开关的三种样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Bold,
    Italic,
    Strike,
}

impl Style {
    fn marker(self) -> char {
        match self {
            Self::Bold => '*',
            Self::Italic => '_',
            Self::Strike => '~',
        }
    }
}

/// 把 mrkdwn 切成记号。样式记号只在“词边界”生效（前面是空白或标点、后面紧跟非空白），
/// 与 Slack 客户端一致；配不上对的标记原样当文字。
pub(crate) fn tokenize(input: &str) -> Vec<Token> {
    let input = truncate(input, MRKDWN_MAX_BYTES);
    let mut raw = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if let Some(after) = rest.strip_prefix("```")
            && let Some(end) = after.find("```")
        {
            flush(&mut text, &mut raw);
            let body = &after[..end];
            let body = decode(body.strip_prefix('\n').unwrap_or(body));
            raw.push(Token::Pre(body.trim_end_matches('\n').to_owned()));
            i += 3 + end + 3;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        match c {
            '`' => {
                if let Some(end) = rest[1..]
                    .find('`')
                    .filter(|end| !rest[1..1 + end].contains('\n'))
                {
                    flush(&mut text, &mut raw);
                    raw.push(Token::Code(decode(&rest[1..1 + end])));
                    i += 1 + end + 1;
                    continue;
                }
            }
            '<' => {
                if let Some(end) = rest.find('>')
                    && let inner = &rest[1..end]
                    && !inner.contains('\n')
                    && !inner.is_empty()
                    && let Some(token) = special(inner)
                {
                    flush(&mut text, &mut raw);
                    raw.push(token);
                    i += end + 1;
                    continue;
                }
            }
            ':' => {
                if let Some(end) = rest[1..].find(':') {
                    let mut name_end = 1 + end;
                    // 肤色写在第二对冒号里：`:wave::skin-tone-3:`。
                    if rest[name_end + 1..].starts_with(":skin-tone-")
                        && let Some(more) = rest[name_end + 2..].find(':')
                    {
                        name_end = name_end + 2 + more;
                    }
                    let name = &rest[1..name_end];
                    if is_emoji_name(name) {
                        flush(&mut text, &mut raw);
                        raw.push(Token::Emoji(name.to_owned()));
                        i += name_end + 1;
                        continue;
                    }
                }
            }
            '\n' => {
                flush(&mut text, &mut raw);
                raw.push(Token::Newline);
                i += 1;
                continue;
            }
            '*' | '_' | '~' => {
                let style = match c {
                    '*' => Style::Bold,
                    '_' => Style::Italic,
                    _ => Style::Strike,
                };
                let before = if i == 0 {
                    None
                } else {
                    input[..i].chars().next_back()
                };
                let after = rest[1..].chars().next();
                let opens = before.is_none_or(|b| b.is_whitespace() || is_punct(b))
                    && after.is_some_and(|a| !a.is_whitespace());
                let closes = before.is_some_and(|b| !b.is_whitespace())
                    && after.is_none_or(|a| a.is_whitespace() || is_punct(a));
                if opens || closes {
                    flush(&mut text, &mut raw);
                    raw.push(Token::Style(style));
                    i += 1;
                    continue;
                }
            }
            _ => {}
        }
        text.push(c);
        i += c.len_utf8();
    }
    flush(&mut text, &mut raw);
    pair_styles(raw)
}

/// 样式标记两两配对，同一行内配不上的还原成文字。
fn pair_styles(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut open: Vec<(Style, usize)> = Vec::new();
    for token in tokens {
        match token {
            Token::Style(style) => {
                if let Some(pos) = open.iter().rposition(|(s, _)| *s == style) {
                    // 配上了；中间还开着的别的样式视为没配上。
                    for (s, index) in open.drain(pos + 1..) {
                        out[index] = Token::Text(s.marker().to_string());
                    }
                    open.pop();
                    out.push(Token::Style(style));
                } else {
                    open.push((style, out.len()));
                    out.push(Token::Style(style));
                }
            }
            Token::Newline | Token::Pre(_) => {
                for (s, index) in open.drain(..) {
                    out[index] = Token::Text(s.marker().to_string());
                }
                out.push(token);
            }
            other => out.push(other),
        }
    }
    for (s, index) in open {
        out[index] = Token::Text(s.marker().to_string());
    }
    merge_text(out)
}

fn merge_text(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    for token in tokens {
        match (out.last_mut(), token) {
            (Some(Token::Text(last)), Token::Text(next)) => last.push_str(&next),
            (_, token) => out.push(token),
        }
    }
    out
}

/// 尖括号里的特殊记号；认不出的返回 None，原样当文字。
fn special(inner: &str) -> Option<Token> {
    let (head, label) = match inner.split_once('|') {
        Some((head, label)) => (head, Some(decode(label))),
        None => (inner, None),
    };
    let label = label.filter(|l| !l.is_empty());
    if let Some(id) = head.strip_prefix('@') {
        return is_id(id).then(|| Token::User(id.to_owned()));
    }
    if let Some(id) = head.strip_prefix('#') {
        return is_id(id).then(|| Token::Channel {
            id: id.to_owned(),
            label,
        });
    }
    if let Some(command) = head.strip_prefix('!') {
        if let Some(id) = command.strip_prefix("subteam^") {
            return is_id(id).then(|| Token::Group {
                id: id.to_owned(),
                label,
            });
        }
        return match command {
            "here" | "channel" | "everyone" => Some(Token::Everyone(command.to_owned())),
            // `<!date^时间戳^格式|兜底文字>`：只显示兜底文字。
            _ if command.starts_with("date^") => Some(Token::Text(label.unwrap_or_default())),
            _ => label.map(Token::Text),
        };
    }
    let url = decode(head);
    let scheme_ok = ["http://", "https://", "mailto:", "tel:"]
        .iter()
        .any(|scheme| url.starts_with(scheme));
    scheme_ok.then_some(Token::Link { url, label })
}

fn is_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Slack 表情名：小写字母、数字、`_`、`-`、`+`、`'`，以及肤色后缀 `::skin-tone-N`。
pub(crate) fn is_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '+' | '\'' | ':'))
        && !name.starts_with(':')
        && !name.ends_with(':')
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || "，。！？、；：（）【】「」《》".contains(c)
}

fn flush(text: &mut String, out: &mut Vec<Token>) {
    if !text.is_empty() {
        out.push(Token::Text(decode(text)));
        text.clear();
    }
}

/// 还原 Slack 的三个实体；别的原样保留。
pub(crate) fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn truncate(input: &str, max: usize) -> &str {
    if input.len() <= max {
        return input;
    }
    let mut end = max;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

/// 记号 → 格式正文的若干段落：每行一段（与飞书富文本按行成段一致），空行不单独成段，
/// 样式成对嵌套，代码块单独成块。
pub(crate) fn to_rich(tokens: &[Token]) -> Vec<RichNode> {
    let mut blocks = Vec::new();
    let mut line: Vec<Token> = Vec::new();
    for token in tokens {
        match token {
            Token::Newline => {
                if !line.is_empty() {
                    blocks.push(paragraph(&line));
                    line.clear();
                }
            }
            Token::Pre(code) => {
                if !line.is_empty() {
                    blocks.push(paragraph(&line));
                    line.clear();
                }
                blocks.push(RichNode::Code {
                    lang: None,
                    value: code.clone(),
                });
            }
            other => line.push(other.clone()),
        }
    }
    if !line.is_empty() {
        blocks.push(paragraph(&line));
    }
    blocks
}

fn paragraph(tokens: &[Token]) -> RichNode {
    RichNode::Paragraph {
        children: inline(tokens),
    }
}

/// 一行里的记号 → 行内节点。样式记号成对包住中间的节点。
fn inline(tokens: &[Token]) -> Vec<RichNode> {
    // 用栈装“还开着的样式”和它们已收集的子节点。
    let mut stack: Vec<(Option<Style>, Vec<RichNode>)> = vec![(None, Vec::new())];
    for token in tokens {
        match token {
            Token::Style(style) => {
                if stack.last().is_some_and(|(open, _)| *open == Some(*style)) {
                    let (_, children) = stack.pop().unwrap_or_default();
                    let node = match style {
                        Style::Bold => RichNode::Strong { children },
                        Style::Italic => RichNode::Emphasis { children },
                        Style::Strike => RichNode::Delete { children },
                    };
                    if let Some((_, parent)) = stack.last_mut() {
                        parent.push(node);
                    }
                } else {
                    stack.push((Some(*style), Vec::new()));
                }
            }
            other => {
                if let (Some(node), Some((_, current))) = (leaf(other), stack.last_mut()) {
                    current.push(node);
                }
            }
        }
    }
    // 配对阶段已保证成对；这里兜底把没关上的样式内容并回上层。
    while stack.len() > 1 {
        let (_, children) = stack.pop().unwrap_or_default();
        if let Some((_, parent)) = stack.last_mut() {
            parent.extend(children);
        }
    }
    stack.pop().map(|(_, nodes)| nodes).unwrap_or_default()
}

fn leaf(token: &Token) -> Option<RichNode> {
    Some(match token {
        Token::Text(value) => RichNode::Text {
            value: value.clone(),
        },
        Token::Code(value) => RichNode::InlineCode {
            value: value.clone(),
        },
        Token::Link { url, label } => RichNode::Link {
            url: url.clone(),
            children: vec![RichNode::Text {
                value: label.clone().unwrap_or_else(|| url.clone()),
            }],
        },
        Token::User(id) => user_mention(id),
        Token::Group { id, label } => group_mention(id, label.as_deref()),
        Token::Channel { id, label } => channel_link(id, label.as_deref()),
        Token::Everyone(word) => everyone_mention(word),
        Token::Emoji(name) => RichNode::Emoji {
            emoji_type: name.clone(),
        },
        Token::Pre(_) | Token::Style(_) | Token::Newline => return None,
    })
}

/// 提及频道里的人：`@here`（在线的人）、`@channel`、`@everyone` 意思不同，带上原词，宿主照写“@原词”
/// （契约 client v18 §2.5、provider-protocol v15）。
pub(crate) fn everyone_mention(word: &str) -> RichNode {
    RichNode::Mention {
        person: Some(PersonRef {
            user_id: word.to_owned(),
            id_type: Some(BROADCAST_ID_TYPE.into()),
            kind: None,
            name: Some(word.to_owned()),
        }),
        everyone: true,
    }
}

/// 提及一个人：名字在翻译时拿不到，由宿主按编号从人名册取。
pub(crate) fn user_mention(id: &str) -> RichNode {
    RichNode::Mention {
        person: Some(PersonRef {
            user_id: id.to_owned(),
            id_type: Some(USER_ID_TYPE.into()),
            kind: None,
            name: None,
        }),
        everyone: false,
    }
}

/// 提及一个用户组：Slack 在文字里带了组名时一起记下（没带的读消息时按工作区的用户组补，见 `names`）。
pub(crate) fn group_mention(id: &str, label: Option<&str>) -> RichNode {
    RichNode::Mention {
        person: Some(PersonRef {
            user_id: id.to_owned(),
            id_type: Some(GROUP_ID_TYPE.into()),
            kind: None,
            name: label.map(|l| l.trim_start_matches('@').to_owned()),
        }),
        everyone: false,
    }
}

/// 频道引用：Slack 只给编号（有时带名字），显示成 `#名字`，点开用 Slack 的跳转地址。
pub(crate) fn channel_link(id: &str, label: Option<&str>) -> RichNode {
    RichNode::Link {
        url: format!("https://slack.com/app_redirect?channel={id}"),
        children: vec![RichNode::Text {
            value: format!("#{}", label.unwrap_or(id)),
        }],
    }
}

/// 记号 → 标准 Markdown 文字（卡片段落用）。纯文字里的 Markdown 符号一律转义。
pub(crate) fn to_markdown(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Text(value) => out.push_str(&escape_markdown(value)),
            Token::Code(value) => {
                let fence = if value.contains('`') { "``" } else { "`" };
                out.push_str(&format!("{fence}{value}{fence}"));
            }
            Token::Pre(value) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
                out.push_str(value);
                out.push_str("\n```\n");
            }
            Token::Link { url, label } => {
                let label = label.as_deref().unwrap_or(url);
                out.push_str(&format!(
                    "[{}](<{}>)",
                    escape_markdown(label),
                    url.replace('>', "%3E")
                ));
            }
            Token::User(id) => out.push_str(&format!("@{id}")),
            Token::Group { id, label } => {
                out.push_str(&format!(
                    "@{}",
                    escape_markdown(
                        label
                            .as_deref()
                            .map(|l| l.trim_start_matches('@'))
                            .unwrap_or(id)
                    )
                ));
            }
            Token::Channel { id, label } => {
                out.push_str(&format!(
                    "#{}",
                    escape_markdown(label.as_deref().unwrap_or(id))
                ));
            }
            Token::Everyone(which) => out.push_str(&format!("@{which}")),
            Token::Emoji(name) => match super::emoji::standard(name) {
                Some(unicode) => out.push_str(&unicode),
                None => out.push_str(&format!("\\:{}\\:", escape_markdown(name))),
            },
            Token::Style(Style::Bold) => out.push_str("**"),
            Token::Style(Style::Italic) => out.push('*'),
            Token::Style(Style::Strike) => out.push_str("~~"),
            Token::Newline => out.push('\n'),
        }
    }
    out.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> RichNode {
        RichNode::Text {
            value: value.into(),
        }
    }

    #[test]
    fn styles_pair_at_word_boundaries_only() {
        assert_eq!(
            to_rich(&tokenize("a *bold* _it_ ~gone~")),
            vec![RichNode::Paragraph {
                children: vec![
                    text("a "),
                    RichNode::Strong {
                        children: vec![text("bold")]
                    },
                    text(" "),
                    RichNode::Emphasis {
                        children: vec![text("it")]
                    },
                    text(" "),
                    RichNode::Delete {
                        children: vec![text("gone")]
                    },
                ]
            }]
        );
        // 词中间的符号、配不上对的符号都是文字。
        assert_eq!(
            to_rich(&tokenize("file_name_x 2*3 *open")),
            vec![RichNode::Paragraph {
                children: vec![text("file_name_x 2*3 *open")]
            }]
        );
    }

    #[test]
    fn angle_tokens_become_links_mentions_and_channels() {
        let tokens = tokenize(
            "看 <https://a.com/x?y=1&amp;z=2|文档> 问 <@U01AB> 在 <#C02|general> <!here> <!subteam^S03|@dev>",
        );
        assert_eq!(
            tokens,
            vec![
                Token::Text("看 ".into()),
                Token::Link {
                    url: "https://a.com/x?y=1&z=2".into(),
                    label: Some("文档".into())
                },
                Token::Text(" 问 ".into()),
                Token::User("U01AB".into()),
                Token::Text(" 在 ".into()),
                Token::Channel {
                    id: "C02".into(),
                    label: Some("general".into())
                },
                Token::Text(" ".into()),
                Token::Everyone("here".into()),
                Token::Text(" ".into()),
                Token::Group {
                    id: "S03".into(),
                    label: Some("@dev".into())
                },
            ]
        );
        // 不认识的尖括号与不安全的协议原样当文字。
        assert_eq!(
            tokenize("<javascript:alert(1)|x> a &lt; b"),
            vec![Token::Text("<javascript:alert(1)|x> a < b".into())]
        );
    }

    #[test]
    fn code_blocks_emoji_and_entities() {
        assert_eq!(
            to_rich(&tokenize(
                "跑 `cargo t` 然后\n```\nfn a() {}\n```\n好 :+1: :wave::skin-tone-3:"
            )),
            vec![
                RichNode::Paragraph {
                    children: vec![
                        text("跑 "),
                        RichNode::InlineCode {
                            value: "cargo t".into()
                        },
                        text(" 然后")
                    ],
                },
                RichNode::Code {
                    lang: None,
                    value: "fn a() {}".into()
                },
                RichNode::Paragraph {
                    children: vec![
                        text("好 "),
                        RichNode::Emoji {
                            emoji_type: "+1".into()
                        },
                        text(" "),
                        RichNode::Emoji {
                            emoji_type: "wave::skin-tone-3".into()
                        },
                    ],
                },
            ]
        );
    }

    #[test]
    fn markdown_output_escapes_plain_text_and_keeps_structure() {
        assert_eq!(
            to_markdown(&tokenize(
                "*构建* 失败 1.2 <https://ci.example/run/7|详情> [x]"
            )),
            "**构建** 失败 1\\.2 [详情](<https://ci.example/run/7>) \\[x\\]"
        );
        assert_eq!(to_markdown(&tokenize("状态 :white_check_mark:")), "状态 ✅");
    }
}
