//! 两台设备经 **S3 兼容存储**（仓库内会校验签名的测试服务端）同步的集成测试。
//!
//! 与 `webdav_two_devices.rs` 是同一套验收，只是换了传输层：S3 没有目录、必须签名、
//! 列举靠键前缀。跑通它说明"换后端不改引擎"这个抽象真的成立。

use cx_backend::s3_testserver::TestS3Server;
use cx_backend::{S3Backend, S3Options};
use cx_clip::ClipContent;
use cx_core::{Config, Identity, Paths};
use cx_sync::{Action, Command, Engine, Event, RemoteSync, RetentionLimits, publish_job};
use std::sync::Arc;
use std::time::Duration;

const ACCESS_KEY: &str = "minioadmin";
const SECRET_KEY: &str = "minioadmin";
const BUCKET: &str = "clipboardx";

/// 一台"设备"：引擎 + 临时目录 + S3 同步器。
struct Device {
    engine: Engine,
    remote: RemoteSync,
    root: std::path::PathBuf,
    device_id: String,
}

impl Device {
    fn new(tag: &str, server: &Arc<TestS3Server>, prefix: &str, name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cx-s3-{tag}-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let identity = Identity::load_or_create(&paths, Some(name)).unwrap();
        let engine = Engine::new(&paths, Config::default(), identity, None).unwrap();
        let device_id = engine.identity().device_id.clone();

        let backend = S3Backend::new(S3Options {
            name: format!("s3-{tag}"),
            endpoint: Some(server.endpoint()),
            region: "us-east-1".to_string(),
            bucket: BUCKET.to_string(),
            prefix: prefix.to_string(),
            access_key_id: ACCESS_KEY.to_string(),
            secret_access_key: SECRET_KEY.to_string(),
            session_token: None,
            path_style: true,
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

    /// 模拟"用户复制了内容"→ 引擎产出发布动作 → 落到 S3。
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
                    .expect("发布到 S3 失败");
            }
        }
    }

    /// 轮询远端并把内容交给引擎，返回被应用的内容（若有）。
    fn poll(&mut self) -> Option<ClipContent> {
        let outcome = self.remote.poll(32).expect("轮询 S3 失败");
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

#[test]
fn 两台设备经_s3_双向同步() {
    let server = Arc::new(TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET));
    let mut a = Device::new("a", &server, "space-x", "device-a");
    let mut b = Device::new("b", &server, "space-x", "device-b");

    // A → B
    let text = "来自 S3 上 A 的内容 🎉";
    a.copy(ClipContent::Text(text.to_string()));
    let key = format!("space-x/v1/manifests/{}.json", a.device_id);
    assert!(server.has(&key), "A 的 manifest 应落到 S3");
    assert!(server.object_count() >= 2, "应写入 item 与 manifest");

    let applied = b.poll().expect("B 应拉到内容并应用");
    assert_eq!(applied.text(), Some(text));
    assert_eq!(b.history_len(), 1);

    // 回声抑制
    assert!(
        !b.feed_clipboard(ClipContent::Text(text.to_string())),
        "写入剪贴板产生的回声不该被再次发布"
    );

    // B → A
    let reverse = "来自 S3 上 B 的回复 🙂";
    b.copy(ClipContent::Text(reverse.to_string()));
    let applied = a.poll().expect("A 应拉到 B 的内容");
    assert_eq!(applied.text(), Some(reverse));
    assert_eq!(a.history_len(), 2);

    // 全程签名必须都被服务端接受
    let failures = server.signature_failures();
    assert!(failures.is_empty(), "签名校验失败：{failures:?}");
}

#[test]
fn s3_下的增量轮询不会重复下载() {
    let server = Arc::new(TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET));
    let mut a = Device::new("inc-a", &server, "space-y", "device-a");
    let mut b = Device::new("inc-b", &server, "space-y", "device-b");

    a.copy(ClipContent::Text("第一条".to_string()));
    assert!(b.poll().is_some());
    assert_eq!(b.history_len(), 1);

    // 无新内容：ETag 未变 + 游标推进 ⇒ 不应重复应用
    assert!(b.poll().is_none(), "无新内容时不该重复应用");
    assert_eq!(b.history_len(), 1);

    // 新内容只拉新的那条
    a.copy(ClipContent::Text("第二条".to_string()));
    let applied = b.poll().expect("应拉到第二条");
    assert_eq!(applied.text(), Some("第二条"));
    assert_eq!(b.history_len(), 2);
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 不同前缀的空间互不干扰() {
    let server = Arc::new(TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET));
    let mut a = Device::new("p-a", &server, "space-one", "device-a");
    let mut c = Device::new("p-c", &server, "space-two", "device-c");

    a.copy(ClipContent::Text(
        "只该被 space-one 里的设备看到".to_string(),
    ));
    // 另一个前缀下没有 manifest，因此拉不到任何东西
    assert!(c.poll().is_none(), "不同前缀的空间不应互相看到");
    assert_eq!(c.history_len(), 0);
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}
