//! 当前授权只声明会话回读。HTTP 样本字段缺失或无效时不能伪造阅读事实。
mod common;

use common::*;
use hhh_im_protocol::ErrorKind;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn reading_sync_preserves_precise_position_and_keeps_missing_position_unknown() {
    for (last_read, expected) in [
        (Some(json!("1790000000.000100")), Some("1790000000000100")),
        (None, None),
        (Some(serde_json::Value::Null), None),
    ] {
        let slack = MockServer::start().await;
        let host = online(&slack).await;
        let mut channel = json!({"id":"C000000099"});
        if let Some(value) = last_read {
            channel["last_read"] = value;
        }
        Mock::given(method("POST"))
            .and(path("/api/conversations.info"))
            .and(header("authorization", format!("Bearer {TOKEN}")))
            .and(body_string_contains("channel=C000000099"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"channel":channel})),
            )
            .expect(1)
            .mount(&slack)
            .await;
        let value = host
            .ok(
                "reading/sync",
                json!({"scope":{"conversationId":"C000000099"},"messageIds":[]}),
            )
            .await;
        assert_eq!(value["scope"]["conversationId"], "C000000099");
        assert_eq!(value["through"].as_str(), expected);
        assert!(value.get("readMessageIds").is_none());
    }
}

#[tokio::test]
async fn reading_sync_rejects_foreign_or_malformed_watermarks_and_threads() {
    for channel in [
        json!({"id":"C000000098","last_read":"1790000000.000100"}),
        json!({"id":"C000000099","last_read":1_790_000_000.000_1}),
        json!({"id":"C000000099","last_read":"1790000000.1"}),
    ] {
        let slack = MockServer::start().await;
        let host = online(&slack).await;
        Mock::given(method("POST"))
            .and(path("/api/conversations.info"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok":true,"channel":channel})),
            )
            .mount(&slack)
            .await;
        let error = host
            .err(
                "reading/sync",
                json!({"scope":{"conversationId":"C000000099"},"messageIds":[]}),
            )
            .await;
        assert_eq!(kind_of(&error), ErrorKind::ProtocolError);
        let error = host.err("reading/sync", json!({"scope":{"conversationId":"C000000099","threadRoot":"1790000000.000100"},"messageIds":[]})).await;
        assert_eq!(kind_of(&error), ErrorKind::Unsupported);
    }
}
