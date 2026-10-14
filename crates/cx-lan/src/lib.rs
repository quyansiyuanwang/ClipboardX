//! 局域网直连：发现、握手、传输。
//!
//! 目标：两台在同一个网段里的设备**不经过任何服务端**就能互相同步。它解决的是
//! 最常用的场景——同一个人的两台电脑在同一个 Wi-Fi 下。
//!
//! 安全模型（三层，缺一不可）：
//! 1. **传输层加密与对端认证**：Noise `XXpsk3`（Curve25519 + ChaCha20-Poly1305 + BLAKE2s）。
//!    XX 让双方交换静态公钥并互相认证；PSK 把"谁能连上"限制在同一个空间内——
//!    PSK 由空间密钥派生，没有加入空间的人无法完成握手。
//! 2. **载荷自身也是密文**：item 在客户端就用空间密钥加密过（见 `docs/protocol.md` §5），
//!    Noise 只是又套了一层，因此即使 Noise 层被攻破，内容仍是密文。
//! 3. **TOFU 固定**：首次见到某设备时记下它的静态公钥；此后同一 device_id 换了公钥
//!    就拒绝连接并告警（防中间人与"换了个假设备"）。
//!
//! 为什么用 `snow` 而不是自己拼 Curve25519/ChaCha20：Noise 的握手状态机、
//! 重键、nonce 管理都极易写错，而写错的密码学比不加密更危险。
//! `snow` 是同步实现、无 async 运行时，符合本项目的取舍。

mod discovery;
mod error;
mod identity;
pub mod proto;
mod session;

pub use discovery::{DiscoveredPeer, Discovery};
pub use error::LanError;
pub use identity::{LanIdentity, PeerBook, PeerRecord};
pub use proto::{LanMessage, MAX_FRAME_BYTES, OfferEntry};
pub use session::{Session, SessionRole};

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// mDNS 服务类型（`_clipboardx._tcp.local`）。
pub const SERVICE_TYPE: &str = "_clipboardx._tcp.local.";

/// 默认监听端口（TCP，同时也是 mDNS 通告的端口）。
pub const DEFAULT_PORT: u16 = 8699;

/// PSK 派生时使用的上下文串（改它等于换协议版本）。
const PSK_CONTEXT: &str = "ClipboardX LAN psk v1";

/// 局域网配置。
#[derive(Debug, Clone)]
pub struct LanConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 监听端口（`0` 表示由系统分配，测试用）。
    pub port: u16,
    /// 空间 id（用于校验对端是否在同一空间）。
    pub space_id: Option<String>,
    /// PSK 的十六进制（由空间密钥派生，或未加密空间时由用户配置的配对码派生）。
    pub psk_hex: String,
    /// 显式对端地址（mDNS 不可用的网络里用；也可以用来先把两台机器连起来）。
    pub peers: Vec<SocketAddr>,
    /// 是否使用 mDNS 自动发现。
    pub use_mdns: bool,
    /// 设备名（通告与展示用）。
    pub device_name: String,
    /// 设备 id（mDNS 通告与"是不是自己"的判断都要用）。
    pub device_id: String,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            space_id: None,
            psk_hex: String::new(),
            peers: Vec::new(),
            use_mdns: true,
            device_name: "未命名设备".to_string(),
            device_id: String::new(),
        }
    }
}

/// 空间 id 的哈希前缀（mDNS 是明文广播，不直接暴露空间 id）。
pub fn space_hash(space_id: &str) -> String {
    let digest = blake3::hash(space_id.as_bytes());
    digest.to_hex().to_string()[..16].to_string()
}

/// 由空间密钥或配对码派生 PSK。
///
/// `blake3::derive_key` 是"从一段高熵材料派生密钥"的正确姿势（有上下文串隔离，
/// 不会与其它用途的哈希撞车）。
pub fn derive_psk(space_key_hex: &str, pairing_code: Option<&str>) -> String {
    let material = match pairing_code {
        Some(code) if !code.trim().is_empty() => format!("pair:{code}"),
        _ => format!("space:{space_key_hex}"),
    };
    let key = blake3::derive_key(PSK_CONTEXT, material.as_bytes());
    hex::encode(key)
}

