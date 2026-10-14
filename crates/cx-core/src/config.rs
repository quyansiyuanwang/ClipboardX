//! `config.toml` 的模型、默认值与校验。
//!
//! 三条硬规矩：
//! 1. **配置文件里不出现明文凭据**：只允许 `*_file` 或 `*_env`，`cx backend add`
//!    用 `--password-stdin` 把凭据写进 0600 的文件。
//! 2. **未知字段被容忍**：新版本写的配置在旧版本里至少能启动（向前兼容），
//!    真正不兼容的改动改 `v`。
//! 3. **校验返回全部问题**而不是第一个：`cx doctor` 需要一次性告诉用户所有毛病。

use crate::error::CoreError;
use crate::fsutil;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};

/// 配置格式版本。
pub const CONFIG_VERSION: u32 = 1;

fn default_version() -> u32 {
    CONFIG_VERSION
}

fn default_true() -> bool {
    true
}

/// 顶层配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// 格式版本。
    #[serde(default = "default_version")]
    pub v: u32,
    /// 设备相关。
    #[serde(default)]
    pub device: DeviceSection,
    /// 空间相关。
    #[serde(default)]
    pub space: SpaceSection,
    /// 同步行为。
    #[serde(default)]
    pub sync: SyncSection,
    /// 同步后端（可多个，同时生效）。
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendSection>,
}

/// `[device]`。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceSection {
    /// 设备名；缺省时取主机名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `[space]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSection {
    /// 空间 id；`cx space init` / `cx space join` 写入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 是否端到端加密载荷。
    #[serde(default = "default_true")]
    pub encrypt: bool,
    /// 本机空间密钥文件名（相对配置目录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
}

impl Default for SpaceSection {
    fn default() -> Self {
        Self {
            id: None,
            encrypt: true,
            key_file: None,
        }
    }
}

/// `[sync]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSection {
    /// 剪贴板轮询兜底间隔（毫秒）。平台事件可用时只是兜底。
    #[serde(default = "default_watch_interval")]
    pub watch_interval_ms: u64,
    /// 变化去抖：复制过程中会连续触发，等这么久再处理。
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    /// 近期有活动时的轮询间隔。
    #[serde(default = "default_poll_active")]
    pub poll_active_ms: u64,
    /// 长时间空闲时的轮询间隔。
    #[serde(default = "default_poll_idle")]
    pub poll_idle_ms: u64,
    /// 收到远端内容后的处理策略。
    #[serde(default)]
    pub apply_remote: ApplyRemoteMode,
    /// 单条内容上限（字节），超限跳过。
    #[serde(default = "default_max_item_bytes")]
    pub max_item_bytes: u64,
    /// 历史保留条数。
    #[serde(default = "default_history_max_items")]
    pub history_max_items: usize,
    /// 历史保留天数。
    #[serde(default = "default_history_max_days")]
    pub history_max_days: u64,
    /// 文本换行是否归一化（默认否：保持原始字节，粘贴保真）。
    #[serde(default)]
    pub normalize_newlines: bool,
    /// 是否忽略纯空白内容。
    #[serde(default = "default_true")]
    pub ignore_blank_text: bool,
    /// 忽略规则（glob，整串匹配；写 `*关键字*` 表示包含）。
    #[serde(default)]
    pub ignore_globs: Vec<String>,
}

fn default_watch_interval() -> u64 {
    500
}
fn default_debounce() -> u64 {
    200
}
fn default_poll_active() -> u64 {
    5_000
}
fn default_poll_idle() -> u64 {
    30_000
}
fn default_max_item_bytes() -> u64 {
    4 * 1024 * 1024
}
fn default_history_max_items() -> usize {
    200
}
fn default_history_max_days() -> u64 {
    30
}

impl Default for SyncSection {
    fn default() -> Self {
        Self {
            watch_interval_ms: default_watch_interval(),
            debounce_ms: default_debounce(),
            poll_active_ms: default_poll_active(),
            poll_idle_ms: default_poll_idle(),
            apply_remote: ApplyRemoteMode::default(),
            max_item_bytes: default_max_item_bytes(),
            history_max_items: default_history_max_items(),
            history_max_days: default_history_max_days(),
            normalize_newlines: false,
            ignore_blank_text: true,
            ignore_globs: Vec::new(),
        }
    }
}

