use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::{Module, ModulePath, Project, Spec, Target, TargetId, TargetName};
use crate::mjar::JarReader;
use crate::parser::{
    DiagnosticSeverity, ParseMode, ParsedInclude, ParsedModule, ParsedReference, Parser,
};

/// 依优先级列出的四种项目根文件相对路径。
pub const ROOT_FILE_NAMES: [&str; 4] = ["main.mf", "lib.mf", ".mkd/main.mf", ".mkd/lib.mf"];

/// 从起始目录向上寻找最近的项目根文件，同一目录内按 ROOT_FILE_NAMES 优先级。
pub fn find_root_file(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|directory| {
        ROOT_FILE_NAMES
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// 返回配置目录：平铺布局为根文件所在目录，隐藏布局为其上级。
pub fn config_directory(root_file: &Path) -> PathBuf {
    let parent = root_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match parent.file_name() {
        Some(name) if name == std::ffi::OsStr::new(".mkd") => parent
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        _ => parent.to_path_buf(),
    }
}

/// 控制项目分析是否交付核心内存对象。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisMode {
    Build,
    DryRun,
}

/// 描述带文件位置的项目级结构化诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisDiagnostic {
    severity: DiagnosticSeverity,
    file: PathBuf,
    line: Option<usize>,
    code: &'static str,
    message: String,
}

impl AnalysisDiagnostic {
    // 将缺失模块错误映射到引用该模块的指令位置。
    pub(crate) fn at(&self, file: PathBuf, line: usize) -> Self {
        Self {
            file,
            line: Some(line),
            ..self.clone()
        }
    }

    /// 将单文件解析诊断附上文件路径。
    pub fn from_parse(file: PathBuf, diagnostic: &crate::parser::Diagnostic) -> Self {
        Self {
            severity: diagnostic.severity(),
            file,
            line: Some(diagnostic.line()),
            code: diagnostic.code(),
            message: diagnostic.message().to_owned(),
        }
    }

    /// 创建一条非阻塞的文件质量警告。
    pub fn warning(
        file: PathBuf,
        line: usize,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            file,
            line: Some(line),
            code,
            message: message.into(),
        }
    }

    // 创建一条阻塞项目构建的错误。
    fn error(
        file: PathBuf,
        line: Option<usize>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            file,
            line,
            code,
            message: message.into(),
        }
    }

    /// 返回诊断严重级别。
    pub fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    /// 返回诊断对应的 Markfile 路径。
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// 返回可用的一基源码行号。
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// 返回稳定的项目诊断代码。
    pub fn code(&self) -> &str {
        self.code
    }

    /// 返回面向用户的诊断消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 汇总项目核心模型与全部分析诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisResult {
    project: Option<Project>,
    diagnostics: Vec<AnalysisDiagnostic>,
    // 分析过程中被挂载的全部包文件路径。
    jar_paths: Vec<PathBuf>,
    // 挂载命名空间到包文件路径的映射。
    mounts: BTreeMap<ModulePath, PathBuf>,
}

impl AnalysisResult {
    /// 在普通模式且图健康时借用项目。
    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    /// 在普通模式且图健康时取出项目。
    pub fn into_project(self) -> Option<Project> {
        self.project
    }

    /// 返回加载、解析和解析引用的全部诊断。
    pub fn diagnostics(&self) -> &[AnalysisDiagnostic] {
        &self.diagnostics
    }

    /// 返回分析期间挂载的全部 .mjar 包路径。
    pub fn jar_paths(&self) -> &[PathBuf] {
        &self.jar_paths
    }

    /// 返回全部挂载命名空间，按字典序排列。
    pub fn mounted_namespaces(&self) -> impl Iterator<Item = &ModulePath> {
        self.mounts.keys()
    }

    /// 返回挂载命名空间对应的包文件路径。
    pub fn mount_path(&self, mount: &ModulePath) -> Option<&PathBuf> {
        self.mounts.get(mount)
    }

    /// 判断是否存在阻塞项目构建的诊断。
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
    }
}

/// 从根 Markfile 按需加载并解析健康依赖图。
#[derive(Clone, Debug)]
pub struct ProjectAnalyzer {
    root_file: PathBuf,
    mode: AnalysisMode,
}

impl ProjectAnalyzer {
    /// 以根 Markfile 的位置和分析模式创建分析器。
    pub fn new(root_file: impl Into<PathBuf>, mode: AnalysisMode) -> Self {
        Self {
            root_file: root_file.into(),
            mode,
        }
    }

    /// 从任意起始目标分析其模块可达图。
    pub fn analyze(&self, start: TargetId) -> AnalysisResult {
        self.analyze_with_progress(start, |_| Ok(()))
            .expect("the no-op progress observer cannot fail")
    }

    /// 在解析每个可读取的模块前通知调用方，并传播观察者错误。
    pub fn analyze_with_progress(
        &self,
        start: TargetId,
        mut progress: impl FnMut(&ModulePath) -> std::io::Result<()>,
    ) -> std::io::Result<AnalysisResult> {
        let mut state = AnalysisState::new(self, Some(start));
        state.load_reachable_modules(&mut progress, &mut |_, _| {}, &mut |path: &Path| {
            fs::read_to_string(path)
        })?;
        Ok(state.resolve())
    }

    /// 扫描项目目录内的所有模块并校验整个图。
    pub fn analyze_all_with_progress(
        &self,
        progress: impl FnMut(&ModulePath) -> std::io::Result<()>,
    ) -> std::io::Result<AnalysisResult> {
        self.analyze_all_with_modules(progress, |_, _| {})
    }

    // 在全量分析期间让检查器复用已解析的文件模型。
    pub(crate) fn analyze_all_with_modules(
        &self,
        mut progress: impl FnMut(&ModulePath) -> std::io::Result<()>,
        mut inspect: impl FnMut(&Path, &ParsedModule),
    ) -> std::io::Result<AnalysisResult> {
        let mut state = AnalysisState::new(self, None);
        let (modules, diagnostics) = self.discover_modules()?;
        state.pending = modules;
        state.diagnostics.extend(diagnostics);
        state.load_reachable_modules(&mut progress, &mut inspect, &mut |path: &Path| {
            fs::read_to_string(path)
        })?;
        Ok(state.resolve())
    }

