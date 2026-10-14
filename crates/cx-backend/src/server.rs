//! 自建服务端（`cx-server`）后端。
//!
//! 协议见 `docs/protocol.md` §9。与 WebDAV/S3 的关系：
//! - 同样是"对象存储"抽象，因此引擎完全不需要改动；
//! - 但它是**我们知道对端实现**的接口，所以可以做得比通用协议更直接：
//!   列举一次拿全（服务端做了前缀过滤与分页）、ETag 由服务端按内容哈希给出、
//!   配额用尽返回 507 时**不该重试**（重试不会让空间变大）。
//!
//! 鉴权：`Authorization: Bearer <设备令牌>`。令牌存在 `secrets/` 下（0600），
//! 令牌本身标识空间，因此 `space` 配置只用于"顺手校验一下没连错空间"。

use crate::error::BackendError;
use crate::http::{HttpSettings, configure};
use crate::{PutOutcome, RemoteBackend, RemoteObject};
use std::path::PathBuf;
use std::time::Duration;

/// 单次列举返回的对象上限（服务端也有自己的上限）。
const LIST_LIMIT: usize = 5000;

/// 构造参数。
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// 后端名（配置里的 `name`）。
    pub name: String,
    /// 服务端基址（例如 `http://192.168.1.10:8688`）。
    pub url: String,
    /// 空间 id（可选；配置了就会校验"没连错空间"）。
    pub space: Option<String>,
    /// 设备令牌。
    pub token: String,
    /// 自定义 CA（PEM）。
    pub ca_file: Option<PathBuf>,
    /// 跳过 TLS 校验（仅调试）。
    pub insecure: bool,
    /// 单次请求超时。
    pub timeout: Duration,
}

/// `cx-server` 后端。
pub struct ServerBackend {
    name: String,
    /// 规范化后的基址（无尾斜杠）。
    root: String,
    space: Option<String>,
    token: String,
    settings: HttpSettings,
}

/// 一次 HTTP 往返。
#[derive(Debug)]
struct Reply {
    status: u16,
    etag: Option<String>,
    body: Vec<u8>,
}

impl ServerBackend {
    /// 构造并校验 URL。
    pub fn new(options: ServerOptions) -> Result<Self, BackendError> {
        let root = normalize_root(&options.url)?;
        if options.token.trim().is_empty() {
            return Err(BackendError::Config(
                "自建服务端需要设备令牌（`cx-space` 注册或管理员签发）".to_string(),
            ));
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
            root,
            space: options.space,
            token: options.token,
            settings,
        })
    }

    fn url_for(&self, path: &str) -> String {
        format!("{}/{}", self.root, path.trim_start_matches('/'))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        extra_headers: &[(&str, &str)],
    ) -> Result<Reply, BackendError> {
        let mut url = self.url_for(path);
        if !query.is_empty() {
            let encoded: Vec<String> = query
                .iter()
                .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
                .collect();
            url = format!("{url}?{}", encoded.join("&"));
        }

        let http_method = attohttpc::Method::from_bytes(method.as_bytes())
            .map_err(|err| BackendError::Config(format!("非法 HTTP 方法 {method}：{err}")))?;
        let mut builder = attohttpc::RequestBuilder::new(http_method, &url);
        builder = configure(builder, &self.settings)?;
        let auth_name = attohttpc::header::HeaderName::from_static("authorization");
        let auth_value =
            attohttpc::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|err| BackendError::Config(format!("设备令牌含非法字符：{err}")))?;
        builder = builder.header(auth_name, auth_value);
        for (name, value) in extra_headers {
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
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let body = response.bytes().map_err(|err| {
            BackendError::Transport(format!("读取 {method} {url} 响应体失败：{err}"))
        })?;
        Ok(Reply { status, etag, body })
    }

    /// 把非 2xx 变成错误。
    fn expect(&self, reply: &Reply, method: &str, path: &str) -> Result<(), BackendError> {
        if (200..300).contains(&reply.status) {
            return Ok(());
        }
        let (kind, message) = parse_error(&reply.body);
        let detail = message.unwrap_or_else(|| format!("HTTP {}", reply.status));
        log::debug!(
            "服务端 {method} {path} 失败：{} {detail}（{}）",
            reply.status,
            kind.as_deref().unwrap_or("unknown")
        );
        Err(match reply.status {
            401 => BackendError::Unauthorized {
                status: 401,
                detail: format!("{detail}（令牌无效或已吊销：用 `cx space join` 重新注册）"),
            },
            403 => BackendError::Unauthorized {
                status: 403,
                detail,
            },
            404 => BackendError::NotFound(detail),
            409 => BackendError::Conflict(detail),
            // 配额用尽是"需要人处理"的错误：重试不会让空间变大
            413 => BackendError::Malformed(format!("对象过大：{detail}")),
            507 => BackendError::Malformed(format!("空间配额用尽：{detail}")),
            status => BackendError::Status { status, detail },
        })
    }
}

