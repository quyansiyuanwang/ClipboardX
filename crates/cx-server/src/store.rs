//! 服务端存储：空间、设备令牌、对象库。
//!
//! 目录布局（服务端自己的元数据与"客户端可见的对象"分开存放，避免混在一起）：
//!
//! ```text
//! <data>/spaces/<space_id>/
//!     space.json              服务端元数据（配额、enroll 密钥哈希、保留策略）
//!     devices.json            设备令牌的哈希（磁盘上不存明文令牌）
//!     objects/<path>          对象字节（<path> 就是客户端看到的 v1/… 或 cx-space.json）
//!     meta/<path>.json        该对象的 {etag, size, created_ms}
//! ```
//!
//! 为什么给每个对象配一个 meta 文件：`list` 是轮询热路径，如果每次都把所有对象读一遍
//! 算哈希，空间里攒到几千个 item 之后会把磁盘打满。写入时算一次哈希存下来，
//! 列举就是纯目录扫描。对象是不可变的（item 用 `If-None-Match: *` 写），
//! manifest 会被覆盖，因此 meta 必须在每次写入后更新——顺序是"先写字节、再写 meta"，
//! 中途崩溃最多是 meta 缺失，列举时回退到 size+mtime 并记一条日志，不会丢数据。
//!
//! 路径安全：所有对象路径都要过 [`validate_object_path`]，拒绝 `..`、绝对路径、
//! 反斜杠与非法字符；再加上"解析后的路径必须仍在空间目录内"的兜底检查。

use crate::error::ServerError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 设备令牌前缀（便于用户一眼认出）。
pub const TOKEN_PREFIX: &str = "cxd_";
/// 注册密钥（enroll secret）前缀。
pub const ENROLL_PREFIX: &str = "cxe_";
/// 单个对象大小上限（默认 16 MiB：剪贴板内容不该更大；预留给大图片）。
pub const MAX_OBJECT_BYTES: u64 = 16 * 1024 * 1024;
/// 默认空间配额（512 MiB）。
pub const DEFAULT_QUOTA_BYTES: u64 = 512 * 1024 * 1024;

/// 服务端自有的空间元数据（与客户端上传的 `cx-space.json` 是两回事）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpaceMeta {
    /// 空间 id。
    pub id: String,
    /// 空间名（便于管理端识别）。
    pub name: String,
    /// 创建时间（毫秒）。
    pub created_ms: u64,
    /// 配额（字节，0 表示不限）。
    pub quota_bytes: u64,
    /// 已用字节（近似值，随写入更新，可被 `recount` 修正）。
    pub used_bytes: u64,
    /// 注册密钥的哈希（十六进制）；`None` 表示不允许自助注册。
    pub enroll_secret_hash: Option<String>,

    pub retention_days: u32,
}

/// 设备记录（令牌只存哈希）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    /// 令牌哈希（blake3 十六进制）。
    pub token_hash: String,
    /// 设备名。
    pub name: String,
    /// 注册时间（毫秒）。
    pub created_ms: u64,
    /// 设备公钥（enroll 时上报，用于后续密封下发空间密钥）。
    #[serde(default)]
    pub pubkey: Option<String>,
    /// 最近一次成功请求的时间（毫秒）。
    #[serde(default)]
    pub last_seen_ms: u64,
}

/// 对象的元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectMeta {
    /// 对象路径（相对空间根，例如 `v1/manifests/ab12.json`）。
    pub path: String,
    /// ETag（带引号，与 HTTP 语义一致）。
    pub etag: String,
    /// 字节数。
    pub size: u64,
    /// 写入时间（毫秒）。
    pub created_ms: u64,
}

/// 写入结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutResult {
    /// 新建。
    Created {
        /// 新对象的 ETag。
        etag: String,
    },
    /// 要求"不存在才写"，但对象已存在。
    AlreadyExists,
    /// 覆盖写成功。
    Replaced {
        /// 新 ETag。
        etag: String,
    },
}

impl PutResult {
    /// 结果的 ETag（若有）。
    pub fn etag(&self) -> Option<&str> {
        match self {
            PutResult::Created { etag } | PutResult::Replaced { etag } => Some(etag.as_str()),
            PutResult::AlreadyExists => None,
        }
    }
}

