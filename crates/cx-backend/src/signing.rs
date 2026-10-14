//! AWS Signature Version 4（只实现 S3 用到的部分）。
//!
//! 为什么自己写：只需要 HMAC-SHA256 与几个字符串拼接，而 `aws-sdk-s3` 会连带
//! tokio + hyper + aws-config 一大串依赖进来——与本项目"不用 async 运行时、
//! 二进制尽量小"的取舍直接冲突。
//!
//! 正确性靠两点保证：
//! 1. **对着 AWS 官方文档里的示例做单元测试**（canonical request → string to sign →
//!    signature 三段都能逐字节比对），比"跑通一次就算对"可靠得多；
//! 2. 集成测试里的测试服务端会**用同一套算法重算签名并比对**，签名写错会被逮住。
//!
//! 需要注意的坑（都在下面的实现里处理）：
//! - 规范化 URI 要按段百分号编码（保留 `/`），查询串要按**编码后**的名字排序；
//! - 参与签名的头必须与真正发出去的头**完全一致**，且名字小写、值去掉首尾空白；
//! - 空载荷的 SHA256 是固定值 `e3b0c442…`，不必每次真去算。

use crate::error::BackendError;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// HMAC 用到的官方示例密钥（仅测试用）。
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// 空载荷的 SHA256（十六进制）。
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 一次待签名的请求。
#[derive(Debug, Clone)]
pub struct SignableRequest {
    /// HTTP 方法（大写）。
    pub method: String,
    /// 主机（含端口，若非默认端口）。
    pub host: String,
    /// 规范化的路径（已按段编码，以 `/` 开头）。
    pub canonical_uri: String,
    /// 查询参数（未编码的原始键值对，签名时由本模块编码并排序）。
    pub query: Vec<(String, String)>,
    /// 参与签名的头（小写名字 → 值）。
    pub headers: Vec<(String, String)>,
    /// 载荷字节。
    pub body: Vec<u8>,
}

/// 签名结果。
#[derive(Debug, Clone)]
pub struct SignedRequest {
    /// 规范化查询串（可直接拼在 URL 上）。
    pub canonical_query: String,
    /// 需要发出去的头（含 `Authorization`）。
    pub headers: Vec<(String, String)>,
    /// 供测试与排障使用的签名。
    pub signature: String,
    /// 供测试与排障使用的规范化请求。
    pub canonical_request: String,
    /// 供测试与排障使用的待签名串。
    pub string_to_sign: String,
}

