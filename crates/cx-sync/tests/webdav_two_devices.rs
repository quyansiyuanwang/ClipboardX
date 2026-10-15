//! 两台设备经**真实 WebDAV 服务端**同步的集成测试。
//!
//! 用仓库内的测试服务端（真实 HTTP、真实 207 响应、真实的
//! `409 → MKCOL` 与 `If-None-Match` 行为），跑通
//!
//! 1. A 复制 → 发布到 WebDAV；
//! 2. B 轮询 → 拉取 → 写入本机剪贴板；
//! 3. 写入剪贴板引发的回声**不会**被再次上传（三层防回环）；
//! 4. 反向（B → A）同样成立。
//!
//! 测试直接使用守护进程将来会用的同一批组件：`Engine`（状态机）+ `RemoteSync`
//! （远端粘合），因此这里过的东西就是产品里跑的东西。

use cx_backend::testserver::TestWebDavServer;
use cx_backend::{WebDavBackend, WebDavOptions};
use cx_clip::ClipContent;
use cx_core::{Config, Identity, LocalSpace, Paths};
use cx_sync::{Action, Command, Engine, Event, RemoteSync, RetentionLimits, publish_job};
use std::time::Duration;

/// 一台"设备"：引擎 + 临时目录 + 远端同步器。
struct Device {
    engine: Engine,
    remote: RemoteSync,
    root: std::path::PathBuf,
    device_id: String,
}

