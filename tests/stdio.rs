//! 真的插件程序：宿主那一侧的子进程传输把它拉起来，握手、心跳、关闭都走标准输入输出（Windows 上按窗口程序编，
//! 标准输入输出照样通）；标准输出里只有协议消息（不合法的行宿主会记成乱输出）。

use hhh_im_protocol::transport::{Connection, Incoming, StdioChild};
use serde_json::json;

#[tokio::test]
async fn the_plugin_program_speaks_the_protocol_over_stdio() {
    let program = env!("CARGO_BIN_EXE_hhh-im-slack");
    let child = StdioChild::spawn(program, &[], &std::env::temp_dir(), |_| {}).unwrap();
    let conn = Connection::new(child.clone());
    let init = conn
        .call(
            "initialize",
            json!({ "protocolVersions": [1], "host": { "name": "HHH", "version": "0.9.0" }, "locale": "zh-CN",
                    "tempDir": std::env::temp_dir(), "network": {} }),
        )
        .await
        .unwrap();
    assert_eq!(init, hhh_im_slack::server::initialize_result());
    assert_eq!(conn.call("ping", json!({})).await.unwrap(), json!({}));
    let open = conn
        .call("session/open", json!({ "settings": {} }))
        .await
        .unwrap();
    assert_eq!(open["state"], "needsLogin");
    assert_eq!(conn.call("shutdown", json!({})).await.unwrap(), json!({}));
    // 插件的日志走 log/write：至少有一条“已启动”。
    let mut logs = 0;
    while let Ok(Some(incoming)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), conn.next_incoming()).await
    {
        if let Incoming::Notification { method, .. } = incoming {
            assert_eq!(method, "log/write");
            logs += 1;
        }
    }
    assert!(logs >= 1);
    assert!(!child.garbled(), "标准输出里只有协议消息");
    child.shutdown().await;
}
