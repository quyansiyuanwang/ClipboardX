//! 极小的 HTTP/1.1 服务端与路由。
//!
//! 为什么手写而不是引 `tiny_http`：路由、鉴权、长轮询这些业务逻辑都要自己写，
//! 框架只省下请求行解析（几十行），却要额外背一个依赖。更关键的是——请求体一律
//! 走 `Content-Length`（我们自己的客户端就是这么发的），不需要 chunked 支持，
//! 手写反而更可控。不支持的形态一律明确报错，不猜。
//!
//! 线程模型与客户端一致：阻塞 + 线程，不用 async 运行时。每个连接一个线程，
//! 用原子计数限制并发上限，超了直接回 503 而不是把文件描述符耗光。

use crate::error::ServerError;
use crate::store::{ListQuery, PutResult, SpaceMeta, Store};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// 请求头上限（防止内存被撑爆）。
const MAX_HEADER_BYTES: usize = 32 * 1024;
/// 请求体上限（与对象上限一致，再加一点余量给 JSON 信封）。
const MAX_BODY_BYTES: usize = 17 * 1024 * 1024;
/// 默认并发连接上限。
pub const DEFAULT_MAX_CONNECTIONS: usize = 128;
/// 长轮询默认上限（秒）。
pub const DEFAULT_LONG_POLL_SECS: u64 = 25;

/// 服务端配置。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 监听地址。
    pub listen: SocketAddr,
    /// 数据目录。
    pub data_dir: PathBuf,
    /// 并发连接上限。
    pub max_connections: usize,
    /// 长轮询保持上限。
    pub long_poll_max: Duration,
}

impl ServerConfig {
    /// 用监听地址与数据目录构造（其余取默认值）。
    pub fn new(listen: SocketAddr, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            listen,
            data_dir: data_dir.into(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            long_poll_max: Duration::from_secs(DEFAULT_LONG_POLL_SECS),
        }
    }
}

/// 服务端句柄。
pub struct Server {
    listener: TcpListener,
    config: ServerConfig,
    store: Arc<Store>,
    live: Arc<AtomicUsize>,
}

/// 已解析的请求。
#[derive(Debug, Clone)]
pub struct Request {
    /// 方法（大写）。
    pub method: String,
    /// 路径（已百分号解码，无前导 `/` 的变成 `""`）。
    pub path: String,
    /// 查询参数（已解码）。
    pub query: BTreeMap<String, String>,
    /// 请求头（名字小写）。
    pub headers: BTreeMap<String, String>,
    /// 请求体。
    pub body: Vec<u8>,
}

impl Request {
    /// 取请求头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|value| value.as_str())
    }

    /// 取查询参数。
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(|value| value.as_str())
    }

    /// 路径的第一个片段（`v1/objects/a` → `v1`）。
    fn segment(&self, index: usize) -> Option<&str> {
        self.path
            .split('/')
            .nth(index)
            .filter(|part| !part.is_empty())
    }

    /// 取从第 `index` 段开始的剩余路径。
    fn rest_after(&self, index: usize) -> String {
        self.path
            .split('/')
            .skip(index)
            .collect::<Vec<_>>()
            .join("/")
            .trim_start_matches('/')
            .to_string()
    }
}

/// 响应。
#[derive(Debug, Clone)]
pub struct Response {
    /// 状态码。
    pub status: u16,
    /// 响应头。
    pub headers: Vec<(String, String)>,
    /// 响应体。
    pub body: Vec<u8>,
}

impl Response {
    /// 空响应。
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// JSON 响应。
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    /// 文本响应。
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body: body.into().into_bytes(),
        }
    }

    /// 带一个响应头。
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// 由错误构造响应。
    pub fn error(err: &ServerError) -> Self {
        Response::json(
            err.status(),
            json!({
                "error": err.kind(),
                "message": err.to_string(),
            }),
        )
    }
}

