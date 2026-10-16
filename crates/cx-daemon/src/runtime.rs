//! 守护进程运行时：装配线程、执行引擎动作、维持控制套接字。
//!
//! 线程模型（都不使用 async 运行时）：
//!
//! | 线程 | 职责 |
//! |---|---|
//! | 主线程 | 等退出信号，然后按顺序收尾（停服务端 → 停引擎并落盘 → 清理 daemon.json） |
//! | `watcher` | 等平台事件（或轮询兜底）→ 读剪贴板 → 送引擎 |
//! | `engine` | 唯一持有引擎状态；执行动作（写剪贴板、落盘、回复、广播事件） |
//! | `cx-backend-*` | 每个启用的后端一个 worker：上传对象、按 manifest 增量拉取 |
//! | `control` | 接受回环连接；每条连接一个短生命周期线程 |
//!
//! 空闲时所有线程都阻塞在 channel/socket 上：CPU 近 0，不需要忙等待。

use crate::control::{
    Broadcaster, ControlServer, DAEMON_INFO_VERSION, DaemonInfo, Dispatcher, SubscribeFactory,
    ensure_control_token,
};
use crate::error::DaemonError;
use crate::lock::InstanceLock;
use crate::logging;
use crate::service;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use cx_backend::build_backend;
use cx_clip::{ChangeNotifier, ClipBackend, ClipContent, SystemClipboard};
use cx_core::{Config, Identity, LocalSpace, Paths};
use cx_lan::{LanConfig, LanEvent, LanIdentity, LanService, PeerBook};
use cx_proto::ManifestEntry;
use cx_proto::control::{
    DoctorCheck, Event as ControlEvent, Reply, Request, cmd as commands, errcode, event as events,
};
use cx_sync::{
    Action, Command as EngineCommand, Engine, Event as EngineEvent, RemoteSync, ReplyPayload,
    RetentionLimits,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 局域网补齐清单的条数上限（太多会把 Manifest 撑大）。
const LAN_OFFER_LIMIT: usize = 500;

/// 等待引擎响应的上限。
const ENGINE_REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// 运行参数。
#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    /// 纯中继模式：不读写本机剪贴板（用于无图形会话的服务器）。
    pub no_clipboard: bool,
    /// 日志同时输出到 stderr（前台调试用）。
    pub stderr_logs: bool,
    /// 覆盖轮询间隔（毫秒）。
    pub poll_interval_ms: Option<u64>,
}

/// 引擎消息。
enum EngineMsg {
    /// 事件（fire-and-forget）。
    Event(EngineEvent),
    /// 控制命令，需要把响应送回发起者。
    Request {
        /// 命令。
        command: EngineCommand,
        /// 响应通道。
        reply: Sender<ReplyPayload>,
    },
    /// 优雅退出。
    Shutdown(Sender<()>),
}

/// 守护进程对外的静态信息（供补齐 status/doctor 用）。
#[derive(Debug)]
struct SharedMeta {
    pid: u32,
    started_ms: u64,
    version: String,
    clipboard: String,
    watch: String,
    no_clipboard: bool,
    max_item_bytes: u64,
    /// 局域网直连的实时状态（`cx doctor` 里展示）。
    lan: Option<Arc<Mutex<LanStatus>>>,
}

/// 局域网直连的可观测状态。
#[derive(Debug, Default)]
struct LanStatus {
    /// 监听端口。
    port: u16,
    /// 当前对端（设备 id, 设备名）。
    peers: Vec<(String, String)>,
}

/// 守护进程实例。
pub struct Daemon {
    paths: Paths,
    config: Config,
    identity: Identity,
    space: Option<LocalSpace>,
    options: DaemonOptions,
}

impl Daemon {
    /// 构造（不启动）。
    pub fn new(
        paths: Paths,
        config: Config,
        identity: Identity,
        space: Option<LocalSpace>,
        options: DaemonOptions,
    ) -> Self {
        Self {
            paths,
            config,
            identity,
            space,
            options,
        }
    }

