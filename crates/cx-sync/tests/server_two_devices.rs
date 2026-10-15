//! 两台设备经**自建服务端**同步的集成测试。
//!
//! 与 `webdav_two_devices.rs` / `s3_two_devices.rs` 是同一套验收，只是传输层换成
//! `cx-server`。三种后端都跑通同一组断言，才说明"换后端不改引擎"这个抽象是真的。

use cx_backend::{ServerBackend, ServerOptions};
use cx_clip::ClipContent;
use cx_core::{Config, Identity, Paths};
use cx_server::{Server, ServerConfig};
use cx_sync::{Action, Command, Engine, Event, RemoteSync, RetentionLimits, publish_job};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

struct Device {
    engine: Engine,
    remote: RemoteSync,
    root: std::path::PathBuf,
}

impl Device {
    fn new(tag: &str, addr: std::net::SocketAddr, space_id: &str, token: &str, name: &str) -> Self {
        // 加一个进程内计数：并行跑用例时 tag 与毫秒都可能相同，光靠它们会撞名
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cx-srv-sync-{tag}-{}-{}-{unique}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let identity = Identity::load_or_create(&paths, Some(name)).unwrap();
        let mut config = Config::default();
        config.space.encrypt = false;
        let engine = Engine::new(&paths, config, identity, None).unwrap();
        let device_id = engine.identity().device_id.clone();

        let backend = ServerBackend::new(ServerOptions {
            name: format!("srv-{tag}"),
            url: format!("http://{addr}"),
            space: Some(space_id.to_string()),
            token: token.to_string(),
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(10),
        })
        .unwrap();
        let remote = RemoteSync::new(Box::new(backend), &device_id, name);

        Self {
            engine,
            remote,
            root,
        }
    }

    fn copy(&mut self, content: ClipContent) {
        let actions = self
            .engine
            .handle(cx_core::now_ms(), Event::ClipboardChanged(Some(content)));
        self.publish_from(actions);
    }

    fn publish_from(&mut self, actions: Vec<Action>) {
        for action in actions {
            if let Some((name, bytes, entry)) = publish_job(&action) {
                self.remote
                    .publish(name, bytes, entry.clone(), RetentionLimits::default())
                    .expect("发布到自建服务端失败");
            }
        }
    }

    fn poll(&mut self) -> Option<ClipContent> {
        let outcome = self.remote.poll(32).expect("轮询自建服务端失败");
        let mut applied = None;
        for (name, item) in outcome.incoming {
            let actions = self.engine.handle(
                cx_core::now_ms(),
                Event::Command(Command::ApplyRemote {
                    backend: self.remote.name().to_string(),
                    name,
                    item: Box::new(item),
                }),
            );
            for action in actions {
                if let Action::ApplyToClipboard(content) = action {
                    applied = Some(content);
                }
            }
        }
        applied
    }

    fn feed_clipboard(&mut self, content: ClipContent) -> bool {
        let actions = self
            .engine
            .handle(cx_core::now_ms(), Event::ClipboardChanged(Some(content)));
        let published = actions.iter().any(|a| matches!(a, Action::Publish { .. }));
        self.publish_from(actions);
        published
    }

    fn history_len(&self) -> usize {
        self.engine.history_len()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 起一个真实服务端，返回（地址, 空间 id, 两个设备令牌, 停止句柄）。
fn start_server(tag: &str) -> (std::net::SocketAddr, String, String, String, HarnessGuard) {
    let data_dir = std::env::temp_dir().join(format!(
        "cx-srv-sync-{tag}-{}-{}",
        std::process::id(),
        cx_core::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), &data_dir);
    let server = Server::bind(config).unwrap();
    let addr = server.local_addr().unwrap();
    let store = server.store();
    let (space, _) = store.create_space("同步测试", 0, 0, false).unwrap();
    let token_a = store.issue_token(&space.id, "设备 A").unwrap();
    let token_b = store.issue_token(&space.id, "设备 B").unwrap();

    let shutdown = Arc::new(AtomicBool::new(false));
    let thread = {
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            let _ = server.serve(shutdown);
        })
    };
    (
        addr,
        space.id,
        token_a,
        token_b,
        HarnessGuard {
            shutdown,
            thread: Some(thread),
            data_dir,
        },
    )
}

