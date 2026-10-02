use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::config_directory;

/// 包路径相对于 mkd.lock 所在目录；哈希固定为 SHA-256 十六进制形式。
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LockedJar {
    path: String,
    hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LockFile {
    version: u32,
    jar: Vec<LockedJar>,
}

fn digest(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
    let hash = Sha256::digest(bytes);
    let mut hex = String::from("sha256:");
    for byte in hash.as_slice() {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

fn relative_path(root_file: &Path, jar: &Path) -> Result<String, String> {
    let root = config_directory(root_file);
    let path = jar.strip_prefix(&root).map_err(|_| {
        format!(
            "package `{}` is outside `{}`",
            jar.display(),
            root.display()
        )
    })?;
    let parts = path
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "package path is not UTF-8".to_owned())?;
    if parts.is_empty() || parts.iter().any(|part| *part == "." || *part == "..") {
        return Err(format!("invalid package path `{}`", jar.display()));
    }
    Ok(parts.join("/"))
}

fn read_lock(root_file: &Path) -> Result<Option<LockFile>, String> {
    let path = config_directory(root_file).join("mkd.lock");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read `{}`: {error}", path.display())),
    };
    let lock: LockFile = toml::from_str(&source)
        .map_err(|error| format!("invalid `{}`: {error}", path.display()))?;
    if lock.version != 1 {
        return Err(format!("unsupported mkd.lock version {}", lock.version));
    }
    let mut seen = BTreeMap::new();
    for jar in &lock.jar {
        if !jar.hash.starts_with("sha256:")
            || jar.hash.len() != 71
            || !jar.hash[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!("invalid SHA-256 hash for `{}`", jar.path));
        }
        if seen.insert(&jar.path, ()).is_some() {
            return Err(format!("duplicate locked package `{}`", jar.path));
        }
    }
    Ok(Some(lock))
}

/// 校验锁中已有的包；没有锁或未记录该包时放行。
pub fn verify_jar(root_file: &Path, jar_path: &Path) -> Result<(), String> {
    let Some(lock) = read_lock(root_file)? else {
        return Ok(());
    };
    let path = relative_path(root_file, jar_path)?;
    if let Some(entry) = lock.jar.iter().find(|entry| entry.path == path) {
        let actual = digest(jar_path)?;
        if actual != entry.hash {
            return Err(format!(
                "package `{path}` hash mismatch (expected {}, got {actual}); run `mkd lock` to update",
                entry.hash
            ));
        }
    }
    Ok(())
}

/// 冻结项目分析实际引用的所有 .mjar 包，重新生成 mkd.lock。
pub fn write_lock(root_file: &Path, jars: &[PathBuf]) -> Result<PathBuf, String> {
    let mut entries = BTreeMap::new();
    for jar in jars {
        entries.insert(relative_path(root_file, jar)?, digest(jar)?);
    }
    let lock = LockFile {
        version: 1,
        jar: entries
            .into_iter()
            .map(|(path, hash)| LockedJar { path, hash })
            .collect(),
    };
    let path = config_directory(root_file).join("mkd.lock");
    let source = toml::to_string_pretty(&lock).map_err(|error| error.to_string())?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| format!("cannot stage `{}`: {error}", path.display()))?;
    std::io::Write::write_all(&mut staged, source.as_bytes())
        .map_err(|error| format!("cannot write `{}`: {error}", path.display()))?;
    staged
        .persist(&path)
        .map_err(|error| format!("cannot publish `{}`: {}", path.display(), error.error))?;
    Ok(path)
}