    /// 启动并阻塞直到退出。返回进程退出码。
    pub fn run(self) -> Result<i32, DaemonError> {
        let paths = self.paths.clone();
        paths.ensure()?;

        // 单实例：抢不到锁直接退出（两个进程同时写历史/状态会互相破坏）
        let _lock = InstanceLock::acquire(&paths)?;

        logging::init(&paths, None, self.options.stderr_logs)?;
        log::info!(
            "ClipboardX {} 启动（pid {}，{}）",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            if self.options.no_clipboard {
                "纯中继模式"
            } else {
                "剪贴板模式"
            }
        );

        let started_ms = cx_core::now_ms();

        // ---- 剪贴板后端 ----
        // 读侧给 watcher、写侧给引擎线程：两个独立的句柄，互不阻塞。
        let (mut read_clip, mut write_clip, notifier, clipboard_desc, watch_desc): (
            Option<SystemClipboard>,
            Option<SystemClipboard>,
            Option<ChangeNotifier>,
            String,
            String,
        ) = if self.options.no_clipboard {
            (
                None,
                None,
                None,
                "已禁用（--no-clipboard）".to_string(),
                "已禁用".to_string(),
            )
        } else {
            match (SystemClipboard::new(), SystemClipboard::new()) {
                (Ok(read), Ok(write)) => {
                    let caps = read.capabilities();
                    let notifier = match ChangeNotifier::spawn() {
                        Ok(notifier) => Some(notifier),
                        Err(err) => {
                            log::warn!("平台变更通知不可用（{err}），改用轮询");
                            None
                        }
                    };
                    let interval = self
                        .options
                        .poll_interval_ms
                        .unwrap_or(self.config.sync.watch_interval_ms);
                    let watch = if notifier.is_some() {
                        format!("平台事件 + {interval} ms 兜底轮询")
                    } else {
                        format!("纯轮询（{interval} ms）")
                    };
                    (Some(read), Some(write), notifier, caps.note, watch)
                }
                (Err(err), _) | (_, Err(err)) => {
                    return Err(DaemonError::Core(cx_core::CoreError::Clip(err.to_string())));
                }
            }
        };

        if write_clip.is_none() {
            // 纯中继模式下没有写侧，引擎的应用动作会被跳过
            write_clip = None;
        }

        // ---- 引擎 ----
        let engine = Engine::new(
            &paths,
            self.config.clone(),
            self.identity.clone(),
            self.space.clone(),
        )?;
        let (engine_tx, engine_rx) = unbounded::<EngineMsg>();
        let broadcaster = Broadcaster::new();
        // 局域网状态：由 start_lan 的线程实时更新，doctor 里展示
        let lan_status: Option<Arc<Mutex<LanStatus>>> = self
            .config
            .lan
            .enabled
            .then(|| Arc::new(Mutex::new(LanStatus::default())));
        let meta = Arc::new(SharedMeta {
            pid: std::process::id(),
            started_ms,
            version: env!("CARGO_PKG_VERSION").to_string(),
            clipboard: clipboard_desc,
            watch: watch_desc,
            no_clipboard: self.options.no_clipboard,
            max_item_bytes: self.config.sync.max_item_bytes,
            lan: lan_status.clone(),
        });

        // ---- 控制服务端 ----
        let token = ensure_control_token(&paths)?;
        let server = ControlServer::bind(token)?;
        let port = server.port()?;
        let server_shutdown = server.shutdown_flag();

        let info = DaemonInfo {
            v: DAEMON_INFO_VERSION,
            pid: std::process::id(),
            port,
            started_ms,
            version: env!("CARGO_PKG_VERSION").to_string(),
            control_protocol: cx_proto::control::CONTROL_PROTOCOL_VERSION,
        };
        info.save(&paths)?;
        log::info!("控制套接字已就绪：127.0.0.1:{port}");

        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

        // 分发器：请求 → 引擎命令 → 响应
        let dispatch_tx = engine_tx.clone();
        let dispatch_meta = meta.clone();
        let dispatch_paths = paths.clone();
        let dispatch_shutdown = server_shutdown.clone();
        let dispatch_signal = shutdown_tx.clone();
        let dispatcher: Dispatcher = Arc::new(move |request: Request| {
            let id = request.id;

            // `shutdown` 由运行时直接处理，不进引擎
            if request.cmd == commands::SHUTDOWN {
                dispatch_shutdown.store(true, Ordering::Relaxed);
                let _ = dispatch_signal.try_send(());
                return Reply::done(id);
            }

            let command = match to_command(&request, &dispatch_meta, &dispatch_paths) {
                Ok(command) => command,
                Err(message) => return Reply::err(id, errcode::BAD_ARGS, message),
            };

            let (reply_tx, reply_rx) = bounded::<ReplyPayload>(1);
            if dispatch_tx
                .send(EngineMsg::Request {
                    command,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Reply::err(id, errcode::INTERNAL, "引擎已停止");
            }

            match reply_rx.recv_timeout(ENGINE_REPLY_TIMEOUT) {
                Ok(payload) => payload_to_reply(payload, &dispatch_meta),
                Err(_) => Reply::err(id, errcode::INTERNAL, "引擎响应超时"),
            }
        });

        let subscriber = broadcaster.clone();
        let subscribe: SubscribeFactory = Arc::new(move || subscriber.subscribe());
        let server_thread = server.serve(dispatcher, subscribe);

        // ---- 后端 worker 线程 ----
        // 必须在引擎线程之前建好：发布队列的所有者随后要被移进引擎线程闭包。
        let worker_shutdown = Arc::new(AtomicBool::new(false));
        let network_blocked = Arc::new(AtomicBool::new(
            engine.is_paused() || !engine.remote_ready(),
        ));
        let mut publishers: Vec<Publisher> = Vec::new();
        let mut worker_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
        {
            let cursors = engine.cursors_snapshot();
            let retention = RetentionLimits {
                max_items: self.config.sync.history_max_items,
                max_age_ms: self
                    .config
                    .sync
                    .history_max_days
                    .saturating_mul(24 * 60 * 60 * 1000),
            };
            for backend in self
                .config
                .enabled_backends()
                .filter(|_| engine.remote_ready())
            {
                let built = build_backend(backend, &paths, Duration::from_secs(60));
                match built {
                    Ok(remote) => {
                        let mut sync =
                            RemoteSync::new(remote, &self.identity.device_id, &self.identity.name);
                        if let Some(existing) = cursors.get(&backend.name) {
                            sync.set_cursors(existing.clone());
                        }
                        let (tx, rx) = bounded::<()>(1);
                        let outbox =
                            crate::outbox::bound_directory(&paths, backend, self.space.as_ref())?;
                        publishers.push(Publisher {
                            wake: tx,
                            outbox: outbox.clone(),
                        });
                        let active_interval = Duration::from_millis(
                            backend
                                .poll_interval_ms
                                .unwrap_or(self.config.sync.poll_active_ms)
                                .max(500),
                        );
                        let idle_interval = Duration::from_millis(
                            backend
                                .poll_interval_ms
                                .unwrap_or(self.config.sync.poll_idle_ms)
                                .max(500),
                        );
                        let backend_name = backend.name.clone();
                        let engine_tx = engine_tx.clone();
                        let shutdown = worker_shutdown.clone();
                        let blocked = network_blocked.clone();
                        let handle = std::thread::Builder::new()
                            .name(format!("cx-backend-{}", backend.name))
                            .spawn(move || {
                                backend_worker_loop(
                                    sync,
                                    rx,
                                    engine_tx,
                                    WorkerSettings {
                                        active_interval,
                                        idle_interval,
                                        retention,
                                        name: backend_name,
                                        outbox,
                                        blocked,
                                        shutdown,
                                    },
                                )
                            })
                            .map_err(DaemonError::io)?;
                        worker_threads.push(handle);
                    }
                    Err(err) => {
                        log::error!("后端 {} 初始化失败：{err}", backend.name);
                        // 仍然登记一条失败状态，让 cx status / cx doctor 看得到
                        let _ = engine_tx.send(EngineMsg::Event(EngineEvent::Command(
                            EngineCommand::BackendReport {
                                name: backend.name.clone(),
                                cursors: Default::default(),
                                published: 0,
                                applied: 0,
                                error: Some(err.to_string()),
                            },
                        )));
                    }
                }
            }
            if self.config.backends.is_empty() {
                log::info!("未配置任何后端：当前只记录本机历史（cx backend add … 添加）");
            }
        }

        // ---- 局域网直连 ----
        // 放在引擎线程之前：要把"我有哪些内容"的清单交给它做补齐，
        // 而那份清单来自引擎的历史索引。
        let lan = start_lan(
            &self.config,
            &self.identity,
            &paths,
            &engine_tx,
            engine.recent_entries(LAN_OFFER_LIMIT),
            lan_status.clone(),
        )?;
        if let Some(service) = &lan {
            log::info!(
                "局域网直连已就绪（设备 id {}），当前对端 {} 个",
                service.device_id(),
                service.peer_count()
            );
        }

        // ---- 引擎线程 ----
        let engine_thread = {
            let engine_broadcaster = broadcaster.clone();
            let mut engine = engine;
            let mut clip_write = write_clip.take();
            let lan = lan.clone();
            std::thread::Builder::new()
                .name("cx-engine".to_string())
                .spawn(move || {
                    loop {
                        let Ok(message) = engine_rx.recv() else {
                            break;
                        };
                        match message {
                            EngineMsg::Event(event) => {
                                let actions = engine.handle(cx_core::now_ms(), event);
                                network_blocked.store(
                                    engine.is_paused() || !engine.remote_ready(),
                                    Ordering::Release,
                                );
                                let mut runner = ActionRunner {
                                    engine: &mut engine,
                                    clip: &mut clip_write,
                                    broadcaster: &engine_broadcaster,
                                    publishers: &publishers,
                                    lan: lan.as_ref(),
                                };
                                let _ = runner.run_all(actions);
                            }
                            EngineMsg::Request { command, reply } => {
                                let actions =
                                    engine.handle(cx_core::now_ms(), EngineEvent::Command(command));
                                network_blocked.store(
                                    engine.is_paused() || !engine.remote_ready(),
                                    Ordering::Release,
                                );
                                let mut runner = ActionRunner {
                                    engine: &mut engine,
                                    clip: &mut clip_write,
                                    broadcaster: &engine_broadcaster,
                                    publishers: &publishers,
                                    lan: lan.as_ref(),
                                };
                                let payloads = runner.run_all(actions);
                                for payload in payloads {
                                    let _ = reply.send(payload);
                                }
                            }
                            EngineMsg::Shutdown(ack) => {
                                if let Err(err) = engine.flush_state() {
                                    log::warn!("退出前状态落盘失败：{err}");
                                }
                                let _ = ack.send(());
                                break;
                            }
                        }
                    }
                    log::debug!("引擎线程已停止");
                })
                .map_err(DaemonError::io)?
        };

        // ---- 监听线程 ----
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        let watcher_thread = match (read_clip.take(), self.options.no_clipboard) {
            (Some(mut clip), false) => {
                let tx = engine_tx.clone();
                let flag = watcher_shutdown.clone();
                let interval = Duration::from_millis(
                    self.options
                        .poll_interval_ms
                        .unwrap_or(self.config.sync.watch_interval_ms),
                );
                let debounce = Duration::from_millis(self.config.sync.debounce_ms);
                Some(
                    std::thread::Builder::new()
                        .name("cx-watcher".to_string())
                        .spawn(move || {
                            // 预热：记录当前剪贴板内容但不发布
                            match clip.read() {
                                Ok(content) => {
                                    let _ = tx.send(EngineMsg::Event(EngineEvent::Prime(content)));
                                }
                                Err(err) => log::warn!("启动时读取剪贴板失败：{err}"),
                            }

                            loop {
                                if flag.load(Ordering::Relaxed) {
                                    break;
                                }
                                // 有平台事件就立刻醒，否则等一个轮询间隔（兜底）
                                let notified = match &notifier {
                                    Some(notifier) => notifier.wait(interval),
                                    None => {
                                        std::thread::sleep(interval);
                                        false
                                    }
                                };

                                let mut content = match clip.read() {
                                    Ok(content) => content,
                                    Err(err) => {
                                        log::debug!("读取剪贴板失败（{err}），等待下次机会");
                                        continue;
                                    }
                                };

                                // 复制动作往往连续触发事件：等去抖时间后再取一次最终内容
                                if notified && !debounce.is_zero() {
                                    std::thread::sleep(debounce);
                                    match clip.read() {
                                        Ok(again) => content = again,
                                        Err(err) => log::debug!("去抖后重读失败：{err}"),
                                    }
                                }

                                if tx
                                    .send(EngineMsg::Event(EngineEvent::ClipboardChanged(content)))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            log::debug!("监听线程已停止");
                        })
                        .map_err(DaemonError::io)?,
                )
            }
            _ => None,
        };

        // ---- 等退出信号 ----
        let _ = shutdown_rx.recv();
        log::info!("收到退出信号，开始收尾");
        server_shutdown.store(true, Ordering::Relaxed);
        watcher_shutdown.store(true, Ordering::Relaxed);
        worker_shutdown.store(true, Ordering::Relaxed);
        // 监听线程最多等一个轮询间隔就会看到标志并退出，随后由它自己 Drop 掉通知器

        let (ack_tx, ack_rx) = bounded::<()>(1);
        let _ = engine_tx.send(EngineMsg::Shutdown(ack_tx));
        if ack_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            log::warn!("引擎未在 5 秒内确认退出，继续收尾");
        }

        broadcaster.send(ControlEvent::new(events::SHUTTING_DOWN, json!({})));
        // 先让局域网收工：它有自己的线程，早点通知就不用在最后等
        if let Some(service) = &lan {
            service.shutdown();
        }
        let _ = engine_thread.join();
        if let Some(service) = lan {
            service.join();
        }
        // 引擎线程退出时会 Drop 掉发布队列的发送端，worker 随即收到 Disconnected 而退出
        for handle in worker_threads {
            let _ = handle.join();
        }
        if let Some(thread) = watcher_thread {
            let _ = thread.join();
        }
        let _ = server_thread.join();
        DaemonInfo::remove(&paths);
        log::info!("已停止");
        Ok(0)
    }
}

/// 发布载荷先写入 outbox；channel 只负责唤醒，不承载唯一副本。
struct Publisher {
    wake: Sender<()>,
    outbox: std::path::PathBuf,
}

/// 记录/移除对端（供 `cx doctor` 展示）。
fn remember_peer(
    status: &Option<Arc<Mutex<LanStatus>>>,
    device_id: &str,
    device_name: &str,
    added: bool,
) {
    let Some(status) = status else {
        return;
    };
    let mut guard = status.lock().expect("局域网状态锁中毒");
    if added {
        if !guard.peers.iter().any(|(id, _)| id == device_id) {
            guard
                .peers
                .push((device_id.to_string(), device_name.to_string()));
        }
    } else {
        guard.peers.retain(|(id, _)| id != device_id);
    }
}

/// 把局域网状态汇报给引擎，让 `cx status` 里能看到它（与后端用同一条通道）。
fn report_lan(engine_tx: &Sender<EngineMsg>, service: &LanService) {
    let _ = engine_tx.send(EngineMsg::Event(EngineEvent::Command(
        EngineCommand::BackendReport {
            name: "lan".to_string(),
            cursors: Default::default(),
            published: service.received(),
            applied: service.received(),
            error: None,
        },
    )));
}

/// 启动局域网直连（未启用或缺少 PSK 来源时返回 `None`）。
///
/// PSK 的来源有两种：有空间密钥时由空间密钥派生（只有同一空间的设备能连上）；
/// 未加密空间则要求用户配一个配对码。两者都没有就不启动——宁可不同步，
/// 也不要在一个"谁都能连"的网段上裸奔。
fn start_lan(
    config: &Config,
    identity: &Identity,
    paths: &Paths,
    engine_tx: &Sender<EngineMsg>,
    offer: Vec<ManifestEntry>,
    status: Option<Arc<Mutex<LanStatus>>>,
) -> Result<Option<Arc<LanService>>, DaemonError> {
    if !config.lan.enabled {
        return Ok(None);
    }

    let space = cx_core::LocalSpace::load(paths).ok();
    if config.space.encrypt && space.as_ref().and_then(|s| s.cipher.space_key()).is_none() {
        log::warn!(
            "要求加密（[space] encrypt = true）但空间密钥未就绪：禁用局域网同步，避免明文外发。\
             请先 `cx space init/join` 取得密钥；确实要用明文空间时，按文档显式设 [space] encrypt = false \
             并在 [lan] 配 pairing_code"
        );
        return Ok(None);
    }
    let (psk_hex, space_id) = match (&space, config.lan.pairing_code.as_deref()) {
        (Some(space), _) if space.cipher.space_key().is_some() => (
            cx_lan::derive_psk(
                &space
                    .cipher
                    .space_key()
                    .map(|key| key.to_hex())
                    .unwrap_or_default(),
                None,
            ),
            Some(space.meta.space_id.clone()),
        ),
        (_, Some(code)) if !code.trim().is_empty() => (cx_lan::derive_psk("", Some(code)), None),
        _ => {
            log::warn!(
                "局域网已启用，但既没有空间密钥也没有配对码：跳过。\
                 （请先 `cx space init/join`，或在 [lan] 配置 pairing_code）"
            );
            return Ok(None);
        }
    };

    let peers: Vec<std::net::SocketAddr> = config
        .lan
        .peers
        .iter()
        .filter_map(|text| match text.parse() {
            Ok(addr) => Some(addr),
            Err(err) => {
                log::warn!("局域网对端地址 {text} 无法解析：{err}");
                None
            }
        })
        .collect();

    let lan_config = LanConfig {
        enabled: true,
        port: config.lan.port,
        space_id,
        psk_hex,
        peers,
        use_mdns: config.lan.use_mdns,
        device_name: identity.name.clone(),
        device_id: identity.device_id.clone(),
    };
    let lan_identity = LanIdentity::load_or_create(&paths.config_dir, &identity.device_id)
        .map_err(|err| {
            DaemonError::Core(cx_core::CoreError::malformed("lan.key", err.to_string()))
        })?;
    let book = PeerBook::open(paths.data_dir.join("lan-peers.json")).map_err(|err| {
        DaemonError::Core(cx_core::CoreError::malformed(
            "lan-peers.json",
            err.to_string(),
        ))
    })?;

    let (service, events) = LanService::start(lan_config, lan_identity, book)
        .map_err(|err| DaemonError::Core(cx_core::CoreError::malformed("lan", err.to_string())))?;

    // 告诉服务"我有哪些内容"，对端连上来时会用它做补齐
    service.set_offer(
        offer
            .into_iter()
            .map(|entry| cx_lan::proto::OfferEntry {
                name: entry.name,
                created_ms: entry.created_ms,
                size: entry.size,
                kind: entry.kind.as_str().to_string(),
            })
            .collect(),
    );

    let service = Arc::new(service);
    let engine_tx = engine_tx.clone();
    let service_for_thread = service.clone();
    if let Some(status) = &status {
        status.lock().expect("局域网状态锁中毒").port = config.lan.port;
    }
    std::thread::Builder::new()
        .name("cx-lan-events".to_string())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    LanEvent::Item { from, name, bytes } => match cx_proto::Item::decode(&bytes) {
                        Ok(item) => {
                            let _ = engine_tx.send(EngineMsg::Event(EngineEvent::Command(
                                EngineCommand::ApplyRemote {
                                    backend: "lan".to_string(),
                                    name,
                                    item: Box::new(item),
                                },
                            )));
                            log::info!("局域网收到来自 {from} 的内容");
                            report_lan(&engine_tx, &service_for_thread);
                        }
                        Err(err) => log::warn!("局域网内容解码失败（来自 {from}）：{err}"),
                    },
                    LanEvent::PeerUp {
                        device_id,
                        device_name,
                        addr,
                    } => {
                        log::info!("局域网对端已连接：{device_name}（{device_id} @ {addr}）");
                        remember_peer(&status, &device_id, &device_name, true);
                        report_lan(&engine_tx, &service_for_thread);
                    }
                    LanEvent::PeerDown { device_id } => {
                        log::info!("局域网对端已断开：{device_id}");
                        remember_peer(&status, &device_id, "", false);
                        report_lan(&engine_tx, &service_for_thread);
                    }
                    LanEvent::Log { level, message } => {
                        logging::log_line(level, &format!("[lan] {message}"));
                    }
                }
            }
            log::debug!("局域网事件线程已停止");
        })
        .map_err(DaemonError::io)?;

    Ok(Some(service))
}
/// 后端 worker 线程的主循环。
///
/// 一个后端一个线程，只做两件事：把引擎产出的对象写上去、按 manifest 增量把别人的
/// 对象取回来。取回的对象**不在这里解密/应用**，一律交给引擎，由它统一做去重、
/// 解密与策略判断——否则"两处都以为对方查过"的漏洞迟早会出现。
struct WorkerSettings {
    active_interval: Duration,
    idle_interval: Duration,
    retention: RetentionLimits,
    name: String,
    outbox: std::path::PathBuf,
    blocked: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

fn backend_worker_loop(
    mut sync: RemoteSync,
    jobs: Receiver<()>,
    engine_tx: Sender<EngineMsg>,
    settings: WorkerSettings,
) {
    let WorkerSettings {
        active_interval,
        idle_interval,
        retention,
        name,
        outbox,
        blocked,
        shutdown,
    } = settings;
    log::info!("后端 {name} 就绪：{}", sync.describe());
    let mut failures = 0u32;
    let mut delay = Duration::ZERO;
    loop {
        let deadline = std::time::Instant::now() + delay;
        while std::time::Instant::now() < deadline {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(100));
            match jobs.recv_timeout(remaining) {
                Ok(()) if failures == 0 => break,
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if blocked.load(Ordering::Acquire) {
            delay = Duration::from_millis(100);
            continue;
        }
        let mut error = None;
        let mut active = false;
        match crate::outbox::pending(&outbox, 64) {
            Ok(pending) => {
                for (entry, bytes) in pending {
                    if blocked.load(Ordering::Acquire) || shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    active = true;
                    match sync.publish(&entry.name, &bytes, entry.clone(), retention) {
                        // 返回的是"保留策略清理失败"的条数：publish 内部已 warn 过，
                        // 这里照常出队——清理问题不该阻塞同步，也不该触发后端退避。
                        Ok(_) => {
                            if let Err(err) = crate::outbox::acknowledge(&outbox, &entry.name) {
                                error = Some(err.to_string());
                                break;
                            }
                        }
                        Err(err) => {
                            error = Some(err.to_string());
                            break;
                        }
                    }
                }
            }
            Err(err) => error = Some(err.to_string()),
        }
        let previous = sync.cursors().clone();

        match sync.poll(cx_sync::remote::DEFAULT_MAX_FETCH) {
            Ok(outcome) => {
                active |= outcome.manifest_changed;
                for (object_name, item) in outcome.incoming {
                    let (tx, rx) = bounded(1);
                    if engine_tx
                        .send(EngineMsg::Request {
                            command: EngineCommand::ApplyRemote {
                                backend: name.clone(),
                                name: object_name,
                                item: Box::new(item),
                            },
                            reply: tx,
                        })
                        .is_err()
                    {
                        return;
                    }
                    match rx.recv_timeout(ENGINE_REPLY_TIMEOUT) {
                        Ok(ReplyPayload::Done { .. }) => sync.note_applied(1),
                        _ => {
                            error = Some("远端对象尚未成功入库，保留游标以便重试".into());
                            break;
                        }
                    }
                }
                if error.is_some() {
                    sync.set_cursors(previous);
                } else if let Some(message) = outcome.errors.first() {
                    error = Some(message.clone());
                }
            }
            Err(err) => {
                log::warn!("后端 {name} 轮询失败：{err}");
                error = Some(err.to_string());
            }
        }

        // 每轮都汇报：游标必须及时落盘，否则重启会把历史重下一遍
        let _ = engine_tx.send(EngineMsg::Event(EngineEvent::Command(
            EngineCommand::BackendReport {
                name: name.clone(),
                cursors: sync.cursors().clone(),
                published: sync.published(),
                applied: sync.applied(),
                error: error.clone(),
            },
        )));
        failures = if error.is_some() {
            failures.saturating_add(1)
        } else {
            0
        };
        delay = if failures > 0 {
            Duration::from_millis(cx_core::state::backoff_ms(failures, 0))
        } else if active {
            active_interval
        } else {
            idle_interval
        };
    }
    log::debug!("后端 {name} worker 已停止");
}

/// 动作执行器：把引擎输出的动作落成实际副作用。
///
/// 单独抽成结构体（而不是内联闭包或宏）有两个原因：
/// 1. `RecheckClipboard` 需要回头**再驱动一次引擎**，宏展开会直接撞上递归限制；
/// 2. 显式传入 `depth` 上限，比"祈祷不会形成环"可靠（例如 pause/resume 与
///    push 交错时有可能连续重读）。
struct ActionRunner<'a> {
    engine: &'a mut Engine,
    clip: &'a mut Option<SystemClipboard>,
    broadcaster: &'a Broadcaster,
    /// 各后端 worker 的发布队列（顺序与配置里的启用后端一致）。
    publishers: &'a [Publisher],
    /// 局域网直连服务（启用时才有）。
    lan: Option<&'a Arc<LanService>>,
}

/// 重读剪贴板的最大嵌套深度。
const MAX_RECHECK_DEPTH: u32 = 3;

impl ActionRunner<'_> {
    /// 执行一批动作，返回其中需要回给客户端响应（`Reply`）。
    fn run_all(&mut self, actions: Vec<Action>) -> Vec<ReplyPayload> {
        let mut replies = Vec::new();
        for action in actions {
            if let Some(payload) = self.run(action, 0) {
                replies.push(payload);
            }
        }
        replies
    }

