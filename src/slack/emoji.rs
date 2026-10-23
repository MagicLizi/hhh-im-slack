//! Slack 标准表情：名字 → Unicode、分类与官方图（契约 client v11 §3.7、v18 §2.3；从 HHH 内置 Slack 原样搬来，
//! 目录写成协议的表情，图的地址插件自己记着，宿主按表情类型来要图）。
//!
//! Slack 的表情回复与 mrkdwn 里只有名字（如 `+1`、`wave::skin-tone-3`），没有接口给标准表情的列表；
//! 自定义表情另由 `emoji.list` 按工作区读取。标准表情表随库自带，来源与许可证写在表文件头。
//! 图用 Slack 客户端自己的那套（Apple 样式，放在 Slack 的静态资源 CDN 上），按码位拼地址。

use hhh_im_protocol::types::{Emoji, EmojiCategory, EmojiTone, ToneOf};
use std::sync::OnceLock;

/// 生成的名字表（HHH 仓库 `hhh-im/examples/slack_emoji_table.rs` 生成，原样带来）。
const TABLE: &str = include_str!("emoji_names.tsv");

/// Slack 标准表情图的地址：`{unified}` 换成名字表里的码位（小写、`-` 连接、保留 FE0F）。
/// 版本与名字表的数据版本一致；比这个版本新的表情、不带 FE0F 的写法 CDN 返回 403（2026-10-01 取样 14.0 / 15.0 / 16.0）。
const IMAGE_URL: &str =
    "https://a.slack-edge.com/production-standard-emoji-assets/16.0/apple-medium/{unified}@2x.png";

/// 一行：名字、码位、能否加肤色、分类、面板里的顺序、是哪个名字的别名、有没有图、五种肤色版本的码位。
struct Entry {
    name: &'static str,
    unified: &'static str,
    skin: bool,
    category: &'static str,
    order: i64,
    primary: &'static str,
    apple: bool,
    tones: Vec<&'static str>,
}

fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        TABLE
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .filter_map(|line| {
                let mut parts = line.split('\t');
                Some(Entry {
                    name: parts.next()?,
                    unified: parts.next()?,
                    skin: parts.next()? == "1",
                    category: parts.next()?,
                    order: parts.next()?.parse().ok()?,
                    primary: parts.next()?,
                    apple: parts.next()? == "1",
                    tones: parts
                        .next()
                        .filter(|t| !t.is_empty())
                        .map(|t| t.split(',').collect())
                        .unwrap_or_default(),
                })
            })
            .collect()
    })
}

fn entry(name: &str) -> Option<&'static Entry> {
    let table = entries();
    table
        .binary_search_by(|entry| entry.name.cmp(name))
        .ok()
        .map(|index| &table[index])
}

/// 别名的主名字那一行；自己就是主名字时是它自己。
fn primary_of(entry: &'static Entry) -> Option<&'static Entry> {
    if entry.primary.is_empty() {
        Some(entry)
    } else {
        self::entry(entry.primary)
    }
}

/// 肤色后缀 `::skin-tone-2` 到 `::skin-tone-6` 的叫法（界面的 `tone`）。
const TONES: [&str; 5] = ["浅色", "中浅色", "中等色", "中深色", "深色"];

/// Slack 面板的分类与顺序（笑脸与人物合成一类，自定义表情在最后）：编号、中文名，以及分类标签画 HHH 图标库里的哪个图标
/// （HHH 契约 provider-protocol v31 的 `icon`，取 HHH 图标库的名字；分类编号本身不决定图标）。
const CATEGORIES: [(&str, &str, &str); 9] = [
    ("smileys", "笑脸与人物", "smileys"),
    ("nature", "动物与自然", "nature"),
    ("food", "食物与饮料", "food"),
    ("travel", "旅行与地点", "travel"),
    ("activities", "活动", "activities"),
    ("objects", "物品", "objects"),
    ("symbols", "符号", "symbols"),
    ("flags", "旗帜", "flags"),
    (CUSTOM, "自定义", "custom"),
];

/// 自定义表情的分类编号。
pub(crate) const CUSTOM: &str = "custom";

/// 目录里的一项：协议的表情与它的图在哪（宿主来要图时插件按地址下载）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogItem {
    /// 协议的表情。
    pub(crate) emoji: Emoji,
    /// 图的地址；没有图时为空。
    pub(crate) image_url: String,
}

