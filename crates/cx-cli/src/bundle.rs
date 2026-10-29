//! 整套配置的打包/还原：一次导出，另一台机器一次导入。
//!
//! 搬一台新机器原本要好几步：加后端、粘 AK/SK、导出空间密钥、登记口令……
//! 这里把 `config.toml`、`secrets/` 下的凭据、`space.json`、`space.key` 打成一个文件，
//! 导入端解包即可，不需要再手抄任何东西。
//!
//! 安全：包里含空间密钥与云凭据，等价于"这个人的全部同步权限"。文件按 0600 写，
//! 且导出时明确提示；导入默认拒绝覆盖已有配置（要覆盖必须显式 --force）。

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use cx_core::{CoreError, Paths};

/// 包格式标识：以后改结构时靠它区分，避免把不认识的文件当配置读进来。
pub const FORMAT: &str = "clipboardx-bundle/1";

/// 一个配置包。
#[derive(Debug, Serialize, Deserialize)]
pub struct Bundle {
    /// 格式标识。
    pub format: String,
    /// 导出时间（Unix 毫秒），只用于排障时判断新旧。
    pub exported_ms: u64,
    /// `config.toml` 原文。
    pub config_toml: String,
    /// `secrets/` 下的文件内容（base64），键是文件名。
    pub secrets: BTreeMap<String, String>,
    /// `space.json` 原文（不含密钥）。
    pub space_json: Option<String>,
    /// `space.key` 原文（base64）。
    pub space_key: Option<String>,
}

impl Bundle {
    /// 从本机收集。
    pub fn collect(paths: &Paths, now_ms: u64) -> Result<Self, CoreError> {
        let config_toml = std::fs::read_to_string(paths.config_dir.join("config.toml"))
            .map_err(|err| CoreError::io(paths.config_dir.join("config.toml"), err))?;
        let mut secrets = BTreeMap::new();
        let secrets_dir = paths.secrets_dir();
        if secrets_dir.is_dir() {
            for entry in
                std::fs::read_dir(&secrets_dir).map_err(|err| CoreError::io(&secrets_dir, err))?
            {
                let entry = entry.map_err(|err| CoreError::io(&secrets_dir, err))?;
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let bytes =
                    std::fs::read(entry.path()).map_err(|err| CoreError::io(entry.path(), err))?;
                secrets.insert(name, encode(&bytes));
            }
        }
        let space_json = read_optional(&paths.space_file())?;
        let space_key = read_optional(&paths.space_key_file())?.map(|text| encode(text.as_bytes()));
        Ok(Self {
            format: FORMAT.to_string(),
            exported_ms: now_ms,
            config_toml,
            secrets,
            space_json,
            space_key,
        })
    }

    /// 解析并校验格式。
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        let bundle: Self = serde_json::from_str(text)
            .map_err(|err| CoreError::malformed("bundle", err.to_string()))?;
        if bundle.format != FORMAT {
            return Err(CoreError::malformed(
                "bundle",
                format!("不认识的格式 {}（期望 {FORMAT}）", bundle.format),
            ));
        }
        Ok(bundle)
    }

    /// 写回本机。`force` 为假时，已有配置会被拒绝（不静默覆盖别人的空间）。
    pub fn restore(&self, paths: &Paths, force: bool) -> Result<(), CoreError> {
        let config_path = paths.config_dir.join("config.toml");
        if config_path.exists() && !force {
            return Err(CoreError::malformed(
                "bundle",
                "本机已经有配置文件；确实要覆盖请加 --force（会替换 config.toml、凭据与空间密钥）",
            ));
        }
        paths.ensure()?;
        cx_core::fsutil::write_atomic(&config_path, self.config_toml.as_bytes())?;
        cx_core::fsutil::tighten_permissions(&config_path, false)?;

        let secrets_dir = paths.secrets_dir();
        std::fs::create_dir_all(&secrets_dir).map_err(|err| CoreError::io(&secrets_dir, err))?;
        for (name, encoded) in &self.secrets {
            // 只接受普通文件名，防止包里的 ../ 把文件写到配置目录之外
            if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
                return Err(CoreError::malformed(
                    "bundle",
                    format!("凭据文件名非法：{name}"),
                ));
            }
            let path = secrets_dir.join(name);
            cx_core::fsutil::write_atomic(&path, &decode(encoded)?)?;
            cx_core::fsutil::tighten_permissions(&path, false)?;
        }
        // 空间的两个文件必须**成对**恢复，否则会出现"space.json 是新的、space.key 是旧的"
        // 这种状态：界面显示新空间，却一把钥匙都打不开。包里没有空间就按没有空间恢复。
        match (&self.space_json, &self.space_key) {
            (Some(json), Some(encoded)) => {
                cx_core::fsutil::write_atomic(&paths.space_file(), json.as_bytes())?;
                let key_path = paths.space_key_file();
                cx_core::fsutil::write_atomic(&key_path, &decode(encoded)?)?;
                cx_core::fsutil::tighten_permissions(&key_path, false)?;
            }
            (Some(json), None) => {
                // 有元数据没密钥：保留元数据，删掉本机旧密钥（旧密钥对新空间无效）
                cx_core::fsutil::write_atomic(&paths.space_file(), json.as_bytes())?;
                let _ = std::fs::remove_file(paths.space_key_file());
            }
            (None, _) => {
                // 包里没有空间：本机也不该留着上一个空间的残骸
                let _ = std::fs::remove_file(paths.space_file());
                let _ = std::fs::remove_file(paths.space_key_file());
            }
        }
        Ok(())
    }

    /// 包里是否带空间密钥（决定导入后能不能直接同步）。
    pub fn has_space_key(&self) -> bool {
        self.space_key.is_some()
    }
}