    /// 从指定模块分析导入可达图，优先通过调用方提供的源码读取器获取文本。
    pub fn analyze_module_with_sources(
        &self,
        module: ModulePath,
        read_source: impl FnMut(&Path) -> std::io::Result<String>,
    ) -> std::io::Result<AnalysisResult> {
        self.analyze_module_with_sources_and_modules(module, read_source, |_, _| {})
    }

    // 扫描整个项目并使用编辑器的内存文件覆盖磁盘版本。
    pub(crate) fn analyze_all_with_sources_and_modules(
        &self,
        mut read_source: impl FnMut(&Path) -> std::io::Result<String>,
        mut inspect: impl FnMut(&Path, &ParsedModule),
        extra_modules: impl IntoIterator<Item = ModulePath>,
    ) -> std::io::Result<AnalysisResult> {
        let mut state = AnalysisState::new(self, None);
        let (mut modules, diagnostics) = self.discover_modules()?;
        modules.extend(extra_modules);
        state.pending = modules;
        state.diagnostics.extend(diagnostics);
        state.load_reachable_modules(&mut |_| Ok(()), &mut inspect, &mut read_source)?;
        Ok(state.resolve())
    }

    // 向编辑器提供分析过程中已解析的模块以定位目标声明。
    pub(crate) fn analyze_module_with_sources_and_modules(
        &self,
        module: ModulePath,
        mut read_source: impl FnMut(&Path) -> std::io::Result<String>,
        mut inspect: impl FnMut(&Path, &ParsedModule),
    ) -> std::io::Result<AnalysisResult> {
        let mut state = AnalysisState::new(self, None);
        state.pending.push(module);
        state.load_reachable_modules(&mut |_| Ok(()), &mut inspect, &mut read_source)?;
        Ok(state.resolve())
    }

    /// 将一个 .mjar 包作为独立项目挂载在根命名空间分析。
    pub fn analyze_jar(&self, jar_path: &Path) -> std::io::Result<AnalysisResult> {
        let reader =
            JarReader::open(jar_path).map_err(|error| std::io::Error::other(error.to_string()))?;
        let crate_roots = reader
            .crate_root_namespaces()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let modules = reader.modules().to_vec();
        let mut state = AnalysisState::new(self, None);
        state.jars.insert(
            jar_path
                .canonicalize()
                .unwrap_or_else(|_| jar_path.to_path_buf()),
            reader,
        );
        state.mounts.insert(
            ModulePath::root(),
            JarMount {
                path: jar_path.to_path_buf(),
                crate_roots,
            },
        );
        state.pending = modules;
        state.load_reachable_modules(&mut |_| Ok(()), &mut |_, _| {}, &mut |path: &Path| {
            fs::read_to_string(path)
        })?;
        Ok(state.resolve())
    }

    // 根据项目根目录扫描全部 .mf 文件并按模块路径排序。
    fn discover_modules(&self) -> std::io::Result<(Vec<ModulePath>, Vec<AnalysisDiagnostic>)> {
        let root = self.root_directory();
        let canonical_root = root.canonicalize()?;
        let mut pending = vec![root.to_path_buf()];
        let mut visited = BTreeSet::new();
        let mut modules = vec![ModulePath::root()];
        let mut diagnostics = Vec::new();
        while let Some(directory) = pending.pop() {
            let canonical = match directory.canonicalize() {
                Ok(canonical) => canonical,
                Err(error) => {
                    diagnostics.push(AnalysisDiagnostic::error(
                        directory,
                        None,
                        "P018",
                        format!("cannot inspect directory: {error}"),
                    ));
                    continue;
                }
            };
            if !canonical.starts_with(&canonical_root) || !visited.insert(canonical) {
                continue;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    diagnostics.push(AnalysisDiagnostic::error(
                        directory,
                        None,
                        "P018",
                        format!("cannot read directory: {error}"),
                    ));
                    continue;
                }
            };
            let mut entries = entries.collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.as_ref().map(|entry| entry.path()).ok());
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        diagnostics.push(AnalysisDiagnostic::error(
                            directory.clone(),
                            None,
                            "P018",
                            format!("cannot read directory entry: {error}"),
                        ));
                        continue;
                    }
                };
                let path = entry.path();
                let metadata = match fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        diagnostics.push(AnalysisDiagnostic::error(
                            path,
                            None,
                            "P018",
                            format!("cannot inspect path: {error}"),
                        ));
                        continue;
                    }
                };
                if metadata.is_dir() {
                    if !is_hidden_entry(&path) {
                        pending.push(path);
                    }
                } else if metadata.is_file()
                    && path.extension().is_some_and(|extension| extension == "mf")
                    && path != self.root_file
                    && !is_hidden_entry(&path)
                {
                    let relative = path.strip_prefix(root).expect("file found inside root");
                    let mut segments = relative
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned())
                        .collect::<Vec<_>>();
                    let last = segments.pop().expect("markfile has a filename");
                    segments.push(
                        last.strip_suffix(".mf")
                            .expect("markfile extension checked")
                            .to_owned(),
                    );
                    match ModulePath::new(segments) {
                        Ok(namespace) => {
                            if modules.contains(&namespace) {
                                diagnostics.push(AnalysisDiagnostic::error(
                                    path,
                                    None,
                                    "P017",
                                    format!("duplicate module path `{namespace}`"),
                                ));
                            } else {
                                modules.push(namespace);
                            }
                        }
                        Err(error) => diagnostics.push(AnalysisDiagnostic::error(
                            path,
                            None,
                            "P017",
                            format!("invalid module file path: {error}"),
                        )),
                    }
                }
            }
        }
        modules.sort();
        modules.reverse();
        Ok((modules, diagnostics))
    }

    // 将稳定模块路径映射为项目内的 Markfile 路径。
    pub(crate) fn module_file(&self, namespace: &ModulePath) -> PathBuf {
        if namespace.is_root() {
            return self.root_file.clone();
        }

        let mut path = self.root_directory().to_path_buf();
        for segment in &namespace.segments()[..namespace.segments().len() - 1] {
            path.push(segment);
        }
        path.push(format!("{}.mf", namespace.segments().last().unwrap()));
        path
    }

    // 返回项目根文件所在目录。
    fn root_directory(&self) -> &Path {
        self.root_file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }
}

