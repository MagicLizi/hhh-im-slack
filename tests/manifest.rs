//! 插件清单 `hhh-im-provider.json` 按协议包的清单类型读得懂、合规，并与 HHH 内置 Slack 的平台事实对得上（名字、
//! 种类、标志的画法与图形范围；标志文件与内置 Slack 随代码带的 `logo.svg` 是同一张图，HHH 那边的对拍另核）。

use serde_json::Value;

const MANIFEST: &str = include_str!("../hhh-im-provider.json");

#[test]
fn the_manifest_uses_the_protocol_field_names_and_passes_its_checks() {
    let manifest: hhh_im_protocol::Manifest = serde_json::from_str(MANIFEST).unwrap();
    assert!(hhh_im_protocol::Manifest::id_ok(&manifest.id));
    assert!(manifest.im_type_ok());
    assert!(manifest.logo_ok());
    manifest.release_fields_ok().unwrap();
    // 清单类型不拒收不认识的字段：原文的每一项再写回去都还在、值也一样，才说明每个字段名都写对了。
    let raw: Value = serde_json::from_str(MANIFEST).unwrap();
    let written = serde_json::to_value(&manifest).unwrap();
    for (key, value) in raw.as_object().unwrap() {
        assert_eq!(written.get(key), Some(value), "清单字段 {key} 协议不认");
    }
}

#[test]
fn the_manifest_matches_the_builtin_slack_facts() {
    let manifest: hhh_im_protocol::Manifest = serde_json::from_str(MANIFEST).unwrap();
    // 保留编号与内置 Slack 同名：带官方签名时顶替它（附录 D）。
    assert_eq!(manifest.id, "slack");
    assert_eq!(manifest.name, "Slack");
    assert_eq!(manifest.im_type.as_deref(), Some("slack"));
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.command, env!("CARGO_PKG_NAME"));
    assert_eq!(
        manifest.logo_style,
        Some(hhh_im_protocol::platform::LogoStyle::Mark)
    );
    // 内置 Slack：四色井字，图形 122.8×122.8，从画布原点起。
    let ink = manifest.logo_ink.unwrap();
    assert_eq!(
        (ink.x, ink.y, ink.width, ink.height),
        (0.0, 0.0, 122.8, 122.8)
    );
    let logo = include_str!("../logo.svg");
    assert!(logo.starts_with("<svg") && logo.contains("viewBox"));
}
