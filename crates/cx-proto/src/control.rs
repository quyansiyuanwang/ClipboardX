//! 本地控制协议（CLI/TUI ↔ 守护进程）。
//!
//! 传输：仅绑定 `127.0.0.1` 的 TCP，认证通过 `control.token`（0600）完成，
//! 之后是 JSON Lines 的请求/响应；`subscribe` 之后同一连接变成事件流。
//!
//! 为什么不用 Unix 域套接字：Windows 上没有它，而"回环 TCP + 令牌文件"在三端
//! 行为完全一致，代码只有一份。
//!
//! 为什么用 JSON 而不是更紧凑的编码：这是本机 IPC，流量可以忽略；可读性的收益
//! （用 `nc` 就能手工调试、日志直接看懂）远大于那点字节数。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// 控制协议版本。握手时客户端必须声明，服务端不匹配即拒绝。
pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

/// 命令名常量（避免各处硬编码字符串）。
pub mod cmd {
    /// 查询状态。
    pub const STATUS: &str = "status";
    /// 暂停同步。
    pub const PAUSE: &str = "pause";
    /// 恢复同步。
    pub const RESUME: &str = "resume";
    /// 列出历史。
    pub const HISTORY_LIST: &str = "history.list";
    /// 查看某条历史的内容预览。
    pub const HISTORY_SHOW: &str = "history.show";
    /// 把历史中的某条写回本机剪贴板。
    pub const COPY: &str = "copy";
    /// 立即发布当前剪贴板（或指定内容）。
    pub const PUSH: &str = "push";
    /// 立即拉取一次所有后端。
    pub const PULL: &str = "pull";
    /// 本机诊断。
    pub const DOCTOR: &str = "doctor";
    /// 订阅事件流。
    pub const SUBSCRIBE: &str = "subscribe";
    /// 优雅退出守护进程。
    pub const SHUTDOWN: &str = "shutdown";
    /// 重载配置。
    pub const RELOAD: &str = "reload";
}

/// 错误码常量。
pub mod errcode {
    /// 未认证或令牌错误。
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// 协议版本不匹配。
    pub const PROTOCOL: &str = "protocol_mismatch";
    /// 命令参数非法。
    pub const BAD_ARGS: &str = "bad_args";
    /// 未知命令。
    pub const UNKNOWN_CMD: &str = "unknown_command";
    /// 内部错误。
    pub const INTERNAL: &str = "internal";
    /// 尚未初始化空间。
    pub const NOT_INITIALIZED: &str = "not_initialized";
}

/// 事件名常量。
pub mod event {
    /// 本机剪贴板产生了新的已发布内容。
    pub const CLIP_PUBLISHED: &str = "clip.published";
    /// 远端内容被应用到本机剪贴板。
    pub const CLIP_APPLIED: &str = "clip.applied";
    /// 内容被跳过（忽略规则/超限/暂停）。
    pub const CLIP_SKIPPED: &str = "clip.skipped";
    /// 状态发生变化（暂停、后端错误等）。
    pub const STATUS_CHANGED: &str = "status.changed";
    /// 守护进程即将退出。
    pub const SHUTTING_DOWN: &str = "shutting_down";
}

/// 握手：客户端发出的第一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Auth {
    /// 控制令牌内容。
    pub auth: String,
    /// 客户端协议版本。
    pub proto: u32,
}

/// 请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// 请求序号（响应里原样返回）。
    pub id: u64,
    /// 命令名，见 [`cmd`]。
    pub cmd: String,
    /// 参数（可选，缺省为空对象）。
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
}

impl Request {
    /// 构造一个请求。
    pub fn new(id: u64, cmd: impl Into<String>) -> Self {
        Self {
            id,
            cmd: cmd.into(),
            args: Map::new(),
        }
    }

    /// 带一个参数。
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.args.insert(key.to_string(), value.into());
        self
    }

    /// 取整型参数。
    pub fn arg_u64(&self, key: &str) -> Option<u64> {
        self.args.get(key).and_then(Value::as_u64)
    }

    /// 取字符串参数。
    pub fn arg_str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }

    /// 取布尔参数。
    pub fn arg_bool(&self, key: &str) -> Option<bool> {
        self.args.get(key).and_then(Value::as_bool)
    }
}

/// 错误体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// 机器可读的错误码，见 [`errcode`]。
    pub code: String,
    /// 人类可读的说明。
    pub msg: String,
}

