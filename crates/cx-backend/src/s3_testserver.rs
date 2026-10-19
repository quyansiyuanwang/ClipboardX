//! 测试用的极小 S3 兼容服务端（含 **SigV4 签名校验**）。
//!
//! 之所以要单独造一个而不是"随便起个 mock 就完事"：S3 后端最容易错的地方不是
//! HTTP，而是签名与规范化——路径怎么编码、查询串怎么排序、哪些头参与签名、
//! Host 写的是什么。只断言"请求到达了"是抓不住这些错误的，因此这里的服务端
//! **从收到的原始字节重建规范化请求并重算签名**，与客户端给的 `Authorization`
//! 逐字节比对：
//!
//! - 签名不对 → `403 SignatureDoesNotMatch`（与真实 S3 一致）；
//! - 访问密钥对不上 → `403 InvalidAccessKeyId`；
//! - 时间偏差超过 15 分钟 → `403 RequestTimeTooSkewed`（可配置成"第一次故意报错"，
//!   用来验证客户端的时钟校正重试）。
//!
//! 支持的动作：`PUT` / `GET` / `HEAD` / `DELETE` 对象，以及 `GET ?list-type=2`
//! （带 `prefix`、`max-keys`、`continuation-token` 的分页列举）。不支持的都返回
//! `501 NotImplemented`。

use crate::signing::{sha256_hex, signing_key};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 单个对象。
#[derive(Debug, Clone)]
struct Object {
    bytes: Vec<u8>,
    etag: String,
    modified_ms: u64,
}

/// 服务端状态。
#[derive(Debug)]
struct State {
    objects: BTreeMap<String, Object>,
    /// 期望的访问密钥 id。
    access_key_id: String,
    /// 期望的私有密钥。
    secret_access_key: String,
    /// 桶名（不匹配返回 404 NoSuchBucket）。
    bucket: String,
    /// 区域（签名里的 scope 必须一致）。
    region: String,
    /// 是否忽略 `If-None-Match`（模拟不支持条件写的实现）。
    ignore_conditional_put: bool,
    /// 阿里云 OSS 模式：**拒绝** `If-None-Match`（400 NotImplemented，与真实 OSS 的
    /// 回应逐字相同），只认 `x-oss-forbid-overwrite: true`，冲突时返回
    /// 409 FileAlreadyExists。CI 靠它回归"OSS 上写入全失败"那类 bug，不需要真桶。
    oss_mode: bool,
    /// 下一次 PUT 强制返回 501（验证客户端退化路径）。
    force_next_put_501: bool,
    /// 下一次请求强制返回 `RequestTimeTooSkewed`（验证时钟校正重试）。
    force_next_skew_error: bool,
    /// 请求记录（断言用）：`METHOD 路径?查询`。
    requests: Vec<String>,
    /// 签名校验失败的次数（应为 0）。
    signature_failures: Vec<String>,
    /// 服务端自己的时钟偏移（模拟"服务器时间比本机快"，用于验证客户端的时钟校正）
    skew_offset_ms: i64,
    /// 被创建过几次桶（断言 ensure_root 用过）
    buckets_created: u32,
    /// 桶当前是否存在（起始为"还没有桶"，与自建 MinIO 的首次使用一致）
    bucket_exists: bool,
    clock: u64,
}