impl Server {
    /// 绑定监听（不阻塞）。
    pub fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let store = Arc::new(Store::open(&config.data_dir)?);
        let listener = TcpListener::bind(config.listen)
            .map_err(|err| ServerError::io(config.listen.to_string(), err))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| ServerError::io("listener", err))?;
        Ok(Self {
            listener,
            config,
            store,
            live: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 实际监听的地址（端口为 0 时可取回随机端口）。
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        self.listener
            .local_addr()
            .map_err(|err| ServerError::io("listener", err))
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &std::path::Path {
        &self.config.data_dir
    }

    /// 配置（测试与后台任务用）。
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// 存储句柄（测试与后台任务用）。
    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    /// 阻塞式服务循环，直到 `shutdown` 被置位。
    pub fn serve(self, shutdown: Arc<AtomicBool>) -> Result<(), ServerError> {
        log::info!(
            "cx-server 监听 {}（数据目录 {}）",
            self.local_addr()?,
            self.config.data_dir.display()
        );
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let live = self.live.fetch_add(1, Ordering::SeqCst);
                    if live >= self.config.max_connections {
                        self.live.fetch_sub(1, Ordering::SeqCst);
                        let mut stream = stream;
                        let response = Response::text(503, "服务端连接数已满，请稍后重试");
                        let _ = write_response(&mut stream, &response);
                        continue;
                    }
                    let store = self.store.clone();
                    let config = self.config.clone();
                    let live = self.live.clone();
                    let shutdown = shutdown.clone();
                    std::thread::Builder::new()
                        .name("cx-server-conn".to_string())
                        .spawn(move || {
                            let mut stream = stream;
                            let _ = handle_connection(&mut stream, &store, &config, &shutdown);
                            live.fetch_sub(1, Ordering::SeqCst);
                            log::debug!("连接结束：{peer}");
                        })
                        .ok();
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    log::warn!("接受连接失败：{err}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        log::info!("cx-server 已停止");
        Ok(())
    }
}

/// 处理一个连接（可能包含多个请求，但本服务端每次响应后关闭）。
pub fn handle_connection(
    stream: &mut TcpStream,
    store: &Store,
    config: &ServerConfig,
    shutdown: &AtomicBool,
) -> Result<(), ServerError> {
    stream
        .set_nonblocking(false)
        .map_err(|err| ServerError::io("stream", err))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| ServerError::io("stream", err))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| ServerError::io("stream", err))?,
    );

    let request = match read_request(&mut reader) {
        Ok(request) => request,
        Err(err) => {
            let response = Response::error(&err);
            return write_response(stream, &response);
        }
    };
    log::debug!("{} /{}", request.method, request.path);

    let response = match route(store, config, &request, shutdown) {
        Ok(response) => response,
        Err(err) => {
            if err.status() >= 500 {
                log::warn!("{} /{} 失败：{err}", request.method, request.path);
            } else {
                log::debug!("{} /{} → {}", request.method, request.path, err.status());
            }
            Response::error(&err)
        }
    };
    write_response(stream, &response)
}

/// 读一个请求。
fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Request, ServerError> {
    let mut request_line = String::new();
    let read = reader
        .read_line(&mut request_line)
        .map_err(|err| ServerError::io("request", err))?;
    if read == 0 {
        return Err(ServerError::Http {
            status: 400,
            detail: "空请求".to_string(),
        });
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    if method.is_empty() || !version.starts_with("HTTP/1.") {
        return Err(ServerError::Http {
            status: 400,
            detail: format!("请求行非法：{}", request_line.trim()),
        });
    }

    let mut headers = BTreeMap::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|err| ServerError::io("header", err))?;
        if read == 0 {
            break;
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ServerError::Http {
                status: 431,
                detail: "请求头过大".to_string(),
            });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return Err(ServerError::Http {
            status: 411,
            detail: "本服务端不支持 chunked 请求体，请带 Content-Length".to_string(),
        });
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(ServerError::TooLarge(format!(
            "请求体 {length} 字节超过上限 {MAX_BODY_BYTES}"
        )));
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|err| ServerError::io("body", err))?;
    }

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let mut query = BTreeMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (decode(name), decode(value)),
            None => (decode(pair), String::new()),
        };
        query.insert(name, value);
    }

    Ok(Request {
        method,
        path: decode(raw_path.trim_start_matches('/')),
        query,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: &Response) -> Result<(), ServerError> {
    let mut text = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    for (name, value) in &response.headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    text.push_str("Server: cx-server\r\n");
    // 每次响应后关连接：省掉 keep-alive 状态机，客户端（attohttpc）能正确处理
    text.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(text.as_bytes())
        .map_err(|err| ServerError::io("response", err))?;
    stream
        .write_all(&response.body)
        .map_err(|err| ServerError::io("response", err))?;
    stream
        .flush()
        .map_err(|err| ServerError::io("response", err))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Unknown",
    }
}

