//! WebDAV 后端（Nextcloud / ownCloud / Apache mod_dav / nginx dav 都能用）。
//!
//! 只用四个 WebDAV 动作：`PROPFIND`（列目录 / 取属性）、`GET`、`PUT`、`DELETE`，
//! 外加一个 `MKCOL`（首次运行时补建目录）。之所以够用，是因为我们的对象布局
//! （见 `layout`）把同步语义全部编码进了**对象命名与内容**里，后端只需要
//! "能列目录、能按名字读写不可变对象"。
//!
//! 几个特意处理的现实差异：
//! - **207 响应千差万别**：前缀可能是 `d:` `D:` 或默认命名空间，一个 `<response>`
//!   里可能有多个 `<propstat>`（200 + 404）。解析器按"本地名"匹配，并且只接受
//!   状态为 2xx 的 propstat。
//! - **目录不存在**：有的服务端返回 404、有的 409、有的 403。列目录时都把
//!   它们当作"空"；只有 401/403 才当权限问题。
//! - **条件写**：优先用 `If-None-Match: *` 保证不可变对象不被覆盖；遇到不支持
//!   条件头的服务端（400/501）就退化为普通 PUT（对象名含内容哈希，覆盖也无害）。
//! - **父目录不存在**：PUT 返回 409 时用 MKCOL 逐级补建后重试一次。

use crate::date::parse_http_date;
use crate::error::BackendError;
use crate::http::{HttpSettings, configure};
use crate::xml::resolve_entity;
use crate::{PutOutcome, RemoteBackend, RemoteObject};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

/// PROPFIND 请求体：只要判断"有没有变化"所需的属性。
pub const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop>"#,
    r#"<d:getetag/><d:getlastmodified/><d:getcontentlength/><d:resourcetype/>"#,
    r#"</d:prop></d:propfind>"#
);

/// 路径分段里需要转义的字符（**不含 `/`**，它必须保持为分隔符）。
///
/// 依据 RFC 3986：unreserved = ALPHA / DIGIT / `-` / `.` / `_` / `~`。
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'[')
    .add(b']')
    .add(b'^')
    .add(b'|')
    .add(b'\\')
    .add(b'\'');

/// 构造参数。
#[derive(Debug, Clone)]
pub struct WebDavOptions {
    /// 后端名（配置里的 `name`）。
    pub name: String,
    /// 集合 URL（例如 `https://cloud.example.com/remote.php/dav/files/alice/clipboardx`）。
    pub url: String,
    /// 用户名。
    pub username: Option<String>,
    /// 口令。
    pub password: Option<String>,
    /// 自定义 CA（PEM）。
    pub ca_file: Option<PathBuf>,
    /// 跳过 TLS 校验（仅调试）。
    pub insecure: bool,
    /// 单次请求超时。
    pub timeout: Duration,
}

/// WebDAV 后端。
pub struct WebDavBackend {
    name: String,
    /// HTTP 客户端设置（超时、CA、是否跳过校验）。
    settings: HttpSettings,
    /// 规范化后的根 URL（以 `/` 结尾）。
    root: String,
    /// 根 URL 的路径部分（以 `/` 结尾），用于把 href 变成相对路径。
    root_path: String,
    /// Basic 认证凭据（用户名, 口令）。
    auth: Option<(String, String)>,
    /// 已确认存在的集合，避免重复 MKCOL。
    known_collections: HashSet<String>,
}

/// 一次 HTTP 往返的结果。
#[derive(Debug)]
struct HttpReply {
    status: u16,
    etag: Option<String>,
    last_modified_ms: Option<u64>,
    server: Option<String>,
    body: Vec<u8>,
}