    fn run(&mut self, action: Action, depth: u32) -> Option<ReplyPayload> {
        match action {
            Action::CopyToClipboard { id, content } => {
                let result = self
                    .clip
                    .as_mut()
                    .ok_or_else(|| "纯中继模式没有本机剪贴板".to_string())
                    .and_then(|clip| {
                        clip.write(&content)
                            .map_err(|err| format!("写入剪贴板失败：{err}"))
                    });
                Some(match result {
                    Ok(()) => ReplyPayload::Done { id },
                    Err(msg) => ReplyPayload::Error {
                        id,
                        body: cx_proto::control::ErrorBody {
                            code: "clipboard_write_failed".into(),
                            msg,
                        },
                    },
                })
            }
            Action::ApplyToClipboard(content) => {
                match self.clip.as_mut() {
                    Some(clip) => match clip.write(&content) {
                        Ok(()) => log::debug!("已把内容写入剪贴板（{}）", content.describe()),
                        Err(err) => log::warn!("写入剪贴板失败：{err}"),
                    },
                    None => log::debug!("纯中继模式：跳过写入剪贴板（{}）", content.describe()),
                }
                None
            }
            Action::Publish { name, entry, bytes } => {
                if !self.engine.remote_ready() {
                    log::warn!("空间密钥未就绪，拒绝外发对象");
                    return None;
                }
                // 局域网直连：优先走它（同一网段里最快，且不经过服务器）。
                // 注意这里必须在"没有后端就返回"之前——局域网与后端是两条独立通路，
                // 只用局域网（不配后端）是完全正常的用法。
                if let Some(lan) = self.lan {
                    let delivered = lan.publish(&name, &bytes);
                    if delivered > 0 {
                        // 用 INFO：这条路径出问题时，用户第一时间就会看日志
                        log::info!("已通过局域网推给 {delivered} 个对端：{name}");
                    } else {
                        log::debug!("局域网当前没有对端，{name} 等对端连上后由清单补齐");
                    }
                    lan.add_offer(cx_lan::proto::OfferEntry {
                        name: name.clone(),
                        created_ms: entry.created_ms,
                        size: entry.size,
                        kind: entry.kind.as_str().to_string(),
                    });
                }

                if self.publishers.is_empty() {
                    log::debug!(
                        "已生成对象 {name}（{} 字节）：没有配置后端，仅记录本机历史",
                        bytes.len()
                    );
                    return None;
                }

                for publisher in self.publishers {
                    if let Err(err) =
                        crate::outbox::enqueue(&publisher.outbox, &name, &entry, &bytes)
                    {
                        log::error!("发布未排队，发件箱写入失败：{err}");
                        self.broadcaster.send(ControlEvent::new(
                            events::CLIP_SKIPPED,
                            json!({"reason":"outbox_write_failed","name":name}),
                        ));
                    } else {
                        let _ = publisher.wake.try_send(());
                    }
                }
                None
            }
            Action::Reply(payload) => Some(payload),
            Action::Notify(event) => {
                self.broadcaster.send(event);
                None
            }
            Action::PollNow => {
                for publisher in self.publishers {
                    let _ = publisher.wake.try_send(());
                }
                None
            }
            Action::RecheckClipboard => {
                self.recheck(depth);
                None
            }
            Action::SaveState => {
                if let Err(err) = self.engine.flush_state() {
                    log::warn!("状态落盘失败：{err}");
                }
                None
            }
            Action::Log { level, message } => {
                logging::log_line(level, &message);
                None
            }
        }
    }

