//! WebDAV 客户端对着**真实 HTTP 服务端**的集成测试。
//!
//! 用仓库内的 `TestWebDavServer`（刻意还原真实服务端的怪癖），因此：
//! - 不需要外网、不需要 Docker，`cargo test` 就能跑；
//! - 三端 CI 都能覆盖（Windows / macOS / Linux）。

use cx_backend::testserver::TestWebDavServer;
use cx_backend::{
    BackendError, PutOutcome, RemoteBackend, WebDavBackend, WebDavOptions, layout, list_manifests,
    load_space_meta, store_space_meta,
};
use std::time::Duration;

fn backend_for(server: &TestWebDavServer, name: &str) -> WebDavBackend {
    WebDavBackend::new(WebDavOptions {
        name: name.to_string(),
        url: server.url(),
        username: None,
        password: None,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(10),
    })
    .unwrap()
}

#[test]
fn 写入读取与列举往返() {
    let server = TestWebDavServer::start();
    let mut backend = backend_for(&server, "t");

    let name = cx_proto::ItemName::build(
        1_759_299_998_712,
        "ab12cd34ef5678901234567890abcdef",
        "9f3c1e77deadbeef",
    );
    let path = layout::item_path(&name).unwrap();
    let payload = b"hello webdav";

    // 第一次写：父目录不存在 ⇒ 服务端返回 409 ⇒ 客户端 MKCOL 补建后重试
    let outcome = backend.put(&path, payload, true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert!(server.has(&path), "对象应落到服务端");
    assert_eq!(server.get(&path).unwrap(), payload);

    // 读回
    let (bytes, meta) = backend.get_with_meta(&path).unwrap().unwrap();
    assert_eq!(bytes, payload);
    assert!(meta.etag.is_some(), "应拿到 ETag");
    assert_eq!(meta.size, Some(payload.len() as u64));

    // 列举（按天目录）
    let objects = backend
        .list(&layout::items_prefix_for_day("20251001"))
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].path, path);
    assert!(!objects[0].is_collection);
    assert!(objects[0].etag.is_some());

    // 条件写：对象已存在 ⇒ AlreadyExists，内容不被覆盖
    assert_eq!(
        backend.put(&path, b"changed", true).unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(server.get(&path).unwrap(), payload);
}

#[test]
fn 服务端不支持条件写时会覆盖() {
    let server = TestWebDavServer::start();
    server.ignore_conditional_put(true);
    let mut backend = backend_for(&server, "t");

    backend
        .put("v1/items/20251001/x.cxi", b"first", true)
        .unwrap();
    let outcome = backend
        .put("v1/items/20251001/x.cxi", b"second", true)
        .unwrap();
    assert!(
        matches!(outcome, PutOutcome::Replaced { .. }),
        "{outcome:?}"
    );
    assert_eq!(server.get("v1/items/20251001/x.cxi").unwrap(), b"second");
}

#[test]
fn put_遇到强行_409_时补建目录后成功() {
    let server = TestWebDavServer::start();
    let mut backend = backend_for(&server, "t");
    server.force_next_put_conflict();

    let outcome = backend
        .put("v1/items/20251002/y.cxi", b"payload", true)
        .unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }));
    assert!(server.has("v1/items/20251002/y.cxi"));

    // 确认确实走了 MKCOL
    let methods = server.methods();
    assert!(
        methods.iter().any(|m| m.starts_with("MKCOL")),
        "应当通过 MKCOL 补建目录：{methods:?}"
    );
}

#[test]
fn 不存在的对象与目录() {
    let server = TestWebDavServer::start();
    let mut backend = backend_for(&server, "t");

    assert!(
        backend
            .get("v1/items/20251001/missing.cxi")
            .unwrap()
            .is_none()
    );
    assert!(
        backend
            .head("v1/items/20251001/missing.cxi")
            .unwrap()
            .is_none()
    );
    // 目录不存在 ⇒ 空列表而不是错误
    assert!(backend.list(layout::ITEMS_PREFIX).unwrap().is_empty());
    // 删除不存在的对象视为成功（幂等）
    backend.delete("v1/items/20251001/missing.cxi").unwrap();
}

#[test]
fn 认证失败会给出可识别的错误() {
    let server = TestWebDavServer::start();
    server.require_auth("alice", "secret");

    // 不带凭据
    let mut anonymous = backend_for(&server, "anon");
    let err = anonymous.list(layout::MANIFESTS_PREFIX).unwrap_err();
    assert!(err.is_auth(), "{err}");
    assert!(!err.is_retryable(), "认证错误不该被退避重试");

    // 凭据正确则可用
    let mut authed = WebDavBackend::new(WebDavOptions {
        name: "auth".to_string(),
        url: server.url(),
        username: Some("alice".to_string()),
        password: Some("secret".to_string()),
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(10),
    })
    .unwrap();
    assert!(authed.list(layout::MANIFESTS_PREFIX).unwrap().is_empty());
}

#[test]
fn 探活报告服务端信息() {
    let server = TestWebDavServer::start();
    let mut backend = backend_for(&server, "t");
    let text = backend.probe().unwrap();
    assert!(text.contains("可访问"), "{text}");
}

#[test]
fn manifest_与空间元数据的完整流程() {
    let server = TestWebDavServer::start();
    let mut backend = backend_for(&server, "t");

    let device = "ab12cd34ef5678901234567890abcdef";
    let path = layout::manifest_path(device);
    let body = br#"{"v":1,"device_id":"ab12cd34ef5678901234567890abcdef","device_name":"ws","updated_ms":1,"items":[]}"#;

    backend.put(&path, body, false).unwrap();
    let manifests = list_manifests(&mut backend).unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].path, path);
    let etag = manifests[0].etag.clone();
    assert!(etag.is_some());

    // 内容未变时 ETag 相同（增量判断的依据）
    let again = list_manifests(&mut backend).unwrap();
    assert!(again[0].matches_etag(etag.as_deref()), "ETag 应保持不变");

    // 改写后 ETag 变化
    backend.put(&path, br#"{"v":1,"items":[]}"#, false).unwrap();
    let changed = list_manifests(&mut backend).unwrap();
    assert!(
        !changed[0].matches_etag(etag.as_deref()),
        "内容变了 ETag 应变化"
    );

    // 空间元数据（可公开读）
    assert!(load_space_meta(&mut backend).unwrap().is_none());
    store_space_meta(&mut backend, r#"{"v":1,"space_id":"aa"}"#).unwrap();
    assert_eq!(
        load_space_meta(&mut backend).unwrap().as_deref(),
        Some(r#"{"v":1,"space_id":"aa"}"#)
    );
}

#[test]
fn 无效_url_被拒绝() {
    for bad in ["not-a-url", "ftp://x/y", "/dav/local"] {
        let result = WebDavBackend::new(WebDavOptions {
            name: "bad".to_string(),
            url: bad.to_string(),
            username: None,
            password: None,
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(1),
        });
        assert!(
            matches!(result, Err(BackendError::Config(_))),
            "{bad} 应被拒绝"
        );
    }
}

#[test]
fn 端口不可达时报传输错误且可重试() {
    // 127.0.0.1:1 基本不可能有人监听
    let mut backend = WebDavBackend::new(WebDavOptions {
        name: "dead".to_string(),
        url: "http://127.0.0.1:1/".to_string(),
        username: None,
        password: None,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_millis(800),
    })
    .unwrap();
    let err = backend.list(layout::MANIFESTS_PREFIX).unwrap_err();
    assert!(matches!(err, BackendError::Transport(_)), "{err}");
    assert!(err.is_retryable());
}