/// 响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// 对应请求的序号。
    pub id: u64,
    /// 是否成功。
    pub ok: bool,
    /// 成功时的数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// 失败时的错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Reply {
    /// 成功响应。
    pub fn ok(id: u64, data: impl Into<Value>) -> Self {
        Self {
            id,
            ok: true,
            data: Some(data.into()),
            error: None,
        }
    }

    /// 成功但没有数据。
    pub fn done(id: u64) -> Self {
        Self {
            id,
            ok: true,
            data: None,
            error: None,
        }
    }

    /// 失败响应。
    pub fn err(id: u64, code: &str, msg: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            data: None,
            error: Some(ErrorBody {
                code: code.to_string(),
                msg: msg.into(),
            }),
        }
    }
}

/// 订阅推送的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// 事件名，见 [`event`]。
    pub event: String,
    /// 事件载荷。
    #[serde(default)]
    pub data: Value,
}

impl Event {
    /// 构造事件。
    pub fn new(name: &str, data: impl Into<Value>) -> Self {
        Self {
            event: name.to_string(),
            data: data.into(),
        }
    }
}

/// 单个后端的运行状态摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    /// 后端名。
    pub name: String,
    /// 后端种类。
    pub kind: String,
    /// 是否启用。
    pub enabled: bool,
    /// 最近一次成功时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    /// 最近一次错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 连续失败次数。
    pub failures: u32,
    /// 下次允许尝试时间。
    pub next_attempt_ms: u64,
    /// 已发布条数。
    pub published: u64,
    /// 已应用条数。
    pub applied: u64,
}

/// `status` 的返回体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// `cx` 版本。
    pub version: String,
    /// 守护进程 pid。
    pub pid: u32,
    /// 启动时间。
    pub started_ms: u64,
    /// 是否暂停。
    pub paused: bool,
    /// 设备 id。
    pub device_id: String,
    /// 设备名。
    pub device_name: String,
    /// 空间 id（未加入空间时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// 空间是否加密。
    pub encrypted: bool,
    /// 各后端状态。
    pub backends: Vec<BackendStatus>,
    /// 历史条数。
    pub history_len: usize,
    /// 已处理对象数。
    pub seen_len: usize,
    /// 因超限跳过条数。
    pub skipped_oversize: u64,
    /// 因忽略规则跳过条数。
    pub skipped_ignored: u64,
    /// 本机最后发布的内容哈希。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_published_hash: Option<String>,
    /// 剪贴板后端描述。
    pub clipboard: String,
    /// 监听方式描述（平台事件 / 轮询兜底）。
    pub watch: String,
    /// 是否以 `--no-clipboard` 运行（纯中继）。
    pub no_clipboard: bool,
}

/// 历史条目摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySummary {
    /// 位置（0 = 最新）。
    pub position: usize,
    /// 对象名。
    pub name: String,
    /// 内容类型。
    pub kind: String,
    /// 产生时间（毫秒，UTC）。
    pub created_ms: u64,
    /// 载荷字节数。
    pub bytes: u32,
    /// 内容哈希。
    pub hash: String,
    /// 来源设备名。
    pub origin: String,
    /// 来源设备 id。
    pub origin_device: String,
}

/// 历史内容预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPreview {
    /// 位置（0 = 最新）。
    pub position: usize,
    /// 内容类型。
    pub kind: String,
    /// 文本内容（图片时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 图片尺寸描述（文本时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// 是否因超出 `max_chars` 被截断。
    pub truncated: bool,
    /// 载荷字节数。
    pub bytes: u32,
}

/// 单条诊断结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// 检查项名。
    pub name: String,
    /// 是否通过。
    pub ok: bool,
    /// 说明。
    pub detail: String,
}

/// `doctor` 的返回体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// 全部检查项。
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// 构造。
    pub fn new(checks: Vec<DoctorCheck>) -> Self {
        Self { checks }
    }

    /// 是否全部通过。
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// 失败项数量。
    pub fn failed(&self) -> usize {
        self.checks.iter().filter(|c| !c.ok).count()
    }
}

/// 便捷构造一个后端状态。
pub fn backend_status(
    name: &str,
    kind: &str,
    state: &crate::control::BackendStateView,
) -> BackendStatus {
    BackendStatus {
        name: name.to_string(),
        kind: kind.to_string(),
        enabled: state.enabled,
        last_ok_ms: state.last_ok_ms,
        last_error: state.last_error.clone(),
        failures: state.failures,
        next_attempt_ms: state.next_attempt_ms,
        published: state.published,
        applied: state.applied,
    }
}

