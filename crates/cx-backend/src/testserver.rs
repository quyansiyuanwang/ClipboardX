//! 测试用的极小 WebDAV 服务端。
//!
//! 存在的理由：WebDAV 客户端的正确性只有对着真实服务端才能验证，而 CI 里既没有
//! Nextcloud 也不该依赖外网。这个 300 行的服务端实现了我们需要的那几个动作
//! （PROPFIND / GET / PUT / DELETE / MKCOL），并且**刻意还原真实服务端的怪癖**：
//!
//! - ETag 在 XML 里用 `&quot;` 转义（Nextcloud 就是这么发的）；
//! - 父目录不存在时 PUT 返回 `409`（Apache mod_dav），逼客户端走 MKCOL 补建；
//! - 可选地忽略 `If-None-Match`（有些服务端不支持条件写）；
//! - 可选地要求 Basic 认证（顺便验证凭据拼装）。
//!
//! 它在 `cargo test` 里跑，因此三端 CI 都能覆盖客户端逻辑。

use std::collections::{BTreeMap, BTreeSet};
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
#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Object>,
    collections: BTreeSet<String>,
    /// 形如 "Basic base64(user:pass)" 的期望值；`None` 表示不校验
    expected_auth: Option<String>,
    /// 忽略 `If-None-Match`
    ignore_conditional_put: bool,
    /// 父目录不存在时返回 409（默认开启，符合 Apache 行为）
    fail_missing_parent: bool,
    /// 下一次 PUT 强制返回 409（用来测"补建后重试"这条路径）
    force_next_put_409: bool,
    /// 请求记录（断言用）
    methods: Vec<String>,
    clock: u64,
}

impl State {
    fn new() -> Self {
        Self {
            fail_missing_parent: true,
            ..Self::default()
        }
    }
}

/// 测试服务端句柄。
#[derive(Debug)]
pub struct TestWebDavServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestWebDavServer {
    /// 启动（监听 127.0.0.1 的随机端口）。
    pub fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("无法绑定测试端口");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread = {
            let state = state.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("test-webdav".to_string())
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

    /// 集合 URL（以 `/` 结尾）。
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// 要求 Basic 认证。
    pub fn require_auth(&self, user: &str, password: &str) {
        use base64::Engine;
        let raw = format!("{user}:{password}");
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        self.state.lock().unwrap().expected_auth = Some(format!("Basic {encoded}"));
    }

    /// 让服务端忽略 `If-None-Match`。
    pub fn ignore_conditional_put(&self, value: bool) {
        self.state.lock().unwrap().ignore_conditional_put = value;
    }

    /// 关闭"父目录缺失返回 409"的行为。
    pub fn allow_implicit_parents(&self, value: bool) {
        self.state.lock().unwrap().fail_missing_parent = !value;
    }

    /// 下一次 PUT 强制返回 409（测重试路径）。
    pub fn force_next_put_conflict(&self) {
        self.state.lock().unwrap().force_next_put_409 = true;
    }

    /// 直接放置对象（预置数据）。
    pub fn seed(&self, path: &str, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.clock += 1;
        let clock = state.clock;
        ensure_parent_collections(&mut state, path);
        state.objects.insert(
            path.trim_matches('/').to_string(),
            Object {
                bytes: bytes.to_vec(),
                etag: etag_for(bytes),
                modified_ms: clock,
            },
        );
    }

    /// 对象是否存在。
    pub fn has(&self, path: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .objects
            .contains_key(path.trim_matches('/'))
    }

    /// 读取对象内容。
    pub fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(path.trim_matches('/'))
            .map(|object| object.bytes.clone())
    }

    /// 对象数量。
    pub fn object_count(&self) -> usize {
        self.state.lock().unwrap().objects.len()
    }

    /// 收到过的方法序列（断言"没有多余请求"）。
    pub fn methods(&self) -> Vec<String> {
        self.state.lock().unwrap().methods.clone()
    }

    /// 停止并等待线程退出。
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TestWebDavServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 解析请求并生成响应。
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

