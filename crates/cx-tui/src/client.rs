//! 与守护进程控制套接字的后台通信。
//!
//! 为什么要分出一个线程：控制套接字是阻塞的（这是刻意的——空闲时不占 CPU），
//! 如果 UI 线程直接读写，网络或守护进程一卡，界面就僵住、连 Ctrl+C 都要等。
//! 这里把 IO 全部丢给后台线程，UI 只做"收发消息 + 画帧"。
//!
//! 用两条连接：一条负责请求/响应，另一条专门订阅事件流。
//! 混在一条连接上会让"读事件"与"读响应"互相抢数据（都是 JSON Lines），
//! 分成两条之后语义清晰，退出时也各自关闭。

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use cx_core::Paths;
use cx_daemon::ControlClient;
use cx_proto::control::{
    DoctorReport, HistoryPreview, HistorySummary, StatusReport, cmd as commands,
};
use serde_json::{Map, json};
use std::time::{Duration, Instant};

/// 后台线程送给界面的消息。
#[derive(Debug)]
pub enum UiEvent {
    /// 守护进程状态。
    Status(Box<StatusReport>),
    /// 历史列表。
    History(Vec<HistorySummary>),
    /// 某条内容的完整预览。按**对象名**回传：位置会随新内容漂移（0 永远是最新），
    /// 名字不会，界面因此能把结果稳定贴到某一行上。
    Preview {
        /// 对象名。
        name: String,
        /// 预览结果（失败时给出原因，界面在那一行上就地标出来）。
        result: Result<Box<HistoryPreview>, String>,
    },
    /// 某个请求失败。
    Failure {
        /// 命令名（便于界面区分是"拉预览失败"还是别的）。
        command: String,
        /// 失败原因。
        message: String,
    },
    /// 连接断开（界面据此退出）。
    Disconnected,
    /// 诊断报告（后端面板用它显示局域网对端等只有 doctor 才知道的信息）。
    Doctor(Box<DoctorReport>),
    /// 一条普通提示。
    Info(String),
}

