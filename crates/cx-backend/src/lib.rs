//! ClipboardX 同步后端。
//!
//! 抽象原则：**后端只提供"对象存储"能力**——列前缀、按名字读写不可变对象、删除。
//! 同步语义（谁发布什么、怎么去重、怎么防回环）全部编码在对象命名与内容里
//! （见 [`layout`] 与 `docs/protocol.md`）。这样 WebDAV、S3、自建服务端三种
//! 差异极大的传输层可以共用同一个引擎，也让"换后端"不必改引擎。
//!
//! 已实现：
//! - [`WebDavBackend`]：Nextcloud / ownCloud / Apache mod_dav / nginx dav；
//! - [`MemoryBackend`]：内存实现，用于单进程内模拟"两台设备连同一个后端"；
//! - [`testserver`]：测试用的极小 WebDAV 服务端（**仅供测试**，但公开导出，
//!   这样 `cx-sync` 的集成测试也能复用）。
//!
//! 待实现：自建服务端 REST。

mod date;
mod error;
mod http;
pub mod layout;
mod memory;
mod s3;
pub mod s3_testserver;
mod server;
pub mod signing;
pub mod testserver;
mod webdav;
mod xml;

pub use error::BackendError;
pub use http::{DEFAULT_TIMEOUT, HttpSettings};
pub use memory::MemoryBackend;
pub use s3::{MAX_LIST_OBJECTS, S3Backend, S3Options, default_path_style};
pub use server::{ServerBackend, ServerOptions, enroll};
pub use webdav::{PROPFIND_BODY, WebDavBackend, WebDavOptions};

use std::time::Duration;

/// 后端种类标识（与配置里的 `kind` 一致）。
pub const KIND_SERVER: &str = "server";
/// WebDAV 后端种类标识。
pub const KIND_WEBDAV: &str = "webdav";
/// S3 后端种类标识。
pub const KIND_S3: &str = "s3";
/// 内存后端种类标识（仅供测试）。
pub const KIND_MEMORY: &str = "memory";

/// 远端对象（由列举或取属性得到）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    /// 相对后端根目录的路径（例如 `v1/manifests/ab12.json`）。
    pub path: String,
    /// ETag（含引号，原样保留）。WebDAV/S3 都可能不给，因此是可选的。
    pub etag: Option<String>,
    /// 最后修改时间（Unix 毫秒）。
    pub last_modified_ms: Option<u64>,
    /// 对象字节数。
    pub size: Option<u64>,
    /// 是否是目录/前缀。
    pub is_collection: bool,
}

impl RemoteObject {
    /// 是否"与给定的 ETag 相比没有变化"。
    ///
    /// ETag 缺失时保守地返回 `false`（当作有变化），避免漏同步。
    pub fn matches_etag(&self, previous: Option<&str>) -> bool {
        match (&self.etag, previous) {
            (Some(current), Some(previous)) => current == previous,
            _ => false,
        }
    }
}

/// 写入结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// 新建成功。
    Created {
        /// 服务端返回的 ETag（若有）。
        etag: Option<String>,
    },
    /// 覆盖了已有对象。
    Replaced {
        /// 服务端返回的 ETag（若有）。
        etag: Option<String>,
    },
    /// 要求"不存在才写"，但对象已存在（对我们而言代表内容已在远端）。
    AlreadyExists,
}

impl PutOutcome {
    /// 服务端返回的 ETag。
    pub fn etag(&self) -> Option<&str> {
        match self {
            PutOutcome::Created { etag } | PutOutcome::Replaced { etag } => etag.as_deref(),
            PutOutcome::AlreadyExists => None,
        }
    }

    /// 是否真的写进去了（`AlreadyExists` 表示内容本来就在）。
    pub fn wrote(&self) -> bool {
        !matches!(self, PutOutcome::AlreadyExists)
    }
}

/// 后端契约。
///
/// `path` 一律是**相对后端根目录**的路径，不带前导 `/`（见 [`layout`]）。
/// 目录在前缀里用结尾的 `/` 表达（例如 `v1/manifests/`）。
pub trait RemoteBackend: Send {
    /// 配置里的后端名。
    fn name(&self) -> &str;

    /// 后端种类。
    fn kind(&self) -> &'static str;

    /// 人类可读描述（`cx status` / `cx doctor` 展示）。
    fn describe(&self) -> String;

