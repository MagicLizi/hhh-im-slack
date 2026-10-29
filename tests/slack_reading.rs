//! 阅读查询与精确写回；权限不足、响应不确定不能假报成功或重放。
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

async fn mark_target(slack: &MockServer, exact: bool) {
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .and(body_string_contains("channel=C000000099"))
        .and(body_string_contains("latest=1790000000.000100"))
        .and(body_string_contains("oldest=1790000000.000100"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok":true,"messages":
                if exact {json!([{"ts":"1790000000.000100"}])}else{json!([])}
            })),
        )
        .mount(slack)
        .await;
}

fn mark_params() -> serde_json::Value {
    json!({"scope":{"conversationId":"C000000099"},"messageId":"1790000000.000100","position":"1790000000000100"})
}

#[tokio::test]
async fn mark_writes_the_exact_microsecond_once_and_confirms_by_reading() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let queries = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .respond_with(move |_: &wiremock::Request| {
            let last = if queries.fetch_add(1, Ordering::SeqCst) == 0 {
                "1790000000.000099"
            } else {
                "1790000000.000100"
            };
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok":true,"channel":{"id":"C000000099","last_read":last}}))
        })
        .expect(2)
        .mount(&slack)
        .await;
    mark_target(&slack, true).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.mark"))
        .and(body_string_contains("channel=C000000099"))
        .and(body_string_contains("ts=1790000000.000100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .expect(1)
        .mount(&slack)
        .await;
    assert_eq!(
        host.ok("reading/mark", mark_params()).await["state"],
        "applied"
    );
}

#[tokio::test]
async fn mark_preserves_permission_denial_and_does_not_retry_an_uncertain_write() {
    for (response, expected) in [
        (
            ResponseTemplate::new(200).set_body_json(json!({"ok":false,"error":"missing_scope"})),
            Some(ErrorKind::PermissionDenied),
        ),
        (
            ResponseTemplate::new(503)
                .set_body_json(json!({"ok":false,"error":"service_unavailable"})),
            None,
        ),
    ] {
        let slack = MockServer::start().await;
        let host = online(&slack).await;
        Mock::given(method("POST"))
            .and(path("/api/conversations.info"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"ok":true,"channel":{"id":"C000000099","last_read":"0"}}),
                ),
            )
            .expect(1)
            .mount(&slack)
            .await;
        mark_target(&slack, true).await;
        Mock::given(method("POST"))
            .and(path("/api/conversations.mark"))
            .respond_with(response)
            .expect(1)
            .mount(&slack)
            .await;
        if let Some(kind) = expected {
            assert_eq!(
                kind_of(&host.err("reading/mark", mark_params()).await),
                kind
            );
        } else {
            assert_eq!(
                host.ok("reading/mark", mark_params()).await["state"],
                "unknown"
            );
        }
    }
}

#[tokio::test]
async fn mark_rejects_a_missing_message_or_mismatched_boundary_without_writing() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok":true,"channel":{"id":"C000000099","last_read":"0"}})),
        )
        .expect(1)
        .mount(&slack)
        .await;
    mark_target(&slack, false).await;
    Mock::given(path("/api/conversations.mark"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&slack)
        .await;
    assert_eq!(
        kind_of(&host.err("reading/mark", mark_params()).await),
        ErrorKind::InvalidRequest
    );
    let mut params = mark_params();
    params["position"] = json!("1790000000000101");
    assert_eq!(
        kind_of(&host.err("reading/mark", params).await),
        ErrorKind::InvalidRequest
    );
}