/// 与 `cx-core::BackendState` 同构的只读视图。
///
/// 单独定义是为了让 `cx-proto` 不反向依赖 `cx-core`（保持依赖方向单向）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackendStateView {
    /// 是否启用。
    pub enabled: bool,
    /// 最近成功时间。
    pub last_ok_ms: Option<u64>,
    /// 最近错误。
    pub last_error: Option<String>,
    /// 连续失败次数。
    pub failures: u32,
    /// 下次尝试时间。
    pub next_attempt_ms: u64,
    /// 已发布条数。
    pub published: u64,
    /// 已应用条数。
    pub applied: u64,
}

/// 把 `serde_json::Value` 包成 `Event` 的便捷函数。
pub fn event_with_count(name: &str, count: u64) -> Event {
    Event::new(name, json!({ "count": count }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 请求与响应_json_往返() {
        let request = Request::new(7, cmd::PUSH)
            .with("force", true)
            .with("text", "你好");
        let line = serde_json::to_string(&request).unwrap();
        assert!(line.contains("\"force\":true"));
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back, request);
        assert_eq!(back.arg_bool("force"), Some(true));
        assert_eq!(back.arg_str("text"), Some("你好"));
        assert_eq!(back.arg_u64("nope"), None);

        let reply = Reply::ok(7, json!({ "ok": true }));
        let line = serde_json::to_string(&reply).unwrap();
        let back: Reply = serde_json::from_str(&line).unwrap();
        assert!(back.ok);
        assert!(back.error.is_none());
    }

    #[test]
    fn 空参数不参与序列化() {
        let request = Request::new(1, cmd::STATUS);
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains("args"), "空参数应被省略：{line}");
    }

    #[test]
    fn 错误响应结构正确() {
        let reply = Reply::err(3, errcode::BAD_ARGS, "缺少参数");
        assert!(!reply.ok);
        assert_eq!(reply.error.as_ref().unwrap().code, "bad_args");
        let line = serde_json::to_string(&reply).unwrap();
        assert!(!line.contains("\"data\""), "失败响应不应带 data");
    }

    #[test]
    fn 状态报告_json_往返() {
        let status = StatusReport {
            version: "0.1.0".to_string(),
            pid: 1234,
            started_ms: 1,
            paused: false,
            device_id: "ab12cd34ef5678901234567890abcdef".to_string(),
            device_name: "ws".to_string(),
            space_id: Some("aa11bb22".to_string()),
            encrypted: true,
            backends: vec![BackendStatus {
                name: "home".to_string(),
                kind: "server".to_string(),
                enabled: true,
                last_ok_ms: Some(5),
                last_error: None,
                failures: 0,
                next_attempt_ms: 6,
                published: 2,
                applied: 3,
            }],
            history_len: 4,
            seen_len: 5,
            skipped_oversize: 6,
            skipped_ignored: 7,
            last_published_hash: Some("beef".to_string()),
            clipboard: "系统剪贴板（windows）".to_string(),
            watch: "平台事件 + 500 ms 兜底".to_string(),
            no_clipboard: false,
        };
        let line = serde_json::to_string(&status).unwrap();
        let back: StatusReport = serde_json::from_str(&line).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn 诊断报告统计() {
        let report = DoctorReport::new(vec![
            DoctorCheck {
                name: "配置".to_string(),
                ok: true,
                detail: "无问题".to_string(),
            },
            DoctorCheck {
                name: "剪贴板".to_string(),
                ok: false,
                detail: "无图形会话".to_string(),
            },
        ]);
        assert!(!report.all_ok());
        assert_eq!(report.failed(), 1);
    }

    #[test]
    fn 事件构造() {
        let event = Event::new(event::CLIP_PUBLISHED, json!({"name": "x.cxi"}));
        assert_eq!(event.event, "clip.published");
        assert_eq!(event.data["name"], "x.cxi");
        assert_eq!(event_with_count(event::CLIP_SKIPPED, 3).data["count"], 3);
    }

    #[test]
    fn 非法_json_不panic() {
        for bad in ["", "{", "null", "{\"id\":\"x\"}", "[]"] {
            assert!(
                serde_json::from_str::<Request>(bad).is_err(),
                "应拒绝 {bad}"
            );
            assert!(serde_json::from_str::<Reply>(bad).is_err(), "应拒绝 {bad}");
        }
    }

    #[test]
    fn 协议版本常量() {
        assert_eq!(CONTROL_PROTOCOL_VERSION, 1);
    }
}