/// 一次列举的参数。
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// 键前缀。
    pub prefix: String,
    /// 只返回排在此键之后的（用于翻页）。
    pub after: Option<String>,
    /// 最多返回多少条（0 表示默认上限）。
    pub limit: usize,
}

/// 存储。
pub struct Store {
    root: PathBuf,
    /// 长轮询用的"有变化"广播；每次成功写入都递增代数并唤醒等待者。
    revision: Mutex<u64>,
    changed: Condvar,
    /// 已用空间的缓存（避免每次写入都扫盘统计）。
    used_cache: Mutex<BTreeMap<String, u64>>,
    /// 临时文件计数（保证并发写入的临时名唯一）。
    temp_counter: AtomicU64,
}

impl Store {
    /// 打开（必要时创建）数据根目录。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ServerError> {
        let root = root.into();
        fs::create_dir_all(root.join("spaces")).map_err(|err| ServerError::io(&root, err))?;
        Ok(Self {
            root,
            revision: Mutex::new(0),
            changed: Condvar::new(),
            used_cache: Mutex::new(BTreeMap::new()),
            temp_counter: AtomicU64::new(0),
        })
    }

    /// 数据根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn space_dir(&self, space_id: &str) -> PathBuf {
        self.root.join("spaces").join(space_id)
    }

    fn space_file(&self, space_id: &str) -> PathBuf {
        self.space_dir(space_id).join("space.json")
    }

    fn devices_file(&self, space_id: &str) -> PathBuf {
        self.space_dir(space_id).join("devices.json")
    }

    fn objects_dir(&self, space_id: &str) -> PathBuf {
        self.space_dir(space_id).join("objects")
    }

    fn meta_dir(&self, space_id: &str) -> PathBuf {
        self.space_dir(space_id).join("meta")
    }

    /// 创建空间；返回（空间元数据, 注册密钥明文）。
    pub fn create_space(
        &self,
        name: &str,
        quota_bytes: u64,
        retention_days: u32,
        with_enroll_secret: bool,
    ) -> Result<(SpaceMeta, Option<String>), ServerError> {
        let id = random_id(16)?;
        let enroll_secret = if with_enroll_secret {
            Some(format!("{ENROLL_PREFIX}{}", random_id(16)?))
        } else {
            None
        };
        let meta = SpaceMeta {
            id: id.clone(),
            name: name.to_string(),
            created_ms: now_ms(),
            quota_bytes: if quota_bytes == 0 {
                DEFAULT_QUOTA_BYTES
            } else {
                quota_bytes
            },
            used_bytes: 0,
            enroll_secret_hash: enroll_secret.as_deref().map(hash_secret),
            retention_days,
        };
        let dir = self.space_dir(&id);
        fs::create_dir_all(dir.join("objects")).map_err(|err| ServerError::io(&dir, err))?;
        fs::create_dir_all(dir.join("meta")).map_err(|err| ServerError::io(&dir, err))?;
        write_json(&self.space_file(&id), &meta)?;
        write_json(&self.devices_file(&id), &Vec::<DeviceRecord>::new())?;
        Ok((meta, enroll_secret))
    }

    /// 读取空间元数据。
    pub fn space(&self, space_id: &str) -> Result<Option<SpaceMeta>, ServerError> {
        let path = self.space_file(space_id);
        if !path.exists() {
            return Ok(None);
        }
        read_json(&path)
    }

    /// 列出所有空间。
    pub fn list_spaces(&self) -> Result<Vec<SpaceMeta>, ServerError> {
        let dir = self.root.join("spaces");
        let mut spaces = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|err| ServerError::io(&dir, err))? {
            let entry = entry.map_err(|err| ServerError::io(&dir, err))?;
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            if let Some(meta) = self.space(&id)? {
                spaces.push(meta);
            }
        }
        spaces.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.id.cmp(&b.id)));
        Ok(spaces)
    }

    /// 保存空间元数据。
    pub fn save_space(&self, meta: &SpaceMeta) -> Result<(), ServerError> {
        write_json(&self.space_file(&meta.id), meta)
    }

    /// 签发一个设备令牌（返回明文，服务端只留哈希）。管理端用。
    pub fn issue_token(&self, space_id: &str, device_name: &str) -> Result<String, ServerError> {
        if self.space(space_id)?.is_none() {
            return Err(ServerError::NotFound(format!("空间不存在：{space_id}")));
        }
        let token = format!("{TOKEN_PREFIX}{}", random_id(24)?);
        let mut devices = self.devices(space_id)?;
        devices.push(DeviceRecord {
            token_hash: hash_secret(&token),
            name: device_name.to_string(),
            created_ms: now_ms(),
            pubkey: None,
            last_seen_ms: 0,
        });
        write_json(&self.devices_file(space_id), &devices)?;
        Ok(token)
    }

    /// 自助注册：用注册密钥换一个设备令牌。
    pub fn enroll(
        &self,
        space_id: &str,
        enroll_secret: &str,
        device_name: &str,
        device_pubkey: Option<&str>,
    ) -> Result<String, ServerError> {
        let Some(meta) = self.space(space_id)? else {
            return Err(ServerError::NotFound(format!("空间不存在：{space_id}")));
        };
        let Some(expected) = &meta.enroll_secret_hash else {
            return Err(ServerError::Forbidden(
                "该空间未开启自助注册（请让管理员签发设备令牌）".to_string(),
            ));
        };
        if !constant_time_eq(&hash_secret(enroll_secret), expected) {
            return Err(ServerError::Forbidden("注册密钥不正确".to_string()));
        }

        let token = format!("{TOKEN_PREFIX}{}", random_id(24)?);
        let mut devices = self.devices(space_id)?;
        devices.push(DeviceRecord {
            token_hash: hash_secret(&token),
            name: device_name.to_string(),
            created_ms: now_ms(),
            pubkey: device_pubkey.map(|value| value.to_string()),
            last_seen_ms: 0,
        });
        write_json(&self.devices_file(space_id), &devices)?;
        Ok(token)
    }

    /// 校验令牌；成功返回（空间元数据, 设备记录）。
    pub fn authorize(&self, token: &str) -> Result<Option<(SpaceMeta, DeviceRecord)>, ServerError> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let hash = hash_secret(token);
        for space in self.list_spaces()? {
            let devices = self.devices(&space.id)?;
            if let Some(record) = devices.into_iter().find(|device| device.token_hash == hash) {
                return Ok(Some((space, record)));
            }
        }
        Ok(None)
    }

    /// 设备列表（不含明文令牌）。
    pub fn devices(&self, space_id: &str) -> Result<Vec<DeviceRecord>, ServerError> {
        let path = self.devices_file(space_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_json(&path)
    }

    /// 吊销某设备令牌（按令牌明文）。
    pub fn revoke_token(&self, space_id: &str, token: &str) -> Result<bool, ServerError> {
        let hash = hash_secret(token);
        let mut devices = self.devices(space_id)?;
        let before = devices.len();
        devices.retain(|device| device.token_hash != hash);
        if devices.len() == before {
            return Ok(false);
        }
        write_json(&self.devices_file(space_id), &devices)?;
        Ok(true)
    }

    /// 记录一次"设备最近活动"（失败不影响主流程）。
    pub fn touch_device(&self, space_id: &str, token_hash: &str) {
        let Ok(mut devices) = self.devices(space_id) else {
            return;
        };
        let mut touched = false;
        for device in devices.iter_mut() {
            if device.token_hash == token_hash {
                device.last_seen_ms = now_ms();
                touched = true;
            }
        }
        if touched {
            let _ = write_json(&self.devices_file(space_id), &devices);
        }
    }

    /// 写入对象。
    pub fn put_object(
        &self,
        space_id: &str,
        path: &str,
        bytes: &[u8],
        only_if_absent: bool,
    ) -> Result<PutResult, ServerError> {
        let path = validate_object_path(path)?;
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(ServerError::TooLarge(format!(
                "对象 {path} 为 {} 字节，超过上限 {MAX_OBJECT_BYTES}",
                bytes.len()
            )));
        }

        let blob = self.object_blob_path(space_id, &path)?;
        if only_if_absent && blob.exists() {
            return Ok(PutResult::AlreadyExists);
        }

        // 配额：先按"这次净增多少"判断
        let existing = self.head_object(space_id, &path)?.map(|meta| meta.size);
        let delta = bytes.len() as u64;
        if let Some(meta) = self.space(space_id)?
            && meta.quota_bytes > 0
        {
            let used = self.used_bytes(space_id)?;
            let projected = used + delta - existing.unwrap_or(0);
            if projected > meta.quota_bytes {
                return Err(ServerError::QuotaExceeded(format!(
                    "空间 {} 配额已满（{used}/{}, 本次需要 {delta} 字节）",
                    meta.id, meta.quota_bytes
                )));
            }
        }

        if let Some(parent) = blob.parent() {
            fs::create_dir_all(parent).map_err(|err| ServerError::io(parent, err))?;
        }
        write_atomic(&self.unique_temp(space_id), &blob, bytes)?;

        let meta = ObjectMeta {
            path: path.clone(),
            etag: quote(&content_hash(bytes)),
            size: bytes.len() as u64,
            created_ms: now_ms(),
        };
        let meta_path = self.object_meta_path(space_id, &path)?;
        if let Some(parent) = meta_path.parent() {
            fs::create_dir_all(parent).map_err(|err| ServerError::io(parent, err))?;
        }
        write_atomic(
            &self.unique_temp(space_id),
            &meta_path,
            &to_json_vec(&meta)?,
        )?;

        // 更新用量缓存与长轮询代数
        {
            let mut cache = self.used_cache.lock().expect("用量缓存锁中毒");
            let entry = cache.entry(space_id.to_string()).or_insert_with(|| 0);
            *entry = entry
                .saturating_add(delta)
                .saturating_sub(existing.unwrap_or(0));
        }
        self.bump_revision();

        Ok(if existing.is_some() {
            PutResult::Replaced { etag: meta.etag }
        } else {
            PutResult::Created { etag: meta.etag }
        })
    }

    /// 读取对象。
    pub fn get_object(
        &self,
        space_id: &str,
        path: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, ServerError> {
        let Some(meta) = self.head_object(space_id, path)? else {
            return Ok(None);
        };
        let blob = self.object_blob_path(space_id, &validate_object_path(path)?)?;
        let bytes = fs::read(&blob).map_err(|err| ServerError::io(&blob, err))?;
        Ok(Some((bytes, meta)))
    }

    /// 取对象元数据。
    pub fn head_object(
        &self,
        space_id: &str,
        path: &str,
    ) -> Result<Option<ObjectMeta>, ServerError> {
        let path = validate_object_path(path)?;
        let blob = self.object_blob_path(space_id, &path)?;
        if !blob.exists() {
            return Ok(None);
        }
        let meta_path = self.object_meta_path(space_id, &path)?;
        if meta_path.exists() {
            return Ok(Some(read_json(&meta_path)?));
        }
        // meta 缺失（例如上次写入中途崩溃）：退回 size+mtime，并让它自愈
        let info = fs::metadata(&blob).map_err(|err| ServerError::io(&blob, err))?;
        let created_ms = info
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_else(now_ms);
        let bytes = fs::read(&blob).map_err(|err| ServerError::io(&blob, err))?;
        let meta = ObjectMeta {
            path,
            etag: quote(&content_hash(&bytes)),
            size: info.len(),
            created_ms,
        };
        log::warn!("对象 {} 缺少 meta 文件，已按内容重建", meta.path);
        let _ = write_json(&self.object_meta_path(space_id, &meta.path)?, &meta);
        Ok(Some(meta))
    }

    /// 删除对象（不存在也算成功）。
    pub fn delete_object(&self, space_id: &str, path: &str) -> Result<(), ServerError> {
        let path = validate_object_path(path)?;
        let blob = self.object_blob_path(space_id, &path)?;
        let meta_path = self.object_meta_path(space_id, &path)?;
        let size = fs::metadata(&blob).ok().map(|info| info.len());
        if blob.exists() {
            fs::remove_file(&blob).map_err(|err| ServerError::io(&blob, err))?;
        }
        if meta_path.exists() {
            let _ = fs::remove_file(&meta_path);
        }
        if let Some(size) = size {
            let mut cache = self.used_cache.lock().expect("用量缓存锁中毒");
            let entry = cache.entry(space_id.to_string()).or_insert_with(|| 0);
            *entry = entry.saturating_sub(size);
        }
        self.bump_revision();
        Ok(())
    }

    /// 列举对象（按键名升序）。
    pub fn list_objects(
        &self,
        space_id: &str,
        query: &ListQuery,
    ) -> Result<Vec<ObjectMeta>, ServerError> {
        let limit = if query.limit == 0 { 1000 } else { query.limit };
        let root = self.objects_dir(space_id);
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        collect_objects(&root, "", &mut found)?;
        found.sort();

        let mut objects = Vec::new();
        for path in found {
            if !path.starts_with(&query.prefix) {
                continue;
            }
            if let Some(after) = &query.after
                && path.as_str() <= after.as_str()
            {
                continue;
            }
            match self.head_object(space_id, &path)? {
                Some(meta) => objects.push(meta),
                None => continue,
            }
            if objects.len() >= limit {
                break;
            }
        }
        Ok(objects)
    }

    /// 空间已用字节（缓存 + 首次扫描）。
    pub fn used_bytes(&self, space_id: &str) -> Result<u64, ServerError> {
        if let Some(used) = self
            .used_cache
            .lock()
            .expect("用量缓存锁中毒")
            .get(space_id)
            .copied()
        {
            return Ok(used);
        }
        let used = self.recount(space_id)?;
        Ok(used)
    }

    /// 重新统计用量（并写入空间元数据）。
    pub fn recount(&self, space_id: &str) -> Result<u64, ServerError> {
        let root = self.objects_dir(space_id);
        let mut total = 0u64;
        if root.exists() {
            let mut paths = Vec::new();
            collect_objects(&root, "", &mut paths)?;
            for path in paths {
                if let Ok(blob) = self.object_blob_path(space_id, &path)
                    && let Ok(info) = fs::metadata(&blob)
                {
                    total += info.len();
                }
            }
        }
        self.used_cache
            .lock()
            .expect("用量缓存锁中毒")
            .insert(space_id.to_string(), total);
        if let Some(mut meta) = self.space(space_id)? {
            meta.used_bytes = total;
            self.save_space(&meta)?;
        }
        Ok(total)
    }

    /// 当前"变化代数"（长轮询用）。
    pub fn revision(&self) -> u64 {
        *self.revision.lock().expect("代数锁中毒")
    }

    /// 等到有新的写入或超时；返回是否等到。
    ///
    /// 用 `Condvar` 而不是轮询：空闲时没有任何 CPU 占用。
    pub fn wait_for_change(&self, since: u64, timeout: Duration) -> bool {
        let guard = self.revision.lock().expect("代数锁中毒");
        if *guard > since {
            return true;
        }
        // 等待期间被唤醒或被写入都会体现在代数上，因此只需比较代数
        let (guard, _) = self
            .changed
            .wait_timeout(guard, timeout)
            .expect("等待变化时锁中毒");
        *guard > since
    }

    fn bump_revision(&self) {
        let mut guard = self.revision.lock().expect("代数锁中毒");
        *guard += 1;
        drop(guard);
        self.changed.notify_all();
    }

    fn unique_temp(&self, space_id: &str) -> PathBuf {
        let counter = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        self.space_dir(space_id)
            .join(format!(".tmp-{}-{counter}", std::process::id()))
    }

    /// 对象字节路径（含"必须仍在空间目录内"的兜底校验）。
    fn object_blob_path(&self, space_id: &str, path: &str) -> Result<PathBuf, ServerError> {
        let base = self.objects_dir(space_id);
        let full = base.join(path);
        ensure_within(&base, &full)?;
        Ok(full)
    }

    fn object_meta_path(&self, space_id: &str, path: &str) -> Result<PathBuf, ServerError> {
        let base = self.meta_dir(space_id);
        let full = base.join(format!("{path}.json"));
        ensure_within(&base, &full)?;
        Ok(full)
    }
}

