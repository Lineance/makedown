use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::{ModulePath, Project, TargetId};
use crate::mjar::{JarBuilder, JarReader};
use crate::parser::{DiagnosticSeverity, ParseMode, Parser};
use crate::project::{AnalysisDiagnostic, AnalysisMode, AnalysisResult, ProjectAnalyzer};

/// 汇总一次成功打包的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackSummary {
    output: PathBuf,
    name: String,
    modules: usize,
}

impl PackSummary {
    /// 返回包产物路径。
    pub fn output(&self) -> &Path {
        &self.output
    }

    /// 返回包清单名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回包内模块数量。
    pub fn modules(&self) -> usize {
        self.modules
    }
}

/// 描述打包失败的原因。
#[derive(Debug)]
pub enum PackError {
    NotALibraryRoot(PathBuf),
    OutputAliasesInput(PathBuf),
    Analysis(Vec<AnalysisDiagnostic>),
    Rewrite {
        file: PathBuf,
        line: usize,
        message: String,
    },
    Jar(String),
    Verification(Vec<AnalysisDiagnostic>),
}

impl fmt::Display for PackError {
    // 将打包错误渲染为面向用户的说明。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotALibraryRoot(path) => write!(
                formatter,
                "mkd pack requires a lib.mf project root, not `{}`",
                path.display()
            ),
            Self::OutputAliasesInput(path) => write!(
                formatter,
                "output path `{}` would overwrite a project input",
                path.display()
            ),
            Self::Analysis(diagnostics) | Self::Verification(diagnostics) => {
                write!(formatter, "project analysis failed")?;
                for diagnostic in diagnostics {
                    write!(
                        formatter,
                        "\n  {}: {}",
                        diagnostic.file().display(),
                        diagnostic.message()
                    )?;
                }
                Ok(())
            }
            Self::Rewrite {
                file,
                line,
                message,
            } => {
                write!(formatter, "{}:{line}: {message}", file.display())
            }
            Self::Jar(message) => formatter.write_str(message),
        }
    }
}

impl Error for PackError {}

// 尽可能规范化路径后判断两条路径是否指向同一文件。
fn paths_alias(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    matches!((left.canonicalize(), right.canonicalize()), (Ok(left), Ok(right)) if left == right)
}