impl CatalogItem {
    /// 平台目录里的一个表情：进面板，还不知道中文名（同 HHH 内置时的默认值）。
    pub(crate) fn new(emoji_type: impl Into<String>, image_url: impl Into<String>) -> Self {
        Self {
            emoji: Emoji {
                emoji_type: emoji_type.into(),
                name: None,
                panel: true,
                unicode: None,
                category: None,
                aliases: Vec::new(),
                tone: None,
                tone_of: None,
                in_text: true,
            },
            image_url: image_url.into(),
        }
    }
}

/// 目录的分类，顺序就是面板里的顺序。
pub(crate) fn categories() -> Vec<EmojiCategory> {
    CATEGORIES
        .iter()
        .map(|(id, name, icon)| EmojiCategory {
            id: (*id).into(),
            name: (*name).into(),
            icon: Some((*icon).into()),
        })
        .collect()
}

/// `emoji/catalog` 的 JSON：每个分类带上它的图标（见 [`category_icon`]）。
pub(crate) fn catalog_json(
    catalog: &hhh_im_protocol::method::EmojiCatalogResult,
) -> serde_json::Value {
    let mut value = serde_json::to_value(catalog).expect("表情目录总能写成 JSON");
    if let Some(categories) = value
        .get_mut("categories")
        .and_then(serde_json::Value::as_array_mut)
    {
        for category in categories {
            let icon = category
                .get("id")
                .and_then(serde_json::Value::as_str)
                .and_then(category_icon);
            if let Some(icon) = icon {
                category["icon"] = serde_json::Value::from(icon);
            }
        }
    }
    value
}

/// 分类的图标（见 [`CATEGORIES`]）；不在面板分类里的没有。
pub(crate) fn category_icon(id: &str) -> Option<&'static str> {
    CATEGORIES
        .iter()
        .find(|(own, _, _)| *own == id)
        .map(|(_, _, icon)| *icon)
}

/// 目录的肤色：编号就是名字后缀 `skin-tone-2` 到 `-6`。
pub(crate) fn tones() -> Vec<EmojiTone> {
    TONES
        .iter()
        .enumerate()
        .map(|(step, name)| EmojiTone {
            id: format!("skin-tone-{}", step + 2),
            name: (*name).into(),
        })
        .collect()
}

fn image_url(unified: &str) -> String {
    IMAGE_URL.replace("{unified}", unified)
}

/// 肤色版本的码位：名字表里写在主名字上；没有（这个肤色没有图）时为空。
fn tone_unified(primary: &Entry, tone: u32) -> Option<&'static str> {
    primary
        .tones
        .get(usize::try_from(tone.checked_sub(2)?).ok()?)
        .copied()
        .filter(|u| !u.is_empty())
}

fn chars(unified: &str) -> String {
    unified
        .split('-')
        .filter_map(|hex| u32::from_str_radix(hex, 16).ok())
        .filter_map(char::from_u32)
        .collect()
}

/// 标准表情目录（契约 client v11 §3.7、v18 §2.3）：主名字按 Slack 面板的分类与顺序进面板，带别名、Unicode
/// 字符与官方图；别名（如 `thumbsup` 之于 `+1`）不进面板、只用于显示；能加肤色的另有
/// `名字::skin-tone-2` 到 `-6` 五项，注明是哪个表情的哪种肤色，不进面板。不在面板分类里的（肤色色块）也不进面板。
pub(crate) fn catalog() -> Vec<CatalogItem> {
    let table = entries();
    let rank = |category: &str| {
        CATEGORIES
            .iter()
            .position(|(id, _, _)| *id == category)
            .unwrap_or(CATEGORIES.len())
    };
    let mut primaries: Vec<&Entry> = table.iter().filter(|e| e.primary.is_empty()).collect();
    primaries.sort_by_key(|e| (rank(e.category), e.order));
    let mut out = Vec::with_capacity(table.len() * 2);
    for entry in primaries {
        let Some((unicode, url)) = standard_info(entry.name) else {
            continue;
        };
        let mut item = CatalogItem::new(entry.name, url);
        item.emoji.unicode = Some(unicode);
        item.emoji.panel = !entry.category.is_empty();
        item.emoji.category = (!entry.category.is_empty()).then(|| entry.category.to_owned());
        item.emoji.aliases = table
            .iter()
            .filter(|alias| alias.primary == entry.name)
            .map(|alias| alias.name.to_owned())
            .collect();
        out.push(item);
        if !entry.skin {
            continue;
        }
        for (step, tone) in TONES.iter().enumerate() {
            let id = format!("skin-tone-{}", step + 2);
            let name = format!("{}::{id}", entry.name);
            let Some((unicode, url)) = standard_info(&name) else {
                continue;
            };
            let mut item = CatalogItem::new(name, url);
            item.emoji.unicode = Some(unicode);
            item.emoji.panel = false;
            item.emoji.tone = Some((*tone).to_owned());
            item.emoji.tone_of = Some(ToneOf {
                base: entry.name.to_owned(),
                tone: id,
            });
            out.push(item);
        }
    }
    for alias in table.iter().filter(|e| !e.primary.is_empty()) {
        if let Some((unicode, url)) = standard_info(alias.name) {
            let mut item = CatalogItem::new(alias.name, url);
            item.emoji.unicode = Some(unicode);
            item.emoji.panel = false;
            out.push(item);
        }
    }
    out
}