/// 测试服务端句柄。
#[derive(Debug)]
pub struct TestS3Server {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestS3Server {
    /// 启动（随机端口）。
    pub fn start(access_key_id: &str, secret_access_key: &str, bucket: &str) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("无法绑定测试端口");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            objects: BTreeMap::new(),
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
            bucket: bucket.to_string(),
            region: "us-east-1".to_string(),
            ignore_conditional_put: false,
            oss_mode: false,
            force_next_put_501: false,
            force_next_skew_error: false,
            requests: Vec::new(),
            signature_failures: Vec::new(),
            skew_offset_ms: 0,
            buckets_created: 0,
            bucket_exists: true,
            clock: 1_759_323_998_000,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread = {
            let state = state.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("test-s3".to_string())
                .spawn(move || {
                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let state = state.clone();
                                std::thread::spawn(move || {
                                    let _ = handle(stream, state);
                                });
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
                .expect("无法启动测试服务端线程")
        };

        Self {
            addr,
            state,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Control initial bucket existence without issuing a cloud-like mutation.
    pub fn set_bucket_exists(&self, exists: bool) {
        self.state.lock().unwrap().bucket_exists = exists;
    }

    /// 端点 URL（`http://127.0.0.1:port`）。
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 让服务端忽略 `If-None-Match`。
    pub fn ignore_conditional_put(&self, value: bool) {
        self.state.lock().unwrap().ignore_conditional_put = value;
    }

    /// 打开阿里云 OSS 模式（拒绝 If-None-Match，只认 x-oss-forbid-overwrite）。
    pub fn oss_mode(&self, value: bool) {
        self.state.lock().unwrap().oss_mode = value;
    }

    /// 下一次 PUT 返回 501（测退化路径）。
    pub fn force_next_put_not_implemented(&self) {
        self.state.lock().unwrap().force_next_put_501 = true;
    }

    /// 让服务端"时钟"偏移 1 小时：第一次请求会被判为时间偏差过大，
    /// 客户端应当用响应里的 `Date` 校正后重试成功。
    pub fn force_clock_skew(&self) {
        let mut state = self.state.lock().unwrap();
        state.force_next_skew_error = true;
        state.skew_offset_ms = 3_600_000;
    }

    /// 直接放置对象。
    pub fn seed(&self, key: &str, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.bucket_exists = true;
        state.clock += 1;
        let clock = state.clock;
        state.objects.insert(
            key.trim_start_matches('/').to_string(),
            Object {
                bytes: bytes.to_vec(),
                etag: format!("\"{}\"", etag_hash(bytes)),
                modified_ms: clock,
            },
        );
    }

    /// 读取对象。
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(key.trim_start_matches('/'))
            .map(|object| object.bytes.clone())
    }

    /// 对象是否存在。
    pub fn has(&self, key: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .objects
            .contains_key(key.trim_start_matches('/'))
    }

    /// 对象数量。
    pub fn object_count(&self) -> usize {
        self.state.lock().unwrap().objects.len()
    }

    /// 收到的请求序列。
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// 桶被创建过几次。
    pub fn buckets_created(&self) -> u32 {
        self.state.lock().unwrap().buckets_created
    }

    /// 签名校验失败记录（必须为空）。
    pub fn signature_failures(&self) -> Vec<String> {
        self.state.lock().unwrap().signature_failures.clone()
    }

    /// 停止。
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TestS3Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 请求的解析结果。
struct RawRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn handle(stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let request = RawRequest {
        method,
        path,
        query,
        headers,
        body,
    };

    let (status, response_headers, payload) = {
        let mut guard = state.lock().unwrap();
        guard.requests.push(format!(
            "{} {}{}",
            request.method,
            request.path,
            if request.query.is_empty() {
                String::new()
            } else {
                format!("?{}", request.query)
            }
        ));
        dispatch(&request, &mut guard)
    };

    let server_now_ms = {
        let guard = state.lock().unwrap();
        guard.skew_offset_ms + cx_core::now_ms() as i64
    };
    let mut response = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    response.push_str(&format!("Date: {}\r\n", http_date(server_now_ms)));
    for (name, value) in &response_headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    // 响应头里可能已经带了 Content-Length（HEAD 要报对象长度）；重复添加会让
    // HTTP 客户端直接判定响应非法
    if !response_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        response.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    response.push_str("Connection: close\r\n\r\n");
    writer.write_all(response.as_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn dispatch(request: &RawRequest, state: &mut State) -> (u16, Vec<(String, String)>, Vec<u8>) {
    if state.force_next_skew_error {
        state.force_next_skew_error = false;
        return error_response(
            403,
            "RequestTimeTooSkewed",
            "The difference between the request time and the current time is too large.",
        );
    }

    // ---- 签名校验 ----
    if let Err((code, message, detail)) = verify_signature(request, state) {
        state.signature_failures.push(detail);
        return error_response(403, code, message);
    }

    // ---- 路由 ----
    let key = decode_path(&request.path);
    let key = key.trim_start_matches('/');
    // path-style：/bucket/key
    let (bucket, object_key) = match key.split_once('/') {
        Some((bucket, rest)) => (bucket.to_string(), rest.to_string()),
        None => (key.to_string(), String::new()),
    };
    // 建桶请求（PUT /bucket/）本身当然要求"桶还不存在"，不能先被下面这句拦掉
    let creating_bucket = request.method == "PUT" && object_key.is_empty();
    if bucket != state.bucket {
        return error_response(404, "NoSuchBucket", "The specified bucket does not exist");
    }
    if !state.bucket_exists && !creating_bucket {
        return error_response(404, "NoSuchBucket", "The specified bucket does not exist");
    }

    match request.method.as_str() {
        "GET" if request.query.contains("list-type=2") => list_objects(request, state),
        "GET" => match state.objects.get(&object_key) {
            Some(object) => (
                200,
                vec![
                    ("ETag".to_string(), object.etag.clone()),
                    (
                        "Content-Type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                ],
                object.bytes.clone(),
            ),
            None => error_response(404, "NoSuchKey", "The specified key does not exist."),
        },
        "HEAD" => match state.objects.get(&object_key) {
            Some(object) => (
                200,
                vec![
                    ("ETag".to_string(), object.etag.clone()),
                    ("Content-Length".to_string(), object.bytes.len().to_string()),
                ],
                Vec::new(),
            ),
            None => (404, Vec::new(), Vec::new()),
        },
        "PUT" if object_key.is_empty() => {
            // 建桶（PUT /bucket/）：首次成功，重复创建返回 409
            if state.bucket_exists {
                return error_response(
                    409,
                    "BucketAlreadyOwnedByYou",
                    "Your previous request to create the named bucket succeeded and you already own it.",
                );
            }
            state.bucket_exists = true;
            state.buckets_created += 1;
            (200, Vec::new(), Vec::new())
        }
        "PUT" => {
            if state.force_next_put_501 {
                state.force_next_put_501 = false;
                return error_response(
                    501,
                    "NotImplemented",
                    "conditional writes are not supported",
                );
            }
            // 阿里云 OSS 模式：不认 `If-None-Match`（真实 OSS 的答复就是这句），
            // 只认 `x-oss-forbid-overwrite`；冲突用 409 FileAlreadyExists。
            // CI 靠它回归"OSS 上写入全失败"那一类 bug——不需要真实桶。
            if state.oss_mode {
                if request.header("if-none-match").is_some() {
                    return error_response(
                        400,
                        "NotImplemented",
                        "A header you provided implies functionality that is not implemented.",
                    );
                }
                if request.header("x-oss-forbid-overwrite") == Some("true")
                    && state.objects.contains_key(&object_key)
                {
                    return error_response(
                        409,
                        "FileAlreadyExists",
                        "The object you specified already exists and can not be overwritten.",
                    );
                }
                state.clock += 1;
                let clock = state.clock;
                let etag = format!("\"{}\"", etag_hash(&request.body));
                state.objects.insert(
                    object_key.clone(),
                    Object {
                        bytes: request.body.clone(),
                        etag: etag.clone(),
                        modified_ms: clock,
                    },
                );
                return (200, vec![("ETag".to_string(), etag)], Vec::new());
            }
            let conditional =
                request.header("if-none-match") == Some("*") && !state.ignore_conditional_put;
            if conditional && state.objects.contains_key(&object_key) {
                return error_response(
                    412,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                );
            }
            state.clock += 1;
            let clock = state.clock;
            let etag = format!("\"{}\"", etag_hash(&request.body));
            state.objects.insert(
                object_key.clone(),
                Object {
                    bytes: request.body.clone(),
                    etag: etag.clone(),
                    modified_ms: clock,
                },
            );
            (200, vec![("ETag".to_string(), etag)], Vec::new())
        }
        "DELETE" => {
            state.objects.remove(&object_key);
            (204, Vec::new(), Vec::new())
        }
        _ => error_response(501, "NotImplemented", "method not implemented"),
    }
}

/// 从原始请求重建规范化请求并校验签名。
fn verify_signature(
    request: &RawRequest,
    state: &State,
) -> Result<(), (&'static str, &'static str, String)> {
    let Some(authorization) = request.header("authorization") else {
        return Err((
            "AccessDenied",
            "Request lacks a valid signature",
            "缺少 Authorization 头".to_string(),
        ));
    };
    let fields = parse_authorization(authorization).ok_or((
        "AuthorizationHeaderMalformed",
        "The authorization header is malformed",
        format!("无法解析 Authorization：{authorization}"),
    ))?;

    if fields.credential_key != state.access_key_id {
        return Err((
            "InvalidAccessKeyId",
            "The AWS Access Key Id you provided does not exist in our records.",
            format!("访问密钥不匹配：{}", fields.credential_key),
        ));
    }
    if fields.region != state.region {
        return Err((
            "AuthorizationHeaderMalformed",
            "The authorization header is malformed; the region is wrong.",
            format!("区域不匹配：{} != {}", fields.region, state.region),
        ));
    }

    // 时间偏差检查（真实 S3 是 15 分钟）
    let amz_date = request.header("x-amz-date").unwrap_or_default();

    if let Some(request_ms) = parse_amz_date(amz_date)
        && (request_ms as i64 - state.now_ms()).abs() > 15 * 60 * 1000
    {
        return Err((
            "RequestTimeTooSkewed",
            "The difference between the request time and the current time is too large.",
            format!("时间偏差过大：{amz_date}"),
        ));
    }

    // 规范化请求：由收到的原始数据重建（不使用客户端自己的结构）
    let canonical_query = canonical_query_from_raw(&request.query);
    let payload_hash = sha256_hex(&request.body);
    if let Some(sent) = request.header("x-amz-content-sha256")
        && sent != payload_hash
    {
        return Err((
            "XAmzContentSHA256Mismatch",
            "The provided 'x-amz-content-sha256' header does not match what was computed.",
            format!("内容哈希不匹配：{sent} != {payload_hash}"),
        ));
    }

    let mut canonical_headers = String::new();
    for name in fields.signed_headers.iter() {
        let value = if name == "host" {
            request.header("host").unwrap_or_default().to_string()
        } else {
            let Some(value) = request.header(name) else {
                return Err((
                    "SignatureDoesNotMatch",
                    "The request signature we calculated does not match the signature you provided.",
                    format!("签名声明了 {name}，但请求里没有这个头"),
                ));
            };
            value.trim().to_string()
        };
        canonical_headers.push_str(&format!("{name}:{value}\n"));
    }
    let signed = fields.signed_headers.join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method, request.path, canonical_query, canonical_headers, signed, payload_hash
    );

    let scope = format!(
        "{}/{}/{}/aws4_request",
        fields.date, fields.region, fields.service
    );
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        fields.algorithm,
        amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes())
    );
    let key = signing_key(
        &state.secret_access_key,
        &fields.date,
        &fields.region,
        &fields.service,
    )
    .map_err(|_| {
        (
            "InternalError",
            "signing key derivation failed",
            "派生密钥失败".to_string(),
        )
    })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC 密钥长度非法");
    mac.update(string_to_sign.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());

    if expected != fields.signature {
        return Err((
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
            format!(
                "签名不匹配\n期望 {expected}\n实际 {}\n规范化请求：\n{canonical_request}",
                fields.signature
            ),
        ));
    }
    Ok(())
}

/// Authorization 头拆解结果。
struct AuthFields {
    algorithm: String,
    credential_key: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(value: &str) -> Option<AuthFields> {
    let rest = value.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("Credential=") {
            credential = Some(value.to_string());
        } else if let Some(value) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(value.to_string());
        } else if let Some(value) = part.strip_prefix("Signature=") {
            signature = Some(value.to_string());
        }
    }
    let credential = credential?;
    let mut pieces = credential.split('/');
    let credential_key = pieces.next()?.to_string();
    let date = pieces.next()?.to_string();
    let region = pieces.next()?.to_string();
    let service = pieces.next()?.to_string();
    Some(AuthFields {
        algorithm: "AWS4-HMAC-SHA256".to_string(),
        credential_key,
        date,
        region,
        service,
        signed_headers: signed_headers?
            .split(';')
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect(),
        signature: signature?,
    })
}

/// 服务端维护的"当前时间"（含模拟出来的时钟偏移）。
impl State {
    fn now_ms(&self) -> i64 {
        cx_core::now_ms() as i64 + self.skew_offset_ms
    }
}

/// 由原始查询串重建规范化查询串（解码后重新编码并排序）。
fn canonical_query_from_raw(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = raw
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (decode_path(name), decode_path(value)),
            None => (decode_path(pair), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{}={}", encode_query(&name), encode_query(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_query(text: &str) -> String {
    crate::signing::uri_encode(text, true)
}

/// 百分号解码。
fn decode_path(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `YYYYMMDDTHHMMSSZ` → 毫秒。
fn parse_amz_date(text: &str) -> Option<u64> {
    if text.len() != 16 || !text.ends_with('Z') {
        return None;
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: u32 = text[4..6].parse().ok()?;
    let day: u32 = text[6..8].parse().ok()?;
    let hour: i64 = text[9..11].parse().ok()?;
    let minute: i64 = text[11..13].parse().ok()?;
    let second: i64 = text[13..15].parse().ok()?;
    let days = cx_proto::civil::days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some((seconds * 1000) as u64)
}

fn list_objects(request: &RawRequest, state: &mut State) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let params: BTreeMap<String, String> = request
        .query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.to_string(), decode_path(value)))
        .collect();

    let prefix = params.get("prefix").cloned().unwrap_or_default();
    let max_keys: usize = params
        .get("max-keys")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1000);
    let token = params.get("continuation-token").cloned();

    // 键名有序：S3 保证按字典序返回，分页游标就是"上个分页之后的第一个键"
    let keys: Vec<&String> = state
        .objects
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .collect();
    let start = match &token {
        Some(token) => keys
            .iter()
            .position(|key| key.as_str() > token.as_str())
            .unwrap_or(keys.len()),
        None => 0,
    };
    let page: Vec<&String> = keys.iter().skip(start).take(max_keys).copied().collect();
    let truncated = start + page.len() < keys.len();
    let next_token = if truncated {
        page.last().map(|key| (*key).clone())
    } else {
        None
    };

    let mut body = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    );
    body.push_str(&format!("<Name>{}</Name>", state.bucket));
    body.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(&prefix)));
    body.push_str(&format!("<KeyCount>{}</KeyCount>", page.len()));
    body.push_str(&format!("<MaxKeys>{max_keys}</MaxKeys>"));
    body.push_str(&format!(
        "<IsTruncated>{}</IsTruncated>",
        if truncated { "true" } else { "false" }
    ));
    if let Some(token) = &next_token {
        body.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(token)
        ));
    }
    for key in page {
        let object = state.objects.get(key).expect("键来自同一张表");
        let (year, month, day) =
            cx_proto::civil::civil_from_days((object.modified_ms / 86_400_000) as i64);
        let rest = (object.modified_ms / 1000) % 86_400;
        body.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z</LastModified><ETag>{}</ETag><Size>{}</Size></Contents>",
            xml_escape(key),
            rest / 3600,
            (rest % 3600) / 60,
            rest % 60,
            xml_escape(&object.etag),
            object.bytes.len()
        ));
    }
    body.push_str("</ListBucketResult>");
    (
        200,
        vec![("Content-Type".to_string(), "application/xml".to_string())],
        body.into_bytes(),
    )
}