/// 收到远端内容后的应用策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyRemoteMode {
    /// 始终把最新内容写入本机剪贴板（默认）。
    #[default]
    Latest,
    /// 仅当本机剪贴板是空闲（未被本地用户占用）时才写入。
    OnlyIfIdle,
    /// 从不自动写入，只入库，由用户手动 `cx copy`。
    Manual,
}

impl ApplyRemoteMode {
    /// 展示用字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyRemoteMode::Latest => "latest",
            ApplyRemoteMode::OnlyIfIdle => "only_if_idle",
            ApplyRemoteMode::Manual => "manual",
        }
    }
}

/// 后端种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// 自建服务端。
    Server,
    /// WebDAV（Nextcloud 等）。
    Webdav,
    /// S3 兼容对象存储。
    S3,
}

impl BackendKind {
    /// 展示用字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Server => "server",
            BackendKind::Webdav => "webdav",
            BackendKind::S3 => "s3",
        }
    }
}

/// `[[backend]]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSection {
    /// 后端名（唯一，用于 `cx backend` 子命令与凭据文件名）。
    pub name: String,
    /// 后端种类。
    pub kind: BackendKind,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 服务端/WebDAV 的基址。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// 空间 id（缺省用 `[space].id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// 设备令牌文件名（相对 `secrets/`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,

    /// WebDAV 用户名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// WebDAV 口令文件名（相对 `secrets/`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<String>,
    /// WebDAV 口令所在环境变量名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// 自定义 CA 证书（私有 CA 场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// 跳过 TLS 校验（仅调试，`cx doctor` 会告警）。
    #[serde(default)]
    pub insecure: bool,

    /// S3 桶名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// S3 前缀（建议以 `/` 结尾）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// S3 区域。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// S3 端点（MinIO / R2 等自建服务）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// S3 Access Key ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    /// S3 Secret 文件名（相对 `secrets/`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key_file: Option<String>,
    /// S3 Secret 环境变量名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key_env: Option<String>,
    /// 是否使用 path-style 寻址（MinIO 通常需要）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_style: Option<bool>,

    /// 该后端的轮询间隔覆盖（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            v: CONFIG_VERSION,
            device: DeviceSection::default(),
            space: SpaceSection::default(),
            sync: SyncSection::default(),
            backends: Vec::new(),
        }
    }
}

impl Config {
    /// 读取配置；文件不存在时返回默认值（首启场景）。
    pub fn load(paths: &Paths) -> Result<Self, CoreError> {
        match fsutil::read_to_string_optional(&paths.config_file())? {
            Some(text) => {
                let config: Self = toml::from_str(&text)
                    .map_err(|e| CoreError::malformed("config.toml", e.to_string()))?;
                config.validate()?;
                Ok(config)
            }
            None => Ok(Self::default()),
        }
    }

    /// 读取配置；不存在则写入默认配置（便于用户直接编辑）。
    pub fn load_or_create(paths: &Paths) -> Result<Self, CoreError> {
        let path = paths.config_file();
        if fsutil::read_to_string_optional(&path)?.is_none() {
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }
        Self::load(paths)
    }

    /// 落盘（原子写 + 收紧权限）。
    pub fn save(&self, paths: &Paths) -> Result<(), CoreError> {
        let text = self.to_toml()?;
        let path = paths.config_file();
        fsutil::write_atomic(&path, text.as_bytes())?;
        fsutil::tighten_permissions(&path, false)?;
        Ok(())
    }

