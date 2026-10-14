//! 服务端错误类型。

/// 服务端操作的失败。
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// 请求本身不合法（路径非法、JSON 坏了）。
    #[error("请求不合法：{0}")]
    BadRequest(String),

    /// 缺少或错误的令牌。
    #[error("未授权：{0}")]
    Unauthorized(String),

    /// 有令牌但无权（空间不匹配、注册密钥不对）。
    #[error("禁止访问：{0}")]
    Forbidden(String),

    /// 目标不存在。
    #[error("不存在：{0}")]
    NotFound(String),

    /// 条件写冲突。
    #[error("冲突：{0}")]
    Conflict(String),

    /// 对象超过单对象上限。
    #[error("对象过大：{0}")]
    TooLarge(String),

    /// 空间配额用尽。
    #[error("配额用尽：{0}")]
    QuotaExceeded(String),

    /// 服务端内部错误。
    #[error("服务端内部错误：{0}")]
    Internal(String),

    /// HTTP 语义错误（方法不允许、长度缺失等）。
    #[error("HTTP 错误 {status}：{detail}")]
    Http {
        /// 状态码。
        status: u16,
        /// 说明。
        detail: String,
    },

    /// 本地 IO。
    #[error("IO 错误（{path}）：{source}")]
    Io {
        /// 出错的路径。
        path: String,
        /// 底层错误。
        source: std::io::Error,
    },
}

impl ServerError {
    /// 构造一个带路径的 IO 错误。
    pub fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        ServerError::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    /// 映射成 HTTP 状态码。
    pub fn status(&self) -> u16 {
        match self {
            ServerError::BadRequest(_) => 400,
            ServerError::Unauthorized(_) => 401,
            ServerError::Forbidden(_) => 403,
            ServerError::NotFound(_) => 404,
            ServerError::Conflict(_) => 409,
            ServerError::TooLarge(_) => 413,
            ServerError::QuotaExceeded(_) => 507,
            ServerError::Http { status, .. } => *status,
            ServerError::Internal(_) | ServerError::Io { .. } => 500,
        }
    }

    /// 便于日志辨别的分类名。
    pub fn kind(&self) -> &'static str {
        match self {
            ServerError::BadRequest(_) => "bad_request",
            ServerError::Unauthorized(_) => "unauthorized",
            ServerError::Forbidden(_) => "forbidden",
            ServerError::NotFound(_) => "not_found",
            ServerError::Conflict(_) => "conflict",
            ServerError::TooLarge(_) => "too_large",
            ServerError::QuotaExceeded(_) => "quota",
            ServerError::Internal(_) => "internal",
            ServerError::Http { .. } => "http",
            ServerError::Io { .. } => "io",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态码映射() {
        assert_eq!(ServerError::BadRequest(String::new()).status(), 400);
        assert_eq!(ServerError::Unauthorized(String::new()).status(), 401);
        assert_eq!(ServerError::Forbidden(String::new()).status(), 403);
        assert_eq!(ServerError::NotFound(String::new()).status(), 404);
        assert_eq!(ServerError::Conflict(String::new()).status(), 409);
        assert_eq!(ServerError::TooLarge(String::new()).status(), 413);
        assert_eq!(ServerError::QuotaExceeded(String::new()).status(), 507);
        assert_eq!(ServerError::Internal(String::new()).status(), 500);
        assert_eq!(
            ServerError::Http {
                status: 405,
                detail: String::new()
            }
            .status(),
            405
        );
    }
}