/// 路由（不碰 socket，便于直接单测）。
pub fn route(
    store: &Store,
    config: &ServerConfig,
    request: &Request,
    shutdown: &AtomicBool,
) -> Result<Response, ServerError> {
    // 无需鉴权的探针
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "healthz") => return Ok(Response::text(200, "ok")),
        ("GET", "v1/version") => {
            return Ok(Response::json(
                200,
                json!({
                    "name": "cx-server",
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": cx_core::CONTROL_PROTOCOL_VERSION,
                    "store_layout": cx_core::STORE_LAYOUT_VERSION,
                    "long_poll_max_secs": config.long_poll_max.as_secs(),
                    "features": ["objects", "manifests", "items", "long-poll", "quota", "enroll"],
                    "sealed_key": false,
                }),
            ));
        }
        _ => {}
    }

    // 注册：用注册密钥换设备令牌
    if request.method == "POST" && request.path == "v1/enroll" {
        let payload: serde_json::Value = serde_json::from_slice(&request.body)
            .map_err(|err| ServerError::BadRequest(format!("enroll 请求体不是 JSON：{err}")))?;
        let space_id = payload
            .get("space_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ServerError::BadRequest("缺少 space_id".to_string()))?;
        let enroll_secret = payload
            .get("enroll_secret")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ServerError::BadRequest("缺少 enroll_secret".to_string()))?;
        let device_name = payload
            .get("device_name")
            .and_then(|value| value.as_str())
            .unwrap_or("未命名设备");
        let pubkey = payload
            .get("device_pubkey")
            .and_then(|value| value.as_str());
        let token = store.enroll(space_id, enroll_secret, device_name, pubkey)?;
        log::info!("设备 {device_name} 注册成功（空间 {space_id}）");
        return Ok(Response::json(
            201,
            json!({ "device_token": token, "space_id": space_id }),
        ));
    }

    // 其余接口都要令牌
    let token = bearer(request).ok_or_else(|| {
        ServerError::Unauthorized("缺少 Authorization: Bearer <设备令牌>".to_string())
    })?;
    let Some((space, device)) = store.authorize(&token)? else {
        return Err(ServerError::Unauthorized(
            "设备令牌无效或已被吊销".to_string(),
        ));
    };
    store.touch_device(&space.id, &device.token_hash);

    match request.segment(0) {
        Some("v1") => match request.segment(1) {
            Some("spaces") => handle_spaces(store, request, &space),
            Some("objects") => handle_objects(store, request, &space),
            Some("manifests") => handle_collection(store, request, &space, "v1/manifests/"),
            Some("items") => {
                if request.path == "v1/items" && request.method == "GET" {
                    handle_items_long_poll(store, config, request, &space, shutdown)
                } else {
                    handle_collection(store, request, &space, "v1/items/")
                }
            }
            _ => Err(ServerError::NotFound(format!(
                "/{} 没有这个接口",
                request.path
            ))),
        },
        _ => Err(ServerError::NotFound(format!(
            "/{} 没有这个接口",
            request.path
        ))),
    }
}