/// 校验对象路径：只允许相对、正斜杠分隔、字符受限。
pub fn validate_object_path(path: &str) -> Result<String, ServerError> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(ServerError::BadRequest("对象路径不能为空".to_string()));
    }
    if trimmed.len() > 512 {
        return Err(ServerError::BadRequest("对象路径过长（>512）".to_string()));
    }
    if trimmed.contains('\\') {
        return Err(ServerError::BadRequest(
            "对象路径不能包含反斜杠".to_string(),
        ));
    }
    if trimmed.ends_with('/') {
        return Err(ServerError::BadRequest(
            "对象路径不能以斜杠结尾（本服务端没有目录概念）".to_string(),
        ));
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ServerError::BadRequest(format!(
                "对象路径含有非法片段：{path}"
            )));
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@' | '~'))
        {
            return Err(ServerError::BadRequest(format!(
                "对象路径含有非法字符：{path}"
            )));
        }
    }
    Ok(trimmed.to_string())
}

/// 把文件树收集成相对路径列表。
fn collect_objects(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), ServerError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(ServerError::io(dir, err)),
    };
    for entry in entries {
        let entry = entry.map_err(|err| ServerError::io(dir, err))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            collect_objects(&path, &relative, out)?;
        } else if !name.starts_with(".tmp-") {
            out.push(relative);
        }
    }
    Ok(())
}