// 描述一个已挂载的 .mjar 包。
struct JarMount {
    // 包文件路径。
    path: PathBuf,
    // 登记的 crate 锚点命名空间（含挂载前缀）。
    crate_roots: Vec<ModulePath>,
}

// 表示模块源文本的存放位置。
enum ModuleSource {
    Disk(PathBuf),
    Jar { path: PathBuf, relative: ModulePath },
}

// 保存一次按需项目分析的工作集和诊断。
struct AnalysisState<'a> {
    analyzer: &'a ProjectAnalyzer,
    start: Option<TargetId>,
    parsed_modules: BTreeMap<ModulePath, ParsedModule>,
    module_files: BTreeMap<ModulePath, PathBuf>,
    // 保存尚待加载的模块路径栈。
    pending: Vec<ModulePath>,
    // 防止导入环导致重复读取模块。
    visited: BTreeSet<ModulePath>,
    diagnostics: Vec<AnalysisDiagnostic>,
    // 已打开的包文件，键为包文件路径。
    jars: BTreeMap<PathBuf, JarReader>,
    // 已挂载的包，键为挂载命名空间。
    mounts: BTreeMap<ModulePath, JarMount>,
}

impl<'a> AnalysisState<'a> {
    // 以起始目标所在模块初始化待加载队列。
    fn new(analyzer: &'a ProjectAnalyzer, start: Option<TargetId>) -> Self {
        let pending = start
            .as_ref()
            .map(|id| vec![id.namespace().clone()])
            .unwrap_or_default();
        Self {
            analyzer,
            pending,
            start,
            parsed_modules: BTreeMap::new(),
            module_files: BTreeMap::new(),
            visited: BTreeSet::new(),
            diagnostics: Vec::new(),
            jars: BTreeMap::new(),
            mounts: BTreeMap::new(),
        }
    }

    // 递归读取并解析导入可达的所有模块。
    fn load_reachable_modules(
        &mut self,
        progress: &mut impl FnMut(&ModulePath) -> std::io::Result<()>,
        inspect: &mut impl FnMut(&Path, &ParsedModule),
        read_source: &mut impl FnMut(&Path) -> std::io::Result<String>,
    ) -> std::io::Result<()> {
        while let Some(namespace) = self.pending.pop() {
            if !self.visited.insert(namespace.clone()) {
                continue;
            }

            let location = self.module_source(&namespace);
            let file = match &location {
                ModuleSource::Disk(path) => path.clone(),
                ModuleSource::Jar { path, .. } => path.clone(),
            };
            self.module_files.insert(namespace.clone(), file.clone());
            if let ModuleSource::Disk(_) = location
                && !namespace.is_root()
                && same_file(&file, &self.analyzer.root_file)
            {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file,
                    None,
                    "P016",
                    format!("module `{namespace}` aliases the project root file"),
                ));
                continue;
            }
            let source = match &location {
                ModuleSource::Disk(path) => match read_source(path) {
                    Ok(source) => source,
                    Err(error) => {
                        self.diagnostics.push(AnalysisDiagnostic::error(
                            file,
                            None,
                            "P001",
                            format!("cannot read module `{namespace}`: {error}"),
                        ));
                        continue;
                    }
                },
                ModuleSource::Jar { path, relative } => {
                    let jar = self.jars.get_mut(path).expect("a mounted package is open");
                    match jar.read_module(relative) {
                        Ok(source) => source,
                        Err(error) => {
                            self.diagnostics.push(AnalysisDiagnostic::error(
                                file,
                                None,
                                "P022",
                                error.to_string(),
                            ));
                            continue;
                        }
                    }
                }
            };

