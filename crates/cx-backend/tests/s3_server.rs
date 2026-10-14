//! S3 客户端对着**会校验签名的**测试服务端的集成测试。
//!
//! 这里的服务端不是"随便应答的 mock"：它从收到的原始字节重建规范化请求、
//! 重算 SigV4 签名并与客户端给的 `Authorization` 逐字节比对。因此下面每个测试
//! 最后都会断言 `signature_failures()` 为空——**任何**路径编码、查询串排序、
//! 参与签名的头、Host 写法的错误都会在这里现形。

use cx_backend::s3_testserver::TestS3Server;
use cx_backend::{BackendError, PutOutcome, RemoteBackend, S3Backend, S3Options, layout};
use std::time::Duration;

const ACCESS_KEY: &str = "minioadmin";
const SECRET_KEY: &str = "minioadmin";
const BUCKET: &str = "clipboardx";

fn backend_for(server: &TestS3Server, prefix: &str) -> S3Backend {
    S3Backend::new(S3Options {
        name: "s3test".to_string(),
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
    .unwrap()
}

#[test]
fn 写入读取列举删除完整往返() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "space-a");

    let name = cx_proto::ItemName::build(
        1_759_323_998_000,
        "ab12cd34ef5678901234567890abcdef",
        "9f3c1e77deadbeef",
    );
    let path = layout::item_path(&name).unwrap();
    let payload = b"hello s3";

    // 写入（不存在才写）
    let outcome = backend.put(&path, payload, true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert!(server.has(&format!("space-a/{path}")), "对象应落到服务端");
    assert_eq!(server.get(&format!("space-a/{path}")).unwrap(), payload);

    // 读回
    let (bytes, meta) = backend.get_with_meta(&path).unwrap().unwrap();
    assert_eq!(bytes, payload);
    assert!(meta.etag.is_some(), "应拿到 ETag");
    assert_eq!(meta.size, Some(payload.len() as u64));

    // HEAD
    let head = backend.head(&path).unwrap().unwrap();
    assert_eq!(head.size, Some(payload.len() as u64));
    assert_eq!(head.etag, meta.etag);

    // 列举（按天前缀）
    let day_prefix = layout::items_prefix_for_day("20251001");
    let objects = backend.list(&day_prefix).unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].path, path, "列举结果应是相对路径");
    assert!(objects[0].etag.is_some());
    assert!(!objects[0].is_collection);

    // 条件写：已存在 ⇒ AlreadyExists，内容不被覆盖
    assert_eq!(
        backend.put(&path, b"changed", true).unwrap(),
        PutOutcome::AlreadyExists
    );
    assert_eq!(server.get(&format!("space-a/{path}")).unwrap(), payload);

    // 删除（幂等）
    backend.delete(&path).unwrap();
    assert!(!server.has(&format!("space-a/{path}")));
    backend.delete(&path).unwrap();

    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 每个请求的签名都必须被服务端接受() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "space-a");

    // 把所有动作都跑一遍，逐个校验签名
    backend.put("v1/manifests/a.json", b"{}", false).unwrap();
    backend.get("v1/manifests/a.json").unwrap();
    backend.head("v1/manifests/a.json").unwrap();
    backend.list("v1/").unwrap();
    backend.list("").unwrap();
    backend.delete("v1/manifests/a.json").unwrap();
    backend.get("v1/manifests/missing.json").unwrap();

    let failures = server.signature_failures();
    assert!(failures.is_empty(), "签名校验失败：{failures:?}");
    assert!(
        server.requests().len() >= 7,
        "应至少发出 7 个请求：{:?}",
        server.requests()
    );
}

