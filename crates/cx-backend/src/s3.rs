//! S3（及其兼容实现：MinIO、Ceph RGW、阿里云 OSS 的 S3 兼容模式、R2…）后端。
//!
//! 与 WebDAV 后端的差别值得先说清楚，因为它决定了实现的形状：
//! - **S3 没有目录**。`v1/items/20251001/xxx.cxi` 只是一个键名，`list(prefix)` 天然就是
//!   "按键前缀列举"，不需要 MKCOL、也不会返回"目录自身"。
//! - **S3 的签名是刚需**。每个请求都要 AWS SigV4 签名（见 [`crate::signing`]），
//!   时间偏差超过 15 分钟服务端直接 403；因此这里实现了"用响应 `Date` 校正时钟后重试一次"。
//! - **ETag 天然适合做增量判断**：非分片上传的 ETag 就是内容 MD5，内容不变则不变，
//!   这与我们在 WebDAV 里用 ETag 做增量的思路一致。
//!
//! 条件写（`If-None-Match: *`）在 S3 与 MinIO 上都支持；若不支持（返回 501/400），
//! 退化为"HEAD 看存在不存在，再决定是 AlreadyExists 还是普通 PUT"。

use crate::date::parse_http_date;
use crate::error::BackendError;
use crate::http::{HttpSettings, configure};
use crate::signing::{SignableRequest, SigningContext, canonical_uri, sha256_hex, sign};
use crate::xml::resolve_entity;
use crate::{PutOutcome, RemoteBackend, RemoteObject};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;
use std::path::PathBuf;
use std::time::Duration;

/// 服务名（签名用）。
const SERVICE: &str = "s3";

/// 一次列举最多取回的对象数（防御性上限，避免超大空间把内存吃满）。
pub const MAX_LIST_OBJECTS: usize = 20_000;

/// 单次 ListObjectsV2 的 `max-keys`。
const LIST_PAGE_SIZE: u32 = 1000;

/// 构造参数。
#[derive(Debug, Clone)]
pub struct S3Options {
    /// 后端名（配置里的 `name`）。
    pub name: String,
    /// 自定义端点（MinIO/自建）；`None` 表示 AWS 官方端点。
    pub endpoint: Option<String>,
    /// 区域。
    pub region: String,
    /// 桶名。
    pub bucket: String,
    /// 键前缀（可为空）。
    pub prefix: String,
    /// 访问密钥 id。
    pub access_key_id: String,
    /// 私有密钥。
    pub secret_access_key: String,
    /// 会话令牌（临时凭据）。
    pub session_token: Option<String>,
    /// 是否用 path-style（`https://endpoint/bucket/key`）而不是 virtual-host。
    pub path_style: bool,
    /// 自定义 CA（PEM）。
    pub ca_file: Option<PathBuf>,
    /// 跳过 TLS 校验（仅调试）。
    pub insecure: bool,
    /// 单次请求超时。
    pub timeout: Duration,
}

/// S3 后端。
pub struct S3Backend {
    name: String,
    /// 端点（`scheme://host[:port]`，无尾斜杠）。
    endpoint: String,
    /// 用于 Host 头与签名的主机部分。
    host: String,
    region: String,
    bucket: String,
    /// 规范化前缀：无前后斜杠（空表示桶根）。
    prefix: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    path_style: bool,
    settings: HttpSettings,
    /// 与服务器的时间偏差（毫秒）：本机时间 + 偏差 = 服务器时间。
    clock_skew_ms: i64,
}

/// 一次 HTTP 往返的结果。
#[derive(Debug)]
struct S3Reply {
    status: u16,
    etag: Option<String>,
    last_modified_ms: Option<u64>,
    size: Option<u64>,
    /// 服务端 `Date` 头（校正时钟用）。
    server_date_ms: Option<u64>,
    body: Vec<u8>,
}