    /// 序列化为 TOML。
    pub fn to_toml(&self) -> Result<String, CoreError> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| CoreError::malformed("config.toml", e.to_string()))?;
        let header = "# ClipboardX 配置\n\
                      # 凭据不要写在这里：用 cx backend add ... --password-stdin，\n\
                      # 或引用 *_env 环境变量。\n\n";
        Ok(format!("{header}{body}"))
    }

    /// 找到某个后端。
    pub fn backend(&self, name: &str) -> Option<&BackendSection> {
        self.backends.iter().find(|b| b.name == name)
    }

    /// 启用的后端。
    pub fn enabled_backends(&self) -> impl Iterator<Item = &BackendSection> {
        self.backends.iter().filter(|b| b.enabled)
    }

    /// 列出全部配置问题（`cx doctor` 用）。
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.v != CONFIG_VERSION {
            problems.push(format!(
                "配置版本 {} 与当前支持的 {} 不一致",
                self.v, CONFIG_VERSION
            ));
        }

        // 后端名唯一且可用于文件名
        let mut seen = std::collections::BTreeSet::new();
        for backend in &self.backends {
            if backend.name.trim().is_empty() {
                problems.push("存在没有 name 的后端".to_string());
                continue;
            }
            if !seen.insert(backend.name.clone()) {
                problems.push(format!("后端名重复：{}", backend.name));
            }
            if backend.name.contains(['/', '\\']) || backend.name.contains("..") {
                problems.push(format!("后端名不能包含路径分隔符：{}", backend.name));
            }
        }

        // 每种后端的必填项与凭据冲突
        for backend in &self.backends {
            let name = &backend.name;
            match backend.kind {
                BackendKind::Server => {
                    if backend.url.as_deref().unwrap_or("").trim().is_empty() {
                        problems.push(format!("后端 {name}（server）缺少 url"));
                    }
                    if backend.token_file.is_some() && backend.password_file.is_some() {
                        problems.push(format!(
                            "后端 {name} 同时配置了 token_file 与 password_file"
                        ));
                    }
                }
                BackendKind::Webdav => {
                    if backend.url.as_deref().unwrap_or("").trim().is_empty() {
                        problems.push(format!("后端 {name}（webdav）缺少 url"));
                    }
                    if backend.username.is_none() {
                        problems.push(format!("后端 {name}（webdav）缺少 username"));
                    }
                    if backend.password_file.is_some() && backend.password_env.is_some() {
                        problems.push(format!(
                            "后端 {name}（webdav）同时配置了 password_file 与 password_env"
                        ));
                    }
                }
                BackendKind::S3 => {
                    if backend.bucket.as_deref().unwrap_or("").trim().is_empty() {
                        problems.push(format!("后端 {name}（s3）缺少 bucket"));
                    }
                    if backend.secret_access_key_file.is_some()
                        && backend.secret_access_key_env.is_some()
                    {
                        problems.push(format!(
                            "后端 {name}（s3）同时配置了 secret_access_key_file 与 secret_access_key_env"
                        ));
                    }
                }
            }
            if backend.insecure {
                problems.push(format!("后端 {name} 关闭了 TLS 校验（仅应在调试时使用）"));
            }
        }

        // 数值合理性
        if self.sync.watch_interval_ms < 50 {
            problems.push("sync.watch_interval_ms 过小（应 ≥ 50）".to_string());
        }
        if self.sync.debounce_ms > self.sync.watch_interval_ms * 10 {
            problems
                .push("sync.debounce_ms 远大于 watch_interval_ms，可能吞掉连续复制".to_string());
        }
        if self.sync.poll_active_ms == 0 || self.sync.poll_idle_ms == 0 {
            problems.push("sync 轮询间隔不能为 0".to_string());
        }
        if self.sync.poll_active_ms > self.sync.poll_idle_ms {
            problems.push("sync.poll_active_ms 不应大于 poll_idle_ms".to_string());
        }
        if self.sync.max_item_bytes < 1024 {
            problems.push("sync.max_item_bytes 过小（应 ≥ 1024）".to_string());
        }
        if self.sync.history_max_items == 0 {
            problems.push("sync.history_max_items 不能为 0".to_string());
        }
        if self.sync.ignore_globs.iter().any(|g| g.is_empty()) {
            problems.push("sync.ignore_globs 含有空规则（会匹配一切）".to_string());
        }

        problems
    }

    /// 严格校验：有任一问题即失败。
    pub fn validate(&self) -> Result<(), CoreError> {
        let problems = self.problems();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Config(problems.join("；")))
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 默认配置自洽() {
        let config = Config::default();
        assert!(config.problems().is_empty(), "{:?}", config.problems());
        assert_eq!(config.sync.apply_remote, ApplyRemoteMode::Latest);
        assert!(config.space.encrypt);
        assert!(config.backends.is_empty());
    }

    #[test]
    fn 缺失文件返回默认配置() {
        let paths = temp_paths("missing");
        let config = Config::load(&paths).unwrap();
        assert_eq!(config, Config::default());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn load_or_create_写出可复读的配置() {
        let paths = temp_paths("create");
        let created = Config::load_or_create(&paths).unwrap();
        assert!(paths.config_file().exists());
        let reloaded = Config::load_or_create(&paths).unwrap();
        assert_eq!(created, reloaded);
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn toml_往返保留全部字段() {
        let mut config = Config::default();
        config.device.name = Some("workstation".to_string());
        config.space.id = Some("space-1".to_string());
        config.sync.apply_remote = ApplyRemoteMode::OnlyIfIdle;
        config.sync.ignore_globs = vec!["*密码*".to_string()];
        config.sync.max_item_bytes = 1024 * 1024;
        config.backends.push(BackendSection {
            name: "nextcloud".to_string(),
            kind: BackendKind::Webdav,
            enabled: true,
            url: Some(
                "https://cloud.example.com/remote.php/dav/files/alice/clipboardx".to_string(),
            ),
            space: None,
            token_file: None,
            username: Some("alice".to_string()),
            password_file: Some("nextcloud.pass".to_string()),
            password_env: None,
            ca_file: Some("corp-ca.pem".to_string()),
            insecure: false,
            bucket: None,
            prefix: None,
            region: None,
            endpoint: None,
            access_key_id: None,
            secret_access_key_file: None,
            secret_access_key_env: None,
            path_style: None,
            poll_interval_ms: Some(10_000),
        });

        let text = config.to_toml().unwrap();
        assert!(text.contains("[[backend]]"));
        assert!(text.contains("kind = \"webdav\""));
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn 未知字段被容忍() {
        let text = r#"
v = 1
future_feature = "whatever"

[sync]
poll_idle_ms = 60000
another_future = 3
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.sync.poll_idle_ms, 60_000);
        assert_eq!(config.sync.poll_active_ms, 5_000, "未写字段用默认值");
    }

    #[test]
    fn 后端名重复被拒绝() {
        let mut config = Config::default();
        for _ in 0..2 {
            config.backends.push(BackendSection {
                name: "dup".to_string(),
                kind: BackendKind::Server,
                enabled: true,
                url: Some("https://x".to_string()),
                space: None,
                token_file: None,
                username: None,
                password_file: None,
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
                path_style: None,
                poll_interval_ms: None,
            });
        }
        assert!(config.validate().is_err());
    }

    #[test]
    fn 缺少必填项被拒绝() {
        let mut config = Config::default();
        config.backends.push(BackendSection {
            name: "s3x".to_string(),
            kind: BackendKind::S3,
            enabled: true,
            url: None,
            space: None,
            token_file: None,
            username: None,
            password_file: None,
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
            path_style: None,
            poll_interval_ms: None,
        });
        let problems = config.problems();
        assert!(
            problems.iter().any(|p| p.contains("bucket")),
            "{problems:?}"
        );
    }

    #[test]
    fn 凭据冲突被拒绝() {
        let mut config = Config::default();
        config.backends.push(BackendSection {
            name: "nc".to_string(),
            kind: BackendKind::Webdav,
            enabled: true,
            url: Some("https://x".to_string()),
            space: None,
            token_file: None,
            username: Some("a".to_string()),
            password_file: Some("f".to_string()),
            password_env: Some("E".to_string()),
            ca_file: None,
            insecure: false,
            bucket: None,
            prefix: None,
            region: None,
            endpoint: None,
            access_key_id: None,
            secret_access_key_file: None,
            secret_access_key_env: None,
            path_style: None,
            poll_interval_ms: None,
        });
        assert!(
            config
                .problems()
                .iter()
                .any(|p| p.contains("password_file"))
        );
    }

    #[test]
    fn 数值越界被拒绝() {
        let mut config = Config::default();
        config.sync.poll_active_ms = 60_000;
        config.sync.poll_idle_ms = 1_000;
        assert!(
            config
                .problems()
                .iter()
                .any(|p| p.contains("poll_active_ms"))
        );

        let mut config = Config::default();
        config.sync.history_max_items = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.sync.ignore_globs = vec![String::new()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn 非法toml不panic() {
        let paths = temp_paths("bad");
        std::fs::write(paths.config_file(), b"[[[ not toml").unwrap();
        assert!(Config::load(&paths).is_err());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 未知后端种类报错() {
        let text = r#"
[[backend]]
name = "x"
kind = "ftp"
"#;
        assert!(toml::from_str::<Config>(text).is_err());
    }
}