/// `/v1/spaces/...`
fn handle_spaces(
    store: &Store,
    request: &Request,
    space: &SpaceMeta,
) -> Result<Response, ServerError> {
    let id = request.rest_after(2);
    let (id, suffix) = match id.split_once('/') {
        Some((id, suffix)) => (id.to_string(), suffix.to_string()),
        None => (id, String::new()),
    };
    if id != space.id {
        return Err(ServerError::Forbidden("该令牌不属于这个空间".to_string()));
    }
    match (request.method.as_str(), suffix.as_str()) {
        ("GET", "") => Ok(Response::json(
            200,
            json!({
                "space_id": space.id,
                "name": space.name,
                "created_ms": space.created_ms,
                "quota_bytes": space.quota_bytes,
                "used_bytes": store.used_bytes(&space.id)?,
                "retention_days": space.retention_days,
                "enroll_enabled": space.enroll_secret_hash.is_some(),
            }),
        )),
        ("GET", "key") => Err(ServerError::Http {
            status: 501,
            detail: "密封下发空间密钥尚未实现；请用 `cx space join` 与口令的方式（见 docs/protocol.md §5）"
                .to_string(),
        }),
        _ => Err(method_not_allowed(request)),
    }
}

/// `/v1/objects...`：通用对象接口（客户端实际使用的就是它）。
fn handle_objects(
    store: &Store,
    request: &Request,
    space: &SpaceMeta,
) -> Result<Response, ServerError> {
    if request.path == "v1/objects" {
        if request.method != "GET" {
            return Err(method_not_allowed(request));
        }
        return list_objects(
            store,
            space,
            request.param("prefix").unwrap_or(""),
            request.param("after"),
            request.param("limit"),
        );
    }
    let path = request.rest_after(2);
    if path.is_empty() {
        return Err(ServerError::BadRequest("缺少对象路径".to_string()));
    }
    object_op(store, space, request, &path)
}

/// `/v1/manifests` 与 `/v1/items/...`：把文档里的专用路径映到通用实现上。
fn handle_collection(
    store: &Store,
    request: &Request,
    space: &SpaceMeta,
    prefix: &str,
) -> Result<Response, ServerError> {
    let relative = request.rest_after(2);
    if relative.is_empty() {
        if request.method != "GET" {
            return Err(method_not_allowed(request));
        }
        return list_objects(
            store,
            space,
            prefix,
            request.param("after"),
            request.param("limit"),
        );
    }
    let path = format!("{prefix}{relative}");
    object_op(store, space, request, &path)
}