impl S3Backend {
    /// 构造并校验参数。
    pub fn new(options: S3Options) -> Result<Self, BackendError> {
        if options.bucket.trim().is_empty() {
            return Err(BackendError::Config("S3 桶名不能为空".to_string()));
        }
        if options.access_key_id.trim().is_empty() || options.secret_access_key.is_empty() {
            return Err(BackendError::Config(
                "S3 需要访问密钥 id 与私有密钥".to_string(),
            ));
        }
        let region = if options.region.trim().is_empty() {
            "us-east-1".to_string()
        } else {
            options.region.trim().to_string()
        };

        let endpoint = match &options.endpoint {
            Some(endpoint) => normalize_endpoint(endpoint)?,
            None => format!("https://s3.{region}.amazonaws.com"),
        };
        let host = endpoint
            .split_once("://")
            .map(|(_, rest)| rest.to_string())
            .ok_or_else(|| BackendError::Config(format!("端点缺少协议：{endpoint}")))?;

        let settings = HttpSettings {
            timeout: options.timeout,
            ca_file: options.ca_file,
            insecure: options.insecure,
        };
        if let Some(path) = &settings.ca_file {
            crate::http::load_ca_certificate(path)?;
        }

        Ok(Self {
            name: options.name,
            endpoint,
            host,
            region,
            bucket: options.bucket.trim().to_string(),
            prefix: options.prefix.trim().trim_matches('/').to_string(),
            access_key_id: options.access_key_id,
            secret_access_key: options.secret_access_key,
            session_token: options.session_token,
            path_style: options.path_style,
            settings,
            clock_skew_ms: 0,
        })
    }

    /// 已校正的"服务器时间"（毫秒）。
    fn server_now_ms(&self) -> i64 {
        cx_core::now_ms() as i64 + self.clock_skew_ms
    }

