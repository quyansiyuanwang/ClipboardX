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
//! 默认信任**操作系统根证书库**（`tls-rustls-native-roots`，企业自签 CA 装进系统信任库即可），
//! 同时叠加 Mozilla 根作为兜底；也可以显式指定 `ca_file`（PEM），或 `insecure` 跳过校验。

use crate::error::BackendError;
use attohttpc::RequestBuilder;
use rustls_pki_types::CertificateDer;
use std::path::{Path, PathBuf};
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

    if let Some(path) = &settings.ca_file {
        builder = builder.add_root_certificate(load_ca_certificate(path)?);
    }
    Ok(builder)
}

/// 读 PEM 文件并转成 rustls 的 DER 证书。
pub fn load_ca_certificate(path: &Path) -> Result<Certificate, BackendError> {
    let pem = std::fs::read(path).map_err(|err| {
        BackendError::Config(format!("无法读取 CA 文件 {}：{err}", path.display()))
    })?;
    let der = pem_to_der(&pem).map_err(|err| {
        BackendError::Config(format!("CA 文件 {} 解析失败：{err}", path.display()))
    })?;
    Ok(Certificate::from(der))
}
/// 取 PEM 里第一个 `CERTIFICATE` 块并 base64 解码为 DER。
///
/// 只用标准库 + base64（已经在依赖里），不为一件事引一个 PEM 库。
pub fn pem_to_der(pem: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(pem).map_err(|err| format!("不是 UTF-8 文本：{err}"))?;
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.contains("-----BEGIN CERTIFICATE-----") {
            inside = true;
            continue;
        }
        if line.contains("-----END CERTIFICATE-----") {
            if inside {
                break;
            }
            continue;
        }
        if inside {
            body.push_str(line);
        }
    }
    if body.is_empty() {
        return Err("没有找到 CERTIFICATE 块".to_string());
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|err| format!("base64 解码失败：{err}"))
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