/// 界面发给后台线程的请求。
#[derive(Debug, Clone)]
enum Request {
    Status,
    History(usize),
    Preview {
        position: usize,
        name: String,
    },
    Copy(usize),
    CopyChecked(usize, String),
    Push,
    Simple(&'static str),
    Delete(usize),
    DeleteChecked(usize, String),
    Doctor,
    /// 守护进程推来事件后触发的刷新。
    ExternalRefresh,
    Feedback(String),
    Stop,
}

/// 控制套接字客户端（后台线程的句柄）。
pub struct DaemonClient {
    tx: Sender<Request>,
    rx: Receiver<UiEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DaemonClient {
    /// 启动后台线程并立刻拉一轮状态与历史。
    pub fn start(paths: Paths) -> Self {
        let (request_tx, request_rx) = unbounded::<Request>();
        let (event_tx, event_rx) = unbounded::<UiEvent>();

        let thread = {
            let request_tx = request_tx.clone();
            let event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name("cx-tui-io".to_string())
                .spawn(move || worker(paths, request_rx, request_tx, event_tx))
                .ok()
        };

        let client = Self {
            tx: request_tx,
            rx: event_rx,
            thread,
        };
        client.request_status();
        client.request_history(crate::HISTORY_LIMIT);
        client
    }

    /// 非阻塞取一条消息。
    pub fn try_recv(&self) -> Result<UiEvent, TryRecvError> {
        self.rx.try_recv()
    }

    /// 请求状态。
    pub fn request_status(&self) {
        let _ = self.tx.send(Request::Status);
    }

    /// 请求历史列表。
    pub fn request_history(&self, limit: usize) {
        let _ = self.tx.send(Request::History(limit));
    }

    /// 删除一条本机历史。
    pub fn request_delete(&self, position: usize) {
        let _ = self.tx.send(Request::Delete(position));
    }

    /// 删除前原子核对用户确认的对象名。
    pub fn request_delete_checked(&self, position: usize, name: String) {
        let _ = self.tx.send(Request::DeleteChecked(position, name));
    }

    /// 复制前原子核对用户选中的对象名。
    pub fn request_copy_checked(&self, position: usize, name: String) {
        let _ = self.tx.send(Request::CopyChecked(position, name));
    }

    /// 请求一次诊断（后端面板显示"局域网连上几个对端"要用）。
    pub fn request_doctor(&self) {
        let _ = self.tx.send(Request::Doctor);
    }

    /// 请求某条内容的完整预览。
    ///
    /// `name` 是这次请求对应的对象名，会原样回传——界面用它把结果贴到正确的行上。
    pub fn request_preview(&self, position: usize, name: String) {
        let _ = self.tx.send(Request::Preview { position, name });
    }

    /// 请求把某条写回剪贴板。
    pub fn request_copy(&self, position: usize) {
        let _ = self.tx.send(Request::Copy(position));
    }

    /// 请求立即发布本机剪贴板内容。
    pub fn request_push(&self) {
        let _ = self.tx.send(Request::Push);
    }

    /// 发一个无参数命令（pause / resume / reload）。
    pub fn request(&self, command: &'static str) {
        let _ = self.tx.send(Request::Simple(command));
    }

    /// 让后台线程收工（会把连接关掉）。
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 后台线程主体。
fn worker(
    paths: Paths,
    requests: Receiver<Request>,
    request_tx: Sender<Request>,
    events: Sender<UiEvent>,
) {
    'connect: loop {
        // 连不上守护进程时**不要退出**：从快捷方式启动时守护进程往往还没起来，
        // 一退出控制台窗口就瞬间关闭，用户只看到黑窗一闪（实测反馈就是这个）。
        // 这里持续重试，界面留在屏幕上显示"未连接"，连上后自动刷新。
        let mut client = loop {
            match ControlClient::connect(&paths) {
                Ok(client) => break client,
                Err(err) => {
                    let _ = events.send(UiEvent::Failure {
                        command: "connect".to_string(),
                        message: format!("{err}（正在自动重试…）"),
                    });
                    let _ = events.send(UiEvent::Disconnected);
                    // 等待下一轮，但**可被打断**：退出请求（界面的 Stop、或句柄被丢弃）
                    // 会立刻结束线程。用 sleep 轮询的话，shutdown() 得等满一轮——
                    // 实测让一个单元测试白等 90 秒，无守护进程时的快照也要等 20 秒。
                    match requests.recv_timeout(std::time::Duration::from_secs(1)) {
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        Ok(Request::Stop) => return,
                        // 重试期间界面还可能发别的请求，一律忽略（等连上再说）
                        _ => {}
                    }
                }
            }
        };

        // 连上了：让界面把"未连接"的提示换成实时状态
        let _ = events.send(UiEvent::Info("已连接守护进程".to_string()));
        let _ = request_tx.send(Request::Status);
        let _ = request_tx.send(Request::History(crate::HISTORY_LIMIT));

        // 事件流单独一条连接：任何事件都触发一次刷新，界面因此"跟着守护进程动"
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _pump = spawn_event_pump(paths.clone(), request_tx.clone());

        let mut last_refresh = Instant::now() - Duration::from_secs(1);
        // 通道关闭（界面退出）时循环自然结束
        loop {
            let request = match requests.recv_timeout(Duration::from_secs(2)) {
                Ok(request) => request,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => Request::ExternalRefresh,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            };
            if !matches!(request, Request::Stop) && client.simple(commands::STATUS).is_err() {
                let _ = events.send(UiEvent::Disconnected);
                // 不重放删除/复制/发布等操作，用户必须明确重试。
                let _ = events.send(UiEvent::Info(
                    "连接中断，正在重连；未完成操作请重新执行".into(),
                ));
                continue 'connect;
            }
            match request {
                Request::Stop => return,
                Request::Status => match client.simple(commands::STATUS) {
                    Ok(value) => match serde_json::from_value::<StatusReport>(value) {
                        Ok(report) => {
                            let _ = events.send(UiEvent::Status(Box::new(report)));
                        }
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: "status".to_string(),
                                message: format!("状态字段解析失败：{err}"),
                            });
                        }
                    },
                    Err(err) => {
                        let _ = events.send(UiEvent::Failure {
                            command: "status".to_string(),
                            message: err.to_string(),
                        });
                    }
                },
                Request::CopyChecked(position, ref name)
                | Request::DeleteChecked(position, ref name) => {
                    let copy = matches!(request, Request::CopyChecked(..));
                    let command = if copy {
                        commands::COPY_CHECKED
                    } else {
                        commands::HISTORY_DELETE_CHECKED
                    };
                    let mut args = Map::new();
                    args.insert("position".into(), json!(position));
                    args.insert("expected_name".into(), json!(name));
                    match client.request(command, args) {
                        Ok(_) => {
                            let _ = events.send(UiEvent::Info(
                                if copy {
                                    "已写回剪贴板"
                                } else {
                                    "已删除所确认的本机历史"
                                }
                                .into(),
                            ));
                        }
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: command.into(),
                                message: err.to_string(),
                            });
                        }
                    }
                }
                Request::Delete(position) => {
                    let mut args = Map::new();
                    args.insert("position".to_string(), json!(position));
                    match client.request(commands::HISTORY_DELETE, args) {
                        Ok(_) => {
                            let _ = events.send(UiEvent::Info(format!(
                                "已删除第 {position} 条的本机历史（其他设备不受影响）"
                            )));
                        }
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: "history.delete".to_string(),
                                message: err.to_string(),
                            });
                        }
                    }
                }
                Request::Doctor => match client.simple(commands::DOCTOR) {
                    Ok(value) => match serde_json::from_value::<DoctorReport>(value) {
                        Ok(report) => {
                            let _ = events.send(UiEvent::Doctor(Box::new(report)));
                        }
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: "doctor".to_string(),
                                message: format!("诊断字段解析失败：{err}"),
                            });
                        }
                    },
                    Err(err) => {
                        let _ = events.send(UiEvent::Failure {
                            command: "doctor".to_string(),
                            message: err.to_string(),
                        });
                    }
                },
                Request::History(limit) => {
                    let mut args = Map::new();
                    args.insert("limit".to_string(), json!(limit));
                    match client.request(commands::HISTORY_LIST, args) {
                        Ok(value) => match serde_json::from_value::<Vec<HistorySummary>>(value) {
                            Ok(items) => {
                                let _ = events.send(UiEvent::History(items));
                            }
                            Err(err) => {
                                let _ = events.send(UiEvent::Failure {
                                    command: "history.list".to_string(),
                                    message: format!("历史字段解析失败：{err}"),
                                });
                            }
                        },
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: "history.list".to_string(),
                                message: err.to_string(),
                            });
                        }
                    }
                }
                Request::Preview { position, name } => {
                    let mut args = Map::new();
                    args.insert("position".to_string(), json!(position));
                    args.insert("expected_name".to_string(), json!(&name));
                    let result = match client.request(commands::HISTORY_SHOW_CHECKED, args) {
                        Ok(value) => serde_json::from_value::<HistoryPreview>(value)
                            .map(Box::new)
                            .map_err(|err| format!("预览字段解析失败：{err}")),
                        Err(err) => Err(err.to_string()),
                    };
                    // 按对象名回传：位置会随新内容漂移（0 永远是最新），名字不会，
                    // 界面因此可以把结果稳定地贴到某一行上。
                    let _ = events.send(UiEvent::Preview { name, result });
                }
                Request::Copy(position) => {
                    let mut args = Map::new();
                    args.insert("position".to_string(), json!(position));
                    match client.request(commands::COPY, args) {
                        Ok(_) => {
                            let _ = events
                                .send(UiEvent::Info(format!("已把第 {position} 条写回剪贴板")));
                        }
                        Err(err) => {
                            let _ = events.send(UiEvent::Failure {
                                command: "copy".to_string(),
                                message: err.to_string(),
                            });
                        }
                    }
                }
                Request::Push => match client.request(commands::PUSH, Map::new()) {
                    Ok(_) => {
                        let _ = events.send(UiEvent::Info(
                            "已请求发布；请查看后端状态确认上传，跳过原因会单独提示".to_string(),
                        ));
                    }
                    Err(err) => {
                        let _ = events.send(UiEvent::Failure {
                            command: "push".to_string(),
                            message: err.to_string(),
                        });
                    }
                },
                Request::Simple(command) => match client.simple(command) {
                    Ok(_) => {
                        let _ = events.send(UiEvent::Info(match command {
                            commands::PAUSE => "已暂停同步".to_string(),
                            commands::RESUME => "已恢复同步".to_string(),
                            commands::RELOAD => "已重新读取配置".to_string(),
                            other => format!("{other} 完成"),
                        }));
                        let _ = request_tx.send(Request::Status);
                    }
                    Err(err) => {
                        let _ = events.send(UiEvent::Failure {
                            command: command.to_string(),
                            message: err.to_string(),
                        });
                    }
                },
                Request::Feedback(message) => {
                    let _ = events.send(UiEvent::Info(message));
                }
                Request::ExternalRefresh => {
                    // 事件可能来得很密，限流到 300ms 一次，别把守护进程问爆
                    if last_refresh.elapsed() < Duration::from_millis(300) {
                        continue;
                    }
                    last_refresh = Instant::now();
                    let _ = request_tx.send(Request::Status);
                    let _ = request_tx.send(Request::History(crate::HISTORY_LIMIT));
                }
            }
        }
    }
}