    /// 列出一层前缀下的对象。
    ///
    /// 前缀不存在时返回空列表而不是错误——"目录还没建"是正常状态。
    fn list(&mut self, prefix: &str) -> Result<Vec<RemoteObject>, BackendError>;

    /// 取单个对象的属性；不存在返回 `None`。
    fn head(&mut self, path: &str) -> Result<Option<RemoteObject>, BackendError>;

    /// 读取对象内容；不存在返回 `None`。
    fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>, BackendError>;

    /// 读取对象内容并附带属性（用于保存 ETag 做增量判断）。
    fn get_with_meta(
        &mut self,
        path: &str,
    ) -> Result<Option<(Vec<u8>, RemoteObject)>, BackendError>;

    /// 写入对象。
    ///
    /// `only_if_absent = true` 时表示"不可变对象，已存在就别覆盖"，
    /// 应返回 [`PutOutcome::AlreadyExists`] 而不是报错。
    fn put(
        &mut self,
        path: &str,
        bytes: &[u8],
        only_if_absent: bool,
    ) -> Result<PutOutcome, BackendError>;

    /// 删除对象（不存在视为成功）。
    fn delete(&mut self, path: &str) -> Result<(), BackendError>;

    /// 确保"根"存在（S3 的桶、WebDAV 的根集合）。
    ///
    /// 默认什么都不做。S3 只读 LIST 检查既有桶，不隐式创建桶；
    /// 创建必须由调用方显式选择 [`S3Backend::create_bucket`]。
    ///
    /// 返回 `true` 表示这次真的创建了。
    fn ensure_root(&mut self) -> Result<bool, BackendError> {
        Ok(false)
    }
}

/// 读取空间元数据 `cx-space.json`（明文，可公开）。
pub fn load_space_meta(backend: &mut dyn RemoteBackend) -> Result<Option<String>, BackendError> {
    match backend.get(layout::SPACE_FILE)? {
        Some(bytes) => String::from_utf8(bytes).map(Some).map_err(|err| {
            BackendError::Malformed(format!("{SPACE_META_NAME} 不是合法 UTF-8：{err}"))
        }),
        None => Ok(None),
    }
}

/// 写入空间元数据。
pub fn store_space_meta(backend: &mut dyn RemoteBackend, json: &str) -> Result<(), BackendError> {
    backend.put(layout::SPACE_FILE, json.as_bytes(), false)?;
    Ok(())
}

/// 列出所有设备的 manifest（同步轮询的第一步）。
pub fn list_manifests(backend: &mut dyn RemoteBackend) -> Result<Vec<RemoteObject>, BackendError> {
    let mut objects = backend.list(layout::MANIFESTS_PREFIX)?;
    objects.retain(|object| {
        !object.is_collection && layout::device_id_from_manifest_path(&object.path).is_some()
    });
    Ok(objects)
}