/// 将 lib.mf 根项目打包为 .mjar，并对产物执行完整自校验。
pub fn pack_project(
    root_file: &Path,
    output: &Path,
    mut progress: impl FnMut(&ModulePath) -> std::io::Result<()>,
) -> Result<PackSummary, PackError> {
    if root_file.file_name().is_none_or(|name| name != "lib.mf") {
        return Err(PackError::NotALibraryRoot(root_file.to_owned()));
    }
    let analyzer = ProjectAnalyzer::new(root_file, AnalysisMode::Build);
    let analysis = analyzer
        .analyze_all_with_progress(&mut progress)
        .map_err(|error| PackError::Jar(format!("cannot read project: {error}")))?;
    let failures = analysis
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.severity() == DiagnosticSeverity::Error)
        .cloned()
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        return Err(PackError::Analysis(failures));
    }
    let project = analysis
        .project()
        .ok_or_else(|| PackError::Analysis(analysis.diagnostics().to_vec()))?;

    // 从公开接口出发收集可达模块，剪枝其余内容。
    let mut kept = reachable_modules(project);
    kept.insert(ModulePath::root());
    // 已打包的依赖保持自身完整的模块树及所有内层 crate 根。
    for mount in analysis.mounted_namespaces() {
        if !kept
            .iter()
            .any(|namespace| namespace.segments().starts_with(mount.segments()))
        {
            continue;
        }
        let jar = analysis
            .mount_path(mount)
            .expect("mounted package has a path");
        let reader = JarReader::open(jar).map_err(|error| PackError::Jar(error.to_string()))?;
        for relative in reader.modules() {
            let mut segments = mount.segments().to_vec();
            segments.extend(relative.segments().iter().cloned());
            kept.insert(ModulePath::new(segments).expect("validated namespaces remain valid"));
        }
    }

    // 产物不能覆盖任何输入文件，包括被剪枝的磁盘模块。
    if project.modules().any(|module| {
        paths_alias(
            output,
            &module_location(&analysis, &analyzer, module.namespace()),
        )
    }) || kept
        .iter()
        .any(|namespace| paths_alias(output, &module_location(&analysis, &analyzer, namespace)))
        || analysis
            .jar_paths()
            .iter()
            .any(|path| paths_alias(output, path))
        || paths_alias(output, root_file)
        || paths_alias(
            output,
            &crate::project::config_directory(root_file).join("mkd.toml"),
        )
    {
        return Err(PackError::OutputAliasesInput(output.to_owned()));
    }

    let mut builder = JarBuilder::new();
    let name = package_name(root_file);
    builder.name(name.clone());

    // 逐模块写入：磁盘模块改写 * 导入，包模块原样展开。
    let mut module_count = 0usize;
    for namespace in &kept {
        let file = module_location(&analysis, &analyzer, namespace);
        let source = read_module_source(&analysis, &analyzer, namespace);
        let rewritten = rewrite_star_includes(&source, namespace).map_err(|(line, message)| {
            PackError::Rewrite {
                file,
                line,
                message,
            }
        })?;
        builder.add_module(namespace, rewritten);
        module_count += 1;
    }

    // 展开后的包可能已包含其他展开子树；递归保留全部相关 crate 根。
    let mut crate_roots = BTreeSet::new();
    for mount in analysis.mounted_namespaces() {
        let jar = analysis
            .mount_path(mount)
            .expect("mounted package has a path");
        let reader = JarReader::open(jar).map_err(|error| PackError::Jar(error.to_string()))?;
        for relative in reader
            .crate_root_namespaces()
            .map_err(|error| PackError::Jar(error.to_string()))?
        {
            let mut segments = mount.segments().to_vec();
            segments.extend(relative.segments().iter().cloned());
            let absolute = ModulePath::new(segments).expect("validated namespaces remain valid");
            if kept
                .iter()
                .any(|namespace| namespace.segments().starts_with(absolute.segments()))
            {
                crate_roots.insert(namespace_entry(&absolute));
            }
        }
    }
    for root in crate_roots {
        builder.add_crate_root(root);
    }

    // 先写入同目录临时文件，自校验成功后才原子替换正式产物。
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| PackError::Jar(format!("cannot create output directory: {error}")))?;
    let staged = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| PackError::Jar(format!("cannot create temporary package: {error}")))?;
    builder
        .write_to(staged.path())
        .map_err(|error| PackError::Jar(error.to_string()))?;

    // 自校验：产物作为独立项目完整加载并比对公开接口。
    let verification = ProjectAnalyzer::new(staged.path(), AnalysisMode::Build)
        .analyze_jar(staged.path())
        .map_err(|error| PackError::Jar(format!("cannot verify package: {error}")))?;
    if verification.has_errors() {
        return Err(PackError::Verification(verification.diagnostics().to_vec()));
    }
    let verified = verification
        .project()
        .ok_or_else(|| PackError::Verification(verification.diagnostics().to_vec()))?;
    let interface = project
        .root()
        .map(|module| module.public_targets().count())
        .unwrap_or(0);
    let packed_interface = verified
        .root()
        .map(|module| module.public_targets().count())
        .unwrap_or(0);
    if interface != packed_interface {
        return Err(PackError::Jar(format!(
            "package interface changed while packing: expected {interface} public targets, got {packed_interface}"
        )));
    }

    staged.persist(output).map_err(|error| {
        PackError::Jar(format!(
            "cannot publish package `{}`: {}",
            output.display(),
            error.error
        ))
    })?;

    Ok(PackSummary {
        output: output.to_owned(),
        name,
        modules: module_count,
    })
}

