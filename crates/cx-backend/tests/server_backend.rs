//! `server` 后端对着**真实 cx-server**（同进程起在同一台机器上）的集成测试。
//!
//! 用真实的 HTTP 监听端口，而不是直接调 handler：这样签名之外的所有东西
//! （请求行、百分号编码、响应头、状态码）都被真实网络路径验证过。

use cx_backend::{BackendError, PutOutcome, RemoteBackend, ServerBackend, ServerOptions, layout};
use cx_server::{Server, ServerConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 一个跑着真实服务的测试环境。
struct Harness {
    backend: ServerBackend,
    addr: std::net::SocketAddr,
    space_id: String,
    store: Arc<cx_server::Store>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    data_dir: std::path::PathBuf,
}

impl Harness {
    fn start(tag: &str, quota: u64, space_override: Option<String>) -> Self {
        let data_dir = std::env::temp_dir().join(format!(
            "cx-server-be-{tag}-{}-{}",
            std::process::id(),
            cx_server::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);

        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), &data_dir);
        let server = Server::bind(config).unwrap();
        let addr = server.local_addr().unwrap();
        let store = server.store();

        let (space, _) = store.create_space("集成测试", quota, 0, true).unwrap();
        let token = store.issue_token(&space.id, "测试设备").unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                let _ = server.serve(shutdown);
            })
        };

        let backend = ServerBackend::new(ServerOptions {
            name: "srv".to_string(),
            url: format!("http://{addr}"),
            space: Some(space_override.unwrap_or_else(|| space.id.clone())),
            token,
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(10),
        })
        .unwrap();

        Self {
            backend,
            addr,
            space_id: space.id,
            store,
            shutdown,
            thread: Some(thread),
            data_dir,
        }
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn 写入读取列举删除完整往返() {
    let mut harness = Harness::start("roundtrip", 0, None);
    let backend = &mut harness.backend;

    // ensure_root：确认令牌与空间都对
    assert!(!backend.ensure_root().unwrap(), "空间已存在，不需要创建");

    let name = cx_proto::ItemName::build(
        1_759_323_998_000,
        "ab12cd34ef5678901234567890abcdef",
        "9f3c1e77deadbeef",
    );
    let path = layout::item_path(&name).unwrap();
    let payload = b"hello cx-server";

    let outcome = backend.put(&path, payload, true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert!(
        harness
            .store
            .get_object(&harness.space_id, &path)
            .unwrap()
            .is_some(),
        "对象应落到服务端"
    );

    // 读回
    let (bytes, meta) = backend.get_with_meta(&path).unwrap().unwrap();
    assert_eq!(bytes, payload);
    assert!(meta.etag.is_some(), "服务端应给出 ETag");
    assert_eq!(meta.size, Some(payload.len() as u64));

    // HEAD
    let head = backend.head(&path).unwrap().unwrap();
    assert_eq!(head.etag, meta.etag);

    // 列举（按天前缀）
    let objects = backend
        .list(&layout::items_prefix_for_day("20251001"))
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].path, path);
    assert!(objects[0].etag.is_some());
    assert!(!objects[0].is_collection);

    // 条件写：已存在 ⇒ AlreadyExists，内容不变
    assert_eq!(
        backend.put(&path, b"changed", true).unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(backend.get(&path).unwrap().unwrap(), payload);

    // 覆盖写 ⇒ Replaced
    let replaced = backend.put(&path, b"changed", false).unwrap();
    assert!(
        matches!(replaced, PutOutcome::Replaced { .. }),
        "{replaced:?}"
    );
    assert_eq!(backend.get(&path).unwrap().unwrap(), b"changed");

    // 删除（幂等）
    backend.delete(&path).unwrap();
    assert!(backend.get(&path).unwrap().is_none());
    backend.delete(&path).unwrap();

    // 不存在的对象/前缀
    assert!(
        backend
            .head("v1/items/20251001/missing.cxi")
            .unwrap()
            .is_none()
    );
    assert!(backend.list("v1/items/19700101/").unwrap().is_empty());
}

#[test]
fn 令牌无效时报认证失败() {
    let mut harness = Harness::start("auth", 0, None);
    let mut backend = ServerBackend::new(ServerOptions {
        name: "bad".to_string(),
        url: format!("http://{}", harness.addr),
        space: None,
        token: "cxd_not-a-real-token".to_string(),
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();

    let err = backend.list("v1/manifests/").unwrap_err();
    assert!(err.is_auth(), "{err}");
    assert!(!err.is_retryable(), "令牌无效不该被无限重试");
    assert!(err.to_string().contains("cx space join"), "{err}");
    harness.stop();
}

#[test]
fn 连错空间时报认证类错误() {
    let mut harness = Harness::start("wrongspace", 0, Some("deadbeefdeadbeef".to_string()));
    let err = harness.backend.ensure_root().unwrap_err();
    assert!(err.is_auth(), "{err}");
    assert!(
        err.to_string().contains("空间") || err.to_string().contains("不属于"),
        "{err}"
    );
    harness.stop();
}

#[test]
fn 配额用尽是明确错误() {
    let mut harness = Harness::start("quota", 200, None);
    let backend = &mut harness.backend;
    backend
        .put("v1/items/20251001/a.cxi", &[0u8; 150], true)
        .unwrap();

    let err = backend
        .put("v1/items/20251001/b.cxi", &[0u8; 150], true)
        .unwrap_err();
    assert!(!err.is_retryable(), "配额用尽不该重试：{err}");
    assert!(err.to_string().contains("配额"), "{err}");
    // 服务端侧用量只统计成功写入的部分
    assert!(harness.store.used_bytes(&harness.space_id).unwrap() >= 150);
    harness.stop();
}

#[test]
fn 传输失败可重试() {
    let mut backend = ServerBackend::new(ServerOptions {
        name: "dead".to_string(),
        url: "http://127.0.0.1:1".to_string(),
        space: None,
        token: "cxd_x".to_string(),
        ca_file: None,
        insecure: false,
        timeout: Duration::from_millis(600),
    })
    .unwrap();
    let err = backend.list("v1/").unwrap_err();
    assert!(matches!(err, BackendError::Transport(_)), "{err}");
    assert!(err.is_retryable());
}