/// 由配置构造后端。
///
/// 目前只支持 WebDAV 与 S3；自建服务端尚未接入（现在就返回明确的
/// 配置错误，而不是静默失败）。
pub fn build_backend(
    section: &cx_core::BackendSection,
    paths: &cx_core::Paths,
    timeout: Duration,
) -> Result<Box<dyn RemoteBackend>, BackendError> {
    match section.kind {
        cx_core::BackendKind::Webdav => {
            let url = section
                .url
                .clone()
                .ok_or_else(|| BackendError::Config(format!("后端 {} 缺少 url", section.name)))?;
            let username = section.username.clone();
            let password = if username.is_some() {
                read_secret(
                    paths,
                    section.password_file.as_deref(),
                    section.password_env.as_deref(),
                    "口令",
                )?
            } else {
                None
            };
            let ca_file = section.ca_file.as_ref().map(|name| paths.secret_file(name));

            let backend = WebDavBackend::new(WebDavOptions {
                name: section.name.clone(),
                url,
                username,
                password,
                ca_file,
                insecure: section.insecure,
                timeout,
            })?;
            Ok(Box::new(backend))
        }
        cx_core::BackendKind::S3 => {
            let bucket = section.bucket.clone().ok_or_else(|| {
                BackendError::Config(format!("后端 {} 缺少 bucket", section.name))
            })?;
            let access_key_id = section.access_key_id.clone().ok_or_else(|| {
                BackendError::Config(format!("后端 {} 缺少 access_key_id", section.name))
            })?;
            let secret_access_key = read_secret(
                paths,
                section.secret_access_key_file.as_deref(),
                section.secret_access_key_env.as_deref(),
                "私有密钥",
            )?
            .ok_or_else(|| {
                BackendError::Config(format!(
                    "后端 {} 需要 secret_access_key_file 或 secret_access_key_env（`cx backend add` 会写好）",
                    section.name
                ))
            })?;
            let path_style = section
                .path_style
                .unwrap_or_else(|| default_path_style(section.endpoint.as_deref()));
            let ca_file = section.ca_file.as_ref().map(|name| paths.secret_file(name));
            // STS 临时凭据：配了就一定要读得到，读不到宁可报凭据错误也不要静默降级
            let session_token = read_secret(
                paths,
                section.session_token_file.as_deref(),
                section.session_token_env.as_deref(),
                "会话令牌",
            )?;

            let backend = S3Backend::new(S3Options {
                name: section.name.clone(),
                endpoint: section.endpoint.clone(),
                region: section
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
                bucket,
                prefix: section.prefix.clone().unwrap_or_default(),
                access_key_id,
                secret_access_key,
                session_token,
                path_style,
                ca_file,
                insecure: section.insecure,
                timeout,
            })?;
            Ok(Box::new(backend))
        }
        cx_core::BackendKind::Server => {
            let url = section
                .url
                .clone()
                .ok_or_else(|| BackendError::Config(format!("后端 {} 缺少 url", section.name)))?;
            let token = read_secret(paths, section.token_file.as_deref(), None, "设备令牌")?
                .ok_or_else(|| {
                    BackendError::Config(format!(
                        "后端 {} 需要 token_file（`cx space join` 会写入）",
                        section.name
                    ))
                })?;
            let ca_file = section.ca_file.as_ref().map(|name| paths.secret_file(name));

            let backend = ServerBackend::new(ServerOptions {
                name: section.name.clone(),
                url,
                space: section.space.clone(),
                token,
                ca_file,
                insecure: section.insecure,
                timeout,
            })?;
            Ok(Box::new(backend))
        }
    }
}

const SPACE_META_NAME: &str = "cx-space.json";