            progress(&namespace)?;
            let parse_mode = match self.analyzer.mode {
                AnalysisMode::Build => ParseMode::Build,
                AnalysisMode::DryRun => ParseMode::DryRun,
            };
            let parse_result = Parser::new(parse_mode).parse(namespace.clone(), &source);
            self.diagnostics
                .extend(
                    parse_result
                        .diagnostics()
                        .iter()
                        .map(|diagnostic| AnalysisDiagnostic {
                            severity: diagnostic.severity(),
                            file: file.clone(),
                            line: Some(diagnostic.line()),
                            code: diagnostic.code(),
                            message: diagnostic.message().to_owned(),
                        }),
                );
            let module = parse_result.partial_module().clone();
            inspect(&file, &module);
            for include in module.includes() {
                if let Some(target) = self.resolve_path(module.namespace(), include.target(), &file)
                {
                    self.pending.push(target.namespace().clone());
                }
            }
            self.parsed_modules.insert(namespace, module);
        }
        Ok(())
    }

    // 将部分语法模型解析为核心项目并检查图健康。
    fn resolve(mut self) -> AnalysisResult {
        let mut project = Project::new(ModulePath::root());
        let namespaces = self.parsed_modules.keys().cloned().collect::<Vec<_>>();
        for namespace in namespaces {
            if let Some(module) = self.build_module(&namespace)
                && let Err(error) = project.add_module(module)
            {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    self.file_for(&namespace),
                    None,
                    "P002",
                    error.to_string(),
                ));
            }
        }

        if self.start.is_some() {
            self.validate_start(&project);
        }
        self.detect_cycles(&project);
        let has_errors = self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
        let project = (!has_errors && self.analyzer.mode == AnalysisMode::Build).then_some(project);
        let mut jar_paths = self
            .mounts
            .values()
            .map(|mount| mount.path.clone())
            .collect::<Vec<_>>();
        jar_paths.sort();
        jar_paths.dedup();
        let mounts = self
            .mounts
            .iter()
            .map(|(namespace, mount)| (namespace.clone(), mount.path.clone()))
            .collect::<BTreeMap<_, _>>();
        AnalysisResult {
            project,
            diagnostics: self.diagnostics,
            jar_paths,
            mounts,
        }
    }

    // 解析单个模块的导入、依赖和可见性。
    fn build_module(&mut self, namespace: &ModulePath) -> Option<Module> {
        let parsed = self.parsed_modules.get(namespace)?.clone();
        let file = self.file_for(namespace);
        let local_names = parsed
            .targets()
            .iter()
            .map(|target| target.name().to_owned())
            .collect::<BTreeSet<_>>();
        let mut imports = BTreeMap::<String, TargetId>::new();

        for include in parsed.includes() {
            let Some(target) = self.resolve_path(namespace, include.target(), &file) else {
                continue;
            };
            if !self.validate_import(&target, include, &file) {
                continue;
            }
            let alias = include.alias().unwrap_or(target.name()).to_owned();
            if local_names.contains(&alias) {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.clone(),
                    Some(include.target().line()),
                    "P003",
                    format!("import name `{alias}` is ambiguous; choose a unique `as` alias"),
                ));
            } else if let Some(existing) = imports.get(&alias) {
                if existing != &target {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.clone(),
                        Some(include.target().line()),
                        "P003",
                        format!("import name `{alias}` is ambiguous; choose a unique `as` alias"),
                    ));
                }
            } else {
                imports.insert(alias, target);
            }
        }

        let mut module = Module::new(namespace.clone());
        for imported_target in imports.values() {
            module.add_include(imported_target.clone());
        }
        for parsed_target in parsed.targets() {
            let Ok(name) = TargetName::parse(parsed_target.name()) else {
                continue;
            };
            let mut target = Target::new(namespace.clone(), name);
            target.set_description(parsed_target.description());
            for specification in parsed_target.specifications() {
                target.add_specification(Spec::new(specification));
            }
            for dependency in parsed_target.dependencies() {
                if dependency.path().is_empty() {
                    continue;
                }
                let segments = dependency.path().segments();
                if segments.len() != 1 {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.clone(),
                        Some(dependency.line()),
                        "P004",
                        "target dependencies must use a local target or imported alias",
                    ));
                    continue;
                }
                let name = &segments[0];
                let resolved = if local_names.contains(name) {
                    TargetName::parse(name)
                        .ok()
                        .map(|name| TargetId::new(namespace.clone(), name))
                } else {
                    imports.get(name).cloned()
                };
                match resolved {
                    Some(resolved) => target.add_dependency(resolved),
                    None => self.diagnostics.push(AnalysisDiagnostic::error(
                        file.clone(),
                        Some(dependency.line()),
                        "P005",
                        format!("dependency `{name}` is neither a local target nor an import"),
                    )),
                }
            }
            if let Err(error) = module.add_target(target) {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.clone(),
                    Some(parsed_target.line()),
                    "P006",
                    error.to_string(),
                ));
            }
        }
        for public_target in parsed.public_targets() {
            if let Err(error) = module.publish_target(public_target.name()) {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.clone(),
                    Some(public_target.line()),
                    "P007",
                    error.to_string(),
                ));
            }
        }
        Some(module)
    }

    // 校验导入目标存在且允许跨模块访问。
    fn validate_import(&mut self, target: &TargetId, include: &ParsedInclude, file: &Path) -> bool {
        let Some(module) = self.parsed_modules.get(target.namespace()) else {
            return false;
        };
        let Some(parsed_target) = module
            .targets()
            .iter()
            .find(|candidate| candidate.name() == target.name())
        else {
            self.diagnostics.push(AnalysisDiagnostic::error(
                file.to_owned(),
                Some(include.target().line()),
                "P008",
                format!("imported target `{target}` does not exist"),
            ));
            return false;
        };
        let is_public = module
            .public_targets()
            .iter()
            .any(|candidate| candidate.name() == parsed_target.name());
        if !is_public {
            self.diagnostics.push(AnalysisDiagnostic::error(
                file.to_owned(),
                Some(include.target().line()),
                "P009",
                format!("imported target `{target}` is private"),
            ));
            return false;
        }
        true
    }

    // 按文件目录语义将导入路径解析为绝对目标。
    fn resolve_path(
        &mut self,
        current: &ModulePath,
        reference: &ParsedReference,
        file: &Path,
    ) -> Option<TargetId> {
        let segments = reference.path().segments();
        if segments.is_empty() {
            return None;
        }
        if segments.len() < 2 {
            self.diagnostics.push(AnalysisDiagnostic::error(
                file.to_owned(),
                Some(reference.line()),
                "P010",
                "an include path requires an anchor and target name",
            ));
            return None;
        }

        let target_name = match TargetName::parse(segments.last().unwrap()) {
            Ok(name) => name,
            Err(error) => {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(reference.line()),
                    "P019",
                    error.to_string(),
                ));
                return None;
            }
        };

        // * 前缀标记包段：包名必须紧邻目标名。
        let star = segments[1..segments.len() - 1]
            .iter()
            .position(|segment| segment.starts_with('*'));
        if let Some(offset) = star {
            let index = offset + 1;
            if index != segments.len() - 2 {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(reference.line()),
                    "P020",
                    "a package import must name its package directly before the target",
                ));
                return None;
            }
            let stem = segments[index].strip_prefix('*').unwrap_or("");
            if ModulePath::new(vec![stem.to_owned()]).is_err() {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(reference.line()),
                    "P020",
                    format!("`{stem}` is not a valid package name"),
                ));
                return None;
            }
            if self.mount_of(current).is_some() {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(reference.line()),
                    "P021",
                    "packages cannot be nested; expand them when packing",
                ));
                return None;
            }
            let anchor =
                self.anchor_segments(current, segments[0].as_str(), file, reference.line())?;
            let mut mount_segments = anchor;
            mount_segments.extend(segments[1..index].iter().cloned());
            mount_segments.push(stem.to_owned());
            let mount = match ModulePath::new(mount_segments) {
                Ok(mount) => mount,
                Err(error) => {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.to_owned(),
                        Some(reference.line()),
                        "P012",
                        error.to_string(),
                    ));
                    return None;
                }
            };
            let mut jar_path = self.analyzer.root_directory().to_path_buf();
            for segment in &segments[1..index] {
                jar_path.push(segment);
            }
            jar_path.push(format!("{stem}.mjar"));
            return self.mount_package(mount, &jar_path, reference, file, target_name);
        }

        let anchor = self.anchor_segments(current, segments[0].as_str(), file, reference.line())?;
        let mut module_segments = anchor;
        module_segments.extend_from_slice(&segments[1..segments.len() - 1]);
        let namespace = match ModulePath::new(module_segments) {
            Ok(namespace) => namespace,
            Err(error) => {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(reference.line()),
                    "P012",
                    error.to_string(),
                ));
                return None;
            }
        };
        Some(TargetId::new(namespace, target_name))
    }

    // 解析 crate/self/super 锚点为当前模块的基础分段。
    fn anchor_segments(
        &mut self,
        current: &ModulePath,
        anchor: &str,
        file: &Path,
        line: usize,
    ) -> Option<Vec<String>> {
        match anchor {
            "crate" => Some(self.crate_anchor(current).segments().to_vec()),
            "self" => Some(parent_segments(current)),
            "super" => {
                let parent = parent_segments(current);
                if parent.is_empty() {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.to_owned(),
                        Some(line),
                        "P015",
                        "`super` escapes the project root",
                    ));
                    None
                } else {
                    Some(parent[..parent.len() - 1].to_vec())
                }
            }
            _ => {
                self.diagnostics.push(AnalysisDiagnostic::error(
                    file.to_owned(),
                    Some(line),
                    "P011",
                    "an include path must start with `crate`, `self`, or `super`",
                ));
                None
            }
        }
    }

    // 打开并登记一个包，返回其根模块的导入目标。
    fn mount_package(
        &mut self,
        mount: ModulePath,
        jar_path: &Path,
        reference: &ParsedReference,
        file: &Path,
        target: TargetName,
    ) -> Option<TargetId> {
        if !self.mounts.contains_key(&mount) {
            let reader = match JarReader::open(jar_path) {
                Ok(reader) => reader,
                Err(error) => {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.to_owned(),
                        Some(reference.line()),
                        "P022",
                        error.to_string(),
                    ));
                    return None;
                }
            };
            let roots = match reader.crate_root_namespaces() {
                Ok(roots) => roots,
                Err(error) => {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.to_owned(),
                        Some(reference.line()),
                        "P022",
                        error.to_string(),
                    ));
                    return None;
                }
            };
            // 包模块与本地磁盘文件撞名时立即报告，避免挂载遮蔽源文件。
            for relative in reader.modules() {
                let full = join_namespaces(&mount, relative);
                let disk = self.analyzer.module_file(&full);
                if disk.is_file() {
                    self.diagnostics.push(AnalysisDiagnostic::error(
                        file.to_owned(),
                        Some(reference.line()),
                        "P002",
                        format!(
                            "duplicate module namespace `{full}`: provided by both package `{}` and local file `{}`",
                            jar_path.display(),
                            disk.display()
                        ),
                    ));
                    return None;
                }
            }
            let crate_roots = roots
                .into_iter()
                .map(|root| join_namespaces(&mount, &root))
                .collect::<Vec<_>>();
            self.jars.insert(
                jar_path
                    .canonicalize()
                    .unwrap_or_else(|_| jar_path.to_path_buf()),
                reader,
            );
            self.mounts.insert(
                mount.clone(),
                JarMount {
                    path: jar_path.to_path_buf(),
                    crate_roots,
                },
            );
        }
        Some(TargetId::new(mount, target))
    }

    // 返回模块源文本的存放位置。
    fn module_source(&self, namespace: &ModulePath) -> ModuleSource {
        let mount = self
            .mounts
            .iter()
            .filter(|(mounted, _)| namespace.segments().starts_with(mounted.segments()))
            .max_by_key(|(mounted, _)| mounted.segments().len());
        match mount {
            Some((mounted, info)) => ModuleSource::Jar {
                path: info.path.clone(),
                relative: sub_namespace(namespace, mounted.segments().len()),
            },
            None => ModuleSource::Disk(self.analyzer.module_file(namespace)),
        }
    }

    // 返回包含当前模块的挂载命名空间。
    fn mount_of(&self, current: &ModulePath) -> Option<&ModulePath> {
        self.mounts
            .keys()
            .find(|mounted| current.segments().starts_with(mounted.segments()))
    }

    // 返回当前模块的 crate 锚点：包内模块锚定包自身的根。
    fn crate_anchor(&self, current: &ModulePath) -> ModulePath {
        self.mounts
            .values()
            .flat_map(|mount| mount.crate_roots.iter())
            .filter(|root| current.segments().starts_with(root.segments()))
            .max_by_key(|root| root.segments().len())
            .cloned()
            .unwrap_or_else(ModulePath::root)
    }

    // 校验用户指定的起始目标存在。
    fn validate_start(&mut self, project: &Project) {
        let start = self
            .start
            .as_ref()
            .expect("only called with a start target");
        let valid = project
            .module(start.namespace())
            .and_then(|module| module.target(start.name()))
            .is_some();
        if !valid {
            self.diagnostics.push(AnalysisDiagnostic::error(
                self.file_for(start.namespace()),
                None,
                "P013",
                format!("start target `{start}` does not exist"),
            ));
        }
    }

    // 对项目中的目标依赖执行完整环检测。
    fn detect_cycles(&mut self, project: &Project) {
        let mut states = BTreeMap::<TargetId, VisitState>::new();
        let mut stack = Vec::new();
        let target_ids = project
            .modules()
            .flat_map(|module| module.targets().map(|target| target.id().clone()))
            .collect::<Vec<_>>();
        for target in target_ids {
            self.visit_target(project, &target, &mut states, &mut stack);
        }
    }

    // 深度优先访问目标并报告回边形成的环。
    fn visit_target(
        &mut self,
        project: &Project,
        target_id: &TargetId,
        states: &mut BTreeMap<TargetId, VisitState>,
        stack: &mut Vec<TargetId>,
    ) {
        match states.get(target_id) {
            Some(VisitState::Complete) => return,
            Some(VisitState::Active) => {
                let start = stack.iter().position(|item| item == target_id).unwrap_or(0);
                let mut cycle = stack[start..].to_vec();
                cycle.push(target_id.clone());
                self.diagnostics.push(AnalysisDiagnostic::error(
                    self.file_for(target_id.namespace()),
                    None,
                    "P014",
                    format!(
                        "dependency cycle: {}",
                        cycle
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ),
                ));
                return;
            }
            None => {}
        }
        states.insert(target_id.clone(), VisitState::Active);
        stack.push(target_id.clone());
        if let Some(target) = project
            .module(target_id.namespace())
            .and_then(|module| module.target(target_id.name()))
        {
            for dependency in target.dependencies() {
                self.visit_target(project, dependency, states, stack);
            }
        }
        stack.pop();
        states.insert(target_id.clone(), VisitState::Complete);
    }

    // 返回模块已记录或可推导的文件路径。
    fn file_for(&self, namespace: &ModulePath) -> PathBuf {
        self.module_files
            .get(namespace)
            .cloned()
            .unwrap_or_else(|| self.analyzer.module_file(namespace))
    }
}