    /// 重新读取剪贴板并送回引擎（`resume` / `push` 用）。
    fn recheck(&mut self, depth: u32) {
        if depth >= MAX_RECHECK_DEPTH {
            log::debug!("重读剪贴板达到深度上限，停止");
            return;
        }
        let Some(clip) = self.clip.as_mut() else {
            return;
        };
        match clip.read() {
            Ok(content) => {
                let actions = self
                    .engine
                    .handle(cx_core::now_ms(), EngineEvent::ClipboardChanged(content));
                for action in actions {
                    // 重读触发的动作不应再产生响应（没有客户端在等）
                    let _ = self.run(action, depth + 1);
                }
            }
            Err(err) => log::debug!("重读剪贴板失败：{err}"),
        }
    }
}

/// 把请求翻译成引擎命令。
fn to_command(
    request: &Request,
    meta: &SharedMeta,
    paths: &Paths,
) -> Result<EngineCommand, String> {
    let id = request.id;
    if let Some(expected_name) = request.arg_str("expected_name") {
        let operation = match request.cmd.as_str() {
            commands::HISTORY_SHOW_CHECKED => "show",
            commands::HISTORY_DELETE_CHECKED => "delete",
            commands::COPY_CHECKED => "copy",
            _ => return Err("expected_name 仅用于历史操作".into()),
        };
        return Ok(EngineCommand::HistoryChecked {
            id,
            position: request.arg_u64("position").unwrap_or(0) as usize,
            expected_name: expected_name.to_string(),
            operation: operation.into(),
            max_chars: request.arg_u64("max_chars").unwrap_or(2_000).min(1_000_000) as usize,
        });
    }
    match request.cmd.as_str() {
        commands::STATUS => Ok(EngineCommand::Status { id }),
        commands::PAUSE => Ok(EngineCommand::Pause { id }),
        commands::RESUME => Ok(EngineCommand::Resume { id }),
        commands::HISTORY_LIST => Ok(EngineCommand::HistoryList {
            id,
            limit: request.arg_u64("limit").unwrap_or(20).min(10_000) as usize,
        }),
        commands::HISTORY_SHOW => Ok(EngineCommand::HistoryShow {
            id,
            position: request.arg_u64("position").unwrap_or(0) as usize,
            max_chars: request.arg_u64("max_chars").unwrap_or(2_000).min(1_000_000) as usize,
        }),
        commands::HISTORY_DELETE => {
            let position = request
                .arg_u64("position")
                .ok_or_else(|| "history.delete 需要 position 参数".to_string())?;
            Ok(EngineCommand::HistoryDelete {
                id,
                position: position as usize,
            })
        }
        commands::COPY => {
            let position = request
                .arg_u64("position")
                .ok_or_else(|| "copy 需要 position 参数".to_string())?;
            Ok(EngineCommand::Copy {
                id,
                position: position as usize,
            })
        }
        commands::PUSH => {
            let content = match request.arg_str("text") {
                Some(text) => {
                    if text.len() as u64 > meta.max_item_bytes {
                        return Err(format!(
                            "内容 {} 字节超过上限 {} 字节",
                            text.len(),
                            meta.max_item_bytes
                        ));
                    }
                    Some(ClipContent::Text(text.to_string()))
                }
                None => None,
            };
            Ok(EngineCommand::Push {
                id,
                content,
                force: request.arg_bool("force").unwrap_or(false),
            })
        }
        commands::PULL => Ok(EngineCommand::Pull { id }),
        commands::DOCTOR => Ok(EngineCommand::Doctor { id }),
        commands::RELOAD => {
            let config = Config::load(paths).map_err(|err| err.to_string())?;
            Ok(EngineCommand::Reload {
                id,
                config: Box::new(config),
            })
        }
        other => Err(format!("未知命令：{other}")),
    }
}