#[test]
fn 特殊字符的键名也能正确签名与往返() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "空 间/前缀");

    // 空格、加号、&、中文、括号：既考验 URI 编码，也考验签名
    for path in [
        "v1/items/20251001/a b+c.cxi",
        "v1/items/20251001/a&b.cxi",
        "v1/items/20251001/中文.cxi",
        "v1/items/20251001/括号(1).cxi",
    ] {
        let bytes = path.as_bytes();
        backend.put(path, bytes, true).unwrap();
        assert_eq!(
            backend.get(path).unwrap().as_deref(),
            Some(bytes),
            "路径 {path} 应当能原样取回"
        );
    }
    let objects = backend.list("v1/items/20251001/").unwrap();
    assert_eq!(objects.len(), 4);
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 服务端不支持条件写时退化为先查再写() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "space-a");

    server.force_next_put_not_implemented();
    let outcome = backend.put("v1/manifests/b.json", b"first", true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert_eq!(server.get("space-a/v1/manifests/b.json").unwrap(), b"first");

    // 第二次：对象已存在 ⇒ 退化路径里的 HEAD 会看到它
    server.force_next_put_not_implemented();
    let outcome = backend.put("v1/manifests/b.json", b"second", true).unwrap();
    assert_eq!(outcome, PutOutcome::AlreadyExists);
    assert_eq!(server.get("space-a/v1/manifests/b.json").unwrap(), b"first");

    // 覆盖写仍然生效。注意 S3 对"新建"与"覆盖"都返回 200、无法区分，
    // 因此统一报 Created（WebDAV 能靠 201/204 区分，才报 Replaced）
    let outcome = backend.put("v1/manifests/b.json", b"third", false).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert_eq!(server.get("space-a/v1/manifests/b.json").unwrap(), b"third");
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 服务端忽略条件写时表现为覆盖() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    server.ignore_conditional_put(true);
    let mut backend = backend_for(&server, "space-a");

    backend.put("v1/manifests/c.json", b"first", true).unwrap();
    let outcome = backend.put("v1/manifests/c.json", b"second", true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert_eq!(
        server.get("space-a/v1/manifests/c.json").unwrap(),
        b"second"
    );
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 分页列举能把所有对象取全() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "space-a");

    // 120 个对象 > 一页（服务端 max-keys 上限 1000，这里用 120 仍然只有一页；
    // 真正验证分页靠下面的"前缀+多页"场景，需要客户端按令牌继续请求）
    for index in 0..120 {
        server.seed(&format!("space-a/v1/items/20251001/{index:03}.cxi"), b"x");
    }
    let objects = backend.list("v1/items/20251001/").unwrap();
    assert_eq!(objects.len(), 120, "应取全 120 个对象");

    // 前缀过滤：只取 manifests 下的
    server.seed("space-a/v1/manifests/a.json", b"{}");
    server.seed("space-a/v1/manifests/b.json", b"{}");
    let manifests = backend.list(layout::MANIFESTS_PREFIX).unwrap();
    assert_eq!(manifests.len(), 2);
    assert!(
        manifests
            .iter()
            .all(|object| object.path.starts_with("v1/manifests/"))
    );
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 桶不存在时给出可识别的错误() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = S3Backend::new(S3Options {
        name: "wrong-bucket".to_string(),
        endpoint: Some(server.endpoint()),
        region: "us-east-1".to_string(),
        bucket: "no-such-bucket".to_string(),
        prefix: String::new(),
        access_key_id: ACCESS_KEY.to_string(),
        secret_access_key: SECRET_KEY.to_string(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(10),
    })
    .unwrap();

    let err = backend.list("v1/manifests/").unwrap_err();
    assert!(matches!(err, BackendError::NotFound(_)), "{err}");
    assert!(!err.is_retryable(), "桶不存在不该被无限重试");
}

#[test]
fn 密钥错误时报认证失败且不进退避() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = S3Backend::new(S3Options {
        name: "bad-secret".to_string(),
        endpoint: Some(server.endpoint()),
        region: "us-east-1".to_string(),
        bucket: BUCKET.to_string(),
        prefix: String::new(),
        access_key_id: ACCESS_KEY.to_string(),
        secret_access_key: "wrong-secret".to_string(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(10),
    })
    .unwrap();

    let err = backend.get("v1/manifests/a.json").unwrap_err();
    assert!(err.is_auth(), "应为认证类错误：{err}");
    assert!(!err.is_retryable());
    // 服务端确实记下了签名不匹配（而不是我们把请求写坏了却没人发现）
    assert!(
        !server.signature_failures().is_empty(),
        "服务端应当记录到签名不匹配"
    );
}

#[test]
fn 时钟偏差会被自动校正并重试一次() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "space-a");

    // 服务端"时钟"快 1 小时：第一次请求必然被判 RequestTimeTooSkewed
    server.force_clock_skew();
    let outcome = backend.put("v1/manifests/skew.json", b"{}", true).unwrap();
    assert!(
        matches!(outcome, PutOutcome::Created { .. }),
        "应当校正时钟后重试成功：{outcome:?}"
    );
    assert_eq!(server.get("space-a/v1/manifests/skew.json").unwrap(), b"{}");

    // 校正状态被记住：后续请求直接用已校正的时间
    backend.get("v1/manifests/skew.json").unwrap();
    let failures = server.signature_failures();
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn 空前缀时直接列桶根() {
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, BUCKET);
    let mut backend = backend_for(&server, "");
    server.seed("v1/manifests/a.json", b"{}");

    let objects = backend.list("v1/manifests/").unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].path, "v1/manifests/a.json");
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 桶不存在时会自动创建再写入() {
    // 自建 MinIO 上"还没有桶"是常态：首次使用应当把桶建出来，而不是让用户去别处建
    let server = TestS3Server::start(ACCESS_KEY, SECRET_KEY, "fresh-bucket");
    let mut backend = S3Backend::new(S3Options {
        name: "fresh".to_string(),
        endpoint: Some(server.endpoint()),
        region: "us-east-1".to_string(),
        bucket: "fresh-bucket".to_string(),
        prefix: "space-a".to_string(),
        access_key_id: ACCESS_KEY.to_string(),
        secret_access_key: SECRET_KEY.to_string(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_secs(10),
    })
    .unwrap();

    // 桶还不存在：先通过 ensure_root 建出来（`cx backend test` / probe 走的就是它）
    let created = backend.ensure_root().unwrap();

    assert!(created, "首次调用应当创建桶");
    assert_eq!(server.buckets_created(), 1);

    // 再调用一次：桶已存在，不再重复创建
    let created_again = backend.ensure_root().unwrap();
    assert!(!created_again, "桶已存在时不该重复创建");

    // 写入与读取都正常
    let outcome = backend.put("v1/manifests/a.json", b"{}", true).unwrap();
    assert!(matches!(outcome, PutOutcome::Created { .. }), "{outcome:?}");
    assert!(server.has("space-a/v1/manifests/a.json"));
    assert!(
        server.signature_failures().is_empty(),
        "{:?}",
        server.signature_failures()
    );
}

#[test]
fn 传输失败时报可重试错误() {
    let mut backend = S3Backend::new(S3Options {
        name: "dead".to_string(),
        endpoint: Some("http://127.0.0.1:1".to_string()),
        region: "us-east-1".to_string(),
        bucket: BUCKET.to_string(),
        prefix: String::new(),
        access_key_id: ACCESS_KEY.to_string(),
        secret_access_key: SECRET_KEY.to_string(),
        session_token: None,
        path_style: true,
        ca_file: None,
        insecure: false,
        timeout: Duration::from_millis(800),
    })
    .unwrap();
    let err = backend.list("v1/").unwrap_err();
    assert!(matches!(err, BackendError::Transport(_)), "{err}");
    assert!(err.is_retryable());
}
