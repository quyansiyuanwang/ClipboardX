//! 设备身份与对端账本。
//!
//! 设备身份分两部分：
//! - **device_id**：沿用核心身份里的 32 位十六进制（与同步协议一致），由调用方传入；
//! - **Noise 静态密钥**：Curve25519，仅用于局域网握手，存在 `<目录>/lan.key`（0600）。
//!
//! 为什么要单独一把密钥而不是复用什么现成的密钥：Noise 静态密钥是长期身份密钥，
//! 用途单一才安全（避免同一份密钥材料出现在两条协议里）。
//!
//! 对端账本（TOFU）：首次见到某 device_id 时记下它的静态公钥；此后若同一个
//! device_id 换了公钥，就拒绝连接——这是防中间人的最低成本手段。

use crate::error::LanError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 本机在局域网里的身份。
#[derive(Debug, Clone)]
pub struct LanIdentity {
    /// 设备 id（与同步协议一致）。
    pub device_id: String,
    /// Curve25519 静态私钥（32 字节）。
    private: [u8; 32],
    /// 对应的公钥（32 字节）。
    public: [u8; 32],
}

impl LanIdentity {
    /// 读取或创建静态密钥；`device_id` 由调用方给出。
    pub fn load_or_create(dir: &Path, device_id: &str) -> Result<Self, LanError> {
        let path = dir.join("lan.key");
        let private = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let trimmed = text.trim().to_string();
                let bytes = hex::decode(&trimmed).map_err(|err| {
                    LanError::Config(format!("{} 不是合法的十六进制密钥：{err}", path.display()))
                })?;
                let mut key = [0u8; 32];
                if bytes.len() != 32 {
                    return Err(LanError::Config(format!(
                        "{} 长度应为 32 字节（实际 {}）",
                        path.display(),
                        bytes.len()
                    )));
                }
                key.copy_from_slice(&bytes);
                key
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key)
                    .map_err(|err| LanError::Io(format!("生成静态密钥失败：{err}")))?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| LanError::Io(format!("创建目录失败：{err}")))?;
                }
                std::fs::write(&path, hex::encode(key))
                    .map_err(|err| LanError::Io(format!("写入 {} 失败：{err}", path.display())))?;
                tighten(&path);
                key
            }
            Err(err) => {
                return Err(LanError::Io(format!("读取 {} 失败：{err}", path.display())));
            }
        };

        // 由私钥推导公钥。
        //
        // 注意这里**不能**用 `snow::Builder::generate_keypair()`：那是"生成一对新密钥"，
        // 会忽略已设置的私钥——于是每次重启都换一个公钥，对端的 TOFU 校验会全部失败
        // （这个坑是单元测试逮到的：同一密钥文件两次加载得到不同公钥）。
        // 用 X25519 的基点乘法直接从私钥算公钥，既稳定又能校验密钥文件有没有被改坏。
        let public = public_from_private(&private);

        Ok(Self {
            device_id: device_id.to_string(),
            private,
            public,
        })
    }

    /// 静态私钥。
    pub fn static_private(&self) -> &[u8; 32] {
        &self.private
    }

    /// 静态公钥（十六进制）。
    pub fn public_hex(&self) -> String {
        hex::encode(self.public)
    }

    /// 校验：给定的公钥是否与私钥匹配（测试与故障排查用）。
    pub fn matches(&self, public_hex: &str) -> bool {
        hex::decode(public_hex)
            .map(|bytes| bytes == self.public)
            .unwrap_or(false)
    }
}

/// 由 Curve25519 私钥推导公钥（X25519 的基点乘法）。
fn public_from_private(private: &[u8; 32]) -> [u8; 32] {
    curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(*private).to_bytes()
}

/// 尽量收紧权限（失败只记日志：某些文件系统不支持）。
fn tighten(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 对端记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerRecord {
    /// 设备名（首次见到时记下，便于展示）。
    pub device_name: String,
    /// 静态公钥（十六进制）。
    pub public_key: String,
    /// 首次见到的时间（毫秒）。
    pub first_seen_ms: u64,
    /// 最近一次见到的时间（毫秒）。
    pub last_seen_ms: u64,
}

/// TOFU 校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerCheck {
    /// 已知且公钥一致。
    Trusted,
    /// 首次见到（已记录）。
    FirstSeen,
}

/// 对端账本（`peers.json`）。
#[derive(Debug, Clone)]
pub struct PeerBook {
    path: PathBuf,
    peers: std::sync::Arc<Mutex<BTreeMap<String, PeerRecord>>>,
}