/// 订阅守护进程事件；任何事件都让 UI 刷一次。
struct EventPump {
    socket: std::net::TcpStream,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for EventPump {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_event_pump(paths: Paths, request_tx: Sender<Request>) -> Option<EventPump> {
    let mut client = ControlClient::connect(&paths).ok()?;
    client.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    client.subscribe().ok()?;
    client.set_read_timeout(None).ok()?;
    let socket = client.cancellation_socket().ok()?;
    let thread = std::thread::Builder::new()
        .name("cx-tui-events".to_string())
        .spawn(move || {
            loop {
                match client.next_event() {
                    Ok(event) => {
                        log::debug!("收到事件 {}", event.event);
                        if event.event == cx_proto::control::event::CLIP_SKIPPED {
                            let reason = event
                                .data
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let label = match reason {
                                "encryption_not_ready" => "空间密钥未就绪，仅记录本机历史，未外发",
                                "too_large" => "内容超过大小上限，未发布",
                                "blank_text" => "空白内容已忽略",
                                "ignore_glob" => "内容命中忽略规则，未发布",
                                "outbox_write_failed" => "发布队列落盘失败，未排队，请检查磁盘",
                                _ => "内容按同步策略跳过",
                            };
                            let _ = request_tx.send(Request::Feedback(label.into()));
                        }
                        if request_tx.send(Request::ExternalRefresh).is_err() {
                            break; // 主线程已退出
                        }
                    }
                    Err(err) => {
                        log::debug!("事件流结束：{err}");
                        break;
                    }
                }
            }
        })
        .ok()?;
    Some(EventPump {
        socket,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 订阅句柄丢弃会终止线程且旧守护进程拒绝受保护操作() {
        use cx_daemon::{Broadcaster, ControlServer, DaemonInfo};
        use std::sync::{Arc, atomic::Ordering};
        let root = std::env::temp_dir().join(format!(
            "cx-tui-pump-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        let token = cx_daemon::control::ensure_control_token(&paths).unwrap();
        let server = ControlServer::bind(token).unwrap();
        let port = server.port().unwrap();
        let shutdown = server.shutdown_flag();
        let broadcast = Broadcaster::new();
        let subscriptions = broadcast.clone();
        let daemon = server.serve(
            Arc::new(|request| {
                cx_proto::control::Reply::err(
                    request.id,
                    "unknown_command",
                    "legacy command not supported",
                )
            }),
            Arc::new(move || subscriptions.subscribe()),
        );
        DaemonInfo {
            v: cx_daemon::DAEMON_INFO_VERSION,
            pid: std::process::id(),
            port,
            started_ms: cx_core::now_ms(),
            version: "legacy".into(),
            control_protocol: cx_proto::control::CONTROL_PROTOCOL_VERSION,
        }
        .save(&paths)
        .unwrap();
        let (tx, _rx) = unbounded();
        for _ in 0..3 {
            let pump = spawn_event_pump(paths.clone(), tx.clone()).unwrap();
            assert_eq!(broadcast.len(), 1);
            let started = Instant::now();
            drop(pump);
            assert!(started.elapsed() < Duration::from_secs(2));
            for _ in 0..20 {
                broadcast.send(cx_proto::control::Event::new("test", json!({})));
                if broadcast.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(broadcast.is_empty());
        }
        let mut client = ControlClient::connect(&paths).unwrap();
        let mut args = Map::new();
        args.insert("position".into(), json!(0));
        args.insert("expected_name".into(), json!("fake"));
        assert!(
            client
                .request(commands::HISTORY_DELETE_CHECKED, args)
                .is_err()
        );
        shutdown.store(true, Ordering::Relaxed);
        drop(client);
        daemon.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 连接失败会立刻报告并退出() {
        // 指向一个几乎不可能有守护进程的目录
        let root = std::env::temp_dir().join(format!(
            "cx-tui-nodaemon-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let client = DaemonClient::start(paths);
        let mut saw_disconnect = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match client.try_recv() {
                Ok(UiEvent::Disconnected) => {
                    saw_disconnect = true;
                    break;
                }
                Ok(_) => continue,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        assert!(saw_disconnect, "没有守护进程时应当报告断开");
        client.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
