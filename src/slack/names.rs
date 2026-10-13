//! Slack 消息里只带编号的频道引用与用户组提及：补上名字（契约 client v18 §2.5；从 HHH 内置 Slack 原样搬来）。
//!
//! Slack 的 `rich_text` 里频道写成 `{"type":"channel","channel_id":"C…"}`、用户组写成
//! `{"type":"usergroup","usergroup_id":"S…"}`，不带名字。频道按本人所在的频道补（会话缓存），补不上的是本人
//! 不在的私有频道，写“#私有频道”；用户组按工作区的用户组补（要 `usergroups:read`），没有这项权限时写“用户组”。
//! 名字表由客户端在读消息前按需刷新。

use super::parts::{Part, walk, walk_mut};
use super::translate::{SlackMessage, is_group};
use hhh_im_protocol::content::RichNode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 名字表记多久。
pub(crate) const NAMES_TTL: Duration = Duration::from_secs(5 * 60);
/// 频道引用的跳转地址前缀（`mrkdwn::channel_link` 写的）。
const CHANNEL_LINK: &str = "https://slack.com/app_redirect?channel=";
/// 补不上名字的频道（本人不在的私有频道）。
pub(crate) const PRIVATE_CHANNEL: &str = "#私有频道";
/// 补不上名字的用户组。
pub(crate) const UNKNOWN_GROUP: &str = "用户组";

/// 频道与用户组的名字。
#[derive(Debug, Default)]
pub(crate) struct Names {
    /// 频道编号 → 名字（本人所在的）。
    pub(crate) channels: HashMap<String, String>,
    /// 用户组编号 → 显示用的名字（`@` 后面那段）。
    pub(crate) groups: HashMap<String, String>,
    /// 频道读成功的时间；没读成功过时为空，这时补不上的频道照原样留着编号，不写“私有频道”。
    pub(crate) loaded_at: Option<Instant>,
    /// 上次去读的时间（成功与否）：[`NAMES_TTL`] 内不再读。
    pub(crate) tried_at: Option<Instant>,
}

impl Names {
    /// 该不该再去读一次。
    pub(crate) fn stale(&self) -> bool {
        self.tried_at.is_none_or(|at| at.elapsed() >= NAMES_TTL)
    }
}

/// 这批消息里有没有要补名字的。
pub(crate) fn needs_names(messages: &[SlackMessage]) -> bool {
    let mut needed = false;
    for message in messages {
        visit(message, &mut || needed = true);
    }
    needed
}

/// 补名字：频道写成 `#名字`，用户组写进提及的名字；补不上按上面的规则写。
pub(crate) fn fill(names: &Names, messages: &mut [SlackMessage]) {
    for message in messages {
        for part in &mut message.parts {
            match part {
                Part::Mention {
                    person: Some(person),
                    ..
                } if is_group(person) && person.name.is_none() => {
                    person.name = Some(group_name(names, &person.user_id));
                }
                Part::Rich(nodes) => walk_mut(nodes, &mut |node| match node {
                    RichNode::Mention {
                        person: Some(person),
                        ..
                    } if is_group(person) && person.name.is_none() => {
                        person.name = Some(group_name(names, &person.user_id));
                    }
                    RichNode::Link { url, children } => {
                        if let Some(id) = unnamed_channel(url, children) {
                            let label = match names.channels.get(&id) {
                                Some(name) => format!("#{name}"),
                                None if names.loaded_at.is_some() => PRIVATE_CHANNEL.to_owned(),
                                None => return,
                            };
                            *children = vec![RichNode::Text { value: label }];
                        }
                    }
                    _ => {}
                }),
                _ => {}
            }
        }
    }
}

fn group_name(names: &Names, id: &str) -> String {
    names
        .groups
        .get(id)
        .cloned()
        .unwrap_or_else(|| UNKNOWN_GROUP.to_owned())
}

/// 只带编号的频道引用：显示的文字就是 `#编号`。
fn unnamed_channel(url: &str, children: &[RichNode]) -> Option<String> {
    let id = url.strip_prefix(CHANNEL_LINK)?;
    match children {
        [RichNode::Text { value }] if value.strip_prefix('#') == Some(id) => Some(id.to_owned()),
        _ => None,
    }
}

/// 每遇到一处要补名字的就调一次 `f`。
fn visit(message: &SlackMessage, f: &mut impl FnMut()) {
    for part in &message.parts {
        match part {
            Part::Mention {
                person: Some(person),
                ..
            } if is_group(person) && person.name.is_none() => f(),
            Part::Rich(nodes) => walk(nodes, &mut |node| match node {
                RichNode::Mention {
                    person: Some(person),
                    ..
                } if is_group(person) && person.name.is_none() => f(),
                RichNode::Link { url, children } if unnamed_channel(url, children).is_some() => f(),
                _ => {}
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::mrkdwn::{channel_link, group_mention};
    use crate::slack::translate::message;
    use serde_json::json;

    fn with(nodes: Vec<RichNode>) -> SlackMessage {
        let mut m = message(
            "C01",
            &json!({"type": "message", "user": "U01", "ts": "1759120000.000100"}),
        )
        .unwrap();
        m.parts = vec![Part::Rich(vec![RichNode::Paragraph { children: nodes }])];
        m
    }

    fn texts(message: &SlackMessage) -> Vec<String> {
        let mut out = Vec::new();
        if let Part::Rich(nodes) = &message.parts[0] {
            walk(nodes, &mut |node| match node {
                RichNode::Text { value } => out.push(value.clone()),
                RichNode::Mention {
                    person: Some(person),
                    ..
                } => out.push(format!("@{}", person.name.as_deref().unwrap_or("?"))),
                _ => {}
            });
        }
        out
    }

    // 本人所在的频道补名字，不在的写“私有频道”；用户组有名字就补，没有写“用户组”；Slack 自己带了名字的不动。
    #[test]
    fn channels_and_groups_get_names_or_a_plain_fallback() {
        let mut messages = vec![with(vec![
            channel_link("C02", None),
            channel_link("C09", None),
            channel_link("C03", Some("带名字")),
            group_mention("S01", None),
            group_mention("S02", None),
        ])];
        assert!(needs_names(&messages));
        let mut names = Names::default();
        // 频道没读成功过：留着编号，不乱写“私有频道”。
        let mut untouched = messages.clone();
        fill(&names, &mut untouched);
        assert_eq!(
            texts(&untouched[0])[..2],
            ["#C02".to_owned(), "#C09".to_owned()]
        );
        names.channels.insert("C02".into(), "发布".into());
        names.groups.insert("S01".into(), "dev".into());
        names.loaded_at = Some(Instant::now());
        fill(&names, &mut messages);
        assert_eq!(
            texts(&messages[0]),
            ["#发布", PRIVATE_CHANNEL, "#带名字", "@dev", "@用户组"]
        );
        assert!(!needs_names(&messages), "补过的不再要补");
    }
}