    /// 完整键名（前缀 + 相对路径）。
    fn full_key(&self, path: &str) -> String {
        let relative = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            relative.to_string()
        } else if relative.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, relative)
        }
    }

    /// 把服务端返回的键名还原成相对路径。
    fn relative_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            return key.to_string();
        }
        let with_slash = format!("{}/", self.prefix);
        key.strip_prefix(&with_slash)
            .or_else(|| key.strip_prefix(&self.prefix))
            .unwrap_or(key)
            .to_string()
    }

    /// 构造请求 URL 与规范化路径。
    fn target(&self, key: &str, query: &[(String, String)]) -> (String, String) {
        let encoded_key = canonical_uri(key);
        let (url, path) = if self.path_style || key.is_empty() {
            // path-style：https://host/bucket/key
            let path = format!("/{}/{}", self.bucket, encoded_key.trim_start_matches('/'));
            let path = if encoded_key.is_empty() {
                format!("/{}/", self.bucket)
            } else {
                path
            };
            (format!("{}{}", self.endpoint, path), path)
        } else {
            // virtual-host：https://bucket.host/key
            let host = format!("{}.{}", self.bucket, self.host);
            let path = if encoded_key.is_empty() {
                "/".to_string()
            } else {
                format!("/{encoded_key}")
            };
            (
                format!("{}://{}{}", scheme_of(&self.endpoint), host, path),
                path,
            )
        };
        let _ = query;
        (url, path)
    }

    /// 发一次已签名的请求。
    fn request(
        &mut self,
        method: &str,
        key: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        extra_headers: &[(&str, &str)],
    ) -> Result<S3Reply, BackendError> {
        let reply = self.request_once(method, key, query, body.clone(), extra_headers)?;

        // 时间偏差：用服务端 Date 校正后重试一次（只重试一次，避免来回折腾）
        if reply.status == 403
            && let Some(server_ms) = reply.server_date_ms
        {
            let local_ms = cx_core::now_ms() as i64;
            let skew = server_ms as i64 - local_ms;
            if skew.abs() > 60_000 {
                log::info!(
                    "后端 {} 与本机时钟相差 {} 秒，已按服务端时间校正并重试",
                    self.name,
                    skew / 1000
                );
                self.clock_skew_ms = skew;
                return self.request_once(method, key, query, body, extra_headers);
            }
        }
        Ok(reply)
    }

    fn request_once(
        &mut self,
        method: &str,
        key: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        extra_headers: &[(&str, &str)],
    ) -> Result<S3Reply, BackendError> {
        let (amz_date, _) = crate::signing::amz_date_from_ms(self.server_now_ms());
        let payload_hash = sha256_hex(&body);

        // S3 要求发送并签名 x-amz-content-sha256
        let mut headers: Vec<(String, String)> =
            vec![("x-amz-content-sha256".to_string(), payload_hash)];
        for (name, value) in extra_headers {
            headers.push((name.to_string(), value.to_string()));
        }

        let (url, path) = self.target(key, query);
        // 签名用的 Host 必须与真正发出的 Host 一致（virtual-host 下含桶名）
        let signing_host = if self.path_style || key.is_empty() {
            self.host.clone()
        } else {
            format!("{}.{}", self.bucket, self.host)
        };

        let request = SignableRequest {
            method: method.to_string(),
            host: signing_host.clone(),
            canonical_uri: path,
            query: query.to_vec(),
            headers: headers.clone(),
            body: body.clone(),
        };
        let context = SigningContext {
            access_key_id: &self.access_key_id,
            secret_access_key: &self.secret_access_key,
            region: &self.region,
            service: SERVICE,
            amz_date: &amz_date,
            session_token: self.session_token.as_deref(),
        };
        let signed = sign(&request, &context)?;

        let url = if signed.canonical_query.is_empty() {
            url
        } else {
            format!("{url}?{}", signed.canonical_query)
        };
        let http_method = attohttpc::Method::from_bytes(method.as_bytes())
            .map_err(|err| BackendError::Config(format!("非法 HTTP 方法 {method}：{err}")))?;
        let mut builder = attohttpc::RequestBuilder::new(http_method, &url);
        builder = configure(builder, &self.settings)?;
        for (name, value) in &signed.headers {
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
        let size = header("content-length").and_then(|text| text.parse::<u64>().ok());
        let server_date_ms = header("date").and_then(|text| parse_http_date(&text));
        let body = response.bytes().map_err(|err| {
            BackendError::Transport(format!("读取 {method} {url} 响应体失败：{err}"))
        })?;

        Ok(S3Reply {
            status,
            etag,
            last_modified_ms,
            size,
            server_date_ms,
            body,
        })
    }

    /// 把非 2xx 响应变成错误（`ok_statuses` 之外的状态都算失败）。
    fn expect(&self, reply: &S3Reply, method: &str, key: &str) -> Result<(), BackendError> {
        if (200..300).contains(&reply.status) {
            return Ok(());
        }
        let (code, message) = parse_error(&reply.body);
        let is_clock_skew = code.as_deref() == Some("RequestTimeTooSkewed");
        let detail = match (code, message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(code), None) => code,
            (None, Some(message)) => message,
            (None, None) => format!("HTTP {}", reply.status),
        };
        log::debug!("S3 {method} {key} 失败：{} {detail}", reply.status);
        // 时钟偏差不是认证问题：报成认证失败会让用户去改密钥，方向完全错了
        if is_clock_skew {
            return Err(BackendError::Status {
                status: reply.status,
                detail: format!(
                    "{detail}（本机时钟与 S3 服务端相差过大，请校准系统时间或启用 NTP）"
                ),
            });
        }
        Err(match reply.status {
            401 | 403 => BackendError::Unauthorized {
                status: reply.status,
                detail,
            },
            404 => BackendError::NotFound(detail),
            409 => BackendError::Conflict(detail),
            status => BackendError::Status { status, detail },
        })
    }
}

