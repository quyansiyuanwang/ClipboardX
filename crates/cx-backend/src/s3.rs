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
//! 条件写使用 `If-None-Match: *`；显式不支持时 fail-closed，
//! 不用有覆盖竞态的 HEAD + 无条件 PUT 冒充不可变对象写入。

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
    hints: std::collections::BTreeMap<String, String>,
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
            None => normalize_endpoint(&format!("https://s3.{region}.amazonaws.com"))?,
        };
        let parsed = url::Url::parse(&endpoint)
            .map_err(|_| BackendError::Config("S3 endpoint URL 非法".into()))?;
        let bucket = options.bucket.trim();
        if bucket
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '.' && ch != '_')
        {
            return Err(BackendError::Config("S3 桶名包含非法字符".into()));
        }
        if !options.path_style {
            if parsed
                .host()
                .is_some_and(|host| !matches!(host, url::Host::Domain(_)))
            {
                return Err(BackendError::Config(
                    "virtual-host 寻址需要 DNS endpoint；IP endpoint 请显式使用 path_style=true"
                        .into(),
                ));
            }
            if parsed
                .host_str()
                .is_some_and(|host| host.starts_with(&format!("{bucket}.")))
            {
                return Err(BackendError::Config(
                    "S3 endpoint 已包含桶名；请配置服务 endpoint，避免重复桶前缀".into(),
                ));
            }
            let mut virtual_url = parsed.clone();
            virtual_url
                .set_host(Some(&format!(
                    "{bucket}.{}",
                    parsed.host_str().unwrap_or_default()
                )))
                .map_err(|_| BackendError::Config("virtual-host 桶名或 endpoint 非法".into()))?;
        }

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
            format!("{}/", self.prefix)
        } else {
            format!("{}/{}", self.prefix, relative)
        }
    }

    /// 把服务端返回的键名还原成相对路径。
    fn relative_key(&self, key: &str) -> Result<String, BackendError> {
        if self.prefix.is_empty() {
            return Ok(key.to_string());
        }
        key.strip_prefix(&format!("{}/", self.prefix))
            .map(str::to_string)
            .ok_or_else(|| BackendError::Malformed("LIST 返回了配置前缀之外的对象".into()))
    }

    /// Build one final URL; its authority is also used verbatim for signing.
    fn target(
        &self,
        key: &str,
        _query: &[(String, String)],
    ) -> Result<(String, String), BackendError> {
        // URL libraries normalize dot segments; never sign one path and send another.
        if key
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(BackendError::Config(
                "S3 key 不能包含 . 或 .. 路径段".into(),
            ));
        }
        let encoded_key = canonical_uri(key);
        let path = if self.path_style {
            format!("/{}/{}", self.bucket, encoded_key.trim_start_matches('/'))
        } else {
            format!("/{}", encoded_key.trim_start_matches('/'))
        };
        let mut url = url::Url::parse(&self.endpoint)
            .map_err(|_| BackendError::Config("S3 endpoint URL 非法".into()))?;
        if !self.path_style {
            let host = format!("{}.{}", self.bucket, url.host_str().unwrap_or_default());
            url.set_host(Some(&host))
                .map_err(|_| BackendError::Config("S3 virtual-host URL 非法".into()))?;
        }
        url.set_path(&path);
        if url.path() != path {
            return Err(BackendError::Config("S3 key 无法无损表示为 URL".into()));
        }
        Ok((url.to_string(), path))
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

        let (url, path) = self.target(key, query)?;
        let parsed =
            url::Url::parse(&url).map_err(|_| BackendError::Config("S3 请求 URL 非法".into()))?;
        let signing_host = parsed[url::Position::BeforeHost..url::Position::AfterPort].to_string();

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
        let mut builder = attohttpc::RequestBuilder::try_new(http_method, &url)
            .map_err(|_| BackendError::Config("S3 请求 URL 非法".into()))?;
        builder = configure(builder, &self.settings)?;
        for (name, value) in &signed.headers {
            let header_name = attohttpc::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|err| BackendError::Config(format!("请求头名 {name} 非法：{err}")))?;
            let header_value = attohttpc::header::HeaderValue::from_str(value)
                .map_err(|err| BackendError::Config(format!("请求头 {name} 的值非法：{err}")))?;
            builder = builder.header(header_name, header_value);
        }

        let response = builder.bytes(body).send().map_err(|err| {
            BackendError::Transport(self.redact(&format!("S3 {method} 请求失败：{err}")))
        })?;

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
        let hints = [
            ("RequestId", "x-oss-request-id"),
            ("RequestId", "x-amz-request-id"),
            ("EC", "x-oss-ec"),
            ("Region", "x-amz-bucket-region"),
            ("Endpoint", "x-oss-endpoint"),
        ]
        .into_iter()
        .filter_map(|(field, name)| header(name).map(|value| (field.to_string(), value)))
        .collect();
        let body = response.bytes().map_err(|err| {
            BackendError::Transport(self.redact(&format!("读取 S3 {method} 响应体失败：{err}")))
        })?;

        Ok(S3Reply {
            status,
            etag,
            last_modified_ms,
            size,
            server_date_ms,
            body,
            hints,
        })
    }

    fn redact(&self, value: &str) -> String {
        let lower = value.to_ascii_lowercase();
        if [
            "authorization",
            "credential=",
            "signature=",
            "security-token",
            "accesskeysecret",
            "secretaccesskey",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            return "[redacted sensitive server detail]".into();
        }
        let mut clean = value.to_string();
        for secret in [&self.access_key_id, &self.secret_access_key]
            .into_iter()
            .chain(self.session_token.iter())
        {
            if !secret.is_empty() {
                clean = clean.replace(secret, "[redacted]");
                clean = clean.replace(&crate::signing::uri_encode(secret, true), "[redacted]");
            }
        }
        clean
            .chars()
            .filter(|ch| !ch.is_control())
            .take(512)
            .collect()
    }

    /// 把非 2xx 响应变成错误（`ok_statuses` 之外的状态都算失败）。
    fn expect(&self, reply: &S3Reply, method: &str, key: &str) -> Result<(), BackendError> {
        if (200..300).contains(&reply.status) {
            return Ok(());
        }
        let info = parse_error_fields(&reply.body);
        let code = info.get("Code").map(String::as_str);
        let is_clock_skew = code == Some("RequestTimeTooSkewed");
        let mut detail = format!("S3 {method} HTTP {}", reply.status);
        for field in [
            "Code",
            "Message",
            "EC",
            "RequestId",
            "HostId",
            "Endpoint",
            "Region",
        ] {
            if let Some(value) = info.get(field) {
                detail.push_str(&format!("; {field}={}", self.redact(value)));
            }
        }
        for (field, value) in &reply.hints {
            if !info.contains_key(field) {
                detail.push_str(&format!("; {field}={}", self.redact(value)));
            }
        }
        let _ = key; // Object names and opaque pagination tokens are not diagnostic output.
        log::debug!("{detail}");
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

/// Shared addressing default: OSS and AWS service endpoints use virtual hosts;
/// preserve path-style for other custom services (MinIO, Ceph, local servers).
pub fn default_path_style(endpoint: Option<&str>) -> bool {
    let Some(endpoint) = endpoint else {
        return false;
    };
    let Ok(endpoint) = normalize_endpoint(endpoint) else {
        return true;
    };
    let Ok(url) = url::Url::parse(&endpoint) else {
        return true;
    };
    let host = url.host_str().unwrap_or_default();
    !(host.ends_with(".aliyuncs.com") && (host.starts_with("oss-") || host.starts_with("s3.oss-"))
        || host == "s3.amazonaws.com"
        || host.starts_with("s3.") && host.ends_with(".amazonaws.com"))
}

/// Endpoint must be a service origin, never credentials, a bucket path or query.
fn normalize_endpoint(endpoint: &str) -> Result<String, BackendError> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) || trimmed.contains('\\') {
        return Err(BackendError::Config(
            "S3 endpoint 为空或包含非法字符".into(),
        ));
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        let local = trimmed.starts_with("127.0.0.1")
            || trimmed.starts_with("localhost")
            || trimmed.starts_with("192.168.")
            || trimmed.starts_with("10.")
            || trimmed.starts_with("[::1]");
        format!("{}://{trimmed}", if local { "http" } else { "https" })
    };
    let raw_origin = with_scheme
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_default();
    let (authority, raw_path) = raw_origin.split_once('/').unwrap_or((raw_origin, ""));
    if authority.is_empty() || authority.contains('@') || !raw_path.is_empty() {
        return Err(BackendError::Config(
            "S3 endpoint 只能是服务 origin，不能包含凭据或路径".into(),
        ));
    }
    let parsed = url::Url::parse(&with_scheme).map_err(|_| {
        BackendError::Config("S3 endpoint URL 非法；请使用 http(s)://host[:port]".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(BackendError::Config("S3 endpoint 必须是 http(s)://host[:port] 服务地址，不能含凭据、路径、query 或 fragment".into()));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// Parse a diagnostic whitelist, never raw headers/body or credential echo fields.
fn parse_error_fields(body: &[u8]) -> std::collections::BTreeMap<String, String> {
    let mut reader = Reader::from_reader(body);
    let mut fields = std::collections::BTreeMap::new();
    let mut current = None;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(event)) => {
                let name = event.local_name();
                current = Some(name.as_ref())
                    .filter(|name| {
                        [
                            "Code",
                            "Message",
                            "EC",
                            "RequestId",
                            "HostId",
                            "Endpoint",
                            "Region",
                        ]
                        .contains(name)
                    })
                    .map(str::to_string);
                text.clear();
            }
            Ok(XmlEvent::Text(chunk)) if current.is_some() => {
                if let Ok(value) = quick_xml::escape::unescape(&chunk.into_inner()) {
                    text.push_str(&value);
                }
            }
            Ok(XmlEvent::CData(chunk)) if current.is_some() => {
                text.push_str(chunk.as_ref());
            }
            Ok(XmlEvent::GeneralRef(reference)) if current.is_some() => {
                if let Some(value) = resolve_entity(&reference.into_inner()) {
                    text.push_str(&value);
                }
            }
            Ok(XmlEvent::End(event)) => {
                if let Some(name) = current.take()
                    && event.local_name().as_ref() == name.as_str()
                {
                    fields.insert(name, std::mem::take(&mut text));
                }
            }
            Ok(XmlEvent::Eof) | Err(_) => break,
            _ => {}
        }
    }
    fields
}