/// 标准表情名 → 显示用的 Unicode 字符与官方图地址（没有图时为空）；不是标准表情时为空。
/// 自定义表情指向标准表情的别名也用它。
pub(crate) fn standard_info(name: &str) -> Option<(String, String)> {
    let unicode = standard(name)?;
    let (base, tone) = split_tone(name);
    let primary = primary_of(entry(base)?)?;
    let url = match tone {
        Some(tone) if primary.skin => tone_unified(primary, tone).map(image_url),
        _ => primary.apple.then(|| image_url(primary.unified)),
    };
    Some((unicode, url.unwrap_or_default()))
}

/// `名字::skin-tone-N` 拆成名字与肤色（2 到 6）；后缀不对时肤色为空。
fn split_tone(name: &str) -> (&str, Option<u32>) {
    match name.split_once("::skin-tone-") {
        Some((base, tone)) => (
            base,
            tone.parse::<u32>().ok().filter(|t| (2..=6).contains(t)),
        ),
        None => (name, None),
    }
}

/// 标准表情名 → 显示用的 Unicode 字符串；不是标准表情（比如工作区自定义表情）时为空。
/// 肤色写成 `名字::skin-tone-2` 到 `::skin-tone-6`：名字表里有这个肤色版本的码位就照它写，
/// 没有时把 U+1F3FB 到 U+1F3FF 接在第一个码位后面。
pub(crate) fn standard(name: &str) -> Option<String> {
    let (base, tone) = split_tone(name);
    let entry = entry(base)?;
    if let Some(tone) = tone
        && entry.skin
        && let Some(unified) = primary_of(entry).and_then(|p| tone_unified(p, tone))
    {
        return Some(chars(unified));
    }
    let mut out: String = entry
        .unified
        .split('-')
        .filter_map(|hex| u32::from_str_radix(hex, 16).ok())
        // 加肤色时去掉“按表情样式显示”的选择符，把肤色接在第一个码位后面。
        .filter(|code| tone.is_none() || *code != 0xFE0F)
        .filter_map(char::from_u32)
        .collect();
    if let (Some(tone), true) = (tone, entry.skin) {
        let modifier = char::from_u32(0x1F3FB + tone - 2)?;
        let first = out.chars().next()?;
        out.insert(first.len_utf8(), modifier);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HHH 契约 provider-protocol v31：每个分类都给了图标，图标都在 HHH 的图标库里（hhh-im-protocol 的 `EMOJI_CATEGORY_ICONS`），
    /// 分类编号与图标分开写；目录 JSON 的其余部分不变。
    #[test]
    fn every_category_names_an_icon_from_the_host_library() {
        const HOST_ICONS: [&str; 9] = [
            "smileys",
            "nature",
            "food",
            "travel",
            "activities",
            "objects",
            "symbols",
            "flags",
            "custom",
        ];
        let catalog = hhh_im_protocol::method::EmojiCatalogResult {
            emojis: Vec::new(),
            categories: categories(),
            tones: tones(),
            updated_at_ms: 0,
        };
        let json = catalog_json(&catalog);
        let categories = json["categories"].as_array().unwrap();
        assert_eq!(categories.len(), CATEGORIES.len());
        for category in categories {
            let icon = category["icon"].as_str().expect("每个分类都有图标");
            assert!(HOST_ICONS.contains(&icon), "{icon}");
        }
        assert_eq!(json["tones"], serde_json::to_value(tones()).unwrap());
    }

    #[test]
    fn table_is_sorted_for_binary_search_and_not_empty() {
        let table = entries();
        assert!(table.len() > 1500, "表只有 {} 个名字", table.len());
        assert!(table.windows(2).all(|pair| pair[0].name < pair[1].name));
    }

    #[test]
    fn common_names_and_skin_tones_resolve() {
        assert_eq!(standard("+1").as_deref(), Some("\u{1F44D}"));
        assert_eq!(standard("thumbsup").as_deref(), Some("\u{1F44D}"));
        assert_eq!(standard("white_check_mark").as_deref(), Some("\u{2705}"));
        assert_eq!(
            standard("wave::skin-tone-3").as_deref(),
            Some("\u{1F44B}\u{1F3FC}")
        );
        // 不能加肤色的表情忽略肤色后缀；不认识的名字（自定义表情）为空。
        assert_eq!(
            standard("white_check_mark::skin-tone-3").as_deref(),
            Some("\u{2705}")
        );
        assert_eq!(standard("our-team-logo"), None);
        assert_eq!(standard("wave::skin-tone-9").as_deref(), Some("\u{1F44B}"));
    }

    #[test]
    fn the_standard_catalog_lists_each_character_once_in_the_panel_and_skin_tones_for_display() {
        let catalog = catalog();
        let find = |name: &str| catalog.iter().find(|e| e.emoji.emoji_type == name).unwrap();
        let plus = find("+1");
        assert_eq!(
            (
                plus.emoji.unicode.as_deref(),
                plus.emoji.panel,
                plus.image_url.as_str()
            ),
            (
                Some("\u{1F44D}"),
                true,
                "https://a.slack-edge.com/production-standard-emoji-assets/16.0/apple-medium/1f44d@2x.png"
            )
        );
        assert_eq!(
            (
                plus.emoji.category.as_deref(),
                plus.emoji.aliases.as_slice()
            ),
            (Some("smileys"), &["thumbsup".to_owned()][..])
        );
        // 同一个字符的别名只用于显示，图与主名字相同。
        let alias = find("thumbsup");
        assert!(!alias.emoji.panel);
        assert_eq!(alias.image_url, plus.image_url);
        let dark = find("+1::skin-tone-6");
        assert_eq!(
            (dark.emoji.panel, dark.emoji.tone.as_deref()),
            (false, Some("深色"))
        );
        assert_eq!(
            dark.emoji.tone_of,
            Some(ToneOf {
                base: "+1".into(),
                tone: "skin-tone-6".into()
            })
        );
        assert!(dark.image_url.ends_with("/1f44d-1f3ff@2x.png"));
        // 保留 FE0F；多人的表情用名字表里的肤色码位，不是简单接在第一个码位后面。
        assert!(find("relaxed").image_url.ends_with("/263a-fe0f@2x.png"));
        assert_eq!(
            find("people_holding_hands::skin-tone-2")
                .emoji
                .unicode
                .as_deref(),
            Some("\u{1F9D1}\u{1F3FB}\u{200D}\u{1F91D}\u{200D}\u{1F9D1}\u{1F3FB}")
        );
        // 面板顺序按 Slack 的分类与数据里的顺序：笑脸在前、旗帜在后；肤色色块不进面板。
        let panel: Vec<&Emoji> = catalog
            .iter()
            .map(|e| &e.emoji)
            .filter(|e| e.panel)
            .collect();
        assert_eq!(panel[0].emoji_type, "grinning");
        assert_eq!(panel.last().unwrap().category.as_deref(), Some("flags"));
        assert!(!find("skin-tone-2").emoji.panel);
        assert!(panel.iter().all(|e| e.category.is_some()));
        assert_eq!(categories().last().unwrap().id, CUSTOM);
        assert_eq!(tones()[0].id, "skin-tone-2");
        let in_panel = catalog.iter().filter(|e| e.emoji.panel).count();
        assert!((1500..=2000).contains(&in_panel), "面板里 {in_panel} 个");
        assert!(
            catalog
                .iter()
                .all(|e| super::super::mrkdwn::is_emoji_name(&e.emoji.emoji_type))
        );
    }
}