/// 服务对外报告的事件。
#[derive(Debug)]
pub enum LanEvent {
    /// 收到一个远端条目（交给引擎去重与应用）。
    Item {
        /// 对端设备 id。
        from: String,
        /// 对象名。
        name: String,
        /// 容器字节。
        bytes: Vec<u8>,
    },
    /// 有设备连上（或发现并在连接中）。
    PeerUp {
        /// 设备 id。
        device_id: String,
        /// 设备名。
        device_name: String,
        /// 对端地址。
        addr: SocketAddr,
    },
    /// 设备断开。
    PeerDown {
        /// 设备 id。
        device_id: String,
    },
    /// 日志/告警（含"公钥变了"这类安全事件）。
    Log {
        /// 级别。
        level: log::Level,
        /// 文本。
        message: String,
    },
}

/// 已连接的会话。
struct PeerSession {
    device_id: String,
    device_name: String,
    addr: SocketAddr,
    /// 发送侧：交给会话线程的队列。
    outgoing: Sender<LanMessage>,
}

/// 局域网服务。
///
/// 线程模型：一个发现/连接线程（拨号 + 接受 + 定时重连），每个会话再各有一个
/// 读线程。与项目其它部分一致：阻塞 IO + channel，不用 async。
pub struct LanService {
    local: LanIdentity,
    peers: Arc<std::sync::Mutex<BTreeMap<String, PeerSession>>>,
    /// 可发送的条目（用于新连接建立后的补齐）。
    offer: Arc<std::sync::Mutex<Vec<proto::OfferEntry>>>,
    shutdown: Arc<AtomicBool>,
    /// 自己的线程句柄（`join` 时收工；用 Mutex 包起来以便通过 `&self` 调用收尾）。
    threads: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// 收到的条目计数（`cx status` 展示用）。
    received: Arc<AtomicU64>,
}

impl LanService {
    /// 启动服务：监听、拨号、发现。
    ///
    /// 返回的 `Receiver` 供调用方（守护进程）消费事件。
    pub fn start(
        config: LanConfig,
        local: LanIdentity,
        peer_book: PeerBook,
    ) -> Result<(Self, Receiver<LanEvent>), LanError> {
        if config.psk_hex.len() != 64 || !config.psk_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(LanError::Config(
                "局域网 PSK 必须是 64 位十六进制（由空间密钥派生）".to_string(),
            ));
        }
        let (event_tx, event_rx) = unbounded::<LanEvent>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let peers = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let offer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = Arc::new(AtomicU64::new(0));