// 从 lib.mf 公开目标出发收集可达模块。
fn reachable_modules(project: &Project) -> BTreeSet<ModulePath> {
    let mut kept = BTreeSet::new();
    let mut queue = project
        .root()
        .map(|module| {
            module
                .public_targets()
                .map(|target| target.id().clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut visited = BTreeSet::<TargetId>::new();
    while let Some(target_id) = queue.pop() {
        if !visited.insert(target_id.clone()) {
            continue;
        }
        let Some(module) = project.module(target_id.namespace()) else {
            continue;
        };
        kept.insert(target_id.namespace().clone());
        if let Some(target) = module.target(target_id.name()) {
            queue.extend(target.dependencies().iter().cloned());
        }
        queue.extend(module.includes().iter().cloned());
    }
    kept
}

// 返回模块对应的分析诊断文件位置。
fn module_location(
    analysis: &AnalysisResult,
    analyzer: &ProjectAnalyzer,
    namespace: &ModulePath,
) -> PathBuf {
    analysis
        .mounted_namespaces()
        .find(|mount| namespace.segments().starts_with(mount.segments()))
        .and_then(|mount| analysis.mount_path(mount))
        .cloned()
        .unwrap_or_else(|| analyzer.module_file(namespace))
}

// 返回模块源文本：磁盘模块读文件，包模块读条目。
fn read_module_source(
    analysis: &AnalysisResult,
    analyzer: &ProjectAnalyzer,
    namespace: &ModulePath,
) -> String {
    if let Some(mount) = analysis
        .mounted_namespaces()
        .filter(|mount: &&ModulePath| namespace.segments().starts_with(mount.segments()))
        .max_by_key(|mount: &&ModulePath| mount.segments().len())
    {
        let relative = ModulePath::new(namespace.segments()[mount.segments().len()..].to_vec())
            .expect("suffixes of valid namespaces stay valid");
        let path = analysis
            .mount_path(mount)
            .expect("mounted namespaces know their package")
            .clone();
        let mut reader = JarReader::open(path).expect("mounted packages stay readable");
        return reader
            .read_module(&relative)
            .expect("mounted modules stay readable");
    }
    fs::read_to_string(analyzer.module_file(namespace)).expect("kept disk modules are readable")
}

// 将挂载命名空间转换为包内 crate 根条目路径（不含 .mf 后缀）。
fn namespace_entry(namespace: &ModulePath) -> String {
    namespace.segments().join("/")
}

// 以配置目录名推导包名。
pub fn package_name(root_file: &Path) -> String {
    crate::project::config_directory(root_file)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "package".to_owned())
}

// 将 * 包导入行改写为普通模块引用，返回新源文本。
fn rewrite_star_includes(source: &str, namespace: &ModulePath) -> Result<String, (usize, String)> {
    let parsed = Parser::new(ParseMode::DryRun).parse(namespace.clone(), source);
    if parsed.has_errors() {
        return Ok(source.to_owned());
    }
    let mut rewrites = BTreeMap::<usize, String>::new();
    for include in parsed.partial_module().includes() {
        let segments = include.target().path().segments();
        let starred = segments
            .iter()
            .filter(|segment| segment.starts_with('*'))
            .count();
        if starred == 0 {
            continue;
        }
        let index = segments
            .iter()
            .position(|segment| segment.starts_with('*'))
            .expect("a starred segment exists");
        if starred != 1 || index != segments.len() - 2 {
            return Err((
                include.target().line(),
                "a package import must name its package directly before the target".to_owned(),
            ));
        }
        let mut plain = segments.to_vec();
        plain[index] = plain[index].trim_start_matches('*').to_owned();
        let mut line = format!("> {}", plain.join("::"));
        if let Some(alias) = include.alias() {
            line.push_str(&format!(" as {alias}"));
        }
        rewrites.insert(include.target().line(), line);
    }
    if rewrites.is_empty() {
        return Ok(source.to_owned());
    }
    let mut lines = Vec::with_capacity(source.lines().count());
    for (index, line) in source.lines().enumerate() {
        if let Some(replacement) = rewrites.get(&(index + 1)) {
            lines.push(replacement.clone());
        } else {
            lines.push(line.to_owned());
        }
    }
    let mut text = lines.join("\n");
    if source.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // 在唯一临时目录中创建一组测试文件。
    fn fixture(files: &[(&str, &str)]) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mkd-pack-{unique}"));
        for (relative, content) in files {
            let file = root.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        root
    }

    // 在临时目录内写入一个示例包。
    fn write_jar(root: &Path, relative: &str, name: &str, modules: &[(String, &str)]) {
        let mut builder = JarBuilder::new();
        builder.name(name);
        for (namespace, source) in modules {
            let namespace = if namespace.is_empty() {
                ModulePath::root()
            } else {
                ModulePath::parse(namespace).unwrap()
            };
            builder.add_module(&namespace, *source);
        }
        builder.write_to(&root.join(relative)).unwrap();
    }

    // 验证打包剪枝不可达模块并通过独立加载自校验。
    #[test]
    fn packing_prunes_unreachable_modules_and_verifies() {
        let root = fixture(&[
            (
                "lib.mf",
                "> crate::internal::helper\n---\n# api\n> helper\n对外能力。\n- 接口可用。\n---\n> api\n",
            ),
            (
                "internal.mf",
                "---\n# helper\n- 辅助就绪。\n---\n> helper\n",
            ),
            ("orphan.mf", "---\n# dead\n- 无关。\n---\n> dead\n"),
        ]);
        let output = root.join("out.mjar");

        let summary = pack_project(&root.join("lib.mf"), &output, |_| Ok(())).unwrap();

        assert_eq!(summary.modules(), 2);
        assert_eq!(summary.name(), root.file_name().unwrap().to_str().unwrap());
        let reader = JarReader::open(&output).unwrap();
        assert_eq!(reader.manifest().crate_roots(), &["".to_owned()]);
        let modules = reader.modules().to_vec();
        assert!(modules.contains(&ModulePath::root()));
        assert!(modules.contains(&ModulePath::parse("internal").unwrap()));
        assert!(!modules.contains(&ModulePath::parse("orphan").unwrap()));
        // 独立加载时公开接口完整。
        let standalone = ProjectAnalyzer::new(&output, AnalysisMode::Build)
            .analyze_jar(&output)
            .unwrap();
        assert!(!standalone.has_errors(), "{:?}", standalone.diagnostics());
        let project = standalone.project().unwrap();
        assert_eq!(
            project
                .root()
                .unwrap()
                .target("api")
                .unwrap()
                .dependencies(),
            &[TargetId::new(
                ModulePath::parse("internal").unwrap(),
                crate::core::TargetName::parse("helper").unwrap(),
            )]
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证打包展开导入的包并改写 * 导入行。
    #[test]
    fn packing_expands_imported_packages_and_rewrites_star_imports() {
        let root = fixture(&[]);
        write_jar(
            &root,
            "vendor/utils.mjar",
            "utils",
            &[
                (
                    String::new(),
                    "> crate::dep::verify\n---\n# check\n> verify\n检查。\n- 检查通过。\n---\n> check\n",
                ),
                (
                    "dep".to_owned(),
                    "---\n# verify\n- 校验通过。\n---\n> verify\n",
                ),
            ],
        );
        fs::write(
            root.join("lib.mf"),
            "> crate::vendor::*utils::check as pkg_check\n---\n# api\n> pkg_check\n对外能力。\n- 接口可用。\n---\n> api\n",
        )
        .unwrap();
        let output = root.join("out.mjar");

        pack_project(&root.join("lib.mf"), &output, |_| Ok(())).unwrap();

        let mut reader = JarReader::open(&output).unwrap();
        let mut sources = BTreeMap::new();
        for namespace in reader.modules().to_vec() {
            let source = reader.read_module(&namespace).unwrap();
            sources.insert(namespace, source);
        }
        assert_eq!(
            sources.get(&ModulePath::root()).unwrap(),
            "> crate::vendor::utils::check as pkg_check\n---\n# api\n> pkg_check\n对外能力。\n- 接口可用。\n---\n> api\n"
        );
        // 展开的子树按位置存放且内容零改写。
        assert_eq!(
            sources
                .get(&ModulePath::parse("vendor::utils").unwrap())
                .unwrap(),
            "> crate::dep::verify\n---\n# check\n> verify\n检查。\n- 检查通过。\n---\n> check\n"
        );
        assert!(sources.contains_key(&ModulePath::parse("vendor::utils::dep").unwrap()));
        assert_eq!(
            reader.manifest().crate_roots(),
            &["".to_owned(), "vendor/utils".to_owned()]
        );
        // 独立加载时 crate 锚点重定向到展开子树。
        let standalone = ProjectAnalyzer::new(&output, AnalysisMode::Build)
            .analyze_jar(&output)
            .unwrap();
        assert!(!standalone.has_errors(), "{:?}", standalone.diagnostics());
        let project = standalone.project().unwrap();
        assert_eq!(
            project
                .root()
                .unwrap()
                .target("api")
                .unwrap()
                .dependencies(),
            &[TargetId::new(
                ModulePath::parse("vendor::utils").unwrap(),
                crate::core::TargetName::parse("check").unwrap(),
            )]
        );
        assert_eq!(
            project
                .module(&ModulePath::parse("vendor::utils").unwrap())
                .unwrap()
                .target("check")
                .unwrap()
                .dependencies(),
            &[TargetId::new(
                ModulePath::parse("vendor::utils::dep").unwrap(),
                crate::core::TargetName::parse("verify").unwrap(),
            )]
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证反复打包仍保留最内层展开包的 crate 锚点。
    #[test]
    fn repacking_preserves_nested_expanded_crate_roots() {
        let root = fixture(&[]);
        let mut inner = JarBuilder::new();
        inner.name("middle");
        inner.add_crate_root("inner");
        inner.add_module(
            &ModulePath::root(),
            "> crate::inner::check\n---\n# entry\n> check\n---\n> entry\n",
        );
        inner.add_module(
            &ModulePath::parse("inner").unwrap(),
            "> crate::dep::ready\n---\n# check\n> ready\n---\n> check\n",
        );
        inner.add_module(
            &ModulePath::parse("inner::dep").unwrap(),
            "---\n# ready\n- ready\n---\n> ready\n",
        );
        inner.write_to(&root.join("middle.mjar")).unwrap();
        fs::write(
            root.join("lib.mf"),
            "> crate::*middle::entry\n---\n# api\n> entry\n---\n> api\n",
        )
        .unwrap();
        let first = root.join("first.mjar");
        pack_project(&root.join("lib.mf"), &first, |_| Ok(())).unwrap();
        let next = fixture(&[(
            "lib.mf",
            "> crate::*first::api\n---\n# api\n> api as prior\n---\n> api\n",
        )]);
        // Alias is declared in the include section, not a dependency; use a valid source.
        fs::write(
            next.join("lib.mf"),
            "> crate::*first::api as prior\n---\n# api\n> prior\n---\n> api\n",
        )
        .unwrap();
        fs::copy(&first, next.join("first.mjar")).unwrap();
        let second = next.join("second.mjar");
        pack_project(&next.join("lib.mf"), &second, |_| Ok(())).unwrap();
        let reader = JarReader::open(&second).unwrap();
        assert!(
            reader
                .manifest()
                .crate_roots()
                .contains(&"first/middle/inner".to_owned())
        );
        let result = ProjectAnalyzer::new(&second, AnalysisMode::Build)
            .analyze_jar(&second)
            .unwrap();
        assert!(!result.has_errors(), "{:?}", result.diagnostics());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(next).unwrap();
    }

    // 验证非 lib.mf 根与输出覆盖输入均被拒绝。
    #[test]
    fn packing_rejects_non_library_roots_and_aliasing_outputs() {
        let root = fixture(&[("main.mf", "---\n# start\n- 开始。\n---\n> start\n")]);

        let error =
            pack_project(&root.join("main.mf"), &root.join("out.mjar"), |_| Ok(())).unwrap_err();
        assert!(matches!(error, PackError::NotALibraryRoot(_)));

        let library = fixture(&[("lib.mf", "---\n# api\n- 接口。\n---\n> api\n")]);
        let error =
            pack_project(&library.join("lib.mf"), &library.join("lib.mf"), |_| Ok(())).unwrap_err();
        assert!(matches!(error, PackError::OutputAliasesInput(_)));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(library).unwrap();
    }
}