// 标记深度优先遍历中的目标访问状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitState {
    Active,
    Complete,
}

// 返回模块文件所在目录的逻辑路径分段。
fn parent_segments(path: &ModulePath) -> Vec<String> {
    if path.is_root() {
        Vec::new()
    } else {
        path.segments()[..path.segments().len() - 1].to_vec()
    }
}

// 判断扫描到的条目是否以点开头而不应作为模块。
fn is_hidden_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

// 连接挂载前缀与包内相对命名空间。
fn join_namespaces(prefix: &ModulePath, relative: &ModulePath) -> ModulePath {
    let mut segments = prefix.segments().to_vec();
    segments.extend(relative.segments().iter().cloned());
    ModulePath::new(segments).expect("joining valid namespaces stays valid")
}

// 截取命名空间去掉指定长度前缀后的部分。
fn sub_namespace(namespace: &ModulePath, prefix_len: usize) -> ModulePath {
    let segments = namespace.segments()[prefix_len..].to_vec();
    ModulePath::new(segments).expect("suffixes of valid namespaces stay valid")
}

// 尽可能规范化路径后判断是否指向同一文件。
fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mjar::JarBuilder;
    use std::time::{SystemTime, UNIX_EPOCH};

    // 为测试快速构造经过校验的目标标识。
    fn id(namespace: ModulePath, name: &str) -> TargetId {
        TargetId::new(namespace, TargetName::parse(name).unwrap())
    }

    // 为测试快速构造合法模块路径。
    fn path(path: &str) -> ModulePath {
        ModulePath::parse(path).unwrap()
    }

    // 在临时目录中创建一组项目测试文件。
    fn fixture(files: &[(&str, &str)]) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("markfile-{unique}"));
        for (relative, content) in files {
            let file = root.join(relative);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        root
    }

    // 按名称读取示例元数据中的配置值。
    fn metadata_value<'a>(metadata: &'a str, name: &str) -> Option<&'a str> {
        metadata
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
    }

    // 将示例元数据中的绝对目标路径转换为目标标识。
    fn metadata_start(metadata: &str) -> TargetId {
        let mut segments = metadata_value(metadata, "start")
            .expect("example metadata must define a start target")
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let name = segments.pop().expect("the start target cannot be empty");
        let namespace = if segments.is_empty() {
            ModulePath::root()
        } else {
            ModulePath::new(segments).unwrap()
        };
        id(namespace, &name)
    }

    // 按稳定顺序返回指定分类中的示例目录。
    fn example_directories(category: &str) -> Vec<PathBuf> {
        let mut directories = fs::read_dir(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples")
                .join(category),
        )
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
        directories.sort();
        directories
    }

    // 验证进度观察者收到每个模块且写入错误能够传播。
    #[test]
    fn progress_observer_receives_modules_and_propagates_errors() {
        let root = fixture(&[
            (
                "main.mf",
                "> crate::shared::check\n---\n# run\n> check\n---\n> run\n",
            ),
            ("shared.mf", "---\n# check\n---\n> check\n"),
        ]);
        let analyzer = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build);
        let start = id(ModulePath::root(), "run");
        let mut seen = Vec::new();

        analyzer
            .analyze_with_progress(start.clone(), |module| {
                seen.push(module.clone());
                Ok(())
            })
            .unwrap();
        let error = analyzer
            .analyze_with_progress(start, |_| Err(std::io::Error::other("log unavailable")))
            .unwrap_err();

        assert_eq!(seen, [ModulePath::root(), path("shared")]);
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        fs::remove_dir_all(root).unwrap();
    }

    // 验证仅加载起始模块的导入可达集合。
    #[test]
    fn loads_only_modules_reachable_from_the_start_module() {
        let root = fixture(&[
            ("main.mf", "---\n# root\n---\n> root\n"),
            (
                "app.mf",
                "> crate::shared::check\n---\n# run\n> check\n---\n> run\n",
            ),
            ("shared.mf", "---\n# check\n---\n> check\n"),
            ("broken.mf", "not valid"),
        ]);
        let start = id(path("app"), "run");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);
        let project = result.project().unwrap();

        assert!(!result.has_errors());
        assert!(project.module(&path("app")).is_some());
        assert!(project.module(&path("shared")).is_some());
        assert!(project.module(&path("broken")).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    // 验证 self 与 super 采用文件目录语义。
    #[test]
    fn resolves_file_directory_self_and_super_paths() {
        let root = fixture(&[
            ("main.mf", "---\n# root_check\n---\n> root_check\n"),
            (
                "catalog/api.mf",
                "> self::model::check as model_check\n> super::root_check\n---\n# publish\n> model_check\n> root_check\n---\n> publish\n",
            ),
            ("catalog/model.mf", "---\n# check\n---\n> check\n"),
        ]);
        let start = id(path("catalog::api"), "publish");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);
        let target = result
            .project()
            .unwrap()
            .module(&path("catalog::api"))
            .unwrap()
            .target("publish")
            .unwrap();

        assert_eq!(
            target.dependencies(),
            &[
                id(path("catalog::model"), "check"),
                id(ModulePath::root(), "root_check"),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证别名冲突、私有导入和依赖环诊断。
    #[test]
    fn reports_alias_collisions_private_imports_and_cycles() {
        let root = fixture(&[
            (
                "main.mf",
                "> crate::one::check\n> crate::two::check\n> crate::private::hidden\n---\n# run\n> check\n---\n> run\n",
            ),
            (
                "one.mf",
                "---\n# check\n> looped\n# looped\n> check\n---\n> check\n> looped\n",
            ),
            ("two.mf", "---\n# check\n---\n> check\n"),
            ("private.mf", "---\n# hidden\n---\n"),
        ]);
        let start = id(ModulePath::root(), "run");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);
        let codes = result
            .diagnostics()
            .iter()
            .map(AnalysisDiagnostic::code)
            .collect::<Vec<_>>();

        assert!(result.project().is_none());
        assert!(codes.contains(&"P003"));
        assert!(codes.contains(&"P009"));
        assert!(codes.contains(&"P014"));
        fs::remove_dir_all(root).unwrap();
    }

    // 验证项目试运行扫描同一可达图但不交付项目。
    #[test]
    fn dry_run_scans_the_same_reachable_graph_without_returning_a_project() {
        let root = fixture(&[("main.mf", "---\n# run\n---\n> run\n")]);
        let start = id(ModulePath::root(), "run");

        let result =
            ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun).analyze(start);

        assert!(!result.has_errors());
        assert!(result.project().is_none());
        assert!(result.into_project().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    // 验证非法导入目标名称被定位且其他文件仍被分析。
    #[test]
    fn invalid_import_target_name_reports_an_error_without_stopping_analysis() {
        let root = fixture(&[
            (
                "main.mf",
                "> crate::shared::bad`name\n> crate::shared::ready\n---\n# run\n> ready\n---\n> run\n",
            ),
            ("shared.mf", "---\n# ready\n- accepted\n---\n> ready\n"),
        ]);
        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun)
            .analyze(id(ModulePath::root(), "run"));

        assert!(result.project().is_none());
        assert!(
            result
                .diagnostics()
                .iter()
                .any(|diagnostic| { diagnostic.code() == "P019" && diagnostic.line() == Some(1) })
        );
        assert!(
            !result
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == "P005")
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证试运行完整收集各类引用错误。
    #[test]
    fn resolver_reports_all_reference_failures_in_dry_run() {
        let root = fixture(&[
            (
                "main.mf",
                "> crate\n> unknown::thing\n> crate::..::target\n> crate::shared::missing\n---\n# run\n> crate::shared::ok\n> unknown\n---\n> run\n",
            ),
            ("shared.mf", "---\n# ok\n---\n> ok\n"),
        ]);
        let start = id(ModulePath::root(), "run");

        let result =
            ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun).analyze(start);
        let codes = result
            .diagnostics()
            .iter()
            .map(AnalysisDiagnostic::code)
            .collect::<BTreeSet<_>>();

        assert!(result.project().is_none());
        for code in ["P004", "P005", "P008", "P010", "P011", "P012"] {
            assert!(codes.contains(code), "missing diagnostic {code}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    // 验证相同目标的重复导入保持幂等。
    #[test]
    fn duplicate_imports_of_the_same_target_are_idempotent() {
        let root = fixture(&[
            (
                "main.mf",
                "> crate::shared::check\n> crate::shared::check\n---\n# run\n> check\n---\n> run\n",
            ),
            ("shared.mf", "---\n# check\n---\n> check\n"),
        ]);
        let start = id(ModulePath::root(), "run");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);

        assert!(!result.has_errors());
        assert_eq!(
            result.project().unwrap().root().unwrap().includes().len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证根文件不能被另一模块路径重复加载。
    #[test]
    fn root_file_cannot_be_loaded_again_as_a_named_module() {
        let root = fixture(&[("main.mf", "> crate::main::root\n---\n# root\n---\n> root\n")]);
        let start = id(ModulePath::root(), "root");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);

        assert!(
            result
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == "P016")
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证 super 无法越过项目根目录。
    #[test]
    fn super_cannot_escape_the_project_root() {
        let root = fixture(&[(
            "main.mf",
            "> super::outside::check\n---\n# run\n---\n> run\n",
        )]);
        let start = id(ModulePath::root(), "run");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);

        assert!(result.project().is_none());
        assert!(
            result
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == "P015")
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证全部合法示例可解析为健康项目。
    #[test]
    fn all_valid_examples_resolve_to_healthy_projects() {
        for directory in example_directories("valid") {
            let metadata = fs::read_to_string(directory.join("expected.txt")).unwrap();
            let analyzer = ProjectAnalyzer::new(directory.join("main.mf"), AnalysisMode::Build);
            let result = analyzer.analyze(metadata_start(&metadata));
            assert!(
                !result.has_errors(),
                "{}: {:?}",
                directory.display(),
                result.diagnostics()
            );
            assert!(result.project().is_some());
        }
    }

    // 验证全部问题示例产生声明的诊断代码。
    #[test]
    fn all_invalid_examples_emit_expected_diagnostics() {
        for directory in example_directories("invalid") {
            let metadata = fs::read_to_string(directory.join("expected.txt")).unwrap();
            let analyzer = ProjectAnalyzer::new(directory.join("main.mf"), AnalysisMode::DryRun);
            let result = analyzer.analyze(metadata_start(&metadata));
            let actual = result
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_owned())
                .collect::<BTreeSet<_>>();
            let expected = metadata_value(&metadata, "codes")
                .expect("invalid example metadata must define diagnostic codes")
                .split(',')
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();

            assert!(result.has_errors(), "{}", directory.display());
            assert!(
                expected.is_subset(&actual),
                "{}: expected {expected:?}, got {actual:?}",
                directory.display()
            );
        }
    }

    // 验证根发现支持隐藏布局且同目录按文档顺序取优。
    #[test]
    fn root_discovery_supports_hidden_layouts_in_documented_order() {
        let root = fixture(&[
            (".mkd/main.mf", "---\n# hidden\n---\n> hidden\n"),
            ("lib.mf", "---\n# lib\n---\n> lib\n"),
            ("nested/.keep", ""),
        ]);

        assert_eq!(
            find_root_file(&root.join("nested")),
            Some(root.join("lib.mf"))
        );

        fs::write(root.join("main.mf"), "---\n# flat\n---\n> flat\n").unwrap();

        assert_eq!(
            find_root_file(&root.join("nested")),
            Some(root.join("main.mf"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证配置目录在隐藏布局指向上级。
    #[test]
    fn config_directory_points_above_hidden_layout() {
        assert_eq!(config_directory(Path::new(".mkd/main.mf")), Path::new("."));
        assert_eq!(
            config_directory(Path::new("host/.mkd/lib.mf")),
            Path::new("host")
        );
        assert_eq!(
            config_directory(Path::new("proj/main.mf")),
            Path::new("proj")
        );
    }

    // 验证模块扫描忽略点开头的目录与文件。
    #[test]
    fn discovery_skips_hidden_directories_and_files() {
        let root = fixture(&[
            ("main.mf", "---\n# run\n---\n> run\n"),
            (".hidden/secret.mf", "---\n# secret\n---\n> secret\n"),
            (".dot.mf", "---\n# dot\n---\n> dot\n"),
        ]);

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun)
            .analyze_all_with_progress(|_| Ok(()))
            .unwrap();

        assert!(!result.has_errors());
        for namespace in [".hidden::secret", ".dot"] {
            assert!(
                result
                    .diagnostics()
                    .iter()
                    .all(|diagnostic| { !diagnostic.message().contains(namespace) })
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    // 在临时目录内写入一个示例包。
    fn write_jar(root: &Path, relative: &str, name: &str, modules: &[(String, &str)]) -> PathBuf {
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
        let path = root.join(relative);
        builder.write_to(&path).unwrap();
        path
    }

    // 验证 * 导入挂载包并将包内可达图并入项目分析。
    #[test]
    fn package_import_mounts_the_jar_and_resolves_internal_references() {
        let root = fixture(&[]);
        write_jar(
            &root,
            "vendor/utils.mjar",
            "utils",
            &[
                (
                    String::new(),
                    "> crate::dep::verify\n> crate::sub::mod::combine\n---\n# check\n> verify\n> combine\n检查。\n- 检查通过。\n---\n> check\n",
                ),
                (
                    "dep".to_owned(),
                    "---\n# verify\n- 校验通过。\n---\n> verify\n",
                ),
                (
                    "sub::mod".to_owned(),
                    "> self::peer::peer\n> super::dep::verify\n---\n# combine\n> peer\n> verify\n---\n> combine\n",
                ),
                (
                    "sub::peer".to_owned(),
                    "---\n# peer\n- 同伴就绪。\n---\n> peer\n",
                ),
            ],
        );
        fs::write(
            root.join("main.mf"),
            "> crate::vendor::*utils::check as pkg_check\n---\n# run\n> pkg_check\n运行。\n---\n> run\n",
        )
        .unwrap();

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build)
            .analyze(id(ModulePath::root(), "run"));

        assert!(!result.has_errors(), "{:?}", result.diagnostics());
        let project = result.project().unwrap();
        let run = project.root().unwrap().target("run").unwrap();
        assert_eq!(run.dependencies(), &[id(path("vendor::utils"), "check")]);
        let check = project
            .module(&path("vendor::utils"))
            .unwrap()
            .target("check")
            .unwrap();
        assert_eq!(
            check.dependencies(),
            &[
                id(path("vendor::utils::dep"), "verify"),
                id(path("vendor::utils::sub::mod"), "combine"),
            ]
        );
        let combine = project
            .module(&path("vendor::utils::sub::mod"))
            .unwrap()
            .target("combine")
            .unwrap();
        assert_eq!(
            combine.dependencies(),
            &[
                id(path("vendor::utils::sub::peer"), "peer"),
                id(path("vendor::utils::dep"), "verify"),
            ]
        );
        assert_eq!(result.jar_paths(), &[root.join("vendor/utils.mjar")]);
        fs::remove_dir_all(root).unwrap();
    }

    // 验证缺失包、非法包段位置与嵌套导入都有定位诊断。
    #[test]
    fn package_import_reports_missing_misplaced_or_nested_references() {
        let root = fixture(&[]);
        write_jar(
            &root,
            "nested.mjar",
            "nested",
            &[(
                String::new(),
                "> crate::*other::x\n---\n# entry\n- 入口。\n---\n> entry\n",
            )],
        );
        fs::write(
            root.join("main.mf"),
            "> crate::vendor::*missing::x\n> crate::*nested::entry\n> crate::vendor::*utils::sub::check\n---\n# run\n- 运行。\n---\n> run\n",
        )
        .unwrap();

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun)
            .analyze(id(ModulePath::root(), "run"));
        let codes = result
            .diagnostics()
            .iter()
            .map(AnalysisDiagnostic::code)
            .collect::<BTreeSet<_>>();

        assert!(result.project().is_none());
        assert!(codes.contains("P020"));
        assert!(codes.contains("P021"));
        assert!(codes.contains("P022"));
        assert!(
            result
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code() == "P022"
                    && diagnostic.message().contains("missing.mjar"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    // 验证包挂载与本地模块文件撞名时报重复命名空间错误。
    #[test]
    fn package_mount_collides_with_local_modules() {
        let root = fixture(&[]);
        write_jar(
            &root,
            "dup.mjar",
            "dup",
            &[(String::new(), "---\n# entry\n- 入口。\n---\n> entry\n")],
        );
        fs::write(
            root.join("dup.mf"),
            "---\n# local\n- 本地。\n---\n> local\n",
        )
        .unwrap();
        fs::write(
            root.join("main.mf"),
            "> crate::*dup::entry\n---\n# run\n- 运行。\n---\n> run\n",
        )
        .unwrap();

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::DryRun)
            .analyze(id(ModulePath::root(), "run"));

        assert!(result.project().is_none());
        assert!(result.diagnostics().iter().any(|diagnostic| {
            diagnostic.code() == "P002"
                && diagnostic
                    .message()
                    .contains("duplicate module namespace `dup`")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    // 验证缺失模块和起始目标均被报告。
    #[test]
    fn missing_modules_and_start_targets_are_reported() {
        let root = fixture(&[(
            "main.mf",
            "> crate::missing::target\n---\n# run\n---\n> run\n",
        )]);
        let start = id(ModulePath::root(), "absent");

        let result = ProjectAnalyzer::new(root.join("main.mf"), AnalysisMode::Build).analyze(start);
        let codes = result
            .diagnostics()
            .iter()
            .map(AnalysisDiagnostic::code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&"P001"));
        assert!(codes.contains(&"P013"));
        assert_eq!(
            result.diagnostics()[0].severity(),
            DiagnosticSeverity::Error
        );
        assert!(result.diagnostics()[0].file().ends_with("missing.mf"));
        assert!(result.diagnostics()[0].line().is_none());
        assert!(!result.diagnostics()[0].message().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
