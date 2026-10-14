//! 控制套接字：服务端（守护进程内）与客户端（CLI/TUI 用）。
//!
//! 传输是**仅回环**的 TCP + 令牌认证 + JSON Lines。安全边界：
//! - 只绑定 `127.0.0.1`，不监听外部地址；
//! - 首行必须带 `control.token` 的内容（0600，常量时间比较），否则立刻断开；
//! - 令牌每次启动重新生成，因此旧进程留下的令牌无效。
//!
//! `daemon.json` 只是"怎么连上我"的公开信息（端口、pid、启动时间），不含秘密。

use crate::error::DaemonError;
use crossbeam_channel::{Receiver, Sender, unbounded};
use cx_core::Paths;
use cx_proto::control::{Auth, CONTROL_PROTOCOL_VERSION, Event, Reply, Request, errcode};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `daemon.json` 格式版本。
pub const DAEMON_INFO_VERSION: u32 = 1;

/// 单条控制连接的读超时：防止半开连接永久占住线程。
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(300);

/// 请求的读超时（`pull` 可能较慢）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// `daemon.json` 的内容（公开信息）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// 格式版本。
    pub v: u32,
    /// 进程 id。
    pub pid: u32,
    /// 控制端口。
    pub port: u16,
    /// 启动时间（毫秒，UTC）。
    pub started_ms: u64,
    /// `cx` 版本。
    pub version: String,
    /// 控制协议版本。
    pub control_protocol: u32,
}

impl DaemonInfo {
    /// 由运行中的守护进程写出。
    pub fn save(&self, paths: &Paths) -> Result<(), DaemonError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| DaemonError::protocol(e.to_string()))?;
        cx_core::fsutil::write_atomic(&paths.daemon_info_file(), text.as_bytes())?;
        Ok(())
    }

    /// 读取；文件缺失或损坏都返回 `None`（视作未运行）。
    pub fn load(paths: &Paths) -> Option<Self> {
        let text = cx_core::fsutil::read_to_string_optional(&paths.daemon_info_file())
            .ok()
            .flatten()?;
        serde_json::from_str(&text).ok()
    }

    /// 删除（守护进程退出时调用）。
    pub fn remove(paths: &Paths) {
        let _ = std::fs::remove_file(paths.daemon_info_file());
    }

    /// 试探端口是否仍有进程在监听。
    pub fn is_alive(&self) -> bool {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
    }
}

