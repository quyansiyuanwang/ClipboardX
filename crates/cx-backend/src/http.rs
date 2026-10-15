//! HTTP 客户端设置（`attohttpc`）。
//!
//! 为什么是 attohttpc 而不是 ureq：
//! **ureq 3.x 发不出 WebDAV 的扩展方法**。它的协议状态机对方法做白名单校验
//! （`ureq-proto` 的 `MethodExt::is_http11()` 只认 PUT/DELETE/CONNECT/OPTIONS/TRACE/PATCH），
//! 构造 `PROPFIND` 请求会直接报
//! `protocol: PROPFIND not valid for HTTP version HTTP/1.1`。
//! 而 WebDAV 的列举（PROPFIND）与建目录（MKCOL）是绕不开的，因此换用支持任意方法的
//! attohttpc：同样是阻塞式、无 async 运行时、依赖可控，且自带 Basic 认证与自定义根证书。
//!
//! 默认显式加载并缓存操作系统根证书库；不依赖 attohttpc 的根特性 cfg。
//! `ca_file` 可追加 PEM CA bundle；`insecure` 仅用于显式调试。

use crate::error::BackendError;
use attohttpc::RequestBuilder;
use rustls_pki_types::CertificateDer;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// 自定义根证书的类型（attohttpc 的 rustls 后端要的就是它）。
pub type Certificate = CertificateDer<'static>;

/// 默认单次请求超时（大图片上传也要够）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 连接超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP 客户端设置。
#[derive(Debug, Clone)]
pub struct HttpSettings {
    /// 单次请求超时。
    pub timeout: Duration,
    /// 自定义 CA 证书（PEM）。`None` 表示只用系统/内置根。
    pub ca_file: Option<PathBuf>,
    /// 跳过 TLS 校验（仅调试）。
    pub insecure: bool,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            ca_file: None,
            insecure: false,
        }
    }
}

/// 把公共设置套到请求构建器上。
pub(crate) fn configure<B>(
    builder: RequestBuilder<B>,
    settings: &HttpSettings,
) -> Result<RequestBuilder<B>, BackendError> {
    let mut builder = builder
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(settings.timeout)
        .timeout(settings.timeout)
        // WebDAV 客户端不该跟着 302 跑：路径错了应当明确报错，而不是被悄悄改写
        .follow_redirects(false)
        .danger_accept_invalid_certs(settings.insecure);

    if builder.inspect().url().scheme() != "https" {
        return Ok(builder);
    }
    // attohttpc 0.31's ring root features do not compile its root-loading branches.
    // Feed roots explicitly; keep the existing ring crypto provider only.
    let mut roots = native_roots().to_vec();
    if let Some(path) = &settings.ca_file {
        roots.extend(load_ca_certificates(path)?);
    }
    ensure_trust_roots(&roots)?;
    for root in roots {
        builder = builder.add_root_certificate(root);
    }
    Ok(builder)
}

fn ensure_trust_roots(roots: &[Certificate]) -> Result<(), BackendError> {
    if roots.is_empty() {
        return Err(BackendError::Config(
            "TLS 信任根为空：请修复系统 CA 根证书库或配置有效的 ca_file（PEM CA bundle），不要关闭 TLS 校验".to_string(),
        ));
    }
    Ok(())
}

/// Load once per process. Invalid native-store entries are ignored individually;
/// an empty final store is a non-retryable configuration error in configure().
fn native_roots() -> &'static [Certificate] {
    static ROOTS: OnceLock<Vec<Certificate>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let result = rustls_native_certs::load_native_certs();
        if !result.errors.is_empty() {
            log::warn!(
                "系统 TLS 根证书加载出现 {} 个错误；仍使用有效证书",
                result.errors.len()
            );
        }
        let mut store = rustls::RootCertStore::empty();
        result
            .certs
            .into_iter()
            .filter(|cert| store.add(cert.clone()).is_ok())
            .collect()
    })
}

/// Read and validate every certificate in a PEM CA bundle, before any request.
pub fn load_ca_certificates(path: &Path) -> Result<Vec<Certificate>, BackendError> {
    let pem = std::fs::read(path).map_err(|err| {
        BackendError::Config(format!("无法读取 CA 文件 {}：{err}", path.display()))
    })?;
    let certificates = pem_certificates(&pem).map_err(|err| {
        BackendError::Config(format!("CA 文件 {} 解析失败：{err}", path.display()))
    })?;
    let mut store = rustls::RootCertStore::empty();
    for cert in &certificates {
        store.add(cert.clone()).map_err(|_| {
            BackendError::Config(format!("CA 文件 {} 包含非法 DER 证书", path.display()))
        })?;
    }
    Ok(certificates)
}

/// Compatibility helper for callers validating a CA file at construction time.
pub fn load_ca_certificate(path: &Path) -> Result<Certificate, BackendError> {
    Ok(load_ca_certificates(path)?.remove(0))
}