/// 兜底：确保 `full` 仍在 `base` 之下（不依赖 canonicalize，允许尚不存在的文件）。
fn ensure_within(base: &Path, full: &Path) -> Result<(), ServerError> {
    let mut depth = 0i32;
    let relative = full
        .strip_prefix(base)
        .map_err(|_| ServerError::BadRequest("对象路径越界".to_string()))?;
    for component in relative.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ServerError::BadRequest("对象路径越界".to_string()));
                }
            }
            _ => return Err(ServerError::BadRequest("对象路径越界".to_string())),
        }
    }
    Ok(())
}

/// 先写临时文件再改名（同目录内，保证原子性）。
fn write_atomic(temp: &Path, target: &Path, bytes: &[u8]) -> Result<(), ServerError> {
    fs::write(temp, bytes).map_err(|err| ServerError::io(temp, err))?;
    fs::rename(temp, target).map_err(|err| ServerError::io(target, err))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ServerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| ServerError::io(parent, err))?;
    }
    let bytes = to_json_vec(value)?;
    let temp = path.with_extension("json.tmp");
    write_atomic(&temp, path, &bytes)
}

fn to_json_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, ServerError> {
    serde_json::to_vec_pretty(value)
        .map_err(|err| ServerError::BadRequest(format!("序列化失败：{err}")))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ServerError> {
    let bytes = fs::read(path).map_err(|err| ServerError::io(path, err))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| ServerError::BadRequest(format!("{} 解析失败：{err}", path.display())))
}