fn error_response(status: u16, code: &str, message: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>{code}</Code><Message>{}</Message></Error>"#,
        xml_escape(message)
    );
    (
        status,
        vec![("Content-Type".to_string(), "application/xml".to_string())],
        body.into_bytes(),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        412 => "Precondition Failed",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

/// 对象标识（放进 ETag）。
///
/// 真实 S3 的 ETag 是内容 MD5；我们的客户端只把它当作"内容变没变"的比较值，
/// 因此这里用 SHA256 的前 16 字节（同样是 32 位十六进制、同样由内容唯一决定），
/// 省掉为测试手写 MD5 的常数表，也避开浮点数生成常数带来的跨平台不确定性。
fn etag_hash(bytes: &[u8]) -> String {
    sha256_hex(bytes)[..32].to_string()
}
/// 毫秒 → RFC 1123（响应 `Date` 头）。
fn http_date(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = cx_proto::civil::civil_from_days(days);
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(month as usize).clamp(1, 12) - 1],
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_由内容决定且形如_md5() {
        let one = etag_hash(b"abc");
        assert_eq!(one.len(), 32);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(one, etag_hash(b"abc"));
        assert_ne!(one, etag_hash(b"abd"));
    }

    #[test]
    fn 解析_authorization_头() {
        let value = "AWS4-HMAC-SHA256 Credential=minioadmin/20251014/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=deadbeef";
        let fields = parse_authorization(value).unwrap();
        assert_eq!(fields.credential_key, "minioadmin");
        assert_eq!(fields.date, "20251014");
        assert_eq!(fields.region, "us-east-1");
        assert_eq!(fields.service, "s3");
        assert_eq!(fields.signature, "deadbeef");
        assert_eq!(
            fields.signed_headers,
            vec!["host", "x-amz-content-sha256", "x-amz-date"]
        );
        assert!(parse_authorization("garbage").is_none());
    }

    #[test]
    fn 规范化查询串会排序并重新编码() {
        assert_eq!(
            canonical_query_from_raw("list-type=2&max-keys=1000&prefix=a%2Fb"),
            "list-type=2&max-keys=1000&prefix=a%2Fb"
        );
        assert_eq!(canonical_query_from_raw("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query_from_raw("prefix=a b"), "prefix=a%20b");
        assert_eq!(canonical_query_from_raw(""), "");
    }

    #[test]
    fn amz_date_解析() {
        assert_eq!(parse_amz_date("20251001T130638Z"), Some(1_759_323_998_000));
        assert!(parse_amz_date("2025-10-01").is_none());
    }
}