fn pem_certificates(pem: &[u8]) -> Result<Vec<Certificate>, String> {
    let text = std::str::from_utf8(pem).map_err(|err| format!("不是 UTF-8 文本：{err}"))?;
    let mut certificates = Vec::new();
    let mut body: Option<String> = None;
    for line in text.lines().map(str::trim) {
        match line {
            "-----BEGIN CERTIFICATE-----" => {
                if body.is_some() {
                    return Err("嵌套 CERTIFICATE 块".into());
                }
                body = Some(String::new());
            }
            "-----END CERTIFICATE-----" => {
                let encoded = body.take().ok_or("CERTIFICATE 结束标记没有开始标记")?;
                use base64::Engine;
                let der = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|err| format!("base64 解码失败：{err}"))?;
                if der.is_empty() {
                    return Err("空 CERTIFICATE 块".into());
                }
                certificates.push(Certificate::from(der));
            }
            _ => {
                if let Some(body) = &mut body {
                    body.push_str(line);
                }
            }
        }
    }
    if body.is_some() {
        return Err("CERTIFICATE 块没有结束标记".into());
    }
    if certificates.is_empty() {
        return Err("没有找到 CERTIFICATE 块".into());
    }
    Ok(certificates)
}

/// Decode the first certificate (legacy helper); file loading validates all DER.
#[cfg(test)]
pub fn pem_to_der(pem: &[u8]) -> Result<Vec<u8>, String> {
    Ok(pem_certificates(pem)?.remove(0).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一张真实的自签 CA（只在测试里用）。
    const TEST_CA_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBhTCCASugAwIBAgIQIRi6zePL6mKjOipn+dNuaTAKBggqhkjOPQQDAjASMRAw
DgYDVQQKEwdBY21lIENvMB4XDTE3MTAyMDE5NDMwNloXDTE4MTAyMDE5NDMwNlow
EjEQMA4GA1UEChMHQWNtZSBDbzBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABD0d
7VNhbWvZLWPuj/RtHFjvtJBEwOkhbN/BnnE8rnZR8+sbwnc/KhCk3FhnpHZnQz7B
5aETbbIgmuvewdjvSBSjYzBhMA4GA1UdDwEB/wQEAwICpDATBgNVHSUEDDAKBggr
BgEFBQcDATAPBgNVHRMBAf8EBTADAQH/MCkGA1UdEQQiMCCCDmxvY2FsaG9zdDo1
NDUzgg4xMjcuMC4wLjE6NTQ1MzAKBggqhkjOPQQDAgNIADBFAiEA2zpJEPQyz6/l
Wf86aX6PepsntZv2GYlA5UpabfT2EZICICpJ5h/iI+i341gBmLiAFQOyTDT+/wQc
6MF9+Yw1Yy0t
-----END CERTIFICATE-----
"#;

    #[test]
    fn pem_转_der() {
        let der = pem_to_der(TEST_CA_PEM.as_bytes()).unwrap();
        // DER 以 SEQUENCE 标签（0x30）开头
        assert_eq!(der[0], 0x30);
        assert!(der.len() > 100);
    }

    #[test]
    fn pem_解析的容错() {
        // 前后有噪声、行内有空格都能处理
        let noisy = format!("# 注释\n\n  {}  \n", TEST_CA_PEM.replace('\n', "\n  "));
        assert!(pem_to_der(noisy.as_bytes()).is_ok());
    }

    #[test]
    fn 非法_pem_报错() {
        for bad in [
            "",
            "not pem at all",
            "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----",
        ] {
            assert!(pem_to_der(bad.as_bytes()).is_err(), "应当拒绝 {bad:?}");
        }
        // base64 里塞了非法字符
        assert!(
            pem_to_der(b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----").is_err()
        );
    }

    #[test]
    fn 不存在的_ca_文件报配置错误() {
        let err = load_ca_certificate(Path::new("/definitely/not/here/ca.pem")).unwrap_err();
        assert!(matches!(err, BackendError::Config(_)), "{err}");
        assert!(!err.is_retryable());
        assert_eq!(err.kind(), "config");
    }

    #[test]
    fn all_bundle_certificates_are_validated_as_der() {
        assert_eq!(
            pem_certificates(format!("{TEST_CA_PEM}{TEST_CA_PEM}").as_bytes())
                .unwrap()
                .len(),
            2
        );
        assert!(pem_certificates(b"-----BEGIN CERTIFICATE-----\nAQID\n").is_err());
        let path = std::env::temp_dir().join(format!("cx-invalid-ca-{}.pem", std::process::id()));
        std::fs::write(
            &path,
            format!("{TEST_CA_PEM}-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n"),
        )
        .unwrap();
        assert!(matches!(
            load_ca_certificates(&path),
            Err(BackendError::Config(_))
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_trust_roots_fail_with_actionable_non_retryable_error() {
        let err = ensure_trust_roots(&[]).unwrap_err();
        assert!(matches!(err, BackendError::Config(_)));
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("ca_file"));
    }

    #[test]
    fn native_roots_are_explicitly_cached() {
        assert!(std::ptr::eq(native_roots(), native_roots()));
        let configured = configure(
            attohttpc::get("https://localhost/"),
            &HttpSettings::default(),
        );
        if native_roots().is_empty() {
            assert!(matches!(configured, Err(BackendError::Config(_))));
        } else {
            assert!(configured.is_ok());
        }
    }

    #[test]
    fn 能把设置套到请求上() {
        let make = || attohttpc::RequestBuilder::new(attohttpc::Method::GET, "http://example.com/");
        assert!(configure(make(), &HttpSettings::default()).is_ok());
        assert!(
            configure(
                make(),
                &HttpSettings {
                    insecure: true,
                    ..HttpSettings::default()
                }
            )
            .is_ok()
        );
    }
}