    // 读头
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
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    // 读体
    let length: usize = header("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    let path = normalized_path(&target);
    let (status, response_headers, payload) = {
        let mut state = state.lock().unwrap();
        state.methods.push(format!("{method} {path}"));
        dispatch(
            &method,
            &path,
            &headers,
            header("authorization").as_deref(),
            &body,
            &mut state,
        )
    };

    let mut response = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in &response_headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    response.push_str("Connection: close\r\n\r\n");
    writer.write_all(response.as_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// URL 路径 → 相对路径（去掉前导 `/`，保留尾部 `/` 以区分集合）。
fn normalized_path(target: &str) -> String {
    let without_query = target.split('?').next().unwrap_or(target);
    let decoded = percent_decode(without_query);
    decoded.trim_start_matches('/').to_string()
}

fn percent_decode(text: &str) -> String {
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

type DispatchResult = (u16, Vec<(String, String)>, Vec<u8>);

fn dispatch(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    authorization: Option<&str>,
    body: &[u8],
    state: &mut State,
) -> DispatchResult {
    // 认证
    if let Some(expected) = &state.expected_auth
        && authorization != Some(expected.as_str())
    {
        return (
            401,
            vec![(
                "WWW-Authenticate".to_string(),
                "Basic realm=\"test\"".to_string(),
            )],
            Vec::new(),
        );
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    match method {
        "PROPFIND" => propfind(path, header("depth").as_deref(), state),
        "GET" => match state.objects.get(path) {
            Some(object) => (
                200,
                vec![
                    (
                        "Content-Type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                    ("ETag".to_string(), object.etag.clone()),
                ],
                object.bytes.clone(),
            ),
            None => (404, Vec::new(), Vec::new()),
        },
        "PUT" => put(
            path,
            body,
            state,
            !state.ignore_conditional_put && header("if-none-match").as_deref() == Some("*"),
        ),
        "DELETE" => {
            state.objects.remove(path);
            (204, Vec::new(), Vec::new())
        }
        "MKCOL" => {
            let name = path.trim_end_matches('/').to_string();
            if state.collections.contains(&name) {
                return (405, Vec::new(), Vec::new());
            }
            state.collections.insert(name);
            (201, Vec::new(), Vec::new())
        }
        _ => (405, Vec::new(), Vec::new()),
    }
}

fn propfind(path: &str, depth: Option<&str>, state: &mut State) -> DispatchResult {
    let depth = depth.unwrap_or("0");
    let collection = path.trim_end_matches('/').to_string();

    // 单个对象
    if let Some(object) = state.objects.get(path) {
        let body = multistatus(&[entry_xml(&format!("/{path}"), object, false)]);
        return (
            207,
            vec![(
                "Content-Type".to_string(),
                "application/xml; charset=utf-8".to_string(),
            )],
            body.into_bytes(),
        );
    }

    // 集合（根或子目录）
    let is_root = collection.is_empty();
    if !is_root && !state.collections.contains(&collection) {
        return (404, Vec::new(), Vec::new());
    }

    let mut entries = vec![collection_xml(&format!(
        "/{}",
        if is_root {
            String::new()
        } else {
            format!("{collection}/")
        }
    ))];
    if depth != "0" {
        let prefix = if is_root {
            String::new()
        } else {
            format!("{collection}/")
        };
        for (key, object) in state.objects.iter() {
            if key.starts_with(&prefix) && !key[prefix.len()..].contains('/') {
                entries.push(entry_xml(&format!("/{key}"), object, false));
            }
        }
        for candidate in state.collections.iter() {
            if candidate.starts_with(&prefix)
                && candidate != &collection
                && !candidate[prefix.len()..].contains('/')
            {
                entries.push(collection_xml(&format!("/{candidate}/")));
            }
        }
    }

    let body = multistatus(&entries);
    (
        207,
        vec![(
            "Content-Type".to_string(),
            "application/xml; charset=utf-8".to_string(),
        )],
        body.into_bytes(),
    )
}

fn put(path: &str, body: &[u8], state: &mut State, conditional: bool) -> DispatchResult {
    let existing = state.objects.contains_key(path);

    if conditional && existing {
        return (412, Vec::new(), Vec::new());
    }

    if state.force_next_put_409 {
        state.force_next_put_409 = false;
        return (409, Vec::new(), Vec::new());
    }

    if state.fail_missing_parent {
        let parent = path.rsplit_once('/').map(|(parent, _)| parent.to_string());
        if let Some(parent) = parent
            && !parent.is_empty()
            && !state.collections.contains(&parent)
        {
            return (409, Vec::new(), Vec::new());
        }
    } else {
        ensure_parent_collections(state, path);
    }

    state.clock += 1;
    let clock = state.clock;
    let etag = etag_for(body);
    state.objects.insert(
        path.to_string(),
        Object {
            bytes: body.to_vec(),
            etag: etag.clone(),
            modified_ms: clock,
        },
    );
    (
        if existing { 204 } else { 201 },
        vec![("ETag".to_string(), etag)],
        Vec::new(),
    )
}

fn ensure_parent_collections(state: &mut State, path: &str) {
    if let Some((parent, _)) = path.rsplit_once('/') {
        let mut current = String::new();
        for segment in parent.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(segment);
            state.collections.insert(current.clone());
        }
    }
}

fn multistatus(entries: &[String]) -> String {
    let mut body = String::from(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    body.push_str(r#"<d:multistatus xmlns:d="DAV:">"#);
    for entry in entries {
        body.push_str(entry);
    }
    body.push_str("</d:multistatus>");
    body
}

/// 文件条目：ETag 用 `&quot;` 转义（还原真实服务端的写法）。
fn entry_xml(href: &str, object: &Object, collection: bool) -> String {
    let resourcetype = if collection {
        "<d:resourcetype><d:collection/></d:resourcetype>"
    } else {
        "<d:resourcetype/>"
    };
    format!(
        "<d:response><d:href>{href}</d:href><d:propstat><d:prop>\
         <d:getetag>{etag}</d:getetag>\
         <d:getlastmodified>{modified}</d:getlastmodified>\
         <d:getcontentlength>{size}</d:getcontentlength>\
         {resourcetype}\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        href = xml_escape(href),
        etag = xml_escape(&object.etag),
        modified = http_date(object.modified_ms),
        size = object.bytes.len(),
    )
}

fn collection_xml(href: &str) -> String {
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop>\
         <d:resourcetype><d:collection/></d:resourcetype>\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        xml_escape(href)
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn etag_for(bytes: &[u8]) -> String {
    format!("\"{}\"", cx_proto::content_hash_hex(bytes))
}

/// 毫秒 → RFC 1123（服务端时钟只是单调序号，日期本身不重要）。
fn http_date(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = cx_proto::civil::civil_from_days(days as i64);
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    // 1970-01-01 是星期四
    let weekday = WEEKDAYS[((days + 4) % 7) as usize];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(month as usize).clamp(1, 12) - 1],
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60,
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        207 => "Multi-Status",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        507 => "Insufficient Storage",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日期格式化形如_rfc1123() {
        let text = http_date(1_759_323_998_000);
        assert!(text.ends_with("GMT"), "{text}");
        assert!(text.contains("Oct 2025"), "{text}");
        assert_eq!(text, "Wed, 01 Oct 2025 13:06:38 GMT");
    }

    #[test]
    fn xml_转义() {
        assert_eq!(xml_escape("\"a\"&<b>"), "&quot;a&quot;&amp;&lt;b&gt;");
    }

    #[test]
    fn 路径规范化与百分号解码() {
        assert_eq!(
            normalized_path("/v1/manifests/a.json"),
            "v1/manifests/a.json"
        );
        assert_eq!(normalized_path("/v1/items/20251001/"), "v1/items/20251001/");
        assert_eq!(normalized_path("/a%20b.json"), "a b.json");
        assert_eq!(normalized_path("/?query=1"), "");
    }
}