struct HarnessGuard {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    data_dir: std::path::PathBuf,
}

impl Drop for HarnessGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

#[test]
fn 两台设备经自建服务端双向同步() {
    let (addr, space_id, token_a, token_b, _guard) = start_server("bidir");
    let mut a = Device::new("a", addr, &space_id, &token_a, "device-a");
    let mut b = Device::new("b", addr, &space_id, &token_b, "device-b");

    // A → B
    let text = "来自自建服务端的 A 内容 🎉";
    a.copy(ClipContent::Text(text.to_string()));
    let applied = b.poll().expect("B 应拉到内容并应用");
    assert_eq!(applied.text(), Some(text));
    assert_eq!(b.history_len(), 1);

    // 回声抑制
    assert!(
        !b.feed_clipboard(ClipContent::Text(text.to_string())),
        "写入剪贴板产生的回声不该被再次发布"
    );

    // B → A
    let reverse = "来自自建服务端的 B 回复 🙂";
    b.copy(ClipContent::Text(reverse.to_string()));
    let applied = a.poll().expect("A 应拉到 B 的内容");
    assert_eq!(applied.text(), Some(reverse));
    assert_eq!(a.history_len(), 2);
}

#[test]
fn 增量轮询不会重复下载() {
    let (addr, space_id, token_a, token_b, _guard) = start_server("inc");
    let mut a = Device::new("a", addr, &space_id, &token_a, "device-a");
    let mut b = Device::new("b", addr, &space_id, &token_b, "device-b");

    a.copy(ClipContent::Text("第一条".to_string()));
    assert!(b.poll().is_some());
    assert_eq!(b.history_len(), 1);

    assert!(b.poll().is_none(), "无新内容时不该重复应用");
    assert_eq!(b.history_len(), 1);

    a.copy(ClipContent::Text("第二条".to_string()));
    let applied = b.poll().expect("应拉到第二条");
    assert_eq!(applied.text(), Some("第二条"));
    assert_eq!(b.history_len(), 2);
}

#[test]
fn 掉线后重新上线能补齐() {
    let (addr, space_id, token_a, token_b, _guard) = start_server("catchup");
    let mut a = Device::new("a", addr, &space_id, &token_a, "device-a");
    let mut b = Device::new("b", addr, &space_id, &token_b, "device-b");

    for index in 1..=3 {
        a.copy(ClipContent::Text(format!("离线期间第 {index} 条")));
    }
    let outcome = b.remote.poll(32).unwrap();
    assert_eq!(outcome.incoming.len(), 3, "应补齐离线期间的 3 条");
    for (name, item) in outcome.incoming {
        let actions = b.engine.handle(
            cx_core::now_ms(),
            Event::Command(Command::ApplyRemote {
                backend: b.remote.name().to_string(),
                name,
                item: Box::new(item),
            }),
        );
        b.publish_from(actions);
    }
    assert_eq!(b.history_len(), 3);
    let preview = b.engine.history_preview(0, 64).unwrap();
    assert_eq!(preview.text.as_deref(), Some("离线期间第 3 条"));
}

#[test]
fn 令牌被吊销后拉取会明确失败() {
    let (addr, space_id, token_a, _token_b, guard) = start_server("revoke");
    let mut a = Device::new("a", addr, &space_id, &token_a, "device-a");
    a.copy(ClipContent::Text("吊销前".to_string()));

    // 直接吊销 A 的令牌（服务端数据目录在同一进程里可访问）
    let store = cx_server::Store::open(&guard.data_dir).unwrap();
    assert!(store.revoke_token(&space_id, &token_a).unwrap());

    let err = a.remote.poll(32).unwrap_err();
    assert!(err.is_auth(), "应报认证类错误：{err}");
    assert!(!err.is_retryable());
}