fn parse_error(body: &[u8]) -> (Option<String>, Option<String>) {
    let mut fields = parse_error_fields(body);
    (fields.remove("Code"), fields.remove("Message"))
}

fn conditional_write_unsupported(body: &[u8]) -> bool {
    let (code, message) = parse_error(body);
    matches!(
        code.as_deref(),
        Some("NotImplemented" | "NotSupported" | "UnsupportedHeader" | "UnsupportedOperation")
    ) && message.is_some_and(|message| {
        let message = message.to_ascii_lowercase();
        message.contains("if-none-match")
            || message.contains("conditional write")
            || message.contains("conditional request")
    })
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
    let mut depth = 0usize;
    let mut root_seen = false;
    let mut root_closed = false;
    let mut truncated_seen = false;

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
                    entry.size = value.trim().parse::<u64>().ok();
                }
            }
            Field::LastModified => {
                if in_contents {
                    entry.modified_ms = parse_http_date(&value);
                }
            }
            Field::Truncated => page.truncated = value.trim() == "true",
            Field::NextToken => page.next_token = Some(value),
        }
    }

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(XmlEvent::Start(event)) => {
                if depth == 0 {
                    if root_seen || event.local_name().as_ref() != "ListBucketResult" {
                        return Err(BackendError::Malformed(
                            "LIST 响应根节点不是 ListBucketResult".into(),
                        ));
                    }
                    root_seen = true;
                }
                let name = event.local_name().into_inner();
                if field.is_some()
                    || matches!(name, "Contents" | "IsTruncated" | "NextContinuationToken")
                        && depth != 1
                    || matches!(name, "Key" | "ETag" | "Size" | "LastModified")
                        && (depth != 2 || !in_contents)
                    || name == "IsTruncated" && truncated_seen
                {
                    return Err(BackendError::Malformed(
                        "LIST 字段嵌套、重复或层级非法".into(),
                    ));
                }
                depth += 1;
                if name == "IsTruncated" {
                    truncated_seen = true;
                }
                match name {
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
                }
            }
            Ok(XmlEvent::Empty(event)) => {
                if depth == 0
                    || ["Contents", "Key", "IsTruncated"].contains(&event.local_name().as_ref())
                {
                    return Err(BackendError::Malformed("LIST 响应包含空或非法节点".into()));
                }
            }
            Ok(XmlEvent::CData(chunk)) => {
                if field.is_some() {
                    text.push_str(chunk.as_ref());
                }
            }
            Ok(XmlEvent::Text(chunk)) => {
                if depth == 0
                    && chunk
                        .as_ref()
                        .bytes()
                        .any(|byte| !byte.is_ascii_whitespace())
                {
                    return Err(BackendError::Malformed("LIST 响应包含根节点外文本".into()));
                }
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
                if depth == 0 {
                    return Err(BackendError::Malformed("LIST XML 节点层级非法".into()));
                }
                depth -= 1;
                if depth == 0 {
                    root_closed = true;
                }
                if field == Some(Field::Truncated) && !matches!(text.trim(), "true" | "false") {
                    return Err(BackendError::Malformed(
                        "LIST IsTruncated 不是布尔值".into(),
                    ));
                }
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
                    if entry.key.is_empty() {
                        return Err(BackendError::Malformed("LIST Contents 缺少有效 Key".into()));
                    }
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
    if !root_seen || !root_closed || depth != 0 || !truncated_seen {
        return Err(BackendError::Malformed(
            "LIST 响应为空、不完整或缺少 IsTruncated".into(),
        ));
    }
    if page.truncated && page.next_token.as_ref().is_none_or(String::is_empty) {
        return Err(BackendError::Malformed(
            "LIST 已截断但缺少 continuation token".into(),
        ));
    }
    Ok(page)
}

