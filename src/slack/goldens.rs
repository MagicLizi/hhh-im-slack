//! 协议样例：Slack 的取样响应（`tests/fixtures/slack/`，取自真实 Slack、去掉了身份信息）翻成线上消息，存在
//! `tests/fixtures/slack-protocol/`。样例锁住插件的输出；它们会拷进 HHH，宿主把它翻回来，要与 HHH 内置 Slack 直接
//! 翻译同一份原始数据的结果逐字段相同（HHH 的 `provider/slack_parity_tests.rs`）。
//!
//! 每条都当作发送者本人看（本人发的、能编辑的带编辑原文），宿主那边顺带核对编辑框与内置一样。
//!
//! 改了翻译、样例要跟着变时：`HHH_UPDATE_GOLDENS=1 cargo test goldens` 重写样例，再看一遍差异。

use std::path::{Path, PathBuf};

use hhh_im_protocol::types::SenderKind;
use serde_json::Value;

use super::translate;

/// 取样消息所在的会话（同内置的夹具测试）。
const CONVERSATION: &str = "C000000099";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|e| panic!("{}：{e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("{}：{e}", path.display()))
}

/// 样例与这次算出的比对；设了 `HHH_UPDATE_GOLDENS` 时改写样例。
pub(crate) fn lock(name: &str, actual: &Value) {
    let path = fixtures().join("slack-protocol").join(name);
    if std::env::var_os("HHH_UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string_pretty(actual).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        return;
    }
    assert!(
        path.exists(),
        "样例 {} 还没有：HHH_UPDATE_GOLDENS=1 cargo test goldens 生成",
        path.display()
    );
    assert_eq!(&read(&path), actual, "样例 {} 对不上", path.display());
}

/// 每份取样响应（`conversations.history` 的一页，里面一条消息）翻成线上消息，样例锁住；线上消息按协议类型读回来不变。
#[test]
fn goldens_every_sampled_response_has_a_wire_golden() {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(fixtures().join("slack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    let mut count = 0;
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("response_") || !name.ends_with(".json") {
            continue;
        }
        let page = read(&path);
        let raw = page["messages"].as_array().expect("取样是一页历史");
        assert_eq!(raw.len(), 1, "{name}：一份取样一条消息");
        let translated = translate::message(CONVERSATION, &raw[0]).unwrap();
        let sender = &translated.message.sender;
        let me = (sender.kind == SenderKind::User).then(|| sender.id.clone());
        let wire = translated.wire(me.as_deref());
        let value = serde_json::to_value(&wire).unwrap();
        let back: hhh_im_protocol::types::Message = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, wire, "{name}：按协议类型读回来变了");
        lock(&name, &value);
        count += 1;
    }
    assert!(count >= 10, "不能用空夹具目录冒充对拍成功：只有 {count} 个");
}
