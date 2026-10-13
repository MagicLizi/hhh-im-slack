//! Slack IM 插件程序（HHH-IM-Protocol v1）。
//!
//! 插件是一个子进程：HHH 从标准输入一行一条发 JSON-RPC 请求，插件从标准输出一行一条回；日志走 `log/write`
//! 通知，标准输出不写别的东西。协议服务与各个方法在库里（`hhh_im_slack::server`）。
//! `--version` 只打印版本就退出（手动核对装的是哪一版用）。

// Windows：按窗口程序编，桌面宿主拉起时不弹控制台窗口；标准输入输出照样走宿主接好的管道。
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|arg| arg == "--version") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Slack 插件的运行时建不起来：{error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(hhh_im_slack::server::run_stdio());
    // 标准输入关了：还在跑的任务至多再等一秒（让最后几条回复写出去）。
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
}