/// 自助注册：用空间的注册密钥换一个设备令牌（`POST /v1/enroll`）。
///
/// 放在后端模块而不是 CLI 里：这是服务端协议的一部分，未来 TUI 与安装脚本
/// 也要走同一条路径。
pub fn enroll(
    url: &str,
    space_id: &str,
    enroll_secret: &str,
    device_name: &str,
    ca_file: Option<PathBuf>,
    insecure: bool,
    timeout: Duration,
) -> Result<String, BackendError> {
    let root = normalize_root(url)?;
    let settings = HttpSettings {
        timeout,
        ca_file,
        insecure,
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "space_id": space_id,
        "enroll_secret": enroll_secret,
        "device_name": device_name,
    }))
    .map_err(|err| BackendError::Config(format!("无法构造注册请求：{err}")))?;

    let target = format!("{root}/v1/enroll");
    let mut builder = attohttpc::RequestBuilder::new(attohttpc::Method::POST, &target);
    builder = configure(builder, &settings)?;
    let content_type = attohttpc::header::HeaderName::from_static("content-type");
    let content_value = attohttpc::header::HeaderValue::from_static("application/json");
    builder = builder.header(content_type, content_value);

    let response = builder
        .bytes(body)
        .send()
        .map_err(|err| BackendError::Transport(format!("POST {target}：{err}")))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .map_err(|err| BackendError::Transport(format!("读取注册响应失败：{err}")))?;

    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|err| BackendError::Malformed(format!("注册响应不是 JSON：{err}")))?;
    if !(200..300).contains(&status) {
        let (_, message) = parse_error(&bytes);
        let detail = message.unwrap_or_else(|| format!("HTTP {status}"));
        return Err(match status {
            401 | 403 => BackendError::Unauthorized { status, detail },
            404 => BackendError::NotFound(detail),
            status => BackendError::Status { status, detail },
        });
    }
    parsed
        .get("device_token")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| BackendError::Malformed("注册响应缺少 device_token".to_string()))
}