impl WebDavBackend {
    /// 构造并校验 URL。
    pub fn new(options: WebDavOptions) -> Result<Self, BackendError> {
        let root = normalize_root(&options.url)?;
        let root_path = url_path(&root).ok_or_else(|| {
            BackendError::Config(format!("无法解析 URL 的路径部分：{}", options.url))
        })?;

        let settings = HttpSettings {
            timeout: options.timeout,
            ca_file: options.ca_file,
            insecure: options.insecure,
        };
        // 提前校验 CA 文件，避免第一次请求才发现配错
        if let Some(path) = &settings.ca_file {
            crate::http::load_ca_certificate(path)?;
        }

        // Basic 认证由 attohttpc 负责编码（比手拼 base64 少一处出错点）
        let auth = options
            .username
            .map(|username| (username, options.password.unwrap_or_default()));

        let mut backend = Self {
            name: options.name,
            settings,
            root,
            root_path,
            auth,
            known_collections: HashSet::new(),
        };
        // 根目录视为已知，免得每次写对象都去 MKCOL 它
        backend.known_collections.insert(String::new());
        Ok(backend)
    }

    /// 探活：确认根目录可访问、可列举（`cx backend test` / `cx doctor` 用）。
    pub fn probe(&mut self) -> Result<String, BackendError> {
        let reply = self.request(
            "PROPFIND",
            "",
            PROPFIND_BODY.as_bytes().to_vec(),
            &[("Depth", "0")],
        )?;
        match reply.status {
            200 | 207 => {}
            401 | 403 => {
                return Err(BackendError::Unauthorized {
                    status: reply.status,
                    detail: "根目录不可访问（检查用户名/口令与路径）".to_string(),
                });
            }
            404 => {
                return Err(BackendError::Config(
                    "根目录不存在（检查 URL 是否需要 /remote.php/dav/files/<用户>/… 前缀）"
                        .to_string(),
                ));
            }
            other => {
                return Err(BackendError::Status {
                    status: other,
                    detail: "PROPFIND 根目录失败".to_string(),
                });
            }
        }
        let server = reply.server.unwrap_or_else(|| "未知服务端".to_string());
        Ok(format!("可访问（服务端：{server}）"))
    }

    fn url_for(&self, path: &str) -> String {
        let encoded: String = path
            .split('/')
            .map(|segment| utf8_percent_encode(segment, SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/");
        format!("{}{}", self.root, encoded)
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, BackendError> {
        let url = self.url_for(path);
        let http_method = attohttpc::Method::from_bytes(method.as_bytes())
            .map_err(|err| BackendError::Config(format!("非法 HTTP 方法 {method}：{err}")))?;
        let mut builder = attohttpc::RequestBuilder::new(http_method, &url);
        builder = configure(builder, &self.settings)?;
        if let Some((username, password)) = &self.auth {
            builder = builder.basic_auth(username, Some(password));
        }
        for (name, value) in headers {
            // 显式构造 HeaderName/HeaderValue：既能给出清晰的错误信息，
            // 也避免 `&str → &'static str` 的隐式生命周期要求
            let header_name = attohttpc::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|err| BackendError::Config(format!("请求头名 {name} 非法：{err}")))?;
            let header_value = attohttpc::header::HeaderValue::from_str(value)
                .map_err(|err| BackendError::Config(format!("请求头 {name} 的值非法：{err}")))?;
            builder = builder.header(header_name, header_value);
        }

        let response = builder
            .bytes(body)
            .send()
            .map_err(|err| BackendError::Transport(format!("{method} {url}：{err}")))?;

        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_string())
        };
        let etag = header("etag");
        let last_modified_ms = header("last-modified").and_then(|text| parse_http_date(&text));
        let server = header("server");
        let body = response.bytes().map_err(|err| {
            BackendError::Transport(format!("读取 {method} {url} 响应体失败：{err}"))
        })?;

        Ok(HttpReply {
            status,
            etag,
            last_modified_ms,
            server,
            body,
        })
    }

