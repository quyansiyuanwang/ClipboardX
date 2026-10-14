//! 两个真实的 `LanService` 在回环上互通的集成测试。
//!
//! 这里不用 mDNS（CI 与容器里多播常被禁），而是显式给对端地址——这也正是
//! `LanConfig::peers` 存在的意义：mDNS 只是"自动填地址"，不是必需品。

use cx_lan::{LanConfig, LanEvent, LanIdentity, LanService, PeerBook};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

struct Node {
    service: LanService,
    events: crossbeam_channel::Receiver<LanEvent>,
    dir: std::path::PathBuf,
}

impl Node {
    fn start(tag: &str, port: u16, psk: &str, peers: Vec<SocketAddr>, space: &str) -> Self {
        // 加一个进程内计数：多个用例用同样的 tag（"a"/"b"）且 now_ms 只有毫秒精度时
        // 会撞到同一个目录，一个用例的 remove_dir_all 会把另一个的密钥文件删空。
        // 这个坑在 CI 上才暴露（本地跑得快、撞不上）。
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cx-lan-node-{tag}-{}-{}-{unique}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let device_id = format!("device-{tag}");
        let identity = LanIdentity::load_or_create(&dir, &device_id).unwrap();
        let book = PeerBook::open(dir.join("peers.json")).unwrap();

        let config = LanConfig {
            enabled: true,
            port,
            space_id: Some(space.to_string()),
            psk_hex: psk.to_string(),
            peers,
            use_mdns: false,
            device_name: format!("设备 {tag}"),
            device_id,
        };
        let (service, events) = LanService::start(config, identity, book).unwrap();
        Self {
            service,
            events,
            dir,
        }
    }

    fn port(&self) -> u16 {
        self.service
            .peers()
            .first()
            .map(|(_, _, addr)| addr.port())
            .unwrap_or(0)
    }

    /// 等到出现一个连接（或超时）。
    fn wait_peer(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.service.peer_count() > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 等到收到一条内容（或超时）。
    fn wait_item(&self, timeout: Duration) -> Option<(String, Vec<u8>, String)> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while let Ok(event) = self.events.try_recv() {
                if let LanEvent::Item { from, name, bytes } = event {
                    return Some((from, bytes, name));
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 选两个空闲端口（先绑定再释放），用于两台"设备"。
fn two_free_ports() -> (u16, u16) {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = first.local_addr().unwrap().port();
    let b = second.local_addr().unwrap().port();
    (a, b)
}

#[test]
fn 两台设备直连并在建立连接后互推内容() {
    let (port_a, port_b) = two_free_ports();
    let psk = cx_lan::derive_psk("space-key-material", None);
    let addr_a: SocketAddr = format!("127.0.0.1:{port_a}").parse().unwrap();
    let _addr_b: SocketAddr = format!("127.0.0.1:{port_b}").parse().unwrap();

    // B 知道 A 的地址（A 不需要知道 B，B 会主动连上来）
    let node_a = Node::start("a", port_a, &psk, Vec::new(), "space-1");
    let node_b = Node::start("b", port_b, &psk, vec![addr_a], "space-1");

    if std::env::var("CX_LAN_DEBUG").is_ok() {
        std::thread::sleep(Duration::from_secs(2));
        for (tag, node) in [("A", &node_a), ("B", &node_b)] {
            while let Ok(event) = node.events.try_recv() {
                eprintln!("[{tag}] {event:?}");
            }
        }
    }
    assert!(
        node_a.wait_peer(Duration::from_secs(10)),
        "A 应当看到 B 连上来"
    );
    assert!(
        node_b.wait_peer(Duration::from_secs(10)),
        "B 应当看到连接建立"
    );
    assert_eq!(node_a.service.peer_count(), 1);
    assert_eq!(node_b.service.peer_count(), 1);

    // A 推一条内容，B 应当收到（经过真实 TCP + Noise 加密）
    let delivered = node_a
        .service
        .publish("1759323998000-ab12cd34-9f3c1e77.cxi", b"hello lan");
    assert_eq!(delivered, 1, "应当投递到 1 个对端");
    let received = node_b
        .wait_item(Duration::from_secs(5))
        .expect("B 应当收到内容");
    assert_eq!(received.1, b"hello lan");
    assert_eq!(received.0, "device-a", "事件里应当带上来源设备 id");

    // 反向也通
    node_b
        .service
        .publish("1759323999000-bb22cd34-9f3c1e77.cxi", b"reply");
    let back = node_a
        .wait_item(Duration::from_secs(5))
        .expect("A 应当收到回复");
    assert_eq!(back.1, b"reply");
}

#[test]
fn psk_不同时连不上也收不到内容() {
    let (port_a, port_b) = two_free_ports();
    let addr_a: SocketAddr = format!("127.0.0.1:{port_a}").parse().unwrap();

    let node_a = Node::start(
        "a",
        port_a,
        &cx_lan::derive_psk("space-one", None),
        Vec::new(),
        "space-1",
    );
    // B 的空间/PSK 都不一样：握手必须失败
    let node_b = Node::start(
        "b",
        port_b,
        &cx_lan::derive_psk("space-two", None),
        vec![addr_a],
        "space-2",
    );

    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(node_a.service.peer_count(), 0, "PSK 不同不该建立会话");
    assert_eq!(node_b.service.peer_count(), 0, "PSK 不同不该建立会话");
    assert!(
        node_b.wait_item(Duration::from_millis(300)).is_none(),
        "不该收到任何内容"
    );
    let _ = node_b.port();
}

#[test]
fn 空间不一致时拒绝会话() {
    let (port_a, port_b) = two_free_ports();
    let addr_a: SocketAddr = format!("127.0.0.1:{port_a}").parse().unwrap();
    // PSK 相同（同一个配对码派生），但声明空间不同：握手能成，业务层必须拒绝
    let psk = cx_lan::derive_psk("shared", Some("1234"));

    let node_a = Node::start("a", port_a, &psk, Vec::new(), "space-one");
    let _node_b = Node::start("b", port_b, &psk, vec![addr_a], "space-two");

    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(node_a.service.peer_count(), 0, "空间不一致不该建立会话");
}

#[test]
fn 没有对端时发布不报错只是投递零() {
    let (port_a, _) = two_free_ports();
    let node = Node::start(
        "solo",
        port_a,
        &cx_lan::derive_psk("solo", None),
        Vec::new(),
        "space-1",
    );

    assert_eq!(node.service.publish("a.cxi", b"nobody listens"), 0);
    assert_eq!(node.service.peer_count(), 0);
    assert_eq!(node.service.received(), 0);
}
