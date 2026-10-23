//! 并发读取的诊断日志必须按源时钟配对，不依赖宿主收到日志的时间。
mod common;

use common::*;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn concurrent_reads_pair_start_and_finish_and_keep_rate_limit_details_without_secrets() {
    let (log_host, mut log_writes) = hhh_im_protocol::runtime::Host::new();
    hhh_im_protocol::runtime::install_log(&log_host, "hhh_im_slack");
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(|request: &Request| {
            let bot = &form(request)["bot"];
            if bot == "B000000001" {
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_delay(Duration::from_millis(100))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok":true,"bot":{"id":bot,"name":"测试机器人"}}))
                    .set_delay(Duration::from_millis(if bot == "B000000002" {
                        20
                    } else {
                        180
                    }))
            }
        })
        .mount(&slack)
        .await;
    let mut reads = Vec::new();
    for number in 1..=3 {
        let conn = host.conn.clone();
        reads.push(tokio::spawn(async move {
            conn.call(
                "people/get",
                json!({"ids":[],"bots":[format!("B00000000{number}")]}),
            )
            .await
        }));
    }
    for reading in reads {
        let _ = reading.await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut logs = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(10), log_writes.next()).await
    {
        logs.push(event.params.unwrap());
    }
    let mut starts = BTreeMap::new();
    let mut finishes = BTreeMap::new();
    for event in logs
        .iter()
        .filter(|event| event["fields"]["method"] == "bots.info")
    {
        let fields = &event["fields"];
        if event["message"] == "向 Slack 读取平台数据" {
            assert!(
                starts
                    .insert(fields["read_started_us"].as_u64().unwrap(), fields)
                    .is_none()
            );
        } else if event["message"] == "Slack 平台请求已返回" {
            let start = fields["read_started_us"]
                .as_str()
                .unwrap()
                .strip_prefix("Some(")
                .unwrap()
                .strip_suffix(')')
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!(finishes.insert(start, fields).is_none());
        }
    }
    assert_eq!(starts.len(), 3, "每次实际请求都有源时钟开始记录");
    assert_eq!(finishes.len(), 3, "包括429在内的每个结果都能精确配对");
    assert!(starts.keys().eq(finishes.keys()));
    for (start, fields) in &finishes {
        let finish = fields["read_finished_us"]
            .as_str()
            .unwrap()
            .strip_prefix("Some(")
            .unwrap()
            .strip_suffix(')')
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(finish >= *start);
    }
    assert_eq!(
        finishes
            .values()
            .filter(|fields| fields["retry_after_secs"] == "Some(7)")
            .count(),
        1
    );
    let recorded = serde_json::to_string(&logs).unwrap();
    assert!(!recorded.contains(TOKEN));
    assert!(!recorded.contains("B00000000"), "不记录请求参数或会话内容");
}