    /// 列出一层目录。
    fn propfind(&mut self, path: &str, depth: &str) -> Result<Vec<RemoteObject>, BackendError> {
        let reply = self.request(
            "PROPFIND",
            path,
            PROPFIND_BODY.as_bytes().to_vec(),
            &[
                ("Depth", depth),
                ("Content-Type", "application/xml; charset=utf-8"),
            ],
        )?;

        match reply.status {
            200 | 207 => {}
            401 | 403 => {
                return Err(BackendError::Unauthorized {
                    status: reply.status,
                    detail: format!("无法列举 {path}"),
                });
            }
            // 目录还不存在（不同服务端 404/409 都有）
            404 | 409 | 410 => return Ok(Vec::new()),
            other => {
                return Err(BackendError::Status {
                    status: other,
                    detail: format!("PROPFIND {path} 失败"),
                });
            }
        }

        let mut objects = parse_multistatus(&reply.body)?;
        // 转成相对路径，并丢掉"目录自身"那一条
        objects = objects
            .into_iter()
            .filter_map(|mut object| {
                let relative = self.relative_path(&object.path)?;
                object.path = relative;
                Some(object)
            })
            .filter(|object| !object.path.is_empty())
            .collect();
        Ok(objects)
    }

    /// 补建父集合（逐级 MKCOL，已存在的会被忽略）。
    fn ensure_collections(&mut self, path: &str) -> Result<(), BackendError> {
        let Some((parent, _)) = path.rsplit_once('/') else {
            return Ok(());
        };
        if parent.is_empty() {
            return Ok(());
        }

        let mut current = String::new();
        for segment in parent.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(segment);
            if self.known_collections.contains(&current) {
                continue;
            }
            self.mkcol(&format!("{current}/"))?;
            self.known_collections.insert(current.clone());
        }
        Ok(())
    }

    fn mkcol(&mut self, path: &str) -> Result<(), BackendError> {
        let reply = self.request("MKCOL", path, Vec::new(), &[])?;
        match reply.status {
            // 201 = 建好了；405 = 已存在（正常，继续）
            200 | 201 | 204 | 405 => Ok(()),
            401 | 403 => Err(BackendError::Unauthorized {
                status: reply.status,
                detail: format!("无法创建目录 {path}"),
            }),
            other => Err(BackendError::Status {
                status: other,
                detail: format!("MKCOL {path} 失败"),
            }),
        }
    }

    /// 把 href（可能是绝对 URL、绝对路径、或百分号编码）转成相对根目录的路径。
    fn relative_path(&self, href: &str) -> Option<String> {
        let path = url_path(href)?;
        let path = percent_decode_str(&path).decode_utf8().ok()?.to_string();
        let stripped = path.strip_prefix(&self.root_path)?;
        Some(stripped.trim_matches('/').to_string())
    }
}