/// 导出到文件（0600）。
pub fn write_file(path: &Path, bundle: &Bundle) -> Result<(), CoreError> {
    let text = serde_json::to_string_pretty(bundle)
        .map_err(|err| CoreError::malformed("bundle", err.to_string()))?;
    cx_core::fsutil::write_atomic(path, text.as_bytes())?;
    cx_core::fsutil::tighten_permissions(path, false)?;
    Ok(())
}

/// 从文件读入。
pub fn read_file(path: &Path) -> Result<Bundle, CoreError> {
    let text = std::fs::read_to_string(path).map_err(|err| CoreError::io(path, err))?;
    Bundle::parse(&text)
}

fn read_optional(path: &Path) -> Result<Option<String>, CoreError> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| CoreError::malformed("bundle", format!("{} 不是 UTF-8", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CoreError::io(path, err)),
    }
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(text: &str) -> Result<Vec<u8>, CoreError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|err| CoreError::malformed("bundle", format!("base64 解不开：{err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 导出再导入能得到同一套配置与密钥() {
        let source = temp_paths("src");
        std::fs::write(
            source.config_dir.join("config.toml"),
            "[sync]\nencrypt = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(source.secrets_dir()).unwrap();
        std::fs::write(source.secrets_dir().join("oss.key"), "s3cr3t").unwrap();
        std::fs::write(source.space_file(), r#"{"space_id":"abc","cipher":"x"}"#).unwrap();
        std::fs::write(source.space_key_file(), "a".repeat(64)).unwrap();

        let bundle = Bundle::collect(&source, 1).unwrap();
        let text = serde_json::to_string(&bundle).unwrap();
        let parsed = Bundle::parse(&text).unwrap();

        let target = temp_paths("dst");
        parsed.restore(&target, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.config_dir.join("config.toml")).unwrap(),
            "[sync]\nencrypt = true\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.secrets_dir().join("oss.key")).unwrap(),
            "s3cr3t"
        );
        assert_eq!(
            std::fs::read_to_string(target.space_key_file()).unwrap(),
            "a".repeat(64)
        );
        assert!(parsed.has_space_key());

        for p in [&source, &target] {
            let _ = std::fs::remove_dir_all(p.config_dir.parent().unwrap());
        }
    }

    #[test]
    fn 导入默认不覆盖已有配置() {
        let paths = temp_paths("noclobber");
        std::fs::write(paths.config_dir.join("config.toml"), "existing").unwrap();
        let bundle = Bundle {
            format: FORMAT.to_string(),
            exported_ms: 0,
            config_toml: "new".to_string(),
            secrets: BTreeMap::new(),
            space_json: None,
            space_key: None,
        };
        assert!(bundle.restore(&paths, false).is_err(), "默认必须拒绝覆盖");
        assert_eq!(
            std::fs::read_to_string(paths.config_dir.join("config.toml")).unwrap(),
            "existing",
            "拒绝时不该动原文件"
        );
        bundle.restore(&paths, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.config_dir.join("config.toml")).unwrap(),
            "new"
        );
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn 导入没有空间的包会清掉本机旧空间() {
        let paths = temp_paths("nospace");
        std::fs::write(paths.config_dir.join("config.toml"), "x").unwrap();
        std::fs::write(paths.space_file(), r#"{"space_id":"old"}"#).unwrap();
        std::fs::write(paths.space_key_file(), "b".repeat(64)).unwrap();
        let bundle = Bundle {
            format: FORMAT.to_string(),
            exported_ms: 0,
            config_toml: "y".to_string(),
            secrets: BTreeMap::new(),
            space_json: None,
            space_key: None,
        };
        bundle.restore(&paths, true).unwrap();
        assert!(!paths.space_file().exists(), "旧空间的元数据要清掉");
        assert!(!paths.space_key_file().exists(), "旧空间的密钥也要清掉");
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn 有元数据没密钥时不会留下旧密钥() {
        let paths = temp_paths("jsononly");
        std::fs::write(paths.config_dir.join("config.toml"), "x").unwrap();
        std::fs::write(paths.space_key_file(), "c".repeat(64)).unwrap();
        let bundle = Bundle {
            format: FORMAT.to_string(),
            exported_ms: 0,
            config_toml: "y".to_string(),
            secrets: BTreeMap::new(),
            space_json: Some(r#"{"space_id":"new"}"#.to_string()),
            space_key: None,
        };
        bundle.restore(&paths, true).unwrap();
        assert!(paths.space_file().exists());
        assert!(
            !paths.space_key_file().exists(),
            "旧密钥对新空间无效，必须删掉，否则会静默解不开"
        );
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }
    #[test]
    fn 不认识的格式会被拒绝() {
        let text = r#"{"format":"something-else","exported_ms":0,"config_toml":"","secrets":{}}"#;
        let err = Bundle::parse(text).unwrap_err().to_string();
        assert!(err.contains("不认识的格式"), "{err}");
    }

    #[test]
    fn 包里的路径穿越会被拒绝() {
        let paths = temp_paths("traversal");
        let mut secrets = BTreeMap::new();
        secrets.insert("../evil.key".to_string(), encode(b"x"));
        let bundle = Bundle {
            format: FORMAT.to_string(),
            exported_ms: 0,
            config_toml: String::new(),
            secrets,
            space_json: None,
            space_key: None,
        };
        assert!(bundle.restore(&paths, true).is_err());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }
}