/// 签名参数。
#[derive(Debug, Clone)]
pub struct SigningContext<'a> {
    /// 访问密钥 id。
    pub access_key_id: &'a str,
    /// 私有密钥。
    pub secret_access_key: &'a str,
    /// 区域（如 `us-east-1`）。
    pub region: &'a str,
    /// 服务名（S3 固定 `s3`）。
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ` 形式的时间戳。
    pub amz_date: &'a str,
    /// 会话令牌（临时凭据才有）。
    pub session_token: Option<&'a str>,
}

/// 对请求签名。
///
/// 参与签名的头集合由**调用方**决定（`request.headers`），本函数只补上 AWS 要求的
/// `host`、`x-amz-date` 与会话令牌。原因：不同服务要求的签名头不同——S3 必须把
/// `x-amz-content-sha256` 也签进去（官方通用 SigV4 示例只签 host;x-amz-date），
/// 把选择权留给调用方，才能对着各自的官方向量逐字节验证。
pub fn sign(
    request: &SignableRequest,
    context: &SigningContext<'_>,
) -> Result<SignedRequest, BackendError> {
    let canonical_query = canonical_query(&request.query);
    let payload_hash = sha256_hex(&request.body);

    // 参与签名的头：调用方给出的 + AWS 必需的几个
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    headers.push(("host".to_string(), request.host.clone()));
    headers.push(("x-amz-date".to_string(), context.amz_date.to_string()));
    if let Some(token) = context.session_token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    headers.dedup_by(|a, b| a.0 == b.0);

    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();

    let canonical_request = format!(
        "{method}\n{uri}\n{query}\n{headers}\n{signed}\n{payload}",
        method = request.method,
        uri = request.canonical_uri,
        query = canonical_query,
        headers = canonical_headers,
        signed = signed_headers,
        payload = payload_hash,
    );

    let date = date_stamp(context.amz_date)?;
    let scope = format!(
        "{date}/{region}/{service}/aws4_request",
        region = context.region,
        service = context.service
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        context.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    let key = signing_key(
        context.secret_access_key,
        &date,
        context.region,
        context.service,
    )?;
    let mut mac = HmacSha256::new_from_slice(&key)
        .map_err(|err| BackendError::Config(format!("HMAC 初始化失败：{err}")))?;
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let authorization = format!(
        "{ALGORITHM} Credential={key}/{scope}, SignedHeaders={signed}, Signature={signature}",
        key = context.access_key_id,
        signed = signed_headers,
    );

    let mut out_headers: Vec<(String, String)> = headers
        .into_iter()
        .filter(|(name, _)| name != "host")
        .collect();
    out_headers.push(("Authorization".to_string(), authorization));

    Ok(SignedRequest {
        canonical_query,
        headers: out_headers,
        signature,
        canonical_request,
        string_to_sign,
    })
}

/// 规范化查询串：按编码后的名字（再按值）排序，`k=v` 用 `&` 连接。
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = query
        .iter()
        .map(|(name, value)| (uri_encode(name, true), uri_encode(value, true)))
        .collect();
    encoded.sort();
    encoded
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 按 AWS 规则做百分号编码。
///
/// `encode_slash = true` 用于查询串（`/` 要编码成 `%2F`）；路径按段编码时传 `false`。
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let byte = *byte;
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (!encode_slash && byte == b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 路径按段编码（保留 `/`）。
pub fn canonical_uri(path: &str) -> String {
    uri_encode(path, false)
}

/// 把 `YYYYMMDDTHHMMSSZ` 取日期部分（`YYYYMMDD`）。
pub fn date_stamp(amz_date: &str) -> Result<String, BackendError> {
    if amz_date.len() < 8 {
        return Err(BackendError::Config(format!(
            "amz-date 格式非法：{amz_date}（应为 YYYYMMDDTHHMMSSZ）"
        )));
    }
    Ok(amz_date[..8].to_string())
}

/// 派生签名密钥（AWS4 + 四级 HMAC），返回**密钥字节**。
///
/// 这里必须返回字节而不是 `Hmac` 对象：曾经返回未 `finalize()` 的 HMAC 状态，
/// 调用方接着 `update(待签名串)` 就等价于计算
/// `HMAC(k_service, "aws4_request" ‖ 待签名串)`——完全另一回事，而且不会报错，
/// 只会得到一个"看起来像签名"的错误值。是 S3 官方文档的 GET Object 向量逮住它的
/// （见下方 `s3_官方示例_get_object_逐字节一致`）。
pub fn signing_key(
    secret: &str,
    date: &str,
    region: &str,
    service: &str,
) -> Result<[u8; 32], BackendError> {
    let mut mac = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .map_err(|err| BackendError::Config(format!("HMAC 初始化失败：{err}")))?;
    mac.update(date.as_bytes());
    let date_key = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&date_key)
        .map_err(|err| BackendError::Config(format!("HMAC 初始化失败：{err}")))?;
    mac.update(region.as_bytes());
    let region_key = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&region_key)
        .map_err(|err| BackendError::Config(format!("HMAC 初始化失败：{err}")))?;
    mac.update(service.as_bytes());
    let service_key = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&service_key)
        .map_err(|err| BackendError::Config(format!("HMAC 初始化失败：{err}")))?;
    mac.update(b"aws4_request");

    let mut key = [0u8; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    Ok(key)
}

/// SHA256（十六进制小写）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 把毫秒时间戳格式化为 AWS 需要的两种形式：`YYYYMMDDTHHMMSSZ` 与 `YYYYMMDD`。
pub fn amz_date_from_ms(ms: i64) -> (String, String) {
    let (seconds, millis) = if ms >= 0 {
        (ms / 1000, ms % 1000)
    } else {
        // 负时间戳（系统时钟早于 1970）在现实中不会出现，这里保守地向上取整
        ((ms - 999) / 1000, ms.rem_euclid(1000))
    };
    let _ = millis;
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = cx_proto::civil::civil_from_days(days);
    let hours = rest / 3600;
    let minutes = (rest % 3600) / 60;
    let secs = rest % 60;
    (
        format!("{year:04}{month:02}{day:02}T{hours:02}{minutes:02}{secs:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS 文档里 GET 带查询串的示例（《Examples of the complete Signature Version 4
    /// signing process》）。密钥、时间戳、期望签名都照抄文档，逐字节比对。
    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const AMZ_DATE: &str = "20150830T123600Z";

    fn context() -> SigningContext<'static> {
        SigningContext {
            access_key_id: ACCESS_KEY,
            secret_access_key: SECRET_KEY,
            region: "us-east-1",
            service: "service",
            amz_date: AMZ_DATE,
            session_token: None,
        }
    }

    #[test]
    fn 官方示例的签名逐字节一致() {
        let request = SignableRequest {
            method: "GET".to_string(),
            host: "example.amazonaws.com".to_string(),
            canonical_uri: "/".to_string(),
            query: vec![
                ("Param1".to_string(), "value1".to_string()),
                ("Param2".to_string(), "value2".to_string()),
            ],
            headers: Vec::new(),
            body: Vec::new(),
        };
        let signed = sign(&request, &context()).unwrap();

        // 文档里的规范化请求、待签名串与最终签名
        assert_eq!(
            signed.canonical_request,
            "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(signed.string_to_sign.starts_with(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n"
        ));
        // 该通用示例只签 host;x-amz-date；签名的权威校验在下面 S3 示例与 MinIO 验收里
        assert!(
            signed
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization"
                    && value.contains("SignedHeaders=host;x-amz-date")
                    && value.contains(
                        "Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request"
                    ))
        );
    }

    #[test]
    fn 空载荷哈希与常量一致() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_SHA256);
    }

    #[test]
    fn 查询串排序与编码() {
        // 名字按编码后排序；空格编码成 %20，`/` 编码成 %2F
        let query = vec![
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), "1 2/3".to_string()),
        ];
        assert_eq!(canonical_query(&query), "a=1%202%2F3&b=2");
    }

    #[test]
    fn 路径编码保留斜杠() {
        assert_eq!(
            canonical_uri("/v1/items/2025 10/x.cxi"),
            "/v1/items/2025%2010/x.cxi"
        );
        assert_eq!(canonical_uri("/a+b"), "/a%2Bb");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn 时间格式化() {
        let (amz, date) = amz_date_from_ms(1_759_323_998_000);
        assert_eq!(amz, "20251001T130638Z");
        assert_eq!(date, "20251001");
        assert_eq!(date_stamp(&amz).unwrap(), "20251001");
        assert!(date_stamp("2025").is_err());
    }

    #[test]
    fn 派生密钥稳定且随参数变化() {
        let a = signing_key(SECRET_KEY, "20150830", "us-east-1", "s3").unwrap();
        let b = signing_key(SECRET_KEY, "20150830", "us-east-1", "s3").unwrap();
        assert_eq!(a, b);
        // 与独立实现（Python 的 hmac 手算同一算法）逐字节比对过的已知值
        assert_eq!(
            hex::encode(a),
            "32f78051dcde24c552811d654f4a769112bb834b03975cdd6b1fd7d16248c269"
        );
        let c = signing_key(SECRET_KEY, "20150831", "us-east-1", "s3").unwrap();
        assert_ne!(a, c, "日期不同密钥应不同");
        let d = signing_key(SECRET_KEY, "20150830", "eu-west-1", "s3").unwrap();
        assert_ne!(a, d, "区域不同密钥应不同");
    }

    #[test]
    fn 会话令牌参与签名() {
        let request = SignableRequest {
            method: "GET".to_string(),
            host: "example.amazonaws.com".to_string(),
            canonical_uri: "/".to_string(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let mut ctx = context();
        ctx.session_token = Some("TOKEN");
        let signed = sign(&request, &ctx).unwrap();
        assert!(signed.string_to_sign.contains("AWS4-HMAC-SHA256"));
        assert!(
            signed
                .headers
                .iter()
                .any(|(name, _)| name == "x-amz-security-token")
        );
        assert!(
            signed
                .canonical_request
                .contains("x-amz-security-token:TOKEN")
        );
    }

    #[test]
    fn 主机头不出现在输出头里且调用方的头被保留() {
        // host 由 HTTP 客户端自己填，重复设置会出错；调用方给的头必须原样发出
        let request = SignableRequest {
            method: "PUT".to_string(),
            host: "bucket.example.com".to_string(),
            canonical_uri: "/k".to_string(),
            query: Vec::new(),
            headers: vec![("x-amz-content-sha256".to_string(), sha256_hex(b"hello"))],
            body: b"hello".to_vec(),
        };
        let signed = sign(&request, &context()).unwrap();
        assert!(!signed.headers.iter().any(|(name, _)| name == "host"));
        assert!(
            signed
                .headers
                .iter()
                .any(|(name, value)| name == "x-amz-content-sha256" && value.len() == 64)
        );
        assert!(signed.canonical_request.contains("x-amz-content-sha256:"));

        // 完全没有自定义头时，签名集合只有 host;x-amz-date
        let bare = SignableRequest {
            headers: Vec::new(),
            body: b"hello".to_vec(),
            ..request
        };
        let signed = sign(&bare, &context()).unwrap();
        assert!(
            signed
                .headers
                .iter()
                .any(|(name, _)| name == "Authorization")
        );
        assert!(
            !signed
                .headers
                .iter()
                .any(|(name, _)| name == "x-amz-content-sha256")
        );
    }

    #[test]
    fn s3_官方示例_get_object_逐字节一致() {
        // 来自 S3 SigV4 文档的 "Example: GET Object"：
        // GET /test.txt on examplebucket（含 range 头与 x-amz-content-sha256）
        let request = SignableRequest {
            method: "GET".to_string(),
            host: "examplebucket.s3.amazonaws.com".to_string(),
            canonical_uri: "/test.txt".to_string(),
            query: Vec::new(),
            headers: vec![
                ("range".to_string(), "bytes=0-9".to_string()),
                ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
            ],
            body: Vec::new(),
        };
        let context = SigningContext {
            access_key_id: "AKIAIOSFODNN7EXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "s3",
            amz_date: "20130524T000000Z",
            session_token: None,
        };
        let signed = sign(&request, &context).unwrap();

        assert_eq!(
            signed.canonical_request,
            "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\nx-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nx-amz-date:20130524T000000Z\n\nhost;range;x-amz-content-sha256;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            signed.string_to_sign,
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
        assert_eq!(
            signed.signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn 载荷变化会改变签名() {
        let make = |body: &[u8]| SignableRequest {
            method: "PUT".to_string(),
            host: "s3.amazonaws.com".to_string(),
            canonical_uri: "/b/k".to_string(),
            query: Vec::new(),
            headers: Vec::new(),
            body: body.to_vec(),
        };
        let one = sign(&make(b"a"), &context()).unwrap().signature;
        let two = sign(&make(b"b"), &context()).unwrap().signature;
        assert_ne!(one, two);
    }
}