impl RemoteBackend for WebDavBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "webdav"
    }

    fn describe(&self) -> String {
        format!("WebDAV {}", self.root)
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<RemoteObject>, BackendError> {
        // PROPFIND 会连"目录自身"一起返回，list 的语义是"目录内容"，因此剔除它
        let self_path = prefix.trim_matches('/').to_string();
        let mut objects = self.propfind(prefix, "1")?;
        objects.retain(|object| object.path != self_path);
        Ok(objects)
    }

    fn head(&mut self, path: &str) -> Result<Option<RemoteObject>, BackendError> {
        // 用 PROPFIND Depth:0 而不是 HEAD：WebDAV 服务端对 HEAD 的支持参差不齐
        let mut objects = self.propfind(path, "0")?;
        objects.retain(|object| object.path == path.trim_matches('/'));
        Ok(objects.into_iter().next())
    }

    fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(self.get_with_meta(path)?.map(|(bytes, _)| bytes))
    }

    fn get_with_meta(
        &mut self,
        path: &str,
    ) -> Result<Option<(Vec<u8>, RemoteObject)>, BackendError> {
        let reply = self.request("GET", path, Vec::new(), &[])?;
        match reply.status {
            200 => {
                let size = Some(reply.body.len() as u64);
                Ok(Some((
                    reply.body,
                    RemoteObject {
                        path: path.to_string(),
                        etag: reply.etag,
                        last_modified_ms: reply.last_modified_ms,
                        size,
                        is_collection: false,
                    },
                )))
            }
            // 每个 404 都可能是"尚未创建"，不是错误
            404 | 410 => Ok(None),
            401 | 403 => Err(BackendError::Unauthorized {
                status: reply.status,
                detail: format!("读取 {path} 被拒绝"),
            }),
            other => Err(BackendError::Status {
                status: other,
                detail: format!("GET {path} 失败"),
            }),
        }
    }

    fn put(
        &mut self,
        path: &str,
        bytes: &[u8],
        only_if_absent: bool,
    ) -> Result<PutOutcome, BackendError> {
        let attempt = |backend: &Self, conditional: bool| -> Result<HttpReply, BackendError> {
            let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/octet-stream")];
            if conditional {
                headers.push(("If-None-Match", "*"));
            }
            backend.request("PUT", path, bytes.to_vec(), &headers)
        };

        let mut reply = attempt(self, only_if_absent)?;

        // 父目录不存在：补建后重试一次
        if reply.status == 409 {
            self.ensure_collections(path)?;
            reply = attempt(self, only_if_absent)?;
        }

        // 服务端不支持条件头：退化为普通 PUT（对象名含内容哈希，覆盖无害）
        if only_if_absent && matches!(reply.status, 400 | 501) {
            log::debug!("{path}：服务端不支持 If-None-Match，退化为普通 PUT");
            reply = attempt(self, false)?;
        }

        match reply.status {
            200 | 204 => Ok(PutOutcome::Replaced { etag: reply.etag }),
            201 => Ok(PutOutcome::Created { etag: reply.etag }),
            412 => Ok(PutOutcome::AlreadyExists),
            401 | 403 => Err(BackendError::Unauthorized {
                status: reply.status,
                detail: format!("写入 {path} 被拒绝"),
            }),
            507 => Err(BackendError::Status {
                status: 507,
                detail: format!("存储配额不足，无法写入 {path}"),
            }),
            other => Err(BackendError::Status {
                status: other,
                detail: format!("PUT {path} 失败（{} 字节）", bytes.len()),
            }),
        }
    }

    fn delete(&mut self, path: &str) -> Result<(), BackendError> {
        let reply = self.request("DELETE", path, Vec::new(), &[])?;
        match reply.status {
            // 404 说明本来就没有，幂等
            200 | 202 | 204 | 404 | 410 => Ok(()),
            401 | 403 => Err(BackendError::Unauthorized {
                status: reply.status,
                detail: format!("删除 {path} 被拒绝"),
            }),
            other => Err(BackendError::Status {
                status: other,
                detail: format!("DELETE {path} 失败"),
            }),
        }
    }
}

/// 规范化为以 `/` 结尾的绝对 URL。
fn normalize_root(url: &str) -> Result<String, BackendError> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(BackendError::Config(format!(
            "WebDAV URL 必须以 http:// 或 https:// 开头：{trimmed}"
        )));
    }
    let mut root = trimmed.to_string();
    // 丢掉查询串：WebDAV 集合地址不该带查询参数
    if let Some(index) = root.find(['?', '#']) {
        root.truncate(index);
    }
    if !root.ends_with('/') {
        root.push('/');
    }
    Ok(root)
}

/// 取出 URL 的路径部分（绝对 URL 取 path，相对路径原样返回）。
fn url_path(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(index) => &url[index + 3..],
        None => url,
    };
    if url.contains("://") {
        // 跳到主机名之后的第一个 '/'
        match after_scheme.find('/') {
            Some(slash) => Some(after_scheme[slash..].to_string()),
            None => Some("/".to_string()),
        }
    } else if after_scheme.starts_with('/') {
        Some(after_scheme.to_string())
    } else {
        None
    }
}