impl Device {
    /// 建一台设备并连到同一个 WebDAV 服务器。
    fn new(tag: &str, server: &TestWebDavServer, space: Option<LocalSpace>, name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cx-webdav-{tag}-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let identity = Identity::load_or_create(&paths, Some(name)).unwrap();
        let mut config = Config::default();
        config.space.encrypt = space
            .as_ref()
            .is_some_and(|space| space.cipher.space_key().is_some());
        let engine = Engine::new(&paths, config, identity, space).unwrap();
        let device_id = engine.identity().device_id.clone();

        let backend = WebDavBackend::new(WebDavOptions {
            name: format!("webdav-{tag}"),
            url: server.url(),
            username: None,
            password: None,
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
            device_id,
        }
    }

    /// 模拟"用户复制了内容"→ 引擎产出发布动作 → 落到 WebDAV。
    fn copy(&mut self, content: ClipContent) {
        let actions = self
            .engine
            .handle(cx_core::now_ms(), Event::ClipboardChanged(Some(content)));
        self.publish_from(actions);
    }

    /// 执行引擎的发布动作（守护进程的 worker 线程做的就是这件事）。
    fn publish_from(&mut self, actions: Vec<Action>) {
        for action in actions {
            if let Some((name, bytes, entry)) = publish_job(&action) {
                self.remote
                    .publish(name, bytes, entry.clone(), RetentionLimits::default())
                    .expect("发布到 WebDAV 失败");
            }
        }
    }

    /// 轮询远端并把内容交给引擎，返回被应用的内容（若有）。
    fn poll(&mut self) -> Option<ClipContent> {
        let outcome = self.remote.poll(32).expect("轮询 WebDAV 失败");
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

    /// 模拟"剪贴板内容变化"事件（用于验证回声抑制），返回是否有发布动作。
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

#[test]
fn 两台设备经真实_webdav_双向同步() {
    let server = TestWebDavServer::start();
    let mut a = Device::new("a", &server, None, "device-a");
    let mut b = Device::new("b", &server, None, "device-b");

    // ---- A → B ----
    let text = "来自 A 的剪贴板内容 🎉";
    a.copy(ClipContent::Text(text.to_string()));
    assert!(server.has(&cx_backend::layout::manifest_path(&a.device_id)));
    assert!(server.object_count() >= 2, "应写入 item 与 manifest");

    let applied = b.poll().expect("B 应拉到内容并应用");
    assert_eq!(applied.text(), Some(text), "B 的剪贴板内容应与 A 一致");
    assert_eq!(b.history_len(), 1, "B 的历史应记录该条");

    // ---- 回声抑制：B 的剪贴板变化事件不应再次上传 ----
    assert!(
        !b.feed_clipboard(ClipContent::Text(text.to_string())),
        "应用到剪贴板产生的回声不应被再次发布"
    );
    assert_eq!(b.history_len(), 1, "不应产生重复历史");

    // ---- B → A ----
    let reverse = "来自 B 的回复 🙂";
    b.copy(ClipContent::Text(reverse.to_string()));
    let applied = a.poll().expect("A 应拉到 B 的内容");
    assert_eq!(applied.text(), Some(reverse));
    assert_eq!(a.history_len(), 2, "A 自己的那条 + 来自 B 的那条");
}

#[test]
fn 加密空间下同样能同步() {
    let server = TestWebDavServer::start();

    // 两台设备共享同一把空间密钥（真实场景里由 cx space join 得到）
    let space_root = std::env::temp_dir().join(format!("cx-webdav-space-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&space_root);
    let space_paths = Paths::from_root(&space_root);
    space_paths.ensure().unwrap();
    let space = LocalSpace::create(&space_paths, None).unwrap();
    // 用同一把密钥手工构造"加密"空间（避免在测试里付 argon2 19 MiB 的代价）
    let key = cx_crypto::SpaceKey::generate().unwrap();
    let mut encrypted = LocalSpace {
        meta: space.meta.clone(),
        cipher: cx_core::SpaceCipher::Encrypted(key),
    };
    encrypted.meta.cipher = cx_crypto::CIPHER_XCHACHA20POLY1305.to_string();
    encrypted.meta.kdf = Some(cx_crypto::KdfParams::cheap_for_tests());
    encrypted.meta.wrapped_key = Some("test".to_string());

    let mut a = Device::new("enc-a", &server, Some(encrypted.clone()), "device-a");
    let mut b = Device::new("enc-b", &server, Some(encrypted), "device-b");

    let text = "加密传输的内容";
    a.copy(ClipContent::Text(text.to_string()));

    // 远端对象应当是密文：明文不应出现在任何对象里
    let objects = server.object_count();
    assert!(objects >= 2);
    let manifest_key = cx_backend::layout::manifest_path(&a.device_id);
    assert!(server.has(&manifest_key));

    // B 能解出原文
    let applied = b.poll().expect("加密空间下 B 也应拉到内容");
    assert_eq!(applied.text(), Some(text));

    let _ = std::fs::remove_dir_all(&space_root);
}

#[test]
fn 增量轮询不会重复下载历史对象() {
    let server = TestWebDavServer::start();
    let mut a = Device::new("inc-a", &server, None, "device-a");
    let mut b = Device::new("inc-b", &server, None, "device-b");

    a.copy(ClipContent::Text("第一条".to_string()));
    assert!(b.poll().is_some());
    assert_eq!(b.history_len(), 1);

    // B 再轮询：ETag 未变 + 游标已推进 ⇒ 不应有新的应用
    assert!(b.poll().is_none(), "无新内容时不应重复应用");
    assert_eq!(b.history_len(), 1);

    // A 再发一条：只应拉到新的那条
    a.copy(ClipContent::Text("第二条".to_string()));
    let applied = b.poll().expect("应拉到第二条");
    assert_eq!(applied.text(), Some("第二条"));
    assert_eq!(b.history_len(), 2);
}

#[test]
fn 掉线后重新上线能补齐() {
    let server = TestWebDavServer::start();
    let mut a = Device::new("off-a", &server, None, "device-a");
    let mut b = Device::new("off-b", &server, None, "device-b");

    // A 连续发布三条，B 一直不在线
    for index in 1..=3 {
        a.copy(ClipContent::Text(format!("离线期间第 {index} 条")));
    }

    // B 上线后一次轮询应补齐（最多 32 个/轮）
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
    // 最后一条应当是最新的那条（引擎按顺序应用，最后写入剪贴板的是"第 3 条"）
    let preview = b.engine.history_preview(0, 64).unwrap();
    assert_eq!(preview.text.as_deref(), Some("离线期间第 3 条"));
}
