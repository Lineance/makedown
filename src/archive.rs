use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::core::ModulePath;
use crate::parser::{ParseMode, Parser};

/// 描述 Git 初始化或归档失败的原因。
#[derive(Debug)]
pub enum ArchiveError {
    GitUnavailable(io::Error),
    GitCommand {
        operation: &'static str,
        detail: String,
    },
    InvalidGitOutput(&'static str),
    NothingToArchive,
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitUnavailable(error) => write!(formatter, "cannot run git: {error}"),
            Self::GitCommand { operation, detail } => {
                write!(formatter, "git {operation} failed")?;
                if !detail.is_empty() {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::InvalidGitOutput(operation) => {
                write!(formatter, "git {operation} returned invalid output")
            }
            Self::NothingToArchive => formatter.write_str("nothing to archive"),
        }
    }
}

impl Error for ArchiveError {}

/// 在项目目录初始化或安全复用 Git 仓库。
pub fn initialize_repository(directory: &Path) -> Result<(), ArchiveError> {
    let output = run_git(directory, [OsStr::new("init"), OsStr::new("--quiet")])?;
    require_success("init", output).map(|_| ())
}

/// 暂存指定范围内的全部变化，推断 Target 摘要并创建提交。
pub fn archive_project(directory: &Path, scopes: &[&str], root_relative: &Path) -> Result<String, ArchiveError> {
    let mut staged = vec![OsStr::new("add"), OsStr::new("--all"), OsStr::new("--")];
    staged.extend(scopes.iter().map(OsStr::new));
    require_success("add", run_git(directory, staged)?)?;

    let mut cached = vec![
        OsStr::new("diff"),
        OsStr::new("--cached"),
        OsStr::new("--quiet"),
        OsStr::new("--"),
    ];
    cached.extend(scopes.iter().map(OsStr::new));
    let changed = run_git(directory, cached)?;
    match changed.status.code() {
        Some(0) => return Err(ArchiveError::NothingToArchive),
        Some(1) => {}
        _ => return Err(git_failure("diff", &changed)),
    }

    let message = infer_commit_message(directory, scopes, root_relative)?;
    let mut commit = vec![
        OsStr::new("commit"),
        OsStr::new("--quiet"),
        OsStr::new("-m"),
        OsStr::new(&message),
        OsStr::new("--"),
    ];
    commit.extend(scopes.iter().map(OsStr::new));
    require_success("commit", run_git(directory, commit)?)?;
    Ok(message)
}

// 根据暂存区相对 HEAD 的 Markfile 变化生成确定性摘要。
fn infer_commit_message(
    directory: &Path,
    scopes: &[&str],
    root_relative: &Path,
) -> Result<String, ArchiveError> {
    let mut diff = vec![
        OsStr::new("diff"),
        OsStr::new("--cached"),
        OsStr::new("--name-status"),
        OsStr::new("-z"),
        OsStr::new("--no-renames"),
        OsStr::new("--relative"),
        OsStr::new("--"),
    ];
    diff.extend(scopes.iter().map(OsStr::new));
    let output = require_success("diff", run_git(directory, diff)?)?;
    let entries = changed_paths(&output.stdout)?;
    let prefix = repository_prefix(directory)?;
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    let mut recognized_all_markfiles = true;

    for (status, path) in entries {
        if path.extension() != Some(OsStr::new("mf")) {
            continue;
        }
        let repository_path = prefix.join(&path);
        if status != b'A' {
            let source = git_blob(directory, &format!("HEAD:{}", git_path(&repository_path)))?;
            match source.and_then(|source| target_snapshots(&path, root_relative, &source)) {
                Some(targets) => before.extend(targets),
                None => recognized_all_markfiles = false,
            }
        }
        if status != b'D' {
            let source = git_blob(directory, &format!(":{}", git_path(&repository_path)))?;
            match source.and_then(|source| target_snapshots(&path, root_relative, &source)) {
                Some(targets) => after.extend(targets),
                None => recognized_all_markfiles = false,
            }
        }
    }

    if !recognized_all_markfiles {
        return Ok("Archive project changes".to_owned());
    }

    let added = after
        .keys()
        .filter(|target| !before.contains_key(*target))
        .cloned()
        .collect::<BTreeSet<_>>();
    let removed = before
        .keys()
        .filter(|target| !after.contains_key(*target))
        .cloned()
        .collect::<BTreeSet<_>>();
    let updated = after
        .iter()
        .filter(|(target, snapshot)| before.get(*target).is_some_and(|old| old != *snapshot))
        .map(|(target, _)| target.clone())
        .collect::<BTreeSet<_>>();

    if added.is_empty() && updated.is_empty() && removed.is_empty() {
        return Ok("Archive project changes".to_owned());
    }
    Ok(format_commit_message(&added, &updated, &removed))
}

// 将三类 Target 变化渲染为带概括主题行和分组清单的提交消息。
fn format_commit_message(
    added: &BTreeSet<String>,
    updated: &BTreeSet<String>,
    removed: &BTreeSet<String>,
) -> String {
    let actions = [("add", added), ("update", updated), ("remove", removed)];
    let summary = actions
        .iter()
        .filter(|(_, targets)| !targets.is_empty())
        .map(|(action, targets)| format!("{action} {}", targets.len()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!("Archive targets: {summary}");
    for (heading, targets) in [("Add:", added), ("Update:", updated), ("Remove:", removed)] {
        if targets.is_empty() {
            continue;
        }
        message.push_str("\n\n");
        message.push_str(heading);
        for target in targets {
            message.push_str(&format!("\n- {target}"));
        }
    }
    message
}

// 将 Git 的 NUL 分隔 name-status 输出转换为状态与项目相对路径。
fn changed_paths(output: &[u8]) -> Result<Vec<(u8, PathBuf)>, ArchiveError> {
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() % 2 != 0 {
        return Err(ArchiveError::InvalidGitOutput("diff"));
    }
    fields
        .chunks_exact(2)
        .map(|pair| {
            let status = *pair[0]
                .first()
                .ok_or(ArchiveError::InvalidGitOutput("diff"))?;
            let path =
                std::str::from_utf8(pair[1]).map_err(|_| ArchiveError::InvalidGitOutput("diff"))?;
            Ok((status, PathBuf::from(path)))
        })
        .collect()
}

fn repository_prefix(directory: &Path) -> Result<PathBuf, ArchiveError> {
    let output = require_success(
        "rev-parse",
        run_git(
            directory,
            [OsStr::new("rev-parse"), OsStr::new("--show-prefix")],
        )?,
    )?;
    let prefix = std::str::from_utf8(&output.stdout)
        .map_err(|_| ArchiveError::InvalidGitOutput("rev-parse"))?
        .trim_end_matches(['\r', '\n']);
    Ok(PathBuf::from(prefix))
}

fn git_blob(directory: &Path, object: &str) -> Result<Option<String>, ArchiveError> {
    let output = run_git(
        directory,
        [
            OsStr::new("show"),
            OsStr::new("--no-textconv"),
            OsStr::new(object),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|_| ArchiveError::InvalidGitOutput("show"))
}

fn git_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Eq, PartialEq)]
struct TargetSnapshot {
    dependencies: Vec<Vec<String>>,
    description: String,
    specifications: Vec<String>,
    includes: Vec<(Vec<String>, Option<String>)>,
    public: bool,
}

fn target_snapshots(
    path: &Path,
    root_relative: &Path,
    source: &str,
) -> Option<BTreeMap<String, TargetSnapshot>> {
    let namespace = module_path(path, root_relative)?;
    let parsed = Parser::new(ParseMode::Build).parse(namespace.clone(), source);
    let module = parsed.module()?;
    let includes = module
        .includes()
        .iter()
        .map(|include| {
            (
                include.target().path().segments().to_vec(),
                include.alias().map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let public = module
        .public_targets()
        .iter()
        .map(|target| target.name())
        .collect::<BTreeSet<_>>();
    Some(
        module
            .targets()
            .iter()
            .map(|target| {
                let name = if namespace.is_root() {
                    target.name().to_owned()
                } else {
                    format!("{namespace}::{}", target.name())
                };
                let snapshot = TargetSnapshot {
                    dependencies: target
                        .dependencies()
                        .iter()
                        .map(|dependency| dependency.path().segments().to_vec())
                        .collect(),
                    description: target.description().to_owned(),
                    specifications: target.specifications().to_vec(),
                    includes: includes.clone(),
                    public: public.contains(target.name()),
                };
                (name, snapshot)
            })
            .collect(),
    )
}

fn module_path(path: &Path, root_relative: &Path) -> Option<ModulePath> {
    if path == root_relative {
        return Some(ModulePath::root());
    }
    let mut segments = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|component| component.as_os_str().to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    segments.push(path.file_stem()?.to_str()?.to_owned());
    ModulePath::new(segments).ok()
}

fn run_git<I, S>(directory: &Path, arguments: I) -> Result<Output, ArchiveError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .map_err(ArchiveError::GitUnavailable)
}

fn require_success(operation: &'static str, output: Output) -> Result<Output, ArchiveError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(git_failure(operation, &output))
    }
}

fn git_failure(operation: &'static str, output: &Output) -> ArchiveError {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ArchiveError::GitCommand { operation, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_snapshots_include_semantic_target_content() {
        let source = "> crate::shared::ready as dependency\n---\n# publish\n> dependency\nShip it.\n- It shipped.\n---\n> publish\n";
        let targets =
            target_snapshots(Path::new("release.mf"), Path::new("main.mf"), source).unwrap();
        let target = targets.get("release::publish").unwrap();

        assert_eq!(target.description, "Ship it.");
        assert_eq!(target.specifications, ["It shipped."]);
        assert_eq!(target.dependencies, [vec!["dependency".to_owned()]]);
        assert!(target.public);
    }

    #[test]
    fn invalid_markfiles_do_not_produce_misleading_targets() {
        let targets = target_snapshots(
            Path::new("main.mf"),
            Path::new("main.mf"),
            "---\n# broken\n- specification\ndescription too late\n---\n> broken\n",
        );

        assert!(targets.is_none());
    }

    #[test]
    fn commit_messages_group_actions_with_a_summary_subject() {
        let mut added = BTreeSet::new();
        added.insert("launch".to_owned());
        added.insert("build".to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("draft".to_owned());

        assert_eq!(
            format_commit_message(&added, &BTreeSet::new(), &removed),
            "Archive targets: add 2, remove 1\n\nAdd:\n- build\n- launch\n\nRemove:\n- draft"
        );

        let mut updated = BTreeSet::new();
        updated.insert("start".to_owned());
        assert_eq!(
            format_commit_message(&BTreeSet::new(), &updated, &BTreeSet::new()),
            "Archive targets: update 1\n\nUpdate:\n- start"
        );
    }

    #[test]
    fn changed_path_parser_accepts_nul_delimited_git_output() {
        let paths = changed_paths(b"M\0main.mf\0A\0notes.txt\0").unwrap();

        assert_eq!(
            paths,
            vec![
                (b'M', PathBuf::from("main.mf")),
                (b'A', PathBuf::from("notes.txt"))
            ]
        );
    }
}