/// 内容哈希（ETag 用）。
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn quote(etag: &str) -> String {
    format!("\"{etag}\"")
}

fn hash_secret(secret: &str) -> String {
    content_hash(secret.as_bytes())
}

/// 常量时间比较（避免用长度/前缀差异探测秘密）。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn random_id(bytes: usize) -> Result<String, ServerError> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|err| ServerError::Internal(format!("随机数失败：{err}")))?;
    Ok(hex::encode(buffer))
}

/// 当前时间（毫秒）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (Store, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cx-server-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        (store, root)
    }

    #[test]
    fn 路径校验挡住越界与非法字符() {
        assert_eq!(
            validate_object_path("/v1/manifests/a.json").unwrap(),
            "v1/manifests/a.json"
        );
        assert_eq!(
            validate_object_path("cx-space.json").unwrap(),
            "cx-space.json"
        );

        for bad in [
            "",
            "/",
            "v1/../secret",
            "..",
            "v1//a.json",
            "v1/manifests/",
            "v1\\manifests\\a.json",
            "v1/中文.json",
            "v1/a b.json",
        ] {
            assert!(validate_object_path(bad).is_err(), "{bad:?} 应当被拒绝");
        }
    }

    #[test]
    fn 空间与令牌的完整流程() {
        let (store, root) = temp_store("space");
        let (meta, enroll) = store.create_space("测试空间", 0, 30, true).unwrap();
        assert_eq!(meta.id.len(), 32, "空间 id 是 16 字节十六进制");
        let enroll = enroll.unwrap();
        assert!(enroll.starts_with(ENROLL_PREFIX));

        // 自助注册
        let token = store
            .enroll(&meta.id, &enroll, "设备 A", Some("pubkey"))
            .unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));

        // 令牌可授权，且能拿到设备记录
        let (space, device) = store.authorize(&token).unwrap().unwrap();
        assert_eq!(space.id, meta.id);
        assert_eq!(device.name, "设备 A");
        assert_eq!(device.pubkey.as_deref(), Some("pubkey"));

        // 错误令牌不行
        assert!(store.authorize("cxd_deadbeef").unwrap().is_none());
        assert!(store.authorize("not-a-token").unwrap().is_none());

        // 错误注册密钥不行
        let err = store
            .enroll(&meta.id, "cxe_wrong", "设备 B", None)
            .unwrap_err();
        assert!(matches!(err, ServerError::Forbidden(_)), "{err}");

        // 管理端签发令牌
        let admin = store.issue_token(&meta.id, "设备 C").unwrap();
        assert!(store.authorize(&admin).unwrap().is_some());
        assert!(store.revoke_token(&meta.id, &admin).unwrap());
        assert!(store.authorize(&admin).unwrap().is_none());

        // 关闭自助注册的空间
        let (closed, enroll2) = store.create_space("只许签发", 0, 0, false).unwrap();
        assert!(enroll2.is_none());
        let err = store
            .enroll(&closed.id, "cxe_anything", "设备 D", None)
            .unwrap_err();
        assert!(matches!(err, ServerError::Forbidden(_)));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 令牌在磁盘上只存哈希() {
        let (store, root) = temp_store("hash");
        let (meta, _) = store.create_space("s", 0, 0, true).unwrap();
        let token = store.issue_token(&meta.id, "设备").unwrap();
        let devices = fs::read_to_string(store.devices_file(&meta.id)).unwrap();
        assert!(!devices.contains(&token), "磁盘上不应出现明文令牌");
        assert!(devices.contains("token_hash"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 对象读写删与列举() {
        let (store, root) = temp_store("objects");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();

        let created = store
            .put_object(&meta.id, "v1/manifests/a.json", b"{\"v\":1}", true)
            .unwrap();
        assert!(matches!(created, PutResult::Created { .. }));

        // 条件写：已存在
        assert_eq!(
            store
                .put_object(&meta.id, "v1/manifests/a.json", b"other", true)
                .unwrap(),
            PutResult::AlreadyExists
        );
        // 覆盖写
        let replaced = store
            .put_object(&meta.id, "v1/manifests/a.json", b"{\"v\":2}", false)
            .unwrap();
        assert!(matches!(replaced, PutResult::Replaced { .. }));

        let (bytes, object) = store
            .get_object(&meta.id, "v1/manifests/a.json")
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"{\"v\":2}");
        assert_eq!(object.size, 7);
        assert!(object.etag.starts_with('"') && object.etag.ends_with('"'));

        // 列举：前缀过滤 + 排序 + 翻页
        store
            .put_object(&meta.id, "v1/items/20251001/b.cxi", b"bb", true)
            .unwrap();
        store
            .put_object(&meta.id, "v1/items/20251002/c.cxi", b"ccc", true)
            .unwrap();
        let all = store.list_objects(&meta.id, &ListQuery::default()).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].path, "v1/items/20251001/b.cxi");

        let manifests = store
            .list_objects(
                &meta.id,
                &ListQuery {
                    prefix: "v1/manifests/".to_string(),
                    ..ListQuery::default()
                },
            )
            .unwrap();
        assert_eq!(manifests.len(), 1);

        let page = store
            .list_objects(
                &meta.id,
                &ListQuery {
                    prefix: "v1/items/".to_string(),
                    after: Some("v1/items/20251001/b.cxi".to_string()),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].path, "v1/items/20251002/c.cxi");

        // 删除后列举为空
        store
            .delete_object(&meta.id, "v1/items/20251001/b.cxi")
            .unwrap();
        store
            .delete_object(&meta.id, "v1/items/20251001/b.cxi")
            .unwrap();
        assert!(
            store
                .head_object(&meta.id, "v1/items/20251001/b.cxi")
                .unwrap()
                .is_none()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn etag_随内容变化() {
        let (store, root) = temp_store("etag");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();
        store
            .put_object(&meta.id, "v1/manifests/a.json", b"one", false)
            .unwrap();
        let first = store
            .head_object(&meta.id, "v1/manifests/a.json")
            .unwrap()
            .unwrap();
        let again = store
            .head_object(&meta.id, "v1/manifests/a.json")
            .unwrap()
            .unwrap();
        assert_eq!(first.etag, again.etag, "内容未变则 ETag 不变");

        store
            .put_object(&meta.id, "v1/manifests/a.json", b"two", false)
            .unwrap();
        let second = store
            .head_object(&meta.id, "v1/manifests/a.json")
            .unwrap()
            .unwrap();
        assert_ne!(first.etag, second.etag, "内容变了 ETag 必须变");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 配额用尽时报错() {
        let (store, root) = temp_store("quota");
        let (meta, _) = store.create_space("s", 100, 0, false).unwrap();
        store
            .put_object(&meta.id, "v1/items/a.cxi", &[0u8; 60], true)
            .unwrap();
        let err = store
            .put_object(&meta.id, "v1/items/b.cxi", &[0u8; 60], true)
            .unwrap_err();
        assert!(matches!(err, ServerError::QuotaExceeded(_)), "{err}");

        // 覆盖同一个对象不该因为"旧字节还在"而被拒
        store
            .put_object(&meta.id, "v1/items/a.cxi", &[0u8; 60], false)
            .unwrap();

        // 删掉之后又能写
        store.delete_object(&meta.id, "v1/items/a.cxi").unwrap();
        store
            .put_object(&meta.id, "v1/items/c.cxi", &[0u8; 60], true)
            .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 超大对象被拒() {
        let (store, root) = temp_store("large");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();
        let err = store
            .put_object(
                &meta.id,
                "v1/items/big.cxi",
                &vec![0u8; (MAX_OBJECT_BYTES + 1) as usize],
                true,
            )
            .unwrap_err();
        assert!(matches!(err, ServerError::TooLarge(_)), "{err}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 用量统计与重算一致() {
        let (store, root) = temp_store("usage");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();
        store
            .put_object(&meta.id, "v1/items/a.cxi", &[0u8; 100], true)
            .unwrap();
        store
            .put_object(&meta.id, "v1/items/b.cxi", &[0u8; 50], true)
            .unwrap();
        assert_eq!(store.used_bytes(&meta.id).unwrap(), 150);

        // 覆盖写：净增应被正确计算
        store
            .put_object(&meta.id, "v1/items/a.cxi", &[0u8; 10], false)
            .unwrap();
        assert_eq!(store.used_bytes(&meta.id).unwrap(), 60);
        assert_eq!(store.recount(&meta.id).unwrap(), 60);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 长轮询在有写入时被唤醒() {
        let (store, root) = temp_store("wait");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();
        let revision = store.revision();

        // 注意：通知是**进程内**的（同一个 Store 实例共享 Condvar）。
        // 服务端就是一个进程一个 Store、所有连接线程共用它，因此这样就够了；
        // 换一个 Store 实例写同一份数据是收不到通知的（也确实没必要）。
        std::thread::scope(|scope| {
            let writer = &store;
            let space_id = meta.id.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                writer
                    .put_object(&space_id, "v1/items/x.cxi", b"x", true)
                    .unwrap();
            });
            let started = std::time::Instant::now();
            let changed = store.wait_for_change(revision, Duration::from_secs(5));
            assert!(changed, "写入应当唤醒等待者");
            assert!(started.elapsed() < Duration::from_secs(2), "不应等到超时");
        });

        // 超时路径：没有新写入时按超时返回
        let started = std::time::Instant::now();
        let changed = store.wait_for_change(store.revision(), Duration::from_millis(150));
        assert!(!changed);
        assert!(started.elapsed() >= Duration::from_millis(140));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn meta_缺失时会自愈() {
        let (store, root) = temp_store("heal");
        let (meta, _) = store.create_space("s", 0, 0, false).unwrap();
        store
            .put_object(&meta.id, "v1/items/a.cxi", b"data", true)
            .unwrap();
        // 删掉 meta 文件，模拟写入中途崩溃
        let meta_path = store.object_meta_path(&meta.id, "v1/items/a.cxi").unwrap();
        fs::remove_file(&meta_path).unwrap();

        let healed = store
            .head_object(&meta.id, "v1/items/a.cxi")
            .unwrap()
            .unwrap();
        assert_eq!(healed.size, 4);
        assert!(meta_path.exists(), "应当重建 meta 文件");
        assert_eq!(
            healed.etag,
            quote(&content_hash(b"data")),
            "重建的 ETag 应与内容一致"
        );
        let _ = fs::remove_dir_all(root);
    }
}