/// 读取控制令牌；不存在则生成一个（0600）。
pub fn ensure_control_token(paths: &Paths) -> Result<String, DaemonError> {
    let path = paths.control_token_file();
    if let Some(text) = cx_core::fsutil::read_to_string_optional(&path)? {
        let token = text.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = cx_crypto::random_hex(32)?;
    cx_core::fsutil::write_atomic(&path, token.as_bytes())?;
    cx_core::fsutil::tighten_permissions(&path, false)?;
    Ok(token)
}

/// 读取已有控制令牌（客户端用）；不存在时报"未运行"。
pub fn read_control_token(paths: &Paths) -> Result<String, DaemonError> {
    let path = paths.control_token_file();
    let text = cx_core::fsutil::read_to_string_optional(&path)?
        .ok_or_else(|| DaemonError::NotRunning("缺少 control.token".to_string()))?;
    Ok(text.trim().to_string())
}

/// 请求分发器：把请求交给引擎并等待响应。
pub type Dispatcher = Arc<dyn Fn(Request) -> Reply + Send + Sync>;

/// 订阅工厂：登记一个订阅者并拿到事件接收端。
pub type SubscribeFactory = Arc<dyn Fn() -> Receiver<Event> + Send + Sync>;

/// 控制服务端。
pub struct ControlServer {
    listener: TcpListener,
    token: String,
    shutdown: Arc<AtomicBool>,
}

impl ControlServer {
    /// 绑定回环地址（端口由系统分配）。
    pub fn bind(token: String) -> Result<Self, DaemonError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(DaemonError::io)?;
        listener.set_nonblocking(true).map_err(DaemonError::io)?;
        Ok(Self {
            listener,
            token,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 实际监听的端口。
    pub fn port(&self) -> Result<u16, DaemonError> {
        Ok(self.listener.local_addr().map_err(DaemonError::io)?.port())
    }

    /// 取得关闭标志（主线程置位后服务端线程退出）。
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// 启动接受循环（在后台线程里跑）。
    pub fn serve(
        self,
        dispatcher: Dispatcher,
        subscribe: SubscribeFactory,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("cx-control".to_string())
            .spawn(move || {
                let token = self.token;
                loop {
                    if self.shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    match self.listener.accept() {
                        Ok((stream, peer)) => {
                            if !peer.ip().is_loopback() {
                                // 理论不可达（绑定在回环），防御性检查
                                log::warn!("拒绝非回环连接：{peer}");
                                continue;
                            }
                            let dispatcher = dispatcher.clone();
                            let subscribe = subscribe.clone();
                            let token = token.clone();
                            let _ = std::thread::Builder::new()
                                .name("cx-control-conn".to_string())
                                .spawn(move || {
                                    if let Err(err) =
                                        handle_connection(stream, &token, &dispatcher, &subscribe)
                                    {
                                        log::debug!("控制连接结束：{err}");
                                    }
                                });
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(err) => {
                            log::warn!("控制套接字接受连接失败：{err}");
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
                log::debug!("控制服务端已停止");
            })
            .expect("无法启动控制服务端线程")
    }
}

/// 处理一条客户端连接。
fn handle_connection(
    stream: TcpStream,
    token: &str,
    dispatcher: &Dispatcher,
    subscribe: &SubscribeFactory,
) -> Result<(), DaemonError> {
    // Windows 上 accept 出来的套接字会**继承监听套接字的非阻塞模式**（Unix 不会），
    // 因此这里必须显式改回阻塞：否则读不到数据时会立刻 WouldBlock，
    // 表现为"握手成功但紧接着连接被关闭"。
    stream.set_nonblocking(false).map_err(DaemonError::io)?;
    stream
        .set_read_timeout(Some(CONNECTION_TIMEOUT))
        .map_err(DaemonError::io)?;
    let writer = stream.try_clone().map_err(DaemonError::io)?;
    let mut writer = writer;
    let mut reader = BufReader::new(stream);

    // 握手
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(DaemonError::io)? == 0 {
        return Ok(());
    }
    let auth: Auth = match serde_json::from_str(line.trim_end()) {
        Ok(auth) => auth,
        Err(err) => {
            write_reply(
                &mut writer,
                &Reply::err(0, errcode::UNAUTHORIZED, format!("握手格式非法：{err}")),
            )?;
            return Ok(());
        }
    };

    if auth.proto != CONTROL_PROTOCOL_VERSION {
        write_reply(
            &mut writer,
            &Reply::err(
                0,
                errcode::PROTOCOL,
                format!(
                    "协议版本不匹配：客户端 {}，服务端 {}",
                    auth.proto, CONTROL_PROTOCOL_VERSION
                ),
            ),
        )?;
        return Ok(());
    }

    // 常量时间比较，避免通过响应时间猜测令牌
    if !cx_crypto::ct_eq(auth.auth.as_bytes(), token.as_bytes()) {
        log::warn!("控制套接字认证失败");
        write_reply(
            &mut writer,
            &Reply::err(0, errcode::UNAUTHORIZED, "令牌无效"),
        )?;
        return Ok(());
    }
    write_reply(
        &mut writer,
        &Reply::ok(
            0,
            serde_json::json!({ "protocol": CONTROL_PROTOCOL_VERSION }),
        ),
    )?;

    // 请求循环
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(err) => return Err(DaemonError::io(err)),
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(err) => {
                write_reply(
                    &mut writer,
                    &Reply::err(0, errcode::BAD_ARGS, format!("请求解析失败：{err}")),
                )?;
                continue;
            }
        };

        if request.cmd == cx_proto::control::cmd::SUBSCRIBE {
            let events = subscribe();
            write_reply(
                &mut writer,
                &Reply::ok(request.id, serde_json::json!({ "subscribed": true })),
            )?;
            // 连接在此之后只推送事件
            while let Ok(event) = events.recv() {
                let line = serde_json::to_string(&event)
                    .map_err(|e| DaemonError::protocol(e.to_string()))?;
                if write_line(&mut writer, &line).is_err() {
                    break;
                }
            }
            return Ok(());
        }

        let reply = dispatcher(request);
        write_reply(&mut writer, &reply)?;
    }
}

fn write_reply(writer: &mut TcpStream, reply: &Reply) -> Result<(), DaemonError> {
    let line = serde_json::to_string(reply).map_err(|e| DaemonError::protocol(e.to_string()))?;
    write_line(writer, &line)
}

fn write_line(writer: &mut TcpStream, line: &str) -> Result<(), DaemonError> {
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(DaemonError::io)
}

/// 控制客户端（CLI 与 TUI 用）。
#[derive(Debug)]
pub struct ControlClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    connected_pid: u32,
}

impl ControlClient {
    /// 连接到运行中的守护进程。
    pub fn connect(paths: &Paths) -> Result<Self, DaemonError> {
        let info = DaemonInfo::load(paths).ok_or_else(|| {
            DaemonError::NotRunning("没有 daemon.json（cx start 启动）".to_string())
        })?;
        let token = read_control_token(paths)?;

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|err| {
            DaemonError::NotRunning(format!(
                "端口 {} 不可达（{err}）；若确认已停止，可删除 {}",
                info.port,
                paths.daemon_info_file().display()
            ))
        })?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .map_err(DaemonError::io)?;

        let writer = stream.try_clone().map_err(DaemonError::io)?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            connected_pid: info.pid,
        };

        let auth = Auth {
            auth: token,
            proto: CONTROL_PROTOCOL_VERSION,
        };
        let line =
            serde_json::to_string(&auth).map_err(|e| DaemonError::protocol(e.to_string()))?;
        write_line(&mut client.writer, &line)?;

        let handshake = client.read_reply()?;
        if !handshake.ok {
            let body = handshake.error.unwrap_or(cx_proto::control::ErrorBody {
                code: errcode::UNAUTHORIZED.to_string(),
                msg: "握手失败".to_string(),
            });
            return Err(DaemonError::Remote {
                code: body.code,
                message: body.msg,
            });
        }
        Ok(client)
    }

    /// 守护进程 pid（仅信息展示）。
    pub fn daemon_pid(&self) -> u32 {
        self.connected_pid
    }

    /// 发送请求并等待响应数据。
    pub fn request(
        &mut self,
        cmd: &str,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, DaemonError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            id,
            cmd: cmd.to_string(),
            args,
        };
        let line =
            serde_json::to_string(&request).map_err(|e| DaemonError::protocol(e.to_string()))?;
        write_line(&mut self.writer, &line)?;

        let reply = self.read_reply()?;
        if reply.id != id {
            return Err(DaemonError::protocol(format!(
                "响应序号不匹配：期望 {id}，收到 {}",
                reply.id
            )));
        }
        if reply.ok {
            Ok(reply.data.unwrap_or(serde_json::Value::Null))
        } else {
            let body = reply.error.unwrap_or(cx_proto::control::ErrorBody {
                code: errcode::INTERNAL.to_string(),
                msg: "未知错误".to_string(),
            });
            Err(DaemonError::Remote {
                code: body.code,
                message: body.msg,
            })
        }
    }

    /// 无参数请求。
    pub fn simple(&mut self, cmd: &str) -> Result<serde_json::Value, DaemonError> {
        self.request(cmd, serde_json::Map::new())
    }

    /// 订阅事件流（之后连接只推送事件）。
    pub fn subscribe(&mut self) -> Result<(), DaemonError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::new(id, cx_proto::control::cmd::SUBSCRIBE);
        let line =
            serde_json::to_string(&request).map_err(|e| DaemonError::protocol(e.to_string()))?;
        write_line(&mut self.writer, &line)?;
        let reply = self.read_reply()?;
        if reply.ok {
            Ok(())
        } else {
            Err(DaemonError::protocol("订阅失败"))
        }
    }

    /// 读取下一条事件（订阅后使用）。
    pub fn next_event(&mut self) -> Result<Event, DaemonError> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).map_err(DaemonError::io)?;
        if read == 0 {
            return Err(DaemonError::NotRunning("连接已关闭".to_string()));
        }
        serde_json::from_str(line.trim_end()).map_err(|e| DaemonError::protocol(e.to_string()))
    }

    fn read_reply(&mut self) -> Result<Reply, DaemonError> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).map_err(DaemonError::io)?;
        if read == 0 {
            return Err(DaemonError::NotRunning("连接被服务端关闭".to_string()));
        }
        serde_json::from_str(line.trim_end()).map_err(|e| DaemonError::protocol(e.to_string()))
    }
}