/// 单个对象的增删读写。
fn object_op(
    store: &Store,
    space: &SpaceMeta,
    request: &Request,
    path: &str,
) -> Result<Response, ServerError> {
    match request.method.as_str() {
        "GET" | "HEAD" => {
            let Some((bytes, meta)) = store.get_object(&space.id, path)? else {
                return Err(ServerError::NotFound(format!("对象不存在：{path}")));
            };
            // If-None-Match：客户端靠它做增量判断
            if request.header("if-none-match") == Some(meta.etag.as_str()) {
                return Ok(Response::empty(304).header("ETag", meta.etag.clone()));
            }
            let body = if request.method == "HEAD" {
                Vec::new()
            } else {
                bytes
            };
            let mut response = Response {
                status: 200,
                headers: vec![
                    (
                        "Content-Type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                    ("ETag".to_string(), meta.etag.clone()),
                ],
                body,
            };
            // HEAD 也要带上真实长度（与 S3/WebDAV 一致）
            if request.method == "HEAD" {
                response
                    .headers
                    .push(("Content-Length-Hint".to_string(), meta.size.to_string()));
            }
            Ok(response)
        }
        "PUT" => {
            let only_if_absent = request.header("if-none-match") == Some("*");
            match store.put_object(&space.id, path, &request.body, only_if_absent)? {
                PutResult::AlreadyExists => Err(ServerError::Http {
                    status: 412,
                    detail: format!("对象已存在：{path}"),
                }),
                PutResult::Created { etag } => Ok(Response::empty(201).header("ETag", etag)),
                PutResult::Replaced { etag } => Ok(Response::empty(200).header("ETag", etag)),
            }
        }
        "DELETE" => {
            store.delete_object(&space.id, path)?;
            Ok(Response::empty(204))
        }
        _ => Err(method_not_allowed(request)),
    }
}

/// 列举对象。
fn list_objects(
    store: &Store,
    space: &SpaceMeta,
    prefix: &str,
    after: Option<&str>,
    limit: Option<&str>,
) -> Result<Response, ServerError> {
    let query = ListQuery {
        prefix: prefix.trim_start_matches('/').to_string(),
        after: after.map(|value| value.trim_start_matches('/').to_string()),
        limit: limit
            .and_then(|value| value.parse().ok())
            .unwrap_or(1000)
            .min(5000),
    };
    let objects = store.list_objects(&space.id, &query)?;
    let items: Vec<serde_json::Value> = objects
        .iter()
        .map(|meta| {
            json!({
                "name": meta.path,
                "etag": meta.etag,
                "size": meta.size,
                "created_ms": meta.created_ms,
            })
        })
        .collect();
    Ok(Response::json(
        200,
        json!({ "objects": items, "count": objects.len() }),
    ))
}

/// 长轮询：`GET /v1/items?since=<ms>&wait=<秒>`。
fn handle_items_long_poll(
    store: &Store,
    config: &ServerConfig,
    request: &Request,
    space: &SpaceMeta,
    shutdown: &AtomicBool,
) -> Result<Response, ServerError> {
    let since: u64 = request
        .param("since")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let wait = request
        .param("wait")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .min(config.long_poll_max.as_secs());
    let prefix = request.param("prefix").unwrap_or("v1/items/").to_string();

    let collect = |since: u64| -> Result<Vec<serde_json::Value>, ServerError> {
        let objects = store.list_objects(
            &space.id,
            &ListQuery {
                prefix: prefix.clone(),
                after: None,
                limit: 5000,
            },
        )?;
        Ok(objects
            .into_iter()
            .filter(|meta| meta.created_ms > since)
            .map(|meta| {
                json!({
                    "name": meta.path,
                    "etag": meta.etag,
                    "size": meta.size,
                    "created_ms": meta.created_ms,
                })
            })
            .collect())
    };

    let mut items = collect(since)?;
    if items.is_empty() && wait > 0 {
        let deadline = Instant::now() + Duration::from_secs(wait);
        while items.is_empty() && Instant::now() < deadline && !shutdown.load(Ordering::Relaxed) {
            let revision = store.revision();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            store.wait_for_change(revision, remaining.min(Duration::from_secs(1)));
            items = collect(since)?;
        }
    }
    let cursor = items
        .iter()
        .filter_map(|item| item.get("created_ms").and_then(|value| value.as_u64()))
        .max()
        .unwrap_or(since);
    Ok(Response::json(
        200,
        json!({ "objects": items, "count": items.len(), "cursor": cursor }),
    ))
}

fn method_not_allowed(request: &Request) -> ServerError {
    ServerError::Http {
        status: 405,
        detail: format!("{} 不支持 /{}", request.method, request.path),
    }
}

fn bearer(request: &Request) -> Option<String> {
    let value = request.header("authorization")?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(rest.trim().to_string())
}

/// 百分号解码（`+` 不当作空格：路径里它是合法字符）。
fn decode(text: &str) -> String {
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
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_ms;

    fn test_server(tag: &str) -> (Server, Arc<AtomicBool>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cx-server-http-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), &root);
        let server = Server::bind(config).unwrap();
        (server, Arc::new(AtomicBool::new(false)), root)
    }

    fn request(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.trim_start_matches('/').to_string(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn 探针不需要鉴权() {
        let (server, shutdown, root) = test_server("probe");
        let store = server.store();
        let config = server.config().clone();

        let health = route(&store, &config, &request("GET", "healthz"), &shutdown).unwrap();
        assert_eq!(health.status, 200);
        assert_eq!(health.body, b"ok");

        let version = route(&store, &config, &request("GET", "v1/version"), &shutdown).unwrap();
        assert_eq!(version.status, 200);
        let parsed: serde_json::Value = serde_json::from_slice(&version.body).unwrap();
        assert_eq!(parsed["name"], "cx-server");
        assert_eq!(parsed["store_layout"], cx_core::STORE_LAYOUT_VERSION);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 没有令牌一律401() {
        let (server, shutdown, root) = test_server("auth");
        let store = server.store();
        let config = server.config().clone();
        let err = route(&store, &config, &request("GET", "v1/objects"), &shutdown).unwrap_err();
        assert_eq!(err.status(), 401);

        // 令牌错误也是 401；令牌正确但空间不匹配是 403
        let mut bad = request("GET", "v1/objects");
        bad.headers
            .insert("authorization".to_string(), "Bearer cxd_nope".to_string());
        let err = route(&store, &config, &bad, &shutdown).unwrap_err();
        assert_eq!(err.status(), 401);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 注册与对象接口的完整往返() {
        let (server, shutdown, root) = test_server("flow");
        let store = server.store();
        let config = server.config().clone();
        let (space, enroll) = store.create_space("t", 0, 0, true).unwrap();
        let enroll = enroll.unwrap();

        // enroll
        let mut enroll_request = request("POST", "v1/enroll");
        enroll_request.body = serde_json::to_vec(&json!({
            "space_id": space.id,
            "enroll_secret": enroll,
            "device_name": "设备 A",
        }))
        .unwrap();
        let response = route(&store, &config, &enroll_request, &shutdown).unwrap();
        assert_eq!(response.status, 201);
        let payload: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let token = payload["device_token"].as_str().unwrap().to_string();

        let with_token = |method: &str, path: &str| {
            let mut req = request(method, path);
            req.headers
                .insert("authorization".to_string(), format!("Bearer {token}"));
            req
        };

        // 空列举
        let empty = route(&store, &config, &with_token("GET", "v1/objects"), &shutdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
        assert_eq!(parsed["count"], 0);

        // 写对象（If-None-Match: *）
        let mut put = with_token("PUT", "v1/objects/cx-space.json");
        put.headers
            .insert("if-none-match".to_string(), "*".to_string());
        put.body = b"{\"v\":1}".to_vec();
        let created = route(&store, &config, &put, &shutdown).unwrap();
        assert_eq!(created.status, 201);
        assert!(created.headers.iter().any(|(name, _)| name == "ETag"));

        // 重复写同一个对象：412
        let conflict = route(&store, &config, &put, &shutdown).unwrap_err();
        assert_eq!(conflict.status(), 412);

        // 读回
        let get = route(
            &store,
            &config,
            &with_token("GET", "v1/objects/cx-space.json"),
            &shutdown,
        )
        .unwrap();
        assert_eq!(get.status, 200);
        assert_eq!(get.body, b"{\"v\":1}");

        // If-None-Match 命中 → 304
        let etag = get
            .headers
            .iter()
            .find(|(name, _)| name == "ETag")
            .map(|(_, value)| value.clone())
            .unwrap();
        let mut conditional = with_token("GET", "v1/objects/cx-space.json");
        conditional
            .headers
            .insert("if-none-match".to_string(), etag.clone());
        let not_modified = route(&store, &config, &conditional, &shutdown).unwrap();
        assert_eq!(not_modified.status, 304);

        // 列举（前缀过滤）
        let list = route(
            &store,
            &config,
            &with_token("GET", "v1/manifests"),
            &shutdown,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&list.body).unwrap();
        assert_eq!(parsed["count"], 0, "manifests 前缀下还没有东西");

        let all = route(&store, &config, &with_token("GET", "v1/objects"), &shutdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&all.body).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["objects"][0]["name"], "cx-space.json");

        // 删除
        let delete = route(
            &store,
            &config,
            &with_token("DELETE", "v1/objects/cx-space.json"),
            &shutdown,
        )
        .unwrap();
        assert_eq!(delete.status, 204);

        // 空间信息
        let info = route(
            &store,
            &config,
            &with_token("GET", &format!("v1/spaces/{}", space.id)),
            &shutdown,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&info.body).unwrap();
        assert_eq!(parsed["space_id"], space.id);
        assert_eq!(parsed["used_bytes"], 0);

        // 别的空间 id → 403
        let err = route(
            &store,
            &config,
            &with_token("GET", "v1/spaces/deadbeef"),
            &shutdown,
        )
        .unwrap_err();
        assert_eq!(err.status(), 403);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 非法路径与未知接口的报错() {
        let (server, shutdown, root) = test_server("errors");
        let store = server.store();
        let config = server.config().clone();
        let (space, _) = store.create_space("t", 0, 0, false).unwrap();
        let token = store.issue_token(&space.id, "d").unwrap();
        let mut req = request("GET", "v1/objects/v1/../etc/passwd");
        req.headers
            .insert("authorization".to_string(), format!("Bearer {token}"));

        // 越界路径在 store 层被拒
        let err = route(&store, &config, &req, &shutdown).unwrap_err();
        assert_eq!(err.status(), 400);

        let unknown = route(
            &store,
            &config,
            &{
                let mut r = request("GET", "v2/whatever");
                r.headers
                    .insert("authorization".to_string(), format!("Bearer {token}"));
                r
            },
            &shutdown,
        )
        .unwrap_err();
        assert_eq!(unknown.status(), 404);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 长轮询等到新对象就返回() {
        let (server, shutdown, root) = test_server("longpoll");
        let store = server.store();
        let mut config = server.config().clone();
        config.long_poll_max = Duration::from_secs(5);
        let (space, _) = store.create_space("t", 0, 0, false).unwrap();
        let token = store.issue_token(&space.id, "d").unwrap();

        let mut poll = request("GET", "v1/items");
        poll.headers
            .insert("authorization".to_string(), format!("Bearer {token}"));
        poll.query.insert("since".to_string(), "0".to_string());
        poll.query.insert("wait".to_string(), "5".to_string());

        std::thread::scope(|scope| {
            let writer = store.clone();
            let space_id = space.id.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                writer
                    .put_object(&space_id, "v1/items/20251001/a.cxi", b"x", true)
                    .unwrap();
            });
            let started = Instant::now();
            let response = route(&store, &config, &poll, &shutdown).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(parsed["count"], 1, "应当等到新对象");
            assert!(started.elapsed() < Duration::from_secs(3), "不该等到超时");
            assert!(parsed["cursor"].as_u64().unwrap() > 0);
        });

        // 没有新对象时按时超时返回空
        let started = Instant::now();
        let mut quick = poll.clone();
        quick.query.insert("wait".to_string(), "1".to_string());
        quick
            .query
            .insert("since".to_string(), (crate::store::now_ms()).to_string());
        let response = route(&store, &config, &quick, &shutdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(parsed["count"], 0);
        assert!(started.elapsed() >= Duration::from_millis(900));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 端到端跑一次真实_socket_请求() {
        let (server, shutdown, root) = test_server("socket");
        let addr = server.local_addr().unwrap();
        let store = server.store();
        let (space, _) = store.create_space("t", 0, 0, false).unwrap();
        let token = store.issue_token(&space.id, "d").unwrap();

        let server_thread = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || server.serve(shutdown))
        };

        // 健康探针
        let body = http_get(addr, "/healthz", None);
        assert!(body.starts_with("HTTP/1.1 200"), "{body}");

        // 带令牌写读一个对象
        let put = http_request(
            addr,
            "PUT",
            "/v1/objects/hello.txt",
            Some(&format!("Bearer {token}")),
            b"hello",
        );
        assert!(put.starts_with("HTTP/1.1 201"), "{put}");
        let get = http_get(
            addr,
            "/v1/objects/hello.txt",
            Some(&format!("Bearer {token}")),
        );
        assert!(get.ends_with("hello"), "{get}");

        shutdown.store(true, Ordering::Relaxed);
        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(root);
    }

    fn http_request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: &[u8],
    ) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
        if let Some(auth) = auth {
            request.push_str(&format!("Authorization: {auth}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        request.push_str("Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8_lossy(&response).to_string()
    }

    fn http_get(addr: SocketAddr, path: &str, auth: Option<&str>) -> String {
        http_request(addr, "GET", path, auth, b"")
    }
}