impl S3Backend {
    /// 创建桶（已存在视为成功）。用于自建 MinIO 的首次使用。
    ///
    /// `target("")` 已经处理好两种寻址：path-style 会请求 `PUT /bucket/`，
    /// virtual-host 会请求 `PUT /`（桶名在 Host 里）。
    pub fn create_bucket(&mut self) -> Result<bool, BackendError> {
        let endpoint = url::Url::parse(&self.endpoint)
            .map_err(|_| BackendError::Config("S3 endpoint URL 非法".into()))?;
        let is_oss = endpoint
            .host_str()
            .is_some_and(|host| host.ends_with(".aliyuncs.com"));
        let body = if self.region != "us-east-1" && !is_oss {
            if self
                .region
                .chars()
                .any(|ch| !ch.is_ascii_alphanumeric() && ch != '-')
            {
                return Err(BackendError::Config("建桶 region 包含非法字符".into()));
            }
            format!("<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>", self.region).into_bytes()
        } else {
            Vec::new()
        };
        let reply = self.request("PUT", "", &[], body, &[])?;
        match reply.status {
            200 | 204 => Ok(true),
            409 | 403
                if parse_error(&reply.body).0.as_deref() == Some("BucketAlreadyOwnedByYou") =>
            {
                Ok(false)
            }
            _ => self.expect(&reply, "PUT create_bucket", "").map(|_| false),
        }
    }
}