/// 把类型化响应转成线协议响应，并补齐守护进程才知道的字段。
fn payload_to_reply(payload: ReplyPayload, meta: &SharedMeta) -> Reply {
    match payload {
        ReplyPayload::Status { id, mut report } => {
            report.pid = meta.pid;
            report.started_ms = meta.started_ms;
            report.version = meta.version.clone();
            report.clipboard = meta.clipboard.clone();
            report.watch = meta.watch.clone();
            report.no_clipboard = meta.no_clipboard;
            Reply::ok(id, to_value(&*report))
        }
        ReplyPayload::HistoryList { id, items } => Reply::ok(id, to_value(&items)),
        ReplyPayload::HistoryShow { id, preview } => Reply::ok(id, to_value(&*preview)),
        ReplyPayload::Doctor { id, mut report } => {
            report.checks.push(DoctorCheck {
                name: "剪贴板".to_string(),
                ok: !meta.no_clipboard,
                detail: meta.clipboard.clone(),
            });
            report.checks.push(DoctorCheck {
                name: "监听方式".to_string(),
                ok: !meta.no_clipboard,
                detail: meta.watch.clone(),
            });
            // 局域网不是"后端"（不走对象存储），但它是另一条同步通路，
            // 状态放在 doctor 里最自然：配了没有、连上几个。
            if let Some(lan) = &meta.lan {
                let status = lan.lock().expect("局域网状态锁中毒");
                let peers = if status.peers.is_empty() {
                    "当前没有对端".to_string()
                } else {
                    status
                        .peers
                        .iter()
                        .map(|(id, name)| format!("{name}（{}）", &id[..8.min(id.len())]))
                        .collect::<Vec<_>>()
                        .join("、")
                };
                report.checks.push(DoctorCheck {
                    name: "局域网".to_string(),
                    ok: !status.peers.is_empty(),
                    detail: format!("已启用（端口 {}），{}", status.port, peers),
                });
            }
            Reply::ok(id, to_value(&*report))
        }
        ReplyPayload::Done { id } => Reply::done(id),
        ReplyPayload::Error { id, body } => Reply {
            id,
            ok: false,
            data: None,
            error: Some(body),
        },
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// 后台挂载：把守护进程从当前终端分离出去。
///
/// **关键约束（Windows）**：必须用 `bInheritHandles = FALSE` 创建进程。
/// Rust 标准库的 `Command` 一律允许句柄继承，于是守护进程会拿到调用方
/// （shell 脚本、CI runner）的 stdout 管道/重定向文件句柄；只要守护进程活着，
/// 那个管道就不会关闭，脚本与 CI 步骤就会一直挂住。实测三种写法全部复现：
/// `cx start`、`cx start *> file`、甚至 `Start-Process -Wait`。
/// 因此 Windows 下直接用 `CreateProcessW` 且不继承任何句柄。
///
/// - Windows：`CreateProcessW(bInheritHandles = FALSE, CREATE_NO_WINDOW)`
/// - Unix：`setsid` 新建会话（fd 由调用方的 CLOEXEC 语义保证不被带过去），
///   没有 `setsid` 时退化为新建进程组。
pub fn spawn_background(paths: &Paths, no_clipboard: bool) -> Result<u32, DaemonError> {
    // 已经有实例在跑就直接返回它
    if let Some(info) = DaemonInfo::load(paths)
        && info.is_alive()
    {
        return Ok(info.pid);
    }

    let exe = std::env::current_exe().map_err(DaemonError::io)?;
    let mut args = vec!["daemon".to_string(), "--foreground".to_string()];
    if no_clipboard {
        args.push("--no-clipboard".to_string());
    }
    // 后台进程的 stderr 不可靠（可能被断开），日志统一写文件
    args.push("--quiet".to_string());

    #[cfg(windows)]
    {
        spawn_detached_windows(&exe, &args)?;
    }

    #[cfg(unix)]
    {
        spawn_detached_unix(&exe, &args)?;
    }

    wait_for_daemon(paths)
}

/// Windows：不继承任何句柄地创建分离进程。
#[cfg(windows)]
fn spawn_detached_windows(exe: &std::path::Path, args: &[String]) -> Result<(), DaemonError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    // 命令行需要自带 argv[0]，且路径含空格时必须加引号
    let mut command_line = format!("\"{}\"", exe.display());
    for arg in args {
        // 我们的参数都是固定的 ASCII 开关，不会含空格
        command_line.push(' ');
        command_line.push_str(arg);
    }
    let mut command_line: Vec<u16> = OsStr::new(&command_line)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: 传入的都是本函数内有效且可写的缓冲区；command_line 以 NUL 结尾；
    // 句柄不可继承（bInheritHandles = FALSE）正是本函数的目的。
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut info,
        )
    };

    if created == 0 {
        return Err(DaemonError::Detach(format!(
            "CreateProcessW 失败：{}",
            std::io::Error::last_os_error()
        )));
    }

    // SAFETY: CreateProcessW 成功时这两个句柄由调用方负责关闭
    unsafe {
        CloseHandle(info.hThread);
        CloseHandle(info.hProcess);
    }
    Ok(())
}