impl RemoteBackend for ServerBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        crate::KIND_SERVER
    }

    fn describe(&self) -> String {
        match &self.space {
            Some(space) => format!("自建服务端 {}（空间 {}）", self.root, space),
            None => format!("自建服务端 {}", self.root),
        }
    }

    fn ensure_root(&mut self) -> Result<bool, BackendError> {
        // 服务端的"根"就是空间：确认令牌可用、空间对得上。
        // 不建空间——建空间是管理员动作（`cx-server space create`），客户端不该悄悄建。
        match &self.space {
            Some(space) => {
                let reply =
                    self.request("GET", &format!("v1/spaces/{space}"), &[], Vec::new(), &[])?;
                self.expect(&reply, "GET", &format!("v1/spaces/{space}"))?;
                Ok(false)
            }
            None => {
                let reply = self.request("GET", "v1/objects", &[], Vec::new(), &[])?;
                self.expect(&reply, "GET", "v1/objects")?;
                Ok(false)
            }
        }
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<RemoteObject>, BackendError> {
        let query = vec![
            (
                "prefix".to_string(),
                prefix.trim_start_matches('/').to_string(),
            ),
            ("limit".to_string(), LIST_LIMIT.to_string()),
        ];
        let reply = self.request("GET", "v1/objects", &query, Vec::new(), &[])?;
        if reply.status == 404 {
            return Ok(Vec::new());
        }
        self.expect(&reply, "GET", "v1/objects")?;
        parse_list(&reply.body)
    }

    fn head(&mut self, path: &str) -> Result<Option<RemoteObject>, BackendError> {
        let reply = self.request("HEAD", &format!("v1/objects/{path}"), &[], Vec::new(), &[])?;
        match reply.status {
            200 => Ok(Some(RemoteObject {
                path: path.trim_start_matches('/').to_string(),
                etag: reply.etag,
                last_modified_ms: None,
                size: None,
                is_collection: false,
            })),
            404 => Ok(None),
            _ => {
                self.expect(&reply, "HEAD", path)?;
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
        let reply = self.request("GET", &format!("v1/objects/{path}"), &[], Vec::new(), &[])?;
        match reply.status {
            200 => {
                let meta = RemoteObject {
                    path: path.trim_start_matches('/').to_string(),
                    etag: reply.etag,
                    last_modified_ms: None,
                    size: Some(reply.body.len() as u64),
                    is_collection: false,
                };
                Ok(Some((reply.body, meta)))
            }
            404 => Ok(None),
            // 304：客户端不该走到这里（我们没发 If-None-Match），但按"没变化"处理更安全
            304 => Ok(None),
            _ => {
                self.expect(&reply, "GET", path)?;
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
        let extra: Vec<(&str, &str)> = if only_if_absent {
            vec![("if-none-match", "*")]
        } else {
            Vec::new()
        };
        let reply = self.request(
            "PUT",
            &format!("v1/objects/{path}"),
            &[],
            bytes.to_vec(),
            &extra,
        )?;
        match reply.status {
            201 => Ok(PutOutcome::Created { etag: reply.etag }),
            200 => Ok(PutOutcome::Replaced { etag: reply.etag }),
            // 服务端用 412 表示"已存在"
            412 => Ok(PutOutcome::AlreadyExists),
            // 409：并发写冲突（客户端会重读合并后重试）
            409 => Err(BackendError::Conflict(format!("写入 {path} 冲突"))),
            _ => {
                self.expect(&reply, "PUT", path)?;
                Ok(PutOutcome::Created { etag: reply.etag })
            }
        }
    }

    fn delete(&mut self, path: &str) -> Result<(), BackendError> {
        let reply = self.request(
            "DELETE",
            &format!("v1/objects/{path}"),
            &[],
            Vec::new(),
            &[],
        )?;
        match reply.status {
            200 | 202 | 204 | 404 => Ok(()),
            _ => self.expect(&reply, "DELETE", path),
        }
    }
}

/// 基址规范化：必须有协议、去掉尾斜杠。
fn normalize_root(url: &str) -> Result<String, BackendError> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(BackendError::Config("服务端地址不能为空".to_string()));
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    let scheme = with_scheme.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some("http") | Some("https") => Ok(with_scheme),
        other => Err(BackendError::Config(format!(
            "服务端地址协议必须是 http 或 https（实际是 {}）",
            other.unwrap_or("缺失")
        ))),
    }
}

/// 百分号编码（只用于查询串里的值）。
fn encode(text: &str) -> String {
    crate::signing::uri_encode(text, true)
}

/// 解析 `{"objects":[{...}]}`。
fn parse_list(body: &[u8]) -> Result<Vec<RemoteObject>, BackendError> {
    let parsed: serde_json::Value = serde_json::from_slice(body)
        .map_err(|err| BackendError::Malformed(format!("列举响应不是 JSON：{err}")))?;
    let objects = parsed
        .get("objects")
        .and_then(|value| value.as_array())
        .ok_or_else(|| BackendError::Malformed("列举响应缺少 objects 字段".to_string()))?;

    let mut out = Vec::new();
    for item in objects {
        let Some(name) = item.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        out.push(RemoteObject {
            path: name.trim_start_matches('/').to_string(),
            etag: item
                .get("etag")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string()),
            last_modified_ms: item.get("created_ms").and_then(|value| value.as_u64()),
            size: item.get("size").and_then(|value| value.as_u64()),
            is_collection: false,
        });
    }
    Ok(out)
}

/// 解析错误响应 `{"error":"...","message":"..."}`。
fn parse_error(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        let text = String::from_utf8_lossy(body).trim().to_string();
        return (None, if text.is_empty() { None } else { Some(text) });
    };
    (
        parsed
            .get("error")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string()),
        parsed
            .get("message")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ServerOptions {
        ServerOptions {
            name: "srv".to_string(),
            url: "http://127.0.0.1:8688".to_string(),
            space: Some("abc".to_string()),
            token: "cxd_deadbeef".to_string(),
            ca_file: None,
            insecure: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn 参数校验() {
        let mut bad = options();
        bad.url = " ".to_string();
        assert!(matches!(
            ServerBackend::new(bad),
            Err(BackendError::Config(_))
        ));

        let mut bad = options();
        bad.token = "  ".to_string();
        assert!(matches!(
            ServerBackend::new(bad),
            Err(BackendError::Config(_))
        ));

        let mut bad = options();
        bad.url = "ftp://x".to_string();
        assert!(matches!(
            ServerBackend::new(bad),
            Err(BackendError::Config(_))
        ));
    }

    #[test]
    fn 地址规范化() {
        assert_eq!(
            normalize_root("http://127.0.0.1:8688/").unwrap(),
            "http://127.0.0.1:8688"
        );
        assert_eq!(
            normalize_root("cx.example.com").unwrap(),
            "http://cx.example.com"
        );
        assert_eq!(
            normalize_root("https://cx.example.com/base/").unwrap(),
            "https://cx.example.com/base"
        );
    }

    #[test]
    fn 解析列举响应() {
        let body = br#"{"objects":[{"name":"v1/manifests/a.json","etag":"\"aa\"","size":12,"created_ms":1759323998000}],"count":1}"#;
        let objects = parse_list(body).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].path, "v1/manifests/a.json");
        assert_eq!(objects[0].etag.as_deref(), Some("\"aa\""));
        assert_eq!(objects[0].size, Some(12));
        assert_eq!(objects[0].last_modified_ms, Some(1_759_323_998_000));

        assert!(parse_list(b"{}").is_err());
        assert!(parse_list(b"not json").is_err());
    }

    #[test]
    fn 解析错误响应() {
        let (kind, message) =
            parse_error(r#"{"error":"quota","message":"空间配额用尽"}"#.as_bytes());
        assert_eq!(kind.as_deref(), Some("quota"));
        assert_eq!(message.as_deref(), Some("空间配额用尽"));

        let (kind, message) = parse_error(b"plain text error");
        assert!(kind.is_none());
        assert_eq!(message.as_deref(), Some("plain text error"));
        assert_eq!(parse_error(b""), (None, None));
    }

    #[test]
    fn 错误状态映射() {
        let backend = ServerBackend::new(options()).unwrap();
        let make = |status: u16, body: &[u8]| Reply {
            status,
            etag: None,
            body: body.to_vec(),
        };

        let auth = backend
            .expect(
                &make(
                    401,
                    r#"{"error":"unauthorized","message":"设备令牌无效"}"#.as_bytes(),
                ),
                "GET",
                "v1/objects",
            )
            .unwrap_err();
        assert!(auth.is_auth());
        assert!(!auth.is_retryable());
        assert!(auth.to_string().contains("cx space join"), "{auth}");

        // 配额用尽不该被当作可重试
        let quota = backend
            .expect(
                &make(
                    507,
                    r#"{"error":"quota","message":"空间配额用尽"}"#.as_bytes(),
                ),
                "PUT",
                "v1/objects/x",
            )
            .unwrap_err();
        assert!(!quota.is_retryable(), "{quota}");
        assert!(quota.to_string().contains("配额"), "{quota}");

        let missing = backend
            .expect(&make(404, b"{}"), "GET", "v1/objects/x")
            .unwrap_err();
        assert!(matches!(missing, BackendError::NotFound(_)));

        let server = backend
            .expect(&make(500, b"boom"), "GET", "v1/objects")
            .unwrap_err();
        assert!(server.is_retryable(), "{server}");
    }

    #[test]
    fn 传输失败可重试() {
        let mut backend = ServerBackend::new(ServerOptions {
            url: "http://127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(500),
            ..options()
        })
        .unwrap();
        let err = backend.list("v1/").unwrap_err();
        assert!(matches!(err, BackendError::Transport(_)), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn 描述文本() {
        let backend = ServerBackend::new(options()).unwrap();
        let text = backend.describe();
        assert!(text.contains("127.0.0.1:8688"), "{text}");
        assert!(text.contains("abc"), "{text}");
        assert_eq!(backend.kind(), "server");
    }
}