/// 端点规范化：补协议、去尾斜杠。
fn normalize_endpoint(endpoint: &str) -> Result<String, BackendError> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(BackendError::Config("S3 端点不能为空".to_string()));
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        // 没写协议时：本地/内网默认 http，其余默认 https
        let host = trimmed.split('/').next().unwrap_or(trimmed);
        let local = host.starts_with("127.0.0.1")
            || host.starts_with("localhost")
            || host.starts_with("192.168.")
            || host.starts_with("10.")
            || host.starts_with("[::1]");
        format!("{}://{}", if local { "http" } else { "https" }, trimmed)
    };
    let scheme = with_scheme
        .split_once("://")
        .map(|(scheme, _)| scheme.to_string());
    match scheme.as_deref() {
        Some("http") | Some("https") => Ok(with_scheme),
        other => Err(BackendError::Config(format!(
            "S3 端点协议必须是 http 或 https（实际是 {}）",
            other.unwrap_or("缺失")
        ))),
    }
}

fn scheme_of(endpoint: &str) -> &str {
    endpoint
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("https")
}

/// 解析 S3 错误响应里的 `<Code>` 与 `<Message>`。
fn parse_error(body: &[u8]) -> (Option<String>, Option<String>) {
    let mut reader = Reader::from_reader(body);
    let mut buffer = Vec::new();
    let mut code = None;
    let mut message = None;
    let mut current: Option<bool> = None; // true = Code, false = Message
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(XmlEvent::Start(event)) => match event.local_name().into_inner() {
                "Code" => {
                    current = Some(true);
                    text.clear();
                }
                "Message" => {
                    current = Some(false);
                    text.clear();
                }
                _ => current = None,
            },
            Ok(XmlEvent::Text(chunk)) => {
                if current.is_some()
                    && let Ok(decoded) = quick_xml::escape::unescape(&chunk.into_inner())
                {
                    text.push_str(&decoded);
                }
            }
            Ok(XmlEvent::GeneralRef(reference)) => {
                if current.is_some()
                    && let Some(resolved) = resolve_entity(&reference.into_inner())
                {
                    text.push_str(&resolved);
                }
            }
            Ok(XmlEvent::End(event)) => {
                if let Some(active) = current.take() {
                    match event.local_name().into_inner() {
                        "Code" if active => code = Some(std::mem::take(&mut text)),
                        "Message" if !active => message = Some(std::mem::take(&mut text)),
                        _ => {}
                    }
                }
            }
            Ok(XmlEvent::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buffer.clear();
    }
    (code, message)
}

/// ListObjectsV2 的一页结果。
#[derive(Debug, Default)]
struct ListPage {
    objects: Vec<RemoteObject>,
    truncated: bool,
    next_token: Option<String>,
}