impl RemoteBackend for S3Backend {
    fn ensure_root(&mut self) -> Result<bool, BackendError> {
        let query = vec![
            ("list-type".into(), "2".into()),
            ("max-keys".into(), "1".into()),
            ("prefix".into(), self.full_key("")),
        ];
        let reply = self.request("GET", "", &query, Vec::new(), &[])?;
        self.expect(&reply, "GET ensure_root LIST", "")?;
        parse_list_page(&reply.body)?;
        Ok(false)
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
        let mut seen_tokens = std::collections::HashSet::new();
        let mut pages = 0;

        loop {
            pages += 1;
            if pages > MAX_LIST_OBJECTS {
                return Err(BackendError::Malformed(
                    "LIST 分页次数超过安全上限；未返回不完整结果".into(),
                ));
            }
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

            self.expect(&reply, "GET LIST", "")?;

            let page = parse_list_page(&reply.body)?;
            for mut object in page.objects {
                if !object.path.starts_with(&list_prefix) {
                    return Err(BackendError::Malformed(
                        "LIST 返回了请求前缀之外的对象".into(),
                    ));
                }
                object.path = self.relative_key(&object.path)?;
                objects.push(object);
            }
            if objects.len() > MAX_LIST_OBJECTS
                || objects.len() == MAX_LIST_OBJECTS && page.truncated
            {
                return Err(BackendError::Malformed(format!(
                    "LIST 超过 {MAX_LIST_OBJECTS} 个对象；未返回不完整结果"
                )));
            }
            if !page.truncated {
                break;
            }
            let next = page
                .next_token
                .ok_or_else(|| BackendError::Malformed("LIST 缺少 continuation token".into()))?;
            if !seen_tokens.insert(next.clone()) {
                return Err(BackendError::Malformed(
                    "LIST continuation token 重复；未返回不完整结果".into(),
                ));
            }
            token = Some(next);
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

        let reply = self.request("PUT", &key, &[], bytes.to_vec(), &extra)?;

        match reply.status {
            // S3 对"新建"与"覆盖"都返回 200，无法区分；调用方只关心 AlreadyExists，
            // 因此这里统一报 Created，不假装知道是覆盖（WebDAV 能靠 201/204 区分）
            200 | 201 | 204 => Ok(PutOutcome::Created {
                etag: reply.etag.clone(),
            }),
            412 => Ok(PutOutcome::AlreadyExists),
            // An explicitly unsupported condition is a capability failure, not
            // permission to race HEAD against an unconditional overwriting PUT.
            400 | 501 if only_if_absent && conditional_write_unsupported(&reply.body) => {
                let provider_error = self
                    .expect(&reply, "PUT conditional", "")
                    .err()
                    .map(|err| err.to_string())
                    .unwrap_or_default();
                Err(BackendError::Config(format!(
                    "{provider_error}；S3 条件写能力不受支持；为避免覆盖不可变对象，拒绝非原子 HEAD+PUT 降级"
                )))
            }

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
            backend.relative_key("space-a/v1/manifests/a.json").unwrap(),
            "v1/manifests/a.json"
        );
        assert!(backend.relative_key("other/key").is_err());
        assert!(backend.relative_key("space-ab/key").is_err());
        assert_eq!(backend.full_key(""), "space-a/");

        let mut no_prefix = options();
        no_prefix.prefix = String::new();
        let backend = S3Backend::new(no_prefix).unwrap();
        assert_eq!(
            backend.full_key("v1/manifests/a.json"),
            "v1/manifests/a.json"
        );
        assert_eq!(
            backend.relative_key("v1/manifests/a.json").unwrap(),
            "v1/manifests/a.json"
        );
    }

    #[test]
    fn url_构造_path_style_与_virtual_host() {
        let backend = S3Backend::new(options()).unwrap();
        let (url, path) = backend.target("v1/items/a b.cxi", &[]).unwrap();
        assert_eq!(url, "http://127.0.0.1:9000/clipboardx/v1/items/a%20b.cxi");
        assert_eq!(path, "/clipboardx/v1/items/a%20b.cxi");

        let mut virtual_host = options();
        virtual_host.path_style = false;
        virtual_host.endpoint = Some("https://s3.us-east-1.amazonaws.com".to_string());
        let backend = S3Backend::new(virtual_host).unwrap();
        let (url, path) = backend.target("v1/items/a.cxi", &[]).unwrap();
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
            hints: Default::default(),
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
    fn oss_defaults_and_bucket_level_virtual_host_urls() {
        assert!(!default_path_style(None));
        for endpoint in [
            "https://oss-cn-beijing.aliyuncs.com",
            "https://s3.oss-cn-beijing.aliyuncs.com",
            "https://oss-cn-beijing-internal.aliyuncs.com",
            "https://s3.eu-west-1.amazonaws.com",
        ] {
            assert!(!default_path_style(Some(endpoint)), "{endpoint}");
        }
        assert!(default_path_style(Some("http://localhost:9000")));
        assert!(default_path_style(Some(
            "https://oss-cn-beijing.aliyuncs.com.attacker.example"
        )));
        let mut opts = options();
        opts.endpoint = Some("https://oss-cn-beijing.aliyuncs.com:9443".into());
        opts.path_style = false;
        let backend = S3Backend::new(opts).unwrap();
        let (target, path) = backend.target("", &[]).unwrap();
        assert_eq!(
            target,
            "https://clipboardx.oss-cn-beijing.aliyuncs.com:9443/"
        );
        assert_eq!(path, "/");
        let parsed = url::Url::parse(&target).unwrap();
        assert_eq!(
            &parsed[url::Position::BeforeHost..url::Position::AfterPort],
            "clipboardx.oss-cn-beijing.aliyuncs.com:9443"
        );
    }

    #[test]
    fn rejects_invalid_endpoints_without_echoing_secrets_or_panicking() {
        for endpoint in [
            "https://",
            "https://a:b@host",
            "https://host/bucket",
            "https://host?token=secret",
            "https://host#secret",
            "https://[broken",
            "https://host:bad",
            "https://host/../bucket",
            "https://host\\evil",
            "https://host\nsecret",
        ] {
            let err = normalize_endpoint(endpoint).unwrap_err();
            assert!(matches!(err, BackendError::Config(_)));
            assert!(!err.to_string().contains("secret"));
        }
        let backend = S3Backend::new(options()).unwrap();
        assert!(backend.target("a/../b", &[]).is_err());
        let mut opts = options();
        opts.path_style = false;
        opts.endpoint = Some("https://clipboardx.oss-cn-beijing.aliyuncs.com".into());
        assert!(S3Backend::new(opts).is_err());
    }

    #[test]
    fn list_parser_preserves_opaque_whitespace_and_rejects_non_list_documents() {
        let page = parse_list_page(b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken> token &amp; </NextContinuationToken><Contents><Key> key </Key></Contents></ListBucketResult>").unwrap();
        assert_eq!(page.next_token.as_deref(), Some(" token & "));
        assert_eq!(page.objects[0].path, " key ");
        for body in [
            "",
            "<html></html>",
            "<Error><Code>AccessDenied</Code></Error>",
            "<ListBucketResult>",
            "<ListBucketResult/>",
            "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>maybe</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>false</IsTruncated><IsTruncated>true</IsTruncated></ListBucketResult>",
            "<ListBucketResult><Other><IsTruncated>false</IsTruncated></Other></ListBucketResult>",
        ] {
            assert!(parse_list_page(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn arbitrary_bad_request_is_not_a_capability_failure() {
        assert!(!conditional_write_unsupported(
            b"<Error><Code>InvalidArgument</Code><Message>if-none-match invalid</Message></Error>"
        ));
        assert!(!conditional_write_unsupported(
            b"<Error><Code>NotImplemented</Code><Message>other operation</Message></Error>"
        ));
        assert!(conditional_write_unsupported(b"<Error><Code>NotImplemented</Code><Message>conditional writes are not supported</Message></Error>"));
    }

    #[test]
    fn diagnostics_preserve_ec_request_id_endpoint_but_redact_secrets() {
        let backend = S3Backend::new(options()).unwrap();
        let reply = S3Reply {
            status: 403, etag: None, last_modified_ms: None, size: None, server_date_ms: None,
            body: b"<Error><Code>SecondLevelDomainForbidden</Code><Message>use virtual host minioadmin</Message><EC>0003-00001401</EC><RequestId>test-request</RequestId><Endpoint>oss-cn-beijing.aliyuncs.com</Endpoint><Authorization>DO-NOT-LOG</Authorization></Error>".to_vec(),
            hints: [("Region".into(), "cn-beijing".into())].into_iter().collect(),
        };
        let err = backend
            .expect(&reply, "GET LIST", "private/object")
            .unwrap_err()
            .to_string();
        for expected in [
            "403",
            "GET LIST",
            "SecondLevelDomainForbidden",
            "0003-00001401",
            "test-request",
            "oss-cn-beijing.aliyuncs.com",
            "cn-beijing",
        ] {
            assert!(err.contains(expected), "{err}");
        }
        for secret in ["minioadmin", "DO-NOT-LOG", "private/object"] {
            assert!(!err.contains(secret), "{err}");
        }
        assert_eq!(
            backend.redact("Authorization: AWS4 foo"),
            "[redacted sensitive server detail]"
        );
    }

    #[test]
    fn 时钟校正后的时间本地可变() {
        let mut backend = S3Backend::new(options()).unwrap();
        assert_eq!(backend.server_now_ms(), cx_core::now_ms() as i64);
        backend.clock_skew_ms = 5_000;
        assert!(backend.server_now_ms() > cx_core::now_ms() as i64 + 4_000);
    }
}