/// Unix：用 `setsid` 脱离会话；没有它时退化为新建进程组。
#[cfg(unix)]
fn spawn_detached_unix(exe: &std::path::Path, args: &[String]) -> Result<(), DaemonError> {
    use std::process::{Command, Stdio};

    // 优先 setsid（新会话，完全脱离控制终端）
    if std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("setsid").is_file()))
        .unwrap_or(false)
    {
        let mut command = Command::new("setsid");
        command.arg(exe);
        command.args(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().map_err(DaemonError::io)?;
        return Ok(());
    }

    let mut command = Command::new(exe);
    command.args(args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map_err(DaemonError::io)?;
    Ok(())
}

/// 等 daemon.json 出现（最多 10 秒）。
fn wait_for_daemon(paths: &Paths) -> Result<u32, DaemonError> {
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(info) = DaemonInfo::load(paths)
            && info.is_alive()
        {
            return Ok(info.pid);
        }
    }
    Err(DaemonError::Detach(
        "后台进程未在 10 秒内就绪，请查看日志".to_string(),
    ))
}

/// 便捷：让正在运行的守护进程退出并等待其消失。
pub fn stop_running(paths: &Paths) -> Result<bool, DaemonError> {
    let Some(info) = DaemonInfo::load(paths) else {
        return Ok(false);
    };
    if !info.is_alive() {
        DaemonInfo::remove(paths);
        return Ok(false);
    }

    // 用控制套接字发退出命令；失败则清掉 stale 元信息
    match crate::control::send_one_way(paths, commands::SHUTDOWN) {
        Ok(()) => {}
        Err(err) => {
            log::warn!("发送退出命令失败：{err}");
            DaemonInfo::remove(paths);
            return Ok(false);
        }
    }

    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if !DaemonInfo::load(paths)
            .map(|info| info.is_alive())
            .unwrap_or(false)
        {
            return Ok(true);
        }
    }
    Ok(true)
}

