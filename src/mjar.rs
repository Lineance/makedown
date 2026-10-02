use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::core::ModulePath;

/// 记录 .mjar 包内的清单信息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JarManifest {
    name: String,
    // 以包内相对路径表示的 crate 锚点根；空串表示包自身的 lib.mf。
    crate_roots: Vec<String>,
}

impl JarManifest {
    /// 返回包的展示名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回以包内相对路径表示的 crate 锚点根。
    pub fn crate_roots(&self) -> &[String] {
        &self.crate_roots
    }
}

/// 描述打开或读取 .mjar 包失败的原因。
#[derive(Debug)]
pub enum JarError {
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    MissingManifest {
        path: PathBuf,
    },
    InvalidManifest {
        path: PathBuf,
        message: String,
    },
    MissingModule {
        path: PathBuf,
        namespace: ModulePath,
    },
}

impl fmt::Display for JarError {
    // 将包错误渲染为带包路径的说明。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(formatter, "cannot open package `{}`: {source}", path.display())
            }
            Self::Read { path, source } => {
                write!(formatter, "cannot read package `{}`: {source}", path.display())
            }
            Self::MissingManifest { path } => {
                write!(
                    formatter,
                    "package `{}` is missing manifest.toml",
                    path.display()
                )
            }
            Self::InvalidManifest { path, message } => {
                write!(
                    formatter,
                    "package `{}` has an invalid manifest: {message}",
                    path.display()
                )
            }
            Self::MissingModule { path, namespace } => write!(
                formatter,
                "package `{}` does not contain module `{namespace}`",
                path.display()
            ),
        }
    }
}

impl Error for JarError {}

/// 以确定性方式构建并写出 .mjar 包。
#[derive(Clone, Debug, Default)]
pub struct JarBuilder {
    name: Option<String>,
    crate_roots: Vec<String>,
    // 条目路径到源文本的有序映射，保证写出结果可重现。
    entries: BTreeMap<String, String>,
}

impl JarBuilder {
    /// 创建一个空包构建器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置包清单中的展示名称。
    pub fn name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into());
    }

    /// 追加清单中的 crate 锚点根（包内相对路径）。
    pub fn add_crate_root(&mut self, root: impl Into<String>) {
        self.crate_roots.push(root.into());
    }

    /// 按命名空间写入一个模块。
    pub fn add_module(&mut self, namespace: &ModulePath, source: impl Into<String>) {
        self.entries
            .insert(module_entry(namespace), source.into());
    }

    /// 按包内相对路径写入一个条目。
    pub fn add_entry(&mut self, entry: &str, source: impl Into<String>) {
        self.entries.insert(entry.to_owned(), source.into());
    }

    // 渲染包内 manifest.toml 文本。
    fn manifest_source(&self) -> String {
        let name = self.name.as_deref().unwrap_or("unnamed");
        let mut roots = vec![String::new()];
        roots.extend(self.crate_roots.iter().cloned());
        let roots = roots
            .iter()
            .map(|root| toml::Value::String(root.clone()).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("format = 1\nname = {}\ncrate_roots = [{roots}]\n", toml::Value::String(name.to_owned()))
    }

    /// 将包写到指定路径，使用固定时间戳与压缩。
    pub fn write_to(&self, path: &Path) -> Result<(), JarError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| JarError::Open {
                path: path.to_owned(),
                source,
            })?;
        }
        let file = File::create(path).map_err(|source| JarError::Open {
            path: path.to_owned(),
            source,
        })?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(
                zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
                    .expect("the epoch of zip timestamps is valid"),
            );
        let mut entries = self.entries.clone();
        entries.insert(
            "manifest.toml".to_owned(),
            self.manifest_source(),
        );
        for (entry, source) in &entries {
            writer
                .start_file(entry.as_str(), options)
                .map_err(|source| JarError::Read {
                    path: path.to_owned(),
                    source: source.into(),
                })?;
            writer
                .write_all(source.as_bytes())
                .map_err(|source| JarError::Read {
                    path: path.to_owned(),
                    source,
                })?;
        }
        writer
            .finish()
            .map_err(|source| JarError::Read {
                path: path.to_owned(),
                source: source.into(),
            })?;
        Ok(())
    }
}

/// 提供对一个 .mjar 包的只读访问。
pub struct JarReader {
    path: PathBuf,
    archive: zip::ZipArchive<File>,
    manifest: JarManifest,
    // 包内全部模块的相对命名空间，根模块在首位。
    modules: Vec<ModulePath>,
}