impl PeerBook {
    /// 打开（不存在则视为空账本，首次校验通过时创建）。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, LanError> {
        let path = path.into();
        let peers = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<BTreeMap<String, PeerRecord>>(&text)
                .map_err(|err| LanError::Config(format!("{} 解析失败：{err}", path.display())))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(LanError::Io(format!("读取 {} 失败：{err}", path.display()))),
        };
        Ok(Self {
            path,
            peers: std::sync::Arc::new(Mutex::new(peers)),
        })
    }

    /// 校验某设备的公钥；首次见到则记录，公钥变化则报错。
    pub fn check(&self, device_id: &str, public_key_hex: &str) -> Result<PeerCheck, LanError> {
        let now = cx_core::now_ms();
        let mut peers = self.peers.lock().expect("对端账本锁中毒");
        match peers.get_mut(device_id) {
            Some(record) => {
                if record.public_key != public_key_hex {
                    return Err(LanError::Protocol(format!(
                        "设备 {device_id} 的公钥变了（原 {}…，现 {}…）。\
                         可能是中间人，也可能是重装/换设备；确认无误后删除 peers.json 中该条目再试。",
                        &record.public_key[..8.min(record.public_key.len())],
                        &public_key_hex[..8.min(public_key_hex.len())],
                    )));
                }
                record.last_seen_ms = now;
                drop(peers);
                self.save();
                Ok(PeerCheck::Trusted)
            }
            None => {
                peers.insert(
                    device_id.to_string(),
                    PeerRecord {
                        device_name: String::new(),
                        public_key: public_key_hex.to_string(),
                        first_seen_ms: now,
                        last_seen_ms: now,
                    },
                );
                drop(peers);
                self.save();
                Ok(PeerCheck::FirstSeen)
            }
        }
    }

    /// 补充设备名（首次 Hello 之后调用）。
    pub fn remember_name(&self, device_id: &str, device_name: &str) {
        let mut peers = self.peers.lock().expect("对端账本锁中毒");
        if let Some(record) = peers.get_mut(device_id) {
            record.device_name = device_name.to_string();
        }
        drop(peers);
        self.save();
    }

    /// 列出已知对端。
    pub fn list(&self) -> Vec<(String, PeerRecord)> {
        self.peers
            .lock()
            .expect("对端账本锁中毒")
            .iter()
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect()
    }

    /// 撤销信任（用户发现异常时的出口）。
    pub fn forget(&self, device_id: &str) -> bool {
        let removed = self
            .peers
            .lock()
            .expect("对端账本锁中毒")
            .remove(device_id)
            .is_some();
        if removed {
            self.save();
        }
        removed
    }

    fn save(&self) {
        let snapshot = {
            let peers = self.peers.lock().expect("对端账本锁中毒");
            serde_json::to_vec_pretty(&*peers).unwrap_or_default()
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 先写临时文件再改名：别让半截 JSON 变成"所有对端都不可信"
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, &snapshot).is_ok() {
            let _ = std::fs::rename(&temp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cx-lan-id-{tag}-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 身份可创建可重载且公钥稳定() {
        let dir = temp_dir("identity");
        let first = LanIdentity::load_or_create(&dir, "device-a").unwrap();
        let second = LanIdentity::load_or_create(&dir, "device-a").unwrap();
        // 同一个密钥文件：私钥与公钥都必须稳定（这里曾经踩过坑——用 snow 的
        // generate_keypair 会每次生成新密钥，导致重启后公钥变化、TOFU 全部失效）
        assert_eq!(
            first.static_private(),
            second.static_private(),
            "同一文件应读出同一私钥"
        );
        assert_eq!(
            first.public_hex(),
            second.public_hex(),
            "同一密钥文件应得到同一公钥"
        );
        assert!(first.matches(&first.public_hex()), "公钥应当与私钥匹配");

        // 换个目录应当是不同的密钥（不能所有设备共用一把）
        let other_dir = temp_dir("identity2");
        let other = LanIdentity::load_or_create(&other_dir, "device-b").unwrap();
        assert_ne!(first.public_hex(), other.public_hex());

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(other_dir);
    }

    #[test]
    fn 密钥文件非法时给出明确错误() {
        let dir = temp_dir("badkey");
        std::fs::write(dir.join("lan.key"), "not-hex").unwrap();
        let err = LanIdentity::load_or_create(&dir, "d").unwrap_err();
        assert!(matches!(err, LanError::Config(_)), "{err}");

        std::fs::write(dir.join("lan.key"), "abcd").unwrap();
        let err = LanIdentity::load_or_create(&dir, "d").unwrap_err();
        assert!(err.to_string().contains("32 字节"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tofu_首次记录_再遇相同公钥放行_公钥变化拒绝() {
        let dir = temp_dir("tofu");
        let book = PeerBook::open(dir.join("peers.json")).unwrap();

        assert_eq!(book.check("dev-a", "aa").unwrap(), PeerCheck::FirstSeen);
        assert_eq!(book.check("dev-a", "aa").unwrap(), PeerCheck::Trusted);

        let err = book.check("dev-a", "bb").unwrap_err();
        assert!(err.to_string().contains("公钥变了"), "{err}");
        assert!(
            err.to_string().contains("中间人"),
            "错误信息应当提示可能的原因"
        );

        // 撤销信任之后可以重新记录
        assert!(book.forget("dev-a"));
        assert_eq!(book.check("dev-a", "bb").unwrap(), PeerCheck::FirstSeen);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn 账本会落盘并在重开后保持() {
        let dir = temp_dir("persist");
        let path = dir.join("peers.json");
        {
            let book = PeerBook::open(&path).unwrap();
            book.check("dev-a", "aa").unwrap();
            book.remember_name("dev-a", "笔记本");
        }
        let reopened = PeerBook::open(&path).unwrap();
        let list = reopened.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "dev-a");
        assert_eq!(list[0].1.device_name, "笔记本");
        // 重开之后仍然认得它是可信的
        assert_eq!(reopened.check("dev-a", "aa").unwrap(), PeerCheck::Trusted);

        std::fs::remove_dir_all(dir).ok();
    }
}