/// 便捷：发出一个请求并丢弃响应（用于 `stop` 等命令）。
pub fn send_one_way(paths: &Paths, cmd: &str) -> Result<(), DaemonError> {
    let mut client = ControlClient::connect(paths)?;
    client.simple(cmd)?;
    Ok(())
}

/// 事件广播器：把事件推给所有订阅者，失败的自动摘除。
#[derive(Clone, Default)]
pub struct Broadcaster {
    inner: Arc<std::sync::Mutex<Vec<Sender<Event>>>>,
}

impl Broadcaster {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个订阅者。
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = unbounded();
        if let Ok(mut list) = self.inner.lock() {
            list.push(tx);
        }
        rx
    }

    /// 广播一个事件。
    pub fn send(&self, event: Event) {
        let Ok(mut list) = self.inner.lock() else {
            return;
        };
        list.retain(|sender| sender.send(event.clone()).is_ok());
    }

    /// 当前订阅者数量。
    pub fn len(&self) -> usize {
        self.inner.lock().map(|list| list.len()).unwrap_or(0)
    }

    /// 是否没有订阅者。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn temp_paths(tag: &str) -> Paths {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cx-control-{tag}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    /// 起一个假服务端：对任何请求都回 `{"echo": cmd}`。返回（关闭标志, 端口）。
    fn start_server(paths: &Paths) -> (Arc<AtomicBool>, u16) {
        let token = ensure_control_token(paths).unwrap();
        let server = ControlServer::bind(token).unwrap();
        let port = server.port().unwrap();
        let shutdown = server.shutdown_flag();
        let dispatcher: Dispatcher = Arc::new(|request: Request| {
            Reply::ok(request.id, serde_json::json!({ "echo": request.cmd }))
        });
        let subscribe: SubscribeFactory = Arc::new(|| {
            let (_tx, rx) = unbounded();
            rx
        });
        server.serve(dispatcher, subscribe);

        let info = DaemonInfo {
            v: DAEMON_INFO_VERSION,
            pid: std::process::id(),
            port,
            started_ms: cx_core::now_ms(),
            version: "test".to_string(),
            control_protocol: CONTROL_PROTOCOL_VERSION,
        };
        info.save(paths).unwrap();
        (shutdown, port)
    }

    #[test]
    fn 客户端能连接并收发请求() {
        let paths = temp_paths("client");
        let (shutdown, port) = start_server(&paths);

        let mut client = ControlClient::connect(&paths).expect("应能连上假服务端");
        assert_eq!(client.daemon_pid(), std::process::id());

        let data = client.simple(cx_proto::control::cmd::STATUS).unwrap();
        assert_eq!(data["echo"], "status");

        // 事件序号递增、响应能对上号
        let data = client
            .request(cx_proto::control::cmd::HISTORY_LIST, serde_json::Map::new())
            .unwrap();
        assert_eq!(data["echo"], "history.list");

        shutdown.store(true, Ordering::Relaxed);
        assert_eq!(port, DaemonInfo::load(&paths).unwrap().port);
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 令牌错误被拒绝() {
        let paths = temp_paths("badtoken");
        let (shutdown, _port) = start_server(&paths);
        // 篡改令牌文件
        std::fs::write(paths.control_token_file(), "deadbeef".repeat(8)).unwrap();
        let err = ControlClient::connect(&paths).unwrap_err();
        assert!(matches!(err, DaemonError::Remote { .. }), "应被拒：{err}");
        shutdown.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 令牌文件生成并保持稳定() {
        let paths = temp_paths("token");
        let first = ensure_control_token(&paths).unwrap();
        let second = ensure_control_token(&paths).unwrap();
        assert_eq!(first, second, "已存在时不应重新生成");
        assert_eq!(first.len(), 64, "32 字节随机 ⇒ 64 个十六进制字符");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn daemon_info_落盘与读取() {
        let paths = temp_paths("info");
        let info = DaemonInfo {
            v: DAEMON_INFO_VERSION,
            pid: 4321,
            port: 55555,
            started_ms: 1_760_000_000_000,
            version: "0.1.0".to_string(),
            control_protocol: CONTROL_PROTOCOL_VERSION,
        };
        info.save(&paths).unwrap();
        assert_eq!(DaemonInfo::load(&paths).unwrap(), info);
        DaemonInfo::remove(&paths);
        assert!(DaemonInfo::load(&paths).is_none());
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 没有_daemon_json_时报未运行() {
        let paths = temp_paths("norun");
        let err = ControlClient::connect(&paths).unwrap_err();
        assert!(matches!(err, DaemonError::NotRunning(_)));
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 广播器推送并清理失效订阅者() {
        let broadcaster = Broadcaster::new();
        assert!(broadcaster.is_empty());

        let rx = broadcaster.subscribe();
        assert_eq!(broadcaster.len(), 1);
        broadcaster.send(Event::new("test.event", serde_json::json!({"n": 1})));
        let event = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(event.event, "test.event");

        drop(rx);
        broadcaster.send(Event::new("test.event", serde_json::json!({"n": 2})));
        assert!(broadcaster.is_empty(), "失效订阅者应被摘除");
    }
}