impl JarReader {
    /// 打开包文件并读取清单与模块列表。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JarError> {
        let path = path.into();
        let file = File::open(&path).map_err(|source| JarError::Open {
            path: path.clone(),
            source,
        })?;
        let mut archive = zip::ZipArchive::new(file).map_err(|source| JarError::Read {
            path: path.clone(),
            source: source.into(),
        })?;
        let manifest = read_manifest(&path, &mut archive)?;
        let mut modules = Vec::new();
        for index in 0..archive.len() {
            let entry = archive
                .by_index(index)
                .map_err(|source| JarError::Read {
                    path: path.clone(),
                    source: source.into(),
                })?;
            if !entry.is_file() || !entry.name().ends_with(".mf") {
                continue;
            }
            match module_namespace(entry.name()) {
                Some(namespace) => {
                    if !modules.contains(&namespace) {
                        modules.push(namespace);
                    }
                }
                None => return Err(JarError::InvalidManifest {
                    path: path.clone(),
                    message: format!("invalid module entry `{}`", entry.name()),
                }),
            }
        }
        modules.sort();
        Ok(Self {
            path,
            archive,
            manifest,
            modules,
        })
    }

    /// 返回包的清单。
    pub fn manifest(&self) -> &JarManifest {
        &self.manifest
    }

    /// 返回包内全部模块的相对命名空间。
    pub fn modules(&self) -> &[ModulePath] {
        &self.modules
    }

    /// 返回包文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 将清单中的 crate 根路径转换为包内相对命名空间。
    pub fn crate_root_namespaces(&self) -> Result<Vec<ModulePath>, JarError> {
        self.manifest
            .crate_roots()
            .iter()
            .map(|root| match root.as_str() {
                "" => Ok(ModulePath::root()),
                _ => module_namespace(&format!("{root}.mf")).ok_or_else(|| {
                    JarError::InvalidManifest {
                        path: self.path.clone(),
                        message: format!("invalid crate root `{root}`"),
                    }
                }),
            })
            .collect()
    }

    /// 读取包内一个模块的源文本。
    pub fn read_module(&mut self, namespace: &ModulePath) -> Result<String, JarError> {
        let entry = module_entry(namespace);
        let mut file = self
            .archive
            .by_name(&entry)
            .map_err(|_| JarError::MissingModule {
                path: self.path.clone(),
                namespace: namespace.clone(),
            })?;
        let mut source = String::new();
        file.read_to_string(&mut source).map_err(|source| JarError::Read {
            path: self.path.clone(),
            source,
        })?;
        Ok(source)
    }
}

// 读取并校验包内 manifest.toml。
fn read_manifest(path: &Path, archive: &mut zip::ZipArchive<File>) -> Result<JarManifest, JarError> {
    let mut file = archive
        .by_name("manifest.toml")
        .map_err(|_| JarError::MissingManifest { path: path.to_owned() })?;
    let mut source = String::new();
    file.read_to_string(&mut source)
        .map_err(|source| JarError::Read {
            path: path.to_owned(),
            source,
        })?;
    let value: toml::Value =
        toml::from_str(&source).map_err(|error| JarError::InvalidManifest {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    let invalid = |message: String| JarError::InvalidManifest {
        path: path.to_owned(),
        message,
    };
    let format = value
        .get("format")
        .and_then(toml::Value::as_integer)
        .ok_or_else(|| invalid("format is required".into()))?;
    if format != 1 {
        return Err(invalid(format!("unsupported format {format}; expected 1")));
    }
    let name = value
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| invalid("name is required".into()))?
        .to_owned();
    if name.is_empty() {
        return Err(invalid("name cannot be empty".into()));
    }
    let crate_roots = match value.get("crate_roots") {
        None => vec![String::new()],
        Some(roots) => roots
            .as_array()
            .ok_or_else(|| invalid("crate_roots must be a list".into()))?
            .iter()
            .map(|root| {
                root.as_str().map(str::to_owned).ok_or_else(|| {
                    invalid("crate_roots entries must be strings".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if !crate_roots.iter().any(|root| root.is_empty()) {
        return Err(invalid("crate_roots must include the package root".into()));
    }
    Ok(JarManifest { name, crate_roots })
}

// 将包内条目路径转换为相对命名空间。
fn module_namespace(entry: &str) -> Option<ModulePath> {
    let entry = entry.trim_end_matches(".mf");
    if entry == "lib" {
        return Some(ModulePath::root());
    }
    let segments = entry
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if segments.is_empty() || entry.starts_with('/') || entry.contains('\\') {
        return None;
    }
    ModulePath::new(segments).ok()
}

// 将相对命名空间映射回包内条目路径。
fn module_entry(namespace: &ModulePath) -> String {
    if namespace.is_root() {
        "lib.mf".to_owned()
    } else {
        format!("{}.mf", namespace.segments().join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 验证条目路径与相对命名空间互相映射。
    #[test]
    fn module_entries_map_to_namespaces_and_back() {
        assert_eq!(
            module_namespace("lib.mf").unwrap(),
            ModulePath::root()
        );
        assert_eq!(
            module_namespace("catalog/api.mf").unwrap(),
            ModulePath::parse("catalog::api").unwrap()
        );
        assert!(module_namespace("bad\\name.mf").is_none());
        assert!(module_namespace("/absolute.mf").is_none());

        assert_eq!(module_entry(&ModulePath::root()), "lib.mf");
        assert_eq!(
            module_entry(&ModulePath::parse("catalog::api").unwrap()),
            "catalog/api.mf"
        );
    }

    // 验证包写出与读回保留模块与清单，且写出结果确定性。
    #[test]
    fn jar_roundtrip_preserves_modules_and_manifest_deterministically() {
        let first = std::env::temp_dir().join(format!("mkd-jar-{}-a", std::process::id()));
        let second = std::env::temp_dir().join(format!("mkd-jar-{}-b", std::process::id()));
        let mut builder = JarBuilder::new();
        builder.name("utils");
        builder.add_module(&ModulePath::root(), "lib source");
        builder.add_module(&ModulePath::parse("dep").unwrap(), "dep source");
        builder.write_to(&first).unwrap();
        builder.write_to(&second).unwrap();

        let mut reader = JarReader::open(&first).unwrap();
        assert_eq!(reader.manifest().name(), "utils");
        assert_eq!(reader.manifest().crate_roots(), &["".to_owned()]);
        assert_eq!(reader.crate_root_namespaces().unwrap(), vec![ModulePath::root()]);
        assert!(reader.modules().contains(&ModulePath::root()));
        assert!(reader.modules().contains(&ModulePath::parse("dep").unwrap()));
        assert_eq!(
            reader.read_module(&ModulePath::root()).unwrap(),
            "lib source"
        );
        assert_eq!(
            reader.read_module(&ModulePath::parse("dep").unwrap()).unwrap(),
            "dep source"
        );
        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
        assert!(JarReader::open(&first.with_file_name("absent.mjar")).is_err());
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }
}