/// 解析 207 Multi-Status。
///
/// 规则：
/// - 按**本地名**匹配元素（`d:href` / `D:href` / `href` 一视同仁）；
/// - 一个 `<response>` 里可能有多个 `<propstat>`，只接受状态为 2xx 的那些；
/// - `href` 缺失的 response 直接丢弃；
/// - **文本要累积**：quick-xml 会在实体边界（如 `&quot;`）切分 Text 事件，
///   逐段覆盖会丢掉前后片段（曾把 `"5f3c1e77"` 解析成 `5f3c1e77`）。
pub(crate) fn parse_multistatus(body: &[u8]) -> Result<Vec<RemoteObject>, BackendError> {
    #[derive(Default)]
    struct Pending {
        href: Option<String>,
        etag: Option<String>,
        last_modified_ms: Option<u64>,
        size: Option<u64>,
        is_collection: bool,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Field {
        Href,
        Etag,
        LastModified,
        ContentLength,
        Status,
    }

    impl Field {
        /// 该字段对应的元素本地名（用于判断累积的文本何时结束）。
        fn element(self) -> &'static str {
            match self {
                Field::Href => "href",
                Field::Etag => "getetag",
                Field::LastModified => "getlastmodified",
                Field::ContentLength => "getcontentlength",
                Field::Status => "status",
            }
        }
    }

    fn field_for(element: &str) -> Option<Field> {
        match element {
            "href" => Some(Field::Href),
            "getetag" => Some(Field::Etag),
            "getlastmodified" => Some(Field::LastModified),
            "getcontentlength" => Some(Field::ContentLength),
            "status" => Some(Field::Status),
            _ => None,
        }
    }

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut buffer = Vec::new();
    let mut objects = Vec::new();
    let mut current: Option<Pending> = None;
    let mut propstat: Pending = Pending::default();
    let mut propstat_ok: Option<bool> = None;
    let mut field: Option<Field> = None;
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(XmlEvent::Start(event)) => {
                let name = event.local_name().into_inner();
                match name {
                    "response" => {
                        current = Some(Pending::default());
                        propstat = Pending::default();
                        propstat_ok = None;
                    }
                    "propstat" => {
                        propstat = Pending::default();
                        propstat_ok = None;
                    }
                    "collection" => {
                        if let Some(entry) = current.as_mut() {
                            entry.is_collection = true;
                        }
                        if propstat_ok.is_none() {
                            propstat.is_collection = true;
                        }
                    }
                    other => {
                        if let Some(found) = field_for(other) {
                            field = Some(found);
                            text.clear();
                        }
                    }
                }
            }
            Ok(XmlEvent::Empty(event)) => {
                if event.local_name().into_inner() == "collection" {
                    if let Some(entry) = current.as_mut() {
                        entry.is_collection = true;
                    }
                    propstat.is_collection = true;
                }
            }
            Ok(XmlEvent::Text(chunk)) => {
                if field.is_none() {
                    buffer.clear();
                    continue;
                }
                let raw = chunk.into_inner();
                let decoded = quick_xml::escape::unescape(&raw)
                    .map_err(|err| BackendError::Xml(format!("实体解码失败：{err}")))?;
                text.push_str(&decoded);
            }
            Ok(XmlEvent::CData(chunk)) => {
                if field.is_some() {
                    text.push_str(&chunk.into_inner());
                }
            }
            // 实体引用是**独立事件**（quick-xml 0.42 的行为）：
            // `<d:getetag>&quot;abc&quot;</d:getetag>` 会产生
            // Text("") + Ref("quot") + Text("abc") + Ref("quot") + Text("")
            // 漏掉这个分支就会把 `"abc"` 解析成 `abc`（真实踩过）。
            Ok(XmlEvent::GeneralRef(reference)) => {
                if field.is_some() {
                    let name = reference.into_inner();
                    match resolve_entity(&name) {
                        Some(resolved) => text.push_str(&resolved),
                        None => {
                            // 未知实体：原样保留，便于发现服务端发了怪东西
                            text.push('&');
                            text.push_str(&name);
                            text.push(';');
                        }
                    }
                }
            }
            Ok(XmlEvent::End(event)) => {
                let name = event.local_name().into_inner();

                // 累积的文本在"字段对应的元素结束"时提交
                if let Some(active) = field
                    && active.element() == name
                {
                    let value = std::mem::take(&mut text);
                    match active {
                        Field::Href => {
                            if let Some(entry) = current.as_mut() {
                                entry.href = Some(value);
                            }
                        }
                        Field::Etag => propstat.etag = Some(value),
                        Field::LastModified => propstat.last_modified_ms = parse_http_date(&value),
                        Field::ContentLength => propstat.size = value.trim().parse::<u64>().ok(),
                        Field::Status => {
                            // 形如 `HTTP/1.1 200 OK`
                            propstat_ok = Some(
                                value
                                    .split_whitespace()
                                    .nth(1)
                                    .and_then(|code| code.parse::<u16>().ok())
                                    .map(|code| (200..300).contains(&code))
                                    .unwrap_or(false),
                            );
                        }
                    }
                    field = None;
                }

                match name {
                    "propstat" => {
                        if propstat_ok.unwrap_or(true)
                            && let Some(entry) = current.as_mut()
                        {
                            if propstat.etag.is_some() {
                                entry.etag = propstat.etag.clone();
                            }
                            if propstat.last_modified_ms.is_some() {
                                entry.last_modified_ms = propstat.last_modified_ms;
                            }
                            if propstat.size.is_some() {
                                entry.size = propstat.size;
                            }
                            if propstat.is_collection {
                                entry.is_collection = true;
                            }
                        }
                        propstat = Pending::default();
                        propstat_ok = None;
                    }
                    "response" => {
                        if let Some(entry) = current.take()
                            && let Some(href) = entry.href
                        {
                            objects.push(RemoteObject {
                                path: href,
                                etag: entry.etag,
                                last_modified_ms: entry.last_modified_ms,
                                size: entry.size,
                                is_collection: entry.is_collection,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(XmlEvent::Eof) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(BackendError::Xml(format!(
                    "位置 {}：{err}",
                    reader.buffer_position()
                )));
            }
        }
        buffer.clear();
    }

    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(url: &str) -> WebDavBackend {
        WebDavBackend::new(WebDavOptions {
            name: "test".to_string(),
            url: url.to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(5),
        })
        .unwrap()
    }

    #[test]
    fn url_规范化() {
        assert_eq!(
            normalize_root("https://x.example.com/dav").unwrap(),
            "https://x.example.com/dav/"
        );
        assert_eq!(
            normalize_root("https://x.example.com/dav/").unwrap(),
            "https://x.example.com/dav/"
        );
        assert_eq!(
            normalize_root("  https://x.example.com/dav?q=1#f  ").unwrap(),
            "https://x.example.com/dav/"
        );
        assert!(normalize_root("ftp://x.example.com/dav").is_err());
        assert!(normalize_root("/remote.php/dav/files/alice").is_err());
    }

    #[test]
    fn 路径拼接会转义但保留分隔符() {
        let backend = backend("https://cloud.example.com/remote.php/dav/files/alice/clipboardx");
        assert_eq!(
            backend.url_for("v1/manifests/ab12cd34.json"),
            "https://cloud.example.com/remote.php/dav/files/alice/clipboardx/v1/manifests/ab12cd34.json"
        );
        assert_eq!(
            backend.url_for("v1/items/20251001/a b.cxi"),
            "https://cloud.example.com/remote.php/dav/files/alice/clipboardx/v1/items/20251001/a%20b.cxi"
        );
    }

    #[test]
    fn href_转相对路径() {
        let backend = backend("https://cloud.example.com/remote.php/dav/files/alice/clipboardx");
        assert_eq!(
            backend
                .relative_path("/remote.php/dav/files/alice/clipboardx/v1/manifests/a.json")
                .as_deref(),
            Some("v1/manifests/a.json")
        );
        // 绝对 URL 形式的 href
        assert_eq!(
            backend
                .relative_path(
                    "https://cloud.example.com/remote.php/dav/files/alice/clipboardx/v1/x.json"
                )
                .as_deref(),
            Some("v1/x.json")
        );
        // 百分号编码
        assert_eq!(
            backend
                .relative_path("/remote.php/dav/files/alice/clipboardx/v1/a%20b.json")
                .as_deref(),
            Some("v1/a b.json")
        );
        // 目录自身
        assert_eq!(
            backend
                .relative_path("/remote.php/dav/files/alice/clipboardx/v1/manifests/")
                .as_deref(),
            Some("v1/manifests")
        );
        // 不在根目录之下 ⇒ 忽略
        assert!(backend.relative_path("/other/place/x.json").is_none());
    }

    #[test]
    fn 凭据被保存为_用户名口令对() {
        let backend = backend("https://x.example.com/dav");
        assert_eq!(
            backend.auth.as_ref(),
            Some(&("alice".to_string(), "secret".to_string()))
        );
        // 编码交给 attohttpc 的 basic_auth，不再自己拼 base64
        let anonymous = WebDavBackend::new(WebDavOptions {
            name: "anon".to_string(),
            url: "https://x.example.com/dav".to_string(),
            username: None,
            password: None,
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(5),
        })
        .unwrap();
        assert!(anonymous.auth.is_none());
    }

    #[test]
    fn 解析_nextcloud_风格响应() {
        let body = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/alice/clipboardx/v1/manifests/</d:href>
  <d:propstat>
   <d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/alice/clipboardx/v1/manifests/ab12cd34.json</d:href>
  <d:propstat>
   <d:prop>
    <d:getlastmodified>Wed, 01 Oct 2025 13:06:38 GMT</d:getlastmodified>
    <d:getetag>&quot;5f3c1e77&quot;</d:getetag>
    <d:getcontentlength>348</d:getcontentlength>
    <d:resourcetype/>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;

        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects.len(), 2);
        assert!(objects[0].is_collection);
        assert_eq!(
            objects[1].etag.as_deref(),
            Some("\"5f3c1e77\""),
            "实体应被解码，实际：{:?}",
            objects[1].etag
        );
        assert_eq!(objects[1].size, Some(348));
        assert_eq!(objects[1].last_modified_ms, Some(1_759_323_998_000));
        assert!(!objects[1].is_collection);
    }

    #[test]
    fn 解析_mod_dav_风格响应_大写前缀() {
        let body = br#"<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/v1/items/20251001/1759299998712-ab12cd34-9f3c1e77.cxi</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"9f3c1e77"</D:getetag>
        <D:getlastmodified>Tue, 30 Sep 2025 23:59:59 GMT</D:getlastmodified>
        <D:getcontentlength>123</D:getcontentlength>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].etag.as_deref(), Some("\"9f3c1e77\""));
        assert_eq!(objects[0].size, Some(123));
        assert!(!objects[0].is_collection);
        assert!(objects[0].last_modified_ms.is_some());
    }

    #[test]
    fn 解析默认命名空间_无前缀() {
        let body = br#"<multistatus xmlns="DAV:">
  <response>
    <href>/dav/v1/manifests/a.json</href>
    <propstat>
      <prop><getetag>"aa"</getetag><getcontentlength>7</getcontentlength></prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].path, "/dav/v1/manifests/a.json");
        assert_eq!(objects[0].etag.as_deref(), Some("\"aa\""));
    }

    #[test]
    fn 多_propstat_时只取_2xx_的属性() {
        // 200 里只有 etag，404 里的 lastmodified 必须被忽略
        let body = br#"<d:multistatus xmlns:d="DAV:">
 <d:response>
  <d:href>/dav/x.json</d:href>
  <d:propstat>
   <d:prop><d:getetag>"abc"</d:getetag></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop><d:getlastmodified>Wed, 01 Oct 2025 13:06:38 GMT</d:getlastmodified></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].etag.as_deref(), Some("\"abc\""));
        assert!(
            objects[0].last_modified_ms.is_none(),
            "404 的属性不应被采纳"
        );

        // 反过来（404 在前、200 在后）同样成立
        let body = br#"<d:multistatus xmlns:d="DAV:">
 <d:response>
  <d:href>/dav/x.json</d:href>
  <d:propstat>
   <d:prop><d:getetag/></d:prop>
   <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:propstat>
  <d:propstat>
   <d:prop><d:getetag>"def"</d:getetag></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects[0].etag.as_deref(), Some("\"def\""));
    }

    #[test]
    fn 空_multistatus_与无_href_的响应() {
        let empty =
            parse_multistatus(br#"<d:multistatus xmlns:d="DAV:"></d:multistatus>"#).unwrap();
        assert!(empty.is_empty());

        let no_href = parse_multistatus(
            br#"<d:multistatus xmlns:d="DAV:"><d:response><d:propstat><d:prop><d:getetag>"x"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
        )
        .unwrap();
        assert!(no_href.is_empty(), "没有 href 的响应应被丢弃");
    }

    #[test]
    fn 非法_xml_报错不panic() {
        for bad in [
            &b""[..],
            &b"<d:multistatus"[..],
            &b"<d:multistatus><d:response>"[..],
            &b"not xml at all"[..],
        ] {
            let result = parse_multistatus(bad);
            // 截断的 XML 必须报错或至少返回空，绝不能 panic
            if let Ok(objects) = result {
                assert!(objects.is_empty() || objects.iter().all(|o| !o.path.is_empty()));
            }
        }
    }

    #[test]
    fn 随机字节不panic() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..2000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = ((state >> 33) as usize) % 96;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                // 偏向 XML 字符，尽量制造"看起来像标签"的输入
                let alphabet = b"<>/\"':=davresponsehrefgetetag 0123456789";
                buf.push(alphabet[(state >> 33) as usize % alphabet.len()]);
            }
            let _ = parse_multistatus(&buf);
        }
    }

    #[test]
    fn 本地名去前缀由_quick_xml_负责() {
        // 我们依赖 quick-xml 的 local_name()：`d:href` / `D:href` / `href` 都解析成 `href`
        use quick_xml::events::BytesStart;
        assert_eq!(BytesStart::new("d:href").local_name().into_inner(), "href");
        assert_eq!(
            BytesStart::new("D:getetag").local_name().into_inner(),
            "getetag"
        );
        assert_eq!(BytesStart::new("href").local_name().into_inner(), "href");
    }

    #[test]
    fn 实体引用被累积成完整文本() {
        // 真实踩过的坑：实体是独立事件，漏掉就会丢引号
        let body = br#"<d:multistatus xmlns:d="DAV:">
 <d:response>
  <d:href>/dav/v1/manifests/a.json</d:href>
  <d:propstat>
   <d:prop><d:getetag>&quot;5f3c1e77&quot;</d:getetag></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].etag.as_deref(), Some("\"5f3c1e77\""));
    }

    #[test]
    fn 数字字符引用也能解析() {
        let body = br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/&#65;&#x42;.json</d:href><d:propstat><d:prop><d:getetag>X</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let objects = parse_multistatus(body).unwrap();
        assert_eq!(objects[0].path, "/dav/AB.json");
    }
}