/// 自启注册（供 `cx service install` 使用）。
pub fn install_autostart(paths: &Paths) -> Result<String, DaemonError> {
    service::install(paths)
}

/// 取消自启。
pub fn uninstall_autostart(paths: &Paths) -> Result<String, DaemonError> {
    service::uninstall(paths)
}

/// 查询自启状态。
pub fn autostart_status() -> String {
    service::status()
}

/// 读取运行时状态里的 pid（若在运行）。
pub fn running_pid(paths: &Paths) -> Option<u32> {
    let info = DaemonInfo::load(paths)?;
    if info.is_alive() {
        Some(info.pid)
    } else {
        None
    }
}

/// 未使用但保留：把接收端暴露给 TUI 的将来版本（订阅由控制层代理）。
#[allow(dead_code)]
fn _unused(_rx: Receiver<ControlEvent>) {}

#[cfg(test)]
mod worker_regressions {
    use super::*;
    use cx_backend::{MemoryBackend, RemoteBackend};
    #[test]
    fn 暂停重启保留积压直到恢复且超过队列容量不丢() {
        let root = std::env::temp_dir().join(format!(
            "cx-worker-test-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let paths = Paths::from_root(&root);
        let outbox = crate::outbox::directory(&paths, "fake-bound-target");
        let store = MemoryBackend::new("fake");
        let ts = 1_759_299_998_712;
        let device = "a".repeat(32);
        for index in 0..70u64 {
            let name = cx_proto::ItemName::build(
                ts + index,
                &device,
                &format!("{index:08x}{}", "b".repeat(56)),
            );
            let entry = ManifestEntry {
                name: name.clone(),
                day: cx_proto::civil::day_from_ms(ts + index),
                size: 4,
                hash: "b".repeat(64),
                kind: cx_proto::Kind::Text,
                created_ms: ts + index,
                mime: None,
            };
            crate::outbox::enqueue(&outbox, &name, &entry, b"fake").unwrap();
        }
        let blocked = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (wake, rx) = bounded(1);
        let (tx, reports) = unbounded();
        let thread = {
            let blocked = blocked.clone();
            let shutdown = shutdown.clone();
            let outbox = outbox.clone();
            let sync = RemoteSync::new(Box::new(store.shared("fake")), &device, "fake-device");
            std::thread::spawn(move || {
                backend_worker_loop(
                    sync,
                    rx,
                    tx,
                    WorkerSettings {
                        active_interval: Duration::from_millis(1),
                        idle_interval: Duration::from_millis(2),
                        retention: RetentionLimits {
                            max_items: 100,
                            max_age_ms: 0,
                        },
                        name: "fake".into(),
                        outbox,
                        blocked,
                        shutdown,
                    },
                )
            })
        };
        assert!(reports.recv_timeout(Duration::from_millis(180)).is_err());
        assert_eq!(crate::outbox::pending(&outbox, 100).unwrap().len(), 70);
        blocked.store(false, Ordering::Release);
        wake.try_send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let _ = reports.recv_timeout(Duration::from_millis(100));
            if crate::outbox::pending(&outbox, 100).unwrap().is_empty() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "积压应全部完成");
        }
        shutdown.store(true, Ordering::Relaxed);
        let _ = wake.try_send(());
        thread.join().unwrap();
        let mut reader = store.shared("reader");
        let bytes = reader
            .get(&cx_backend::layout::manifest_path(&device))
            .unwrap()
            .unwrap();
        assert_eq!(
            cx_proto::Manifest::from_json(std::str::from_utf8(&bytes).unwrap())
                .unwrap()
                .items
                .len(),
            70
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