/// 读取凭据：优先文件（相对 `secrets/`），其次环境变量。
fn read_secret(
    paths: &cx_core::Paths,
    file: Option<&str>,
    env: Option<&str>,
    what: &str,
) -> Result<Option<String>, BackendError> {
    if let Some(name) = file {
        let path = paths.secret_file(name);
        let text = std::fs::read_to_string(&path).map_err(|err| {
            BackendError::Credentials(format!("无法读取{what}文件 {}：{err}", path.display()))
        })?;
        return Ok(Some(if what == "口令" {
            text
        } else {
            text.trim().to_string()
        }));
    }
    if let Some(name) = env {
        let value = std::env::var(name)
            .map_err(|_| BackendError::Credentials(format!("环境变量 {name} 未设置（{what}）")))?;
        return Ok(Some(if what == "口令" {
            value
        } else {
            value.trim().to_string()
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_section(kind: cx_core::BackendKind) -> cx_core::BackendSection {
        cx_core::BackendSection {
            name: "t".to_string(),
            kind,
            enabled: true,
            url: Some("https://example.com/dav".to_string()),
            space: None,
            token_file: None,
            username: Some("alice".to_string()),
            password_file: Some("webdav.pass".to_string()),
            password_env: None,
            ca_file: None,
            insecure: false,
            bucket: None,
            prefix: None,
            region: None,
            endpoint: None,
            access_key_id: None,
            secret_access_key_file: None,
            secret_access_key_env: None,
            session_token_file: None,
            session_token_env: None,
            path_style: None,
            poll_interval_ms: None,
        }
    }

    fn temp_paths(tag: &str) -> cx_core::Paths {
        let root = std::env::temp_dir().join(format!("cx-backend-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = cx_core::Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    /// `Result<Box<dyn RemoteBackend>, _>::unwrap_err()` 需要 `dyn RemoteBackend: Debug`，
    /// 而 trait 对象上并不方便要求 Debug；测试里用这个助手替代。
    fn take_error(result: Result<Box<dyn RemoteBackend>, BackendError>) -> BackendError {
        match result {
            Ok(backend) => panic!("本应失败，却拿到了 {} 后端", backend.kind()),
            Err(err) => err,
        }
    }

    #[test]
    fn put_outcome_语义() {
        let created = PutOutcome::Created {
            etag: Some("\"a\"".to_string()),
        };
        assert!(created.wrote());
        assert_eq!(created.etag(), Some("\"a\""));

        let existed = PutOutcome::AlreadyExists;
        assert!(!existed.wrote());
        assert!(existed.etag().is_none());
    }

    #[test]
    fn 口令文件保留首尾空格与换行() {
        let root = std::env::temp_dir().join(format!(
            "cx-password-exact-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let paths = cx_core::Paths::from_root(&root);
        paths.ensure().unwrap();
        std::fs::write(paths.secret_file("fake.pass"), "  fake password \n").unwrap();
        assert_eq!(
            read_secret(&paths, Some("fake.pass"), None, "口令")
                .unwrap()
                .unwrap(),
            "  fake password \n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn etag_比较() {
        let object = RemoteObject {
            path: "x".to_string(),
            etag: Some("\"a\"".to_string()),
            last_modified_ms: None,
            size: None,
            is_collection: false,
        };
        assert!(object.matches_etag(Some("\"a\"")));
        assert!(!object.matches_etag(Some("\"b\"")));
        assert!(!object.matches_etag(None), "没有旧 ETag 时应保守认为有变化");

        let no_etag = RemoteObject {
            etag: None,
            ..object
        };
        assert!(!no_etag.matches_etag(Some("\"a\"")));
    }

    #[test]
    fn 空间元数据读写往返() {
        let mut backend = MemoryBackend::new("m");
        assert!(load_space_meta(&mut backend).unwrap().is_none());
        store_space_meta(&mut backend, r#"{"v":1}"#).unwrap();
        assert_eq!(
            load_space_meta(&mut backend).unwrap().as_deref(),
            Some(r#"{"v":1}"#)
        );
    }

    #[test]
    fn 列出_manifest_会过滤无关对象() {
        let mut backend = MemoryBackend::new("m");
        backend.seed("v1/manifests/ab12cd34ef5678901234567890abcdef.json", b"{}");
        backend.seed("v1/manifests/not-a-device.json", b"{}");
        backend.seed("v1/items/20251001/x.cxi", b"x");
        let manifests = list_manifests(&mut backend).unwrap();
        assert_eq!(manifests.len(), 1, "只保留合法设备 id 的 manifest");
        assert_eq!(
            manifests[0].path,
            "v1/manifests/ab12cd34ef5678901234567890abcdef.json"
        );
    }

    #[test]
    fn 自建服务端尚未实现时给出明确错误() {
        let paths = temp_paths("kinds");
        // 只有 url、没有令牌文件：应当给出可操作的错误
        let err = take_error(build_backend(
            &config_section(cx_core::BackendKind::Server),
            &paths,
            Duration::from_secs(1),
        ));
        assert!(matches!(err, BackendError::Config(_)));
        assert!(err.to_string().contains("token_file"), "{err}");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn server_后端可从配置构造并读取令牌文件() {
        let paths = temp_paths("servercfg");
        std::fs::write(paths.secret_file("srv.token"), b"cxd_testtoken\n").unwrap();
        let mut section = config_section(cx_core::BackendKind::Server);
        section.token_file = Some("srv.token".to_string());
        section.space = Some("abc123".to_string());

        let backend = build_backend(&section, &paths, Duration::from_secs(5)).unwrap();
        assert_eq!(backend.kind(), "server");
        let text = backend.describe();
        assert!(text.contains("example.com"), "{text}");
        assert!(text.contains("abc123"), "{text}");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn s3_后端可从配置构造并读取密钥文件() {
        let paths = temp_paths("s3cfg");
        std::fs::write(paths.secret_file("s3.key"), b"minioadmin\n").unwrap();
        let mut section = config_section(cx_core::BackendKind::S3);
        section.url = None;
        section.username = None;
        section.password_file = None;
        section.bucket = Some("clipboardx".to_string());
        section.prefix = Some("space-a/".to_string());
        section.endpoint = Some("http://127.0.0.1:9000".to_string());
        section.access_key_id = Some("minioadmin".to_string());
        section.secret_access_key_file = Some("s3.key".to_string());
        // 自建端点未显式指定 path_style 时应默认 path-style
        section.path_style = None;

        let backend = build_backend(&section, &paths, Duration::from_secs(5)).unwrap();
        assert_eq!(backend.kind(), "s3");
        let text = backend.describe();
        assert!(text.contains("127.0.0.1:9000"), "{text}");
        assert!(text.contains("clipboardx"), "{text}");
        assert!(text.contains("path-style"), "{text}");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn s3_会话令牌文件缺失时报凭据错误而不静默降级() {
        let paths = temp_paths("s3stsmissing");
        let mut section = config_section(cx_core::BackendKind::S3);
        section.bucket = Some("clipboardx".to_string());
        section.access_key_id = Some("fake-ak".to_string());
        section.secret_access_key_file = Some("s3.key".to_string());
        std::fs::write(paths.secret_file("s3.key"), b"fake-secret").unwrap();
        section.session_token_file = Some("missing.sessiontoken".to_string());

        let err = take_error(build_backend(&section, &paths, Duration::from_secs(1)));
        assert!(
            matches!(err, BackendError::Credentials(_)) && err.to_string().contains("会话令牌"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn s3_会话令牌文件存在时可构造后端() {
        let paths = temp_paths("s3sts");
        let mut section = config_section(cx_core::BackendKind::S3);
        section.endpoint = Some("https://oss-cn-beijing.aliyuncs.com".to_string());
        section.region = Some("cn-beijing".to_string());
        section.bucket = Some("clipboardx".to_string());
        section.access_key_id = Some("fake-ak".to_string());
        section.secret_access_key_file = Some("s3.key".to_string());
        section.session_token_file = Some("s3.sessiontoken".to_string());
        std::fs::write(paths.secret_file("s3.key"), b"fake-secret").unwrap();
        std::fs::write(paths.secret_file("s3.sessiontoken"), b"fake-session-token").unwrap();

        let backend = build_backend(&section, &paths, Duration::from_secs(1)).unwrap();
        assert_eq!(backend.kind(), "s3");
        assert!(
            backend.describe().contains("virtual-host"),
            "{}",
            backend.describe()
        );
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn s3_缺字段时给出可操作的错误() {
        let paths = temp_paths("s3missing");
        let mut section = config_section(cx_core::BackendKind::S3);
        section.url = None;
        section.username = None;
        section.password_file = None;
        // 缺 bucket
        let err = take_error(build_backend(&section, &paths, Duration::from_secs(1)));
        assert!(err.to_string().contains("bucket"), "{err}");

        // 有桶但缺密钥
        section.bucket = Some("clipboardx".to_string());
        section.access_key_id = Some("minioadmin".to_string());
        let err = take_error(build_backend(&section, &paths, Duration::from_secs(1)));
        assert!(err.to_string().contains("secret_access_key"), "{err}");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn webdav_后端可从配置构造并读取口令文件() {
        let paths = temp_paths("webdav");
        std::fs::write(paths.secret_file("webdav.pass"), b"secret\n").unwrap();
        let backend = build_backend(
            &config_section(cx_core::BackendKind::Webdav),
            &paths,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(backend.kind(), "webdav");
        assert!(backend.describe().contains("https://example.com/dav"));
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 缺少凭据文件时报凭据错误() {
        let paths = temp_paths("nopass");
        let err = take_error(build_backend(
            &config_section(cx_core::BackendKind::Webdav),
            &paths,
            Duration::from_secs(5),
        ));
        assert!(matches!(err, BackendError::Credentials(_)), "{err}");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 凭据可用环境变量提供() {
        let paths = temp_paths("envpass");
        let mut section = config_section(cx_core::BackendKind::Webdav);
        section.password_file = None;
        section.password_env = Some("CX_TEST_WEBDAV_PASS".to_string());
        unsafe { std::env::set_var("CX_TEST_WEBDAV_PASS", "fromenv") };
        let backend = build_backend(&section, &paths, Duration::from_secs(5)).unwrap();
        assert_eq!(backend.kind(), "webdav");
        unsafe { std::env::remove_var("CX_TEST_WEBDAV_PASS") };

        // 环境变量缺失时给出明确错误
        let err = take_error(build_backend(&section, &paths, Duration::from_secs(5)));
        assert!(matches!(err, BackendError::Credentials(_)));
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