/// 解析 ListObjectsV2 响应。
fn parse_list_page(body: &[u8]) -> Result<ListPage, BackendError> {
    let mut reader = Reader::from_reader(body);
    let mut buffer = Vec::new();
    let mut page = ListPage::default();

    let mut in_contents = false;
    let mut field: Option<Field> = None;
    let mut text = String::new();
    let mut entry = Entry::default();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Field {
        Key,
        Etag,
        Size,
        LastModified,
        Truncated,
        NextToken,
    }

    #[derive(Default)]
    struct Entry {
        key: String,
        etag: Option<String>,
        size: Option<u64>,
        modified_ms: Option<u64>,
    }

    fn commit(
        field: Field,
        text: &mut String,
        entry: &mut Entry,
        page: &mut ListPage,
        in_contents: bool,
    ) {
        let value = std::mem::take(text);
        let value = value.trim().to_string();
        match field {
            Field::Key => {
                if in_contents {
                    entry.key = value;
                }
            }
            Field::Etag => {
                if in_contents {
                    entry.etag = Some(value);
                }
            }
            Field::Size => {
                if in_contents {
                    entry.size = value.parse::<u64>().ok();
                }
            }
            Field::LastModified => {
                if in_contents {
                    entry.modified_ms = parse_http_date(&value);
                }
            }
            Field::Truncated => page.truncated = value.eq_ignore_ascii_case("true"),
            Field::NextToken => page.next_token = Some(value),
        }
    }

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(XmlEvent::Start(event)) => match event.local_name().into_inner() {
                "Contents" => {
                    in_contents = true;
                    entry = Entry::default();
                }
                "Key" => field = Some(Field::Key),
                "ETag" => field = Some(Field::Etag),
                "Size" => field = Some(Field::Size),
                "LastModified" => field = Some(Field::LastModified),
                "IsTruncated" => field = Some(Field::Truncated),
                "NextContinuationToken" => field = Some(Field::NextToken),
                _ => field = None,
            },
            Ok(XmlEvent::Text(chunk)) => {
                if field.is_some()
                    && let Ok(decoded) = quick_xml::escape::unescape(&chunk.into_inner())
                {
                    text.push_str(&decoded);
                }
            }
            Ok(XmlEvent::GeneralRef(reference)) => {
                if field.is_some()
                    && let Some(resolved) = resolve_entity(&reference.into_inner())
                {
                    text.push_str(&resolved);
                }
            }
            Ok(XmlEvent::End(event)) => {
                let name = event.local_name().into_inner();
                if let Some(active) = field
                    && match active {
                        Field::Key => name == "Key",
                        Field::Etag => name == "ETag",
                        Field::Size => name == "Size",
                        Field::LastModified => name == "LastModified",
                        Field::Truncated => name == "IsTruncated",
                        Field::NextToken => name == "NextContinuationToken",
                    }
                {
                    commit(active, &mut text, &mut entry, &mut page, in_contents);
                    field = None;
                }
                if name == "Contents" && in_contents {
                    in_contents = false;
                    if !entry.key.is_empty() {
                        page.objects.push(RemoteObject {
                            path: std::mem::take(&mut entry.key),
                            etag: entry.etag.take(),
                            last_modified_ms: entry.modified_ms.take(),
                            size: entry.size.take(),
                            is_collection: false,
                        });
                    }
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
    Ok(page)
}

impl S3Backend {
    /// 创建桶（已存在视为成功）。用于自建 MinIO 的首次使用。
    ///
    /// `target("")` 已经处理好两种寻址：path-style 会请求 `PUT /bucket/`，
    /// virtual-host 会请求 `PUT /`（桶名在 Host 里）。
    pub fn create_bucket(&mut self) -> Result<bool, BackendError> {
        let reply = self.request("PUT", "", &[], Vec::new(), &[])?;
        match reply.status {
            200 | 204 => Ok(true),
            // 桶已存在（BucketAlreadyOwnedByYou 是 409，部分实现给 200/403）
            409 => Ok(false),
            403 => {
                let (code, _) = parse_error(&reply.body);
                if code.as_deref() == Some("BucketAlreadyOwnedByYou") {
                    Ok(false)
                } else {
                    self.expect(&reply, "PUT", "（建桶）").map(|_| false)
                }
            }
            _ => self.expect(&reply, "PUT", "（建桶）").map(|_| false),
        }
    }
}

impl RemoteBackend for S3Backend {
    fn ensure_root(&mut self) -> Result<bool, BackendError> {
        self.create_bucket()
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        crate::KIND_S3
    }

    fn describe(&self) -> String {
        let style = if self.path_style {
            "path-style"
        } else {
            "virtual-host"
        };
        let prefix = if self.prefix.is_empty() {
            "（桶根）".to_string()
        } else {
            format!("/{}", self.prefix)
        };
        format!(
            "S3 {}/{}（{}，{style}）",
            self.endpoint, self.bucket, prefix
        )
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<RemoteObject>, BackendError> {
        // S3 的列举就是按键前缀；前缀为空时列整个前缀空间
        let list_prefix = self.full_key(prefix);
        let mut objects = Vec::new();
        let mut token: Option<String> = None;

        loop {
            let mut query: Vec<(String, String)> = vec![
                ("list-type".to_string(), "2".to_string()),
                ("max-keys".to_string(), LIST_PAGE_SIZE.to_string()),
            ];
            if !list_prefix.is_empty() {
                query.push(("prefix".to_string(), list_prefix.clone()));
            }
            if let Some(token) = &token {
                query.push(("continuation-token".to_string(), token.clone()));
            }

            let reply = self.request("GET", "", &query, Vec::new(), &[])?;

            self.expect(&reply, "GET", &format!("?list-type=2&prefix={list_prefix}"))?;

            let page = parse_list_page(&reply.body)?;
            for mut object in page.objects {
                object.path = self.relative_key(&object.path);
                objects.push(object);
            }
            if objects.len() >= MAX_LIST_OBJECTS {
                log::warn!("列举结果超过 {MAX_LIST_OBJECTS} 个对象，先按当前批次处理");
                break;
            }
            if !page.truncated {
                break;
            }
            match page.next_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }

        Ok(objects)
    }

    fn head(&mut self, path: &str) -> Result<Option<RemoteObject>, BackendError> {
        let key = self.full_key(path);
        let reply = self.request("HEAD", &key, &[], Vec::new(), &[])?;
        match reply.status {
            200 => Ok(Some(RemoteObject {
                path: path.trim_start_matches('/').to_string(),
                etag: reply.etag,
                last_modified_ms: reply.last_modified_ms,
                size: reply.size,
                is_collection: false,
            })),
            404 => Ok(None),

            _ => {
                self.expect(&reply, "HEAD", &key)?;
                Ok(None)
            }
        }
    }

    fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(self.get_with_meta(path)?.map(|(bytes, _)| bytes))
    }

    fn get_with_meta(
        &mut self,
        path: &str,
    ) -> Result<Option<(Vec<u8>, RemoteObject)>, BackendError> {
        let key = self.full_key(path);
        let reply = self.request("GET", &key, &[], Vec::new(), &[])?;
        match reply.status {
            200 => {
                let meta = RemoteObject {
                    path: path.trim_start_matches('/').to_string(),
                    etag: reply.etag,
                    last_modified_ms: reply.last_modified_ms,
                    size: Some(reply.body.len() as u64),
                    is_collection: false,
                };
                Ok(Some((reply.body, meta)))
            }
            404 => Ok(None),

            _ => {
                self.expect(&reply, "GET", &key)?;
                Ok(None)
            }
        }
    }

    fn put(
        &mut self,
        path: &str,
        bytes: &[u8],
        only_if_absent: bool,
    ) -> Result<PutOutcome, BackendError> {
        let key = self.full_key(path);
        let extra: Vec<(&str, &str)> = if only_if_absent {
            vec![("if-none-match", "*")]
        } else {
            Vec::new()
        };

        let mut reply = self.request("PUT", &key, &[], bytes.to_vec(), &extra)?;

        // 桶还不存在（自建 MinIO 的首次使用）：建桶后重试一次。
        // 这与 WebDAV 侧"父目录缺失 → MKCOL → 重试"是同一套思路。
        if reply.status == 404 && parse_error(&reply.body).0.as_deref() == Some("NoSuchBucket") {
            if self.create_bucket()? {
                log::info!("后端 {} 的桶 {} 不存在，已创建", self.name, self.bucket);
            }
            reply = self.request("PUT", &key, &[], bytes.to_vec(), &extra)?;
        }

        match reply.status {
            // S3 对"新建"与"覆盖"都返回 200，无法区分；调用方只关心 AlreadyExists，
            // 因此这里统一报 Created，不假装知道是覆盖（WebDAV 能靠 201/204 区分）
            200 | 201 | 204 => Ok(PutOutcome::Created {
                etag: reply.etag.clone(),
            }),
            412 => Ok(PutOutcome::AlreadyExists),
            // 有些实现不支持条件写：退化为"先查再写"
            400 | 501 if only_if_absent => {
                log::debug!("后端 {} 不支持 If-None-Match，退化为 HEAD + PUT", self.name);
                match self.head(path)? {
                    Some(_) => Ok(PutOutcome::AlreadyExists),
                    None => {
                        let reply = self.request("PUT", &key, &[], bytes.to_vec(), &[])?;
                        self.expect(&reply, "PUT", &key)?;
                        Ok(PutOutcome::Created {
                            etag: reply.etag.clone(),
                        })
                    }
                }
            }
            409 => Err(BackendError::Conflict(format!(
                "写入 {key} 冲突（HTTP 409，检查桶是否存在/是否有权限）"
            ))),

            _ => {
                self.expect(&reply, "PUT", &key)?;
                Ok(PutOutcome::Replaced {
                    etag: reply.etag.clone(),
                })
            }
        }
    }

    fn delete(&mut self, path: &str) -> Result<(), BackendError> {
        let key = self.full_key(path);
        let reply = self.request("DELETE", &key, &[], Vec::new(), &[])?;
        match reply.status {
            // S3 的 DELETE 是幂等的：对象不存在也返回 204
            200 | 202 | 204 | 404 => Ok(()),

            _ => self.expect(&reply, "DELETE", &key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> S3Options {
        S3Options {
            name: "s3test".to_string(),
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            region: "us-east-1".to_string(),
            bucket: "clipboardx".to_string(),
            prefix: "space-a".to_string(),
            access_key_id: "minioadmin".to_string(),
            secret_access_key: "minioadmin".to_string(),
            session_token: None,
            path_style: true,
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn 参数校验() {
        let mut bad = options();
        bad.bucket = " ".to_string();
        assert!(matches!(S3Backend::new(bad), Err(BackendError::Config(_))));

        let mut bad = options();
        bad.secret_access_key = String::new();
        assert!(matches!(S3Backend::new(bad), Err(BackendError::Config(_))));

        let mut bad = options();
        bad.endpoint = Some("ftp://x".to_string());
        assert!(matches!(S3Backend::new(bad), Err(BackendError::Config(_))));
    }

    #[test]
    fn 端点规范化() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:9000/").unwrap(),
            "http://127.0.0.1:9000"
        );
        // 本机与内网默认 http
        assert_eq!(
            normalize_endpoint("192.168.1.5:9000").unwrap(),
            "http://192.168.1.5:9000"
        );
        // 公网默认 https
        assert_eq!(
            normalize_endpoint("s3.example.com").unwrap(),
            "https://s3.example.com"
        );
    }

    #[test]
    fn 键名与前缀() {
        let backend = S3Backend::new(options()).unwrap();
        assert_eq!(
            backend.full_key("v1/manifests/a.json"),
            "space-a/v1/manifests/a.json"
        );
        assert_eq!(
            backend.full_key("/v1/items/x.cxi"),
            "space-a/v1/items/x.cxi"
        );
        assert_eq!(
            backend.relative_key("space-a/v1/manifests/a.json"),
            "v1/manifests/a.json"
        );
        // 前缀外的键原样返回（不该发生，但不要静默丢数据）
        assert_eq!(backend.relative_key("other/key"), "other/key");

        let mut no_prefix = options();
        no_prefix.prefix = String::new();
        let backend = S3Backend::new(no_prefix).unwrap();
        assert_eq!(
            backend.full_key("v1/manifests/a.json"),
            "v1/manifests/a.json"
        );
        assert_eq!(
            backend.relative_key("v1/manifests/a.json"),
            "v1/manifests/a.json"
        );
    }

    #[test]
    fn url_构造_path_style_与_virtual_host() {
        let backend = S3Backend::new(options()).unwrap();
        let (url, path) = backend.target("v1/items/a b.cxi", &[]);
        assert_eq!(url, "http://127.0.0.1:9000/clipboardx/v1/items/a%20b.cxi");
        assert_eq!(path, "/clipboardx/v1/items/a%20b.cxi");

        let mut virtual_host = options();
        virtual_host.path_style = false;
        virtual_host.endpoint = Some("https://s3.us-east-1.amazonaws.com".to_string());
        let backend = S3Backend::new(virtual_host).unwrap();
        let (url, path) = backend.target("v1/items/a.cxi", &[]);
        assert_eq!(
            url,
            "https://clipboardx.s3.us-east-1.amazonaws.com/v1/items/a.cxi"
        );
        assert_eq!(path, "/v1/items/a.cxi");
    }

    #[test]
    fn 解析_list_objects_v2() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
 <Name>clipboardx</Name>
 <Prefix>space-a/v1/</Prefix>
 <KeyCount>2</KeyCount>
 <MaxKeys>1000</MaxKeys>
 <IsTruncated>true</IsTruncated>
 <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
 <Contents>
  <Key>space-a/v1/manifests/a&amp;b.json</Key>
  <LastModified>2025-10-01T13:06:38.000Z</LastModified>
  <ETag>&quot;5f3c1e77&quot;</ETag>
  <Size>348</Size>
 </Contents>
 <Contents>
  <Key>space-a/v1/manifests/c.json</Key>
  <LastModified>2025-10-01T13:06:39.000Z</LastModified>
  <ETag>&quot;aa11&quot;</ETag>
  <Size>10</Size>
 </Contents>
</ListBucketResult>"#;
        let page = parse_list_page(body).unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].path, "space-a/v1/manifests/a&b.json");
        assert_eq!(page.objects[0].etag.as_deref(), Some("\"5f3c1e77\""));
        assert_eq!(page.objects[0].size, Some(348));
        assert_eq!(page.objects[0].last_modified_ms, Some(1_759_323_998_000));
        assert!(page.truncated);
        assert_eq!(
            page.next_token.as_deref(),
            Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
        );
    }

    #[test]
    fn 解析空列举结果() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult><Name>b</Name><Prefix>x/</Prefix><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        let page = parse_list_page(body).unwrap();
        assert!(page.objects.is_empty());
        assert!(!page.truncated);
        assert!(page.next_token.is_none());
    }

    #[test]
    fn 解析错误响应() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message><BucketName>x</BucketName></Error>"#;
        let (code, message) = parse_error(body);
        assert_eq!(code.as_deref(), Some("NoSuchBucket"));
        assert_eq!(
            message.as_deref(),
            Some("The specified bucket does not exist")
        );

        // 不是 XML 时不能 panic
        assert_eq!(parse_error(b"not xml"), (None, None));
        assert_eq!(parse_error(b""), (None, None));
    }

    #[test]
    fn 错误状态映射() {
        let backend = S3Backend::new(options()).unwrap();
        let make = |status: u16, body: &[u8]| S3Reply {
            status,
            etag: None,
            last_modified_ms: None,
            size: None,
            server_date_ms: None,
            body: body.to_vec(),
        };
        let unauthorized = backend
            .expect(&make(403, br#"<Error><Code>SignatureDoesNotMatch</Code><Message>bad sig</Message></Error>"#), "GET", "k")
            .unwrap_err();
        assert!(unauthorized.is_auth(), "{unauthorized}");
        assert!(!unauthorized.is_retryable());

        let missing = backend.expect(&make(404, b""), "GET", "k").unwrap_err();
        assert!(matches!(missing, BackendError::NotFound(_)));

        let server = backend.expect(&make(503, b""), "GET", "k").unwrap_err();
        assert!(server.is_retryable(), "{server}");
    }

    #[test]
    fn 描述文本包含关键信息() {
        let backend = S3Backend::new(options()).unwrap();
        let text = backend.describe();
        assert!(text.contains("127.0.0.1:9000"), "{text}");
        assert!(text.contains("clipboardx"), "{text}");
        assert!(text.contains("path-style"), "{text}");
    }

    #[test]
    fn 时钟校正后的时间本地可变() {
        let mut backend = S3Backend::new(options()).unwrap();
        assert_eq!(backend.server_now_ms(), cx_core::now_ms() as i64);
        backend.clock_skew_ms = 5_000;
        assert!(backend.server_now_ms() > cx_core::now_ms() as i64 + 4_000);
    }
}