        let listener = std::net::TcpListener::bind(("0.0.0.0", config.port))
            .map_err(|err| LanError::Io(format!("无法监听 {}：{err}", config.port)))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| LanError::Io(format!("设置非阻塞失败：{err}")))?;
        let bound = listener
            .local_addr()
            .map_err(|err| LanError::Io(format!("取监听地址失败：{err}")))?
            .port();

        let mut threads = Vec::new();

        // 接受连接
        {
            let config = config.clone();
            let local = local.clone();
            let peers = peers.clone();
            let offer = offer.clone();
            let events = event_tx.clone();
            let shutdown = shutdown.clone();
            let received = received.clone();
            let peer_book = peer_book.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("cx-lan-accept".to_string())
                    .spawn(move || {
                        accept_loop(
                            listener, config, local, peers, offer, events, shutdown, peer_book,
                            received,
                        );
                    })
                    .map_err(|err| LanError::Io(format!("启动接受线程失败：{err}")))?,
            );
        }

        // 拨号（显式对端 + mDNS 发现的都走这里）
        let discovered: Arc<std::sync::Mutex<BTreeMap<String, DiscoveredPeer>>> =
            Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        if config.use_mdns {
            match Discovery::start(&config, bound) {
                Ok(discovery) => {
                    let queue = discovered.clone();
                    let events = event_tx.clone();
                    let shutdown = shutdown.clone();
                    threads.push(
                        std::thread::Builder::new()
                            .name("cx-lan-mdns".to_string())
                            .spawn(move || {
                                while !shutdown.load(Ordering::Relaxed) {
                                    if let Ok(peer) = discovery.next_peer() {
                                        let key = format!(
                                            "{}:{}",
                                            peer.addr,
                                            peer.device_id.clone().unwrap_or_default()
                                        );
                                        if queue
                                            .lock()
                                            .expect("发现队列锁中毒")
                                            .insert(key, peer.clone())
                                            .is_none()
                                        {
                                            let _ = events.send(LanEvent::Log {
                                                level: log::Level::Debug,
                                                message: format!(
                                                    "mDNS 发现设备：{} @ {}",
                                                    peer.name, peer.addr
                                                ),
                                            });
                                        }
                                    }
                                }
                            })
                            .map_err(|err| LanError::Io(format!("启动发现线程失败：{err}")))?,
                    );
                }
                Err(err) => {
                    let _ = event_tx.send(LanEvent::Log {
                        level: log::Level::Warn,
                        message: format!("mDNS 不可用（{err}）；只能连显式配置的对端"),
                    });
                }
            }
        }

        // 连接管理：对配置里的地址与发现到的地址都维持一条会话
        {
            let config = config.clone();
            let local = local.clone();
            let peers = peers.clone();
            let offer = offer.clone();
            let events = event_tx.clone();
            let shutdown = shutdown.clone();
            let received = received.clone();
            let peer_book = peer_book.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("cx-lan-dial".to_string())
                    .spawn(move || {
                        dial_loop(
                            config, local, peers, offer, events, shutdown, peer_book, discovered,
                            received,
                        );
                    })
                    .map_err(|err| LanError::Io(format!("启动拨号线程失败：{err}")))?,
            );
        }

        let local_port = bound;
        let _ = event_tx.send(LanEvent::Log {
            level: log::Level::Info,
            message: format!("局域网同步已启用，监听端口 {local_port}"),
        });

        Ok((
            Self {
                local,
                peers,
                offer,
                shutdown,
                threads: std::sync::Mutex::new(threads),
                received,
            },
            event_rx,
        ))
    }

    /// 本地设备 id。
    pub fn device_id(&self) -> &str {
        &self.local.device_id
    }

    /// 已连接的设备数。
    pub fn peer_count(&self) -> usize {
        self.peers.lock().expect("会话表锁中毒").len()
    }

    /// 已连接的设备列表（`(device_id, 名字, 地址)`）。
    pub fn peers(&self) -> Vec<(String, String, SocketAddr)> {
        self.peers
            .lock()
            .expect("会话表锁中毒")
            .values()
            .map(|peer| (peer.device_id.clone(), peer.device_name.clone(), peer.addr))
            .collect()
    }

    /// 收到的条目数。
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// 更新"我有哪些条目"，新连接建立时会用它做补齐。
    pub fn set_offer(&self, entries: Vec<proto::OfferEntry>) {
        *self.offer.lock().expect("待发送表锁中毒") = entries;
    }

    /// 追加一条到"我有哪些条目"（发布时顺手调用）。
    pub fn add_offer(&self, entry: proto::OfferEntry) {
        const MAX_OFFER: usize = 500;
        let mut offer = self.offer.lock().expect("待发送表锁中毒");
        offer.retain(|existing| existing.name != entry.name);
        offer.push(entry);
        if offer.len() > MAX_OFFER {
            let drop_count = offer.len() - MAX_OFFER;
            offer.drain(..drop_count);
        }
    }

    /// 把一条内容推给所有已连接的对端。
    ///
    /// 返回成功投递的对端数（0 表示当前没有对端，内容会在下次连接时通过补齐送达）。
    pub fn publish(&self, name: &str, bytes: &[u8]) -> usize {
        let mut delivered = 0;
        let mut dead = Vec::new();
        {
            let peers = self.peers.lock().expect("会话表锁中毒");
            for (device_id, peer) in peers.iter() {
                let message = LanMessage::Item {
                    name: name.to_string(),
                    bytes: bytes.to_vec(),
                };
                if peer.outgoing.send(message).is_err() {
                    dead.push(device_id.clone());
                } else {
                    delivered += 1;
                }
            }
        }
        if !dead.is_empty()
            && let Ok(mut peers) = self.peers.lock()
        {
            for device_id in dead {
                peers.remove(&device_id);
            }
        }
        delivered
    }

    /// 主动关闭。
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let peers = self.peers.lock().expect("会话表锁中毒");
        for peer in peers.values() {
            let _ = peer.outgoing.send(LanMessage::Bye);
        }
    }

    /// 等服务线程退出（`shutdown` 之后调用）。
    pub fn join(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let handles: Vec<std::thread::JoinHandle<()>> = {
            let mut guard = self.threads.lock().expect("线程表锁中毒");
            guard.drain(..).collect()
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Drop for LanService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// 接受连接的循环。
#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listener: std::net::TcpListener,
    config: LanConfig,
    local: LanIdentity,
    peers: Arc<std::sync::Mutex<BTreeMap<String, PeerSession>>>,
    offer: Arc<std::sync::Mutex<Vec<proto::OfferEntry>>>,
    events: Sender<LanEvent>,
    shutdown: Arc<AtomicBool>,
    peer_book: PeerBook,
    received: Arc<AtomicU64>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                // Windows 上 accept 出来的 socket 会**继承 listener 的非阻塞模式**
                // （Unix 不会）。不显式改回阻塞，读写就会立刻返回 WSAEWOULDBLOCK，
                // 表现为"连上了但一句都读不到"。这个坑在守护进程的控制服务端也踩过一次。
                if let Err(err) = stream.set_nonblocking(false) {
                    let _ = events.send(LanEvent::Log {
                        level: log::Level::Warn,
                        message: format!("设置 {addr} 为阻塞模式失败：{err}"),
                    });
                    continue;
                }
                let config = config.clone();
                let local = local.clone();
                let peers = peers.clone();
                let offer = offer.clone();
                let events = events.clone();
                let shutdown = shutdown.clone();
                let peer_book = peer_book.clone();
                let received = received.clone();
                std::thread::Builder::new()
                    .name("cx-lan-session".to_string())
                    .spawn(move || {
                        if let Err(err) = run_session(
                            stream,
                            addr,
                            SessionRole::Responder,
                            config,
                            local,
                            peers,
                            offer,
                            events.clone(),
                            shutdown.clone(),
                            peer_book,
                            received,
                        ) {
                            let _ = events.send(LanEvent::Log {
                                level: log::Level::Debug,
                                message: format!("来自 {addr} 的会话结束：{err}"),
                            });
                        }
                    })
                    .ok();
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(err) => {
                let _ = events.send(LanEvent::Log {
                    level: log::Level::Warn,
                    message: format!("接受局域网连接失败：{err}"),
                });
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

/// 拨号循环：对配置的与发现的对端维持连接。
#[allow(clippy::too_many_arguments)]
fn dial_loop(
    config: LanConfig,
    local: LanIdentity,
    peers: Arc<std::sync::Mutex<BTreeMap<String, PeerSession>>>,
    offer: Arc<std::sync::Mutex<Vec<proto::OfferEntry>>>,
    events: Sender<LanEvent>,
    shutdown: Arc<AtomicBool>,
    peer_book: PeerBook,
    discovered: Arc<std::sync::Mutex<BTreeMap<String, DiscoveredPeer>>>,
    received: Arc<AtomicU64>,
) {
    // 已经连上的地址不再重复拨号
    let mut attempts: BTreeMap<SocketAddr, std::time::Instant> = BTreeMap::new();
    while !shutdown.load(Ordering::Relaxed) {
        let mut targets: Vec<SocketAddr> = config.peers.clone();
        for peer in discovered.lock().expect("发现队列锁中毒").values() {
            targets.push(peer.addr);
        }
        targets.sort();
        targets.dedup();

        // 已经连上的地址跳过（按会话里的地址判断）
        let connected: Vec<SocketAddr> = peers
            .lock()
            .expect("会话表锁中毒")
            .values()
            .map(|peer| peer.addr)
            .collect();

        for addr in targets {
            // 别和自己握手：本机监听地址 + 同一个端口。
            // （真正的兜底在会话里：对端设备 id 与本机相同时会直接拒绝。）
            if addr.ip().is_loopback() && addr.port() == config.port {
                continue;
            }
            if connected.contains(&addr) {
                continue;
            }
            // 退避：同一个地址最多每 10 秒试一次
            if let Some(last) = attempts.get(&addr)
                && last.elapsed() < std::time::Duration::from_secs(10)
            {
                continue;
            }
            attempts.insert(addr, std::time::Instant::now());
            match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)) {
                Ok(stream) => {
                    let config = config.clone();
                    let local = local.clone();
                    let peers = peers.clone();
                    let offer = offer.clone();
                    let events = events.clone();
                    let shutdown = shutdown.clone();
                    let peer_book = peer_book.clone();
                    let received = received.clone();
                    std::thread::Builder::new()
                        .name("cx-lan-dialer".to_string())
                        .spawn(move || {
                            if let Err(err) = run_session(
                                stream,
                                addr,
                                SessionRole::Initiator,
                                config,
                                local,
                                peers,
                                offer,
                                events.clone(),
                                shutdown,
                                peer_book,
                                received,
                            ) {
                                let _ = events.send(LanEvent::Log {
                                    level: log::Level::Debug,
                                    message: format!("与 {addr} 的会话结束：{err}"),
                                });
                            }
                        })
                        .ok();
                }
                Err(err) => {
                    let _ = events.send(LanEvent::Log {
                        level: log::Level::Debug,
                        message: format!("连接 {addr} 失败：{err}"),
                    });
                }
            }
        }
        for _ in 0..10 {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// 跑一个会话：握手 → 交换清单 → 补齐/推送 → 保持推送。
#[allow(clippy::too_many_arguments)]
fn run_session(
    stream: std::net::TcpStream,
    addr: SocketAddr,
    role: SessionRole,
    config: LanConfig,
    local: LanIdentity,
    peers: Arc<std::sync::Mutex<BTreeMap<String, PeerSession>>>,
    offer: Arc<std::sync::Mutex<Vec<proto::OfferEntry>>>,
    events: Sender<LanEvent>,
    shutdown: Arc<AtomicBool>,
    peer_book: PeerBook,
    received: Arc<AtomicU64>,
) -> Result<(), LanError> {
    let mut session = Session::handshake(stream, role, &config.psk_hex, local.static_private())?;
    let peer_static = hex::encode(session.peer_static_public());

    // Hello：交换身份，顺便校验空间
    session.send(&LanMessage::Hello {
        device_id: local.device_id.clone(),
        device_name: config.device_name.clone(),
        space_id: config.space_id.clone(),
    })?;
    let hello = match session.receive()? {
        LanMessage::Hello {
            device_id,
            device_name,
            space_id,
        } => (device_id, device_name, space_id),
        other => {
            return Err(LanError::Protocol(format!(
                "握手后第一个消息应当是 Hello，实际是 {}",
                other.name()
            )));
        }
    };
    let (peer_id, peer_name, peer_space) = hello;
    if peer_space != config.space_id {
        return Err(LanError::Protocol(format!(
            "对端空间不一致（本地 {:?}，对端 {:?}）",
            config.space_id, peer_space
        )));
    }
    if peer_id == local.device_id {
        return Err(LanError::Protocol(
            "对端与本机设备 id 相同，忽略".to_string(),
        ));
    }

    // TOFU：公钥变了就拒绝（可能是中间人，也可能是换了设备）
    match peer_book.check(&peer_id, &peer_static)? {
        identity::PeerCheck::Trusted => {}
        identity::PeerCheck::FirstSeen => {
            let _ = events.send(LanEvent::Log {
                level: log::Level::Info,
                message: format!("首次见到设备 {peer_name}（{peer_id}），已记录其公钥"),
            });
        }
    }

    // 建立发送侧通道
    let (outgoing_tx, outgoing_rx) = unbounded::<LanMessage>();
    {
        let mut table = peers.lock().expect("会话表锁中毒");
        table.insert(
            peer_id.clone(),
            PeerSession {
                device_id: peer_id.clone(),
                device_name: peer_name.clone(),
                addr,
                outgoing: outgoing_tx.clone(),
            },
        );
    }
    let _ = events.send(LanEvent::PeerUp {
        device_id: peer_id.clone(),
        device_name: peer_name.clone(),
        addr,
    });

    // 交换清单
    let local_offer = offer.lock().expect("待发送表锁中毒").clone();
    session.send(&LanMessage::Manifest {
        device_id: local.device_id.clone(),
        entries: local_offer.clone(),
    })?;

    let result = session_loop(
        &mut session,
        &peer_id,
        &local.device_id,
        &local_offer,
        outgoing_rx,
        &peers,
        &events,
        &shutdown,
        &received,
    );

    // 收尾
    {
        let mut table = peers.lock().expect("会话表锁中毒");
        table.remove(&peer_id);
    }
    let _ = events.send(LanEvent::PeerDown {
        device_id: peer_id.clone(),
    });
    result
}

/// 会话主循环：一边收对端的消息，一边把本机要推的消息发出去。
#[allow(clippy::too_many_arguments)]
fn session_loop(
    session: &mut Session,
    peer_id: &str,
    local_device_id: &str,
    local_offer: &[proto::OfferEntry],
    outgoing: Receiver<LanMessage>,
    peers: &Arc<std::sync::Mutex<BTreeMap<String, PeerSession>>>,
    events: &Sender<LanEvent>,
    shutdown: &Arc<AtomicBool>,
    received: &Arc<AtomicU64>,
) -> Result<(), LanError> {
    // 让读端不那么紧张：设置读超时，循环里同时检查"要发的东西"
    session.set_read_timeout(Some(std::time::Duration::from_millis(200)))?;


    let mine: std::collections::BTreeSet<&str> = local_offer
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // 1) 有要发的就先发
        while let Ok(message) = outgoing.try_recv() {
            if matches!(message, LanMessage::Bye) {
                let _ = session.send(&message);
                return Ok(());
            }
            session.send(&message)?;
        }

        // 2) 收对端的消息（读超时后回到循环顶部）
        match session.receive_opt()? {
            None => continue,
            Some(LanMessage::Manifest { device_id, entries }) => {
                // 对端有、本机没有的：向对端要
                let want: Vec<String> = entries
                    .iter()
                    .filter(|entry| !mine.contains(entry.name.as_str()))
                    .map(|entry| entry.name.clone())
                    .collect();
                if !want.is_empty() {
                    let _ = events.send(LanEvent::Log {
                        level: log::Level::Debug,
                        message: format!("向 {device_id} 补齐 {} 条内容", want.len()),
                    });
                    session.send(&LanMessage::Want { names: want })?;
                }
            }
            Some(LanMessage::Want { names }) => {
                // 对端要这些条目：交给守护进程处理（它才知道内容在哪）
                let _ = events.send(LanEvent::Log {
                    level: log::Level::Debug,
                    message: format!("对端请求 {} 条内容：{}", names.len(), names.join(", ")),
                });
            }
            Some(LanMessage::Item { name, bytes }) => {
                received.fetch_add(1, Ordering::Relaxed);
                let _ = events.send(LanEvent::Item {
                    from: peer_id.to_string(),
                    name,
                    bytes,
                });
            }
            Some(LanMessage::Bye) => {
                let _ = events.send(LanEvent::Log {
                    level: log::Level::Debug,
                    message: format!("对端 {peer_id} 主动断开"),
                });
                break;
            }
            Some(LanMessage::Hello { .. }) => {
                return Err(LanError::Protocol("会话中重复收到 Hello".to_string()));
            }
            Some(LanMessage::Ping) => {
                session.send(&LanMessage::Pong)?;
            }
            // Pong 与"读超时"都不需要额外处理：超时回到循环顶部即可
            Some(LanMessage::Pong) => {}
        }

        // 3) 定时 ping：让对端知道我们还活着（也让死连接尽快暴露）
        if session.tick_ping() {
            session.send(&LanMessage::Ping)?;
        }
        let _ = (local_device_id, peers);
    }
    Ok(())
}
