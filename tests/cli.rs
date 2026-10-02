use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

// 返回当前测试构建生成的 mkd 二进制路径。
fn mkd() -> &'static str {
    env!("CARGO_BIN_EXE_mkd")
}

// 创建用于隔离 CLI 输出的唯一临时目录。
fn temporary_directory() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("mkd-e2e-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

// 返回仓库内指定分类和名称的示例目录。
fn example(category: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(category)
        .join(name)
}

// 使用指定工作目录执行 mkd 并捕获输出。
fn run(current_directory: &Path, arguments: &[&str]) -> Output {
    Command::new(mkd())
        .current_dir(current_directory)
        .args(arguments)
        .output()
        .unwrap()
}

// 在隔离仓库中运行 Git 并要求命令成功。
fn git(current_directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .current_dir(current_directory)
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    output
}

// 为归档测试配置仓库本地提交身份。
fn configure_git_identity(repository: &Path) {
    git(repository, &["config", "user.name", "Makedown Test"]);
    git(
        repository,
        &["config", "user.email", "makedown@example.invalid"],
    );
}

// 验证从项目深层目录运行时能够向上发现根文件并编译。
#[test]
fn binary_discovers_main_from_a_nested_directory() {
    let output_directory = temporary_directory();
    let artifact = output_directory.join("plan.md");
    let project = example("valid", "09-deep-directory");
    let result = run(
        &project.join("org/product/api"),
        &["target", "expose_service", "-o", artifact.to_str().unwrap()],
    );

    assert!(result.status.success(), "{:?}", result);
    assert!(result.stdout.is_empty());
    let logs = String::from_utf8(result.stderr).unwrap();
    assert!(logs.contains("Parsing <root>"));
    assert!(logs.contains("Compiling expose_service"));
    assert!(logs.contains("Finished built target `expose_service`"));
    let markdown = fs::read_to_string(&artifact).unwrap();
    assert!(markdown.contains("## 阶段 5：最终目标"));
    assert!(markdown.contains("### 目标 `org::product::api::handler::publish_api`"));
    fs::remove_dir_all(output_directory).unwrap();
}

// 验证显式根文件允许从项目外编译任意命名空间目标。
#[test]
fn binary_compiles_a_namespaced_target_with_an_explicit_root() {
    let working_directory = temporary_directory();
    let artifact = working_directory.join("identity.md");
    let root_file = example("valid", "10-enterprise-platform").join("main.mf");
    let result = run(
        &working_directory,
        &[
            "target",
            "platform::services::identity::ready",
            "--root",
            root_file.to_str().unwrap(),
            "-o",
            artifact.to_str().unwrap(),
        ],
    );

    assert!(result.status.success(), "{:?}", result);
    let markdown = fs::read_to_string(&artifact).unwrap();
    assert!(markdown.contains("# 规格实施计划：platform::services::identity::ready"));
    assert!(markdown.contains("### 目标 `platform::infra::database::database_ready`"));
    fs::remove_dir_all(working_directory).unwrap();
}

// 验证模板路径相对工作目录并将多阶段目标渲染为 Markdown。
#[test]
fn binary_builds_with_a_custom_template_from_the_working_directory() {
    let working_directory = temporary_directory();
    let root = example("valid", "09-deep-directory").join("main.mf");
    let template = "# {{ plan.root_target }}\n{% for stage in plan.stages %}## 阶段 {{ stage.index }}\n{% for target in stage.targets %}- {{ target.id }}: {{ target.description }}\n{% for spec in target.specifications %}  - [ ] {{ spec }}\n{% endfor %}{% endfor %}{% endfor %}";
    fs::write(working_directory.join("custom.md"), template).unwrap();
    let result = run(
        &working_directory,
        &[
            "target",
            "expose_service",
            "--root",
            root.to_str().unwrap(),
            "--template",
            "custom.md",
            "-o",
            "result.md",
        ],
    );

    assert!(result.status.success(), "{result:?}");
    let markdown = fs::read_to_string(working_directory.join("result.md")).unwrap();
    assert!(markdown.starts_with("# expose_service\n## 阶段 1\n"));
    assert!(markdown.contains("- org::product::api::handler::publish_api:"));
    assert!(markdown.contains("  - [ ] "));
    fs::remove_dir_all(working_directory).unwrap();
}

// 验证检查模式校验模板却不写输出并保持单独检查的行为。
#[test]
fn binary_check_with_template_validates_rendering_without_writing() {
    let working_directory = temporary_directory();
    let root = example("valid", "01-single-target").join("main.mf");
    let template = working_directory.join("plan.md");
    fs::write(&template, "{{ plan.root_target }}").unwrap();
    let args = [
        "target",
        "greet",
        "--root",
        root.to_str().unwrap(),
        "--check",
        "--template",
        "plan.md",
    ];
    let valid = run(&working_directory, &args);
    assert!(valid.status.success(), "{valid:?}");
    assert_eq!(
        fs::read_to_string(&template).unwrap(),
        "{{ plan.root_target }}"
    );
    assert!(!working_directory.join("result.md").exists());
    fs::write(&template, "{{ plan.missing }}").unwrap();
    let invalid = run(&working_directory, &args);
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(stderr.contains("plan.md`:1:"), "{stderr}");
    assert!(stderr.contains("undefined value"), "{stderr}");
    let plain_check = run(
        &working_directory,
        &[
            "target",
            "greet",
            "--root",
            root.to_str().unwrap(),
            "--check",
        ],
    );
    assert!(plain_check.status.success(), "{plain_check:?}");
    fs::remove_dir_all(working_directory).unwrap();
}

// 验证模板语法错误和外部模板引用不能生成或覆盖输出。
#[test]
fn binary_rejects_invalid_templates_without_overwriting_output() {
    let project = temporary_directory();
    fs::write(
        project.join("main.mf"),
        "---\n# build\n- accepted\n---\n> build\n",
    )
    .unwrap();
    fs::write(project.join("layout.md"), "{% for stage in plan.stages %}").unwrap();
    fs::write(project.join("result.md"), "keep").unwrap();
    let result = run(
        &project,
        &[
            "target",
            "build",
            "--template",
            "layout.md",
            "-o",
            "result.md",
        ],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("layout.md`:1:"));
    assert_eq!(
        fs::read_to_string(project.join("result.md")).unwrap(),
        "keep"
    );
    fs::write(project.join("layout.md"), "{% include 'secret.md' %}").unwrap();
    let result = run(
        &project,
        &[
            "target",
            "build",
            "--template",
            "layout.md",
            "-o",
            "result.md",
        ],
    );
    assert!(!result.status.success());
    assert_eq!(
        fs::read_to_string(project.join("result.md")).unwrap(),
        "keep"
    );
    let missing = run(
        &project,
        &[
            "target",
            "build",
            "--template",
            "missing.md",
            "-o",
            "result.md",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing.md"));
    assert_eq!(
        fs::read_to_string(project.join("result.md")).unwrap(),
        "keep"
    );
    fs::remove_dir_all(project).unwrap();
}

// 验证编译产物不能覆盖模板、根文件或分析过的依赖模块。
#[test]
fn binary_refuses_to_overwrite_compilation_inputs() {
    let project = temporary_directory();
    fs::write(
        project.join("main.mf"),
        "> crate::shared::ready\n---\n# build\n> ready\n---\n> build\n",
    )
    .unwrap();
    fs::write(
        project.join("shared.mf"),
        "---\n# ready\n- yes\n---\n> ready\n",
    )
    .unwrap();
    fs::write(project.join("layout.md"), "# {{ plan.root_target }}\n").unwrap();
    for file in ["main.mf", "shared.mf", "layout.md"] {
        let before = fs::read_to_string(project.join(file)).unwrap();
        let result = run(
            &project,
            &["target", "build", "--template", "layout.md", "-o", file],
        );
        assert!(!result.status.success(), "{file}: {result:?}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("would overwrite"));
        assert_eq!(fs::read_to_string(project.join(file)).unwrap(), before);
    }
    let before = fs::read_to_string(project.join("shared.mf")).unwrap();
    let no_template = run(&project, &["target", "build", "-o", "shared.mf"]);
    assert!(!no_template.status.success());
    assert_eq!(
        fs::read_to_string(project.join("shared.mf")).unwrap(),
        before
    );
    fs::remove_dir_all(project).unwrap();
}

// 验证 init 生成的项目可从子目录按默认配方构建，且支持多个命名入口。
#[test]
fn init_and_named_profiles_build_reproducibly_from_nested_directory() {
    let directory = temporary_directory();
    let project = directory.join("project");
    let initialized = run(&directory, &["init", "--flat", "project"]);
    assert!(initialized.status.success(), "{initialized:?}");
    assert!(project.join(".git").is_dir());
    fs::create_dir_all(project.join("nested")).unwrap();
    let default = run(&project.join("nested"), &["build"]);
    assert!(default.status.success(), "{default:?}");
    assert!(
        fs::read_to_string(project.join("dist/plan.md"))
            .unwrap()
            .contains("规格实施计划：start")
    );
    fs::write(
        project.join("guide.mf"),
        "---\n# write\n- 文档已完成。\n---\n> write\n",
    )
    .unwrap();
    fs::write(project.join("mkd.toml"), "version = 1\n[build]\ndefault = 'main'\n[build.main]\ntarget = 'start'\noutput = 'dist/plan.md'\n[build.guide]\ntarget = 'guide::write'\noutput = 'dist/guide.md'\n").unwrap();
    let named = run(&project.join("nested"), &["build", "guide"]);
    assert!(named.status.success(), "{named:?}");
    assert!(
        fs::read_to_string(project.join("dist/guide.md"))
            .unwrap()
            .contains("guide::write")
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证 archive 暂存全部项目变化并根据 Target 语义变化生成提交消息。
#[test]
fn archive_commits_all_changes_with_inferred_target_messages() {
    let directory = temporary_directory();
    let project = directory.join("project");
    let initialized = run(&directory, &["init", "--flat", "project"]);
    assert!(initialized.status.success(), "{initialized:?}");
    configure_git_identity(&project);

    fs::write(project.join("notes.txt"), "first\n").unwrap();
    let first = run(&project, &["archive"]);
    assert!(first.status.success(), "{first:?}");
    let first_message = git(&project, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(first_message.stdout).unwrap().trim(),
        "Archive targets: add 1\n\nAdd:\n- start"
    );
    assert!(git(&project, &["status", "--porcelain"]).stdout.is_empty());

    fs::write(
        project.join("main.mf"),
        "---\n# launch\nShip the project.\n- The project shipped.\n---\n> launch\n",
    )
    .unwrap();
    fs::write(project.join("notes.txt"), "second\n").unwrap();
    let second = run(&project, &["archive"]);
    assert!(second.status.success(), "{second:?}");
    let second_message = git(&project, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(second_message.stdout).unwrap().trim(),
        "Archive targets: add 1, remove 1\n\nAdd:\n- launch\n\nRemove:\n- start"
    );
    assert!(git(&project, &["status", "--porcelain"]).stdout.is_empty());

    fs::write(
        project.join("main.mf"),
        "---\n# launch\nShip the final project.\n- The project shipped.\n---\n> launch\n",
    )
    .unwrap();
    let third = run(&project, &["archive"]);
    assert!(third.status.success(), "{third:?}");
    let third_message = git(&project, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(third_message.stdout).unwrap().trim(),
        "Archive targets: update 1\n\nUpdate:\n- launch"
    );

    let empty = run(&project, &["archive"]);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("nothing to archive"));
    fs::remove_dir_all(directory).unwrap();
}

// 验证非 Markfile 变化使用稳定兜底消息，且可从项目子目录归档。
#[test]
fn archive_uses_fallback_message_for_non_target_changes() {
    let directory = temporary_directory();
    let initialized = run(&directory, &["init", "--flat"]);
    assert!(initialized.status.success(), "{initialized:?}");
    configure_git_identity(&directory);
    assert!(run(&directory, &["archive"]).status.success());

    fs::create_dir_all(directory.join("nested")).unwrap();
    fs::write(directory.join("notes.txt"), "changed\n").unwrap();
    let archived = run(&directory.join("nested"), &["archive"]);
    assert!(archived.status.success(), "{archived:?}");
    let message = git(&directory, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(message.stdout).unwrap().trim(),
        "Archive project changes"
    );
    assert!(
        git(&directory, &["status", "--porcelain"])
            .stdout
            .is_empty()
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证项目嵌在上级仓库中时，归档只提交项目目录并正确推断 Target 变化。
#[test]
fn archive_inside_a_parent_repository_commits_only_the_project_directory() {
    let directory = temporary_directory();
    let project = directory.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("main.mf"),
        "---\n# start\n- ready\n---\n> start\n",
    )
    .unwrap();
    fs::write(
        project.join("mkd.toml"),
        "version = 1\n[build]\ndefault = 'main'\n[build.main]\ntarget = 'start'\noutput = 'dist/plan.md'\n",
    )
    .unwrap();
    git(&directory, &["init", "--quiet"]);
    configure_git_identity(&directory);
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "--quiet", "-m", "base"]);
    fs::write(directory.join("outside.txt"), "untouched\n").unwrap();

    fs::write(
        project.join("main.mf"),
        "---\n# launch\nShip it.\n- shipped\n---\n> launch\n",
    )
    .unwrap();
    let archived = run(&project, &["archive"]);
    assert!(archived.status.success(), "{archived:?}");
    let message = git(&directory, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(message.stdout).unwrap().trim(),
        "Archive targets: add 1, remove 1\n\nAdd:\n- launch\n\nRemove:\n- start"
    );
    let committed = git(&directory, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !String::from_utf8(committed.stdout)
            .unwrap()
            .contains("outside.txt")
    );
    let status = git(&directory, &["status", "--porcelain"]);
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        "?? outside.txt\n"
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证配置模板与 CLI 覆盖选项、检查模式不写出制品。
#[test]
fn build_uses_configured_template_and_check_respects_no_template() {
    let directory = temporary_directory();
    fs::write(
        directory.join("main.mf"),
        "---\n# start\n- 完成。\n---\n> start\n",
    )
    .unwrap();
    fs::write(directory.join("layout.md.j2"), "{{ plan.missing }}").unwrap();
    fs::write(directory.join("mkd.toml"), "version = 1\n[build]\ndefault = 'main'\n[build.main]\ntarget = 'start'\noutput = 'dist/plan.md'\ntemplate = 'layout.md.j2'\n").unwrap();
    let checked = run(&directory, &["build", "--check"]);
    assert!(!checked.status.success());
    assert!(!directory.join("dist").exists());
    assert!(
        run(&directory, &["build", "--check", "--no-template"])
            .status
            .success()
    );
    fs::write(
        directory.join("layout.md.j2"),
        "# 自定义：{{ plan.root_target }}\n",
    )
    .unwrap();
    let built = run(&directory, &["build"]);
    assert!(built.status.success(), "{built:?}");
    assert_eq!(
        fs::read_to_string(directory.join("dist/plan.md")).unwrap(),
        "# 自定义：start"
    );
    fs::write(
        directory.join("alternate.md.j2"),
        "# 替代：{{ plan.root_target }}",
    )
    .unwrap();
    let override_template = run(
        &directory,
        &[
            "build",
            "--template",
            "alternate.md.j2",
            "-o",
            "custom/alternate.md",
        ],
    );
    assert!(override_template.status.success(), "{override_template:?}");
    assert_eq!(
        fs::read_to_string(directory.join("custom/alternate.md")).unwrap(),
        "# 替代：start"
    );
    let override_output = run(
        &directory,
        &["build", "-o", "custom/output.md", "--no-template"],
    );
    assert!(override_output.status.success(), "{override_output:?}");
    assert!(
        fs::read_to_string(directory.join("custom/output.md"))
            .unwrap()
            .contains("规格实施计划：start")
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证 target 子命令不读配置，并允许与构建子命令同名的 Target。
#[test]
fn direct_target_command_remains_independent_of_configuration() {
    let directory = temporary_directory();
    fs::write(
        directory.join("main.mf"),
        "---\n# build\n- done\n# target\n- done\n# profile\n- done\n---\n> build\n> target\n> profile\n",
    )
    .unwrap();
    fs::write(directory.join("mkd.toml"), "version = 9\n").unwrap();
    let direct = run(&directory, &["target", "build", "-o", "output/nested.md"]);
    assert!(direct.status.success(), "{direct:?}");
    assert!(directory.join("output/nested.md").exists());
    assert!(!run(&directory, &["build"]).status.success());
    for name in ["build", "target", "profile"] {
        assert!(
            run(&directory, &["target", name, "--check"])
                .status
                .success()
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

// 验证 init 补齐已有 Markfile 需显式可编译目标且不会覆盖既有文件。
#[test]
fn init_safely_completes_existing_project() {
    let directory = temporary_directory();
    let source = "---\n# publish\n- ready\n---\n> publish\n";
    fs::write(directory.join("main.mf"), source).unwrap();
    assert!(!run(&directory, &["init", "--flat"]).status.success());
    assert!(
        !run(&directory, &["init", "--flat", "--target", "missing"])
            .status
            .success()
    );
    assert!(!directory.join("mkd.toml").exists());
    let init = run(&directory, &["init", "--flat", "--target", "publish"]);
    assert!(init.status.success(), "{init:?}");
    assert!(run(&directory, &["build"]).status.success());
    assert_eq!(
        fs::read_to_string(directory.join("main.mf")).unwrap(),
        source
    );
    assert!(
        !run(&directory, &["init", "--flat", "--target", "publish"])
            .status
            .success()
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证配置的未知字段、无效默认配方和逃逸路径不会被忽略。
#[test]
fn build_rejects_invalid_configuration_and_escaping_paths() {
    let directory = temporary_directory();
    fs::write(
        directory.join("main.mf"),
        "---\n# start\n- done\n---\n> start\n",
    )
    .unwrap();
    for config in [
        "version = 1\n[build]\ndefault = 'missing'\n[build.main]\ntarget = 'start'\noutput = 'dist/plan.md'\n",
        "version = 1\n[build]\ndefault = 'main'\n[build.main]\ntarget = 'start'\noutput = '../outside.md'\n",
        "version = 1\n[build]\ndefault = 'main'\n[build.main]\ntarget = 'start'\noutput = 'dist/plan.md'\nunknown = 2\n",
    ] {
        fs::write(directory.join("mkd.toml"), config).unwrap();
        let result = run(&directory, &["build"]);
        assert!(!result.status.success(), "{result:?}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("invalid config"));
    }
    assert!(!directory.join("dist/plan.md").exists());
    fs::remove_dir_all(directory).unwrap();
}

// 验证配方即使命令行覆盖了输出，也不能覆盖源配置文件。
#[test]
fn build_output_cannot_overwrite_mkd_toml() {
    let directory = temporary_directory();
    let init = run(&directory, &["init"]);
    assert!(init.status.success(), "{init:?}");
    let config = fs::read_to_string(directory.join("mkd.toml")).unwrap();
    let result = run(&directory, &["build", "-o", "mkd.toml"]);
    assert!(!result.status.success());
    assert_eq!(
        fs::read_to_string(directory.join("mkd.toml")).unwrap(),
        config
    );
    fs::remove_dir_all(directory).unwrap();
}

// 验证检查模式以失败状态返回完整项目诊断。
#[test]
fn binary_check_reports_invalid_project_diagnostics() {
    let project = example("invalid", "10-dependency-cycle");
    let result = run(&project, &["target", "first", "--check"]);

    assert!(!result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("Error "));
    assert!(stderr.contains("[P014]"));
    assert!(stderr.contains("dependency cycle"));
    assert!(result.stdout.is_empty());
}

// 验证非法目标参数返回失败状态和明确消息。
#[test]
fn binary_rejects_invalid_target_syntax() {
    let project = example("valid", "01-single-target");
    let result = run(&project, &["target", "bad::", "--check"]);

    assert_eq!(result.status.code(), Some(1));
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("Error invalid target `bad::`"));
}

// 验证管道默认无颜色且颜色选项可以显式覆盖。
#[test]
fn binary_color_mode_controls_ansi_sequences() {
    let project = example("valid", "01-single-target");
    let plain = run(&project, &["target", "greet", "--check"]);
    let colored = run(
        &project,
        &["target", "greet", "--check", "--color", "always"],
    );
    let never = run(
        &project,
        &["target", "greet", "--check", "--color", "never"],
    );

    assert!(plain.status.success());
    assert!(colored.status.success());
    assert!(never.status.success());
    assert!(!plain.stderr.contains(&0x1b));
    assert!(colored.stderr.starts_with(b"\x1b[36m"));
    assert!(
        colored
            .stderr
            .windows(6)
            .any(|window| window == b"\x1b[1;36")
    );
    assert!(!never.stderr.contains(&0x1b));
}

// 验证 graph 默认输出模块分组的终端 DAG，并可聚焦单个 Target。
#[test]
fn binary_graph_renders_the_project_and_focused_prerequisites() {
    let project = example("valid", "08-diamond-release");
    let whole = run(&project, &["graph", "--color", "never"]);
    assert!(whole.status.success(), "{whole:?}");
    let graph = String::from_utf8(whole.stdout).unwrap();
    assert!(graph.contains("shared/schema.mf"));
    assert!(graph.contains("workflow/release.mf"));
    assert!(graph.contains("[freeze_schema]"));
    assert!(graph.contains("[ship]"));
    assert!(graph.contains('↓'));

    let focused = run(
        &project,
        &[
            "graph",
            "workflow::release::release",
            "--charset",
            "ascii",
            "--color",
            "never",
        ],
    );
    assert!(focused.status.success(), "{focused:?}");
    let graph = String::from_utf8(focused.stdout).unwrap();
    assert!(graph.contains("[release]"));
    assert!(graph.contains("[approve]"));
    assert!(!graph.contains("[ship]"));
    assert!(!graph.contains('↓'));
}

// 验证 DOT 图写入文件并携带布局、模块与颜色信息。
#[test]
fn binary_graph_exports_colored_dot_without_ansi_sequences() {
    let project = example("valid", "08-diamond-release");
    let directory = temporary_directory();
    let output = directory.join("project.dot");
    let output_arg = output.to_string_lossy().into_owned();
    let result = run(
        &project,
        &[
            "graph",
            "--format",
            "dot",
            "--direction",
            "left-right",
            "--color",
            "always",
            "-o",
            &output_arg,
        ],
    );

    assert!(result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty());
    let dot = fs::read_to_string(&output).unwrap();
    assert!(dot.starts_with("digraph makedown {\n"));
    assert!(dot.contains("rankdir=LR"));
    assert!(dot.contains("subgraph cluster_"));
    assert!(dot.contains("color=\"#"));
    assert!(!dot.contains('\u{1b}'));
    fs::remove_dir_all(directory).unwrap();
}

// 验证内置图片导出生成独立真彩色 SVG，并且无需 Graphviz。
#[test]
fn binary_graph_exports_a_standalone_svg_image() {
    let project = example("valid", "08-diamond-release");
    let directory = temporary_directory();
    let output = directory.join("project.svg");
    let output_arg = output.to_string_lossy().into_owned();
    let result = run(
        &project,
        &[
            "graph",
            "--direction",
            "left-right",
            "--image",
            &output_arg,
            "--color",
            "never",
        ],
    );

    assert!(result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty());
    let svg = fs::read_to_string(&output).unwrap();
    assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
    assert!(svg.contains("viewBox="));
    assert!(svg.contains("fill=\"#"));
    assert!(svg.contains("[release]"));
    assert!(!svg.contains('\u{1b}'));
    fs::remove_dir_all(directory).unwrap();
}

// 验证省略图片文件名时在项目根目录写入 graph.svg。
#[test]
fn binary_graph_image_uses_a_project_relative_default() {
    let project = temporary_directory();
    fs::write(
        project.join("main.mf"),
        "---\n# start\n- ready\n---\n> start\n",
    )
    .unwrap();

    let result = run(&project, &["graph", "--image"]);
    assert!(result.status.success(), "{result:?}");
    assert!(project.join("graph.svg").is_file());
    assert!(result.stdout.is_empty());
    fs::remove_dir_all(project).unwrap();
}

// 验证交互模式拒绝管道，避免在非 TTY 上破坏终端状态。
#[test]
fn binary_graph_interactive_requires_a_terminal() {
    let project = example("valid", "01-single-target");
    let result = run(&project, &["graph", "--interactive"]);

    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("interactive graph mode requires both stdin and stdout")
    );
}

// 验证无效项目和不存在的聚焦目标拒绝生成图，并保留结构化分析诊断。
#[test]
fn binary_graph_rejects_invalid_projects_with_diagnostics() {
    let project = example("invalid", "10-dependency-cycle");
    let result = run(&project, &["graph", "--color", "never"]);

    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("[P014]"));
    assert!(stderr.contains("dependency cycle"));

    let project = example("valid", "01-single-target");
    let result = run(&project, &["graph", "missing", "--color", "never"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("[P013]"));
}

// 验证 graph 文件输出不能覆盖任何项目输入。
#[test]
fn binary_graph_refuses_to_overwrite_project_inputs() {
    let project = temporary_directory();
    let source = "---\n# start\n- ready\n---\n> start\n";
    fs::write(project.join("main.mf"), source).unwrap();
    fs::write(project.join("mkd.toml"), "version = 1\n").unwrap();

    for output in ["main.mf", "mkd.toml"] {
        let result = run(&project, &["graph", "-o", output]);
        assert_eq!(result.status.code(), Some(1));
        let result = run(&project, &["graph", "--image", output]);
        assert_eq!(result.status.code(), Some(1));
    }
    assert_eq!(fs::read_to_string(project.join("main.mf")).unwrap(), source);
    assert_eq!(
        fs::read_to_string(project.join("mkd.toml")).unwrap(),
        "version = 1\n"
    );
    fs::remove_dir_all(project).unwrap();
}

// 验证单文件 lint 警告不阻塞成功，全量 lint 检查未引用文件。
#[test]
fn binary_lint_scopes_and_exit_codes() {
    let project = temporary_directory();
    fs::write(
        project.join("main.mf"),
        "---\n# start\n- ready\n---\n> start\n",
    )
    .unwrap();
    fs::write(project.join("extra.mf"), "---\n# unused\n---\n> unused\n").unwrap();
    let single = run(&project, &["lint", "extra.mf", "--color", "never"]);
    assert!(single.status.success(), "{single:?}");
    assert!(String::from_utf8_lossy(&single.stderr).contains("[L001]"));

    fs::write(
        project.join("extra.mf"),
        "---\n# unused\n> missing\n---\n> unused\n",
    )
    .unwrap();
    let all = run(&project, &["lint", "--all", "--color", "never"]);
    assert_eq!(all.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&all.stderr).contains("[P005]"));
    fs::remove_dir_all(project).unwrap();
}

// 验证格式化写回、检查模式、幂等与错误文件保护。
#[test]
fn binary_fmt_checks_and_writes_without_changing_invalid_files() {
    let project = temporary_directory();
    let file = project.join("draft.mf");
    fs::write(&file, "---\n##  task  \ntext  \n-  spec  \n---\n>  task\n").unwrap();
    let check = run(&project, &["fmt", "draft.mf", "--check"]);
    assert_eq!(check.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&check.stderr).contains("needs formatting"));
    let format = run(&project, &["fmt", "draft.mf"]);
    assert!(format.status.success(), "{format:?}");
    let once = fs::read_to_string(&file).unwrap();
    assert!(once.contains("# task\ntext\n- spec"));
    assert!(
        run(&project, &["fmt", "draft.mf", "--check"])
            .status
            .success()
    );
    assert!(run(&project, &["fmt", "draft.mf"]).status.success());
    assert_eq!(once, fs::read_to_string(&file).unwrap());

    fs::write(&file, "invalid\n").unwrap();
    let invalid = run(&project, &["fmt", "draft.mf"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&file).unwrap(), "invalid\n");
    fs::remove_dir_all(project).unwrap();
}

// 验证编译模式缺少输出参数时由 clap 返回用法错误。
#[test]
fn binary_requires_output_outside_check_mode() {
    let project = example("valid", "01-single-target");
    let result = run(&project, &["target", "greet"]);

    assert_eq!(result.status.code(), Some(2));
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("--output <FILE>"));
}

// 验证裸 Target 与旧 profile 子命令不再被接受。
#[test]
fn binary_rejects_legacy_bare_target_and_profile_command() {
    let project = example("valid", "01-single-target");
    for arguments in [&["greet", "--check"][..], &["profile"][..]] {
        let result = run(&project, arguments);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
    }
}

// 验证默认 init 创建 .mkd/ 隐藏布局，且 mkd.toml 与产物在配置目录。
#[test]
fn init_defaults_to_hidden_layout_with_external_config() {
    let directory = temporary_directory();
    let initialized = run(&directory, &["init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    assert!(directory.join(".mkd/main.mf").is_file());
    assert!(directory.join("mkd.toml").is_file());
    assert!(!directory.join("main.mf").exists());

    fs::create_dir_all(directory.join("src/deep")).unwrap();
    let built = run(&directory.join("src/deep"), &["build"]);
    assert!(built.status.success(), "{built:?}");
    assert!(
        fs::read_to_string(directory.join("dist/plan.md"))
            .unwrap()
            .contains("规格实施计划：start")
    );

    // 配置目录里的产物不会与 .mkd 内的 Markfile 冲突。
    fs::write(
        directory.join(".mkd/helper.mf"),
        "---\n# assist\n- 辅助完成。\n---\n> assist\n",
    )
    .unwrap();
    fs::write(
        directory.join("src/stray.mf"),
        "---\n# stray\n- 不应被发现。\n---\n> stray\n",
    )
    .unwrap();
    let graph = run(&directory.join("src"), &["graph"]);
    assert!(graph.status.success(), "{graph:?}");
    let graph_text = String::from_utf8(graph.stdout).unwrap();
    assert!(graph_text.contains("assist"));
    assert!(!graph_text.contains("stray"));
    fs::remove_dir_all(directory).unwrap();
}

// 验证隐藏布局的归档只提交 Markfile 相关范围。
#[test]
fn archive_in_hidden_layout_commits_only_markdown_scope() {
    let directory = temporary_directory();
    let initialized = run(&directory, &["init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    configure_git_identity(&directory);

    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(directory.join("src/main.rs"), "fn main() {}\n").unwrap();
    let first = run(&directory, &["archive"]);
    assert!(first.status.success(), "{first:?}");
    let message = git(&directory, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(message.stdout).unwrap().trim(),
        "Archive targets: add 1\n\nAdd:\n- start"
    );
    assert_eq!(
        String::from_utf8(git(&directory, &["status", "--porcelain"]).stdout)
            .unwrap()
            .trim(),
        "?? src/"
    );

    fs::write(
        directory.join(".mkd/main.mf"),
        "---\n# launch\nShip it.\n- shipped\n---\n> launch\n",
    )
    .unwrap();
    let second = run(&directory, &["archive"]);
    assert!(second.status.success(), "{second:?}");
    let message = git(&directory, &["log", "-1", "--pretty=%B"]);
    assert_eq!(
        String::from_utf8(message.stdout).unwrap().trim(),
        "Archive targets: add 1, remove 1\n\nAdd:\n- launch\n\nRemove:\n- start"
    );
    fs::write(directory.join("mkd.lock"), "version = 1\njar = []\n").unwrap();
    assert!(run(&directory, &["archive"]).status.success());
    fs::remove_file(directory.join("mkd.lock")).unwrap();
    let deleted = run(&directory, &["archive"]);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(!String::from_utf8_lossy(&git(&directory, &["ls-files"]).stdout).contains("mkd.lock"));
    fs::remove_dir_all(directory).unwrap();
}

// 验证 --lib 创建以 lib.mf 为根的平铺包项目并可以构建公开目标。
#[test]
fn init_lib_creates_flat_package_project() {
    let directory = temporary_directory();
    let initialized = run(&directory, &["init", "--lib"]);
    assert!(initialized.status.success(), "{initialized:?}");
    assert!(directory.join("lib.mf").is_file());
    assert!(directory.join("mkd.toml").is_file());
    assert!(!directory.join("main.mf").exists());
    assert!(!directory.join(".mkd").exists());

    let lib = fs::read_to_string(directory.join("lib.mf")).unwrap();
    assert!(lib.contains("---\n> start\n"));

    let built = run(&directory, &["build"]);
    assert!(built.status.success(), "{built:?}");
    assert!(
        fs::read_to_string(directory.join("dist/plan.md"))
            .unwrap()
            .contains("规格实施计划：start")
    );

    // lib.mf 作为根不能再被当作 lib 模块导入。
    fs::write(
        directory.join("lib.mf"),
        "> crate::lib::start\n---\n# start\n- ready\n---\n> start\n",
    )
    .unwrap();
    let broken = run(&directory, &["build"]);
    assert!(!broken.status.success(), "{broken:?}");
    fs::remove_dir_all(directory).unwrap();
}

// 从包项目打包后，在隐藏布局应用中导入并编译公开目标。
#[test]
fn pack_then_import_into_hidden_application() {
    let root = temporary_directory();
    let library = root.join("library");
    let app = root.join("app");
    assert!(run(&root, &["init", "--lib", "library"]).status.success());
    fs::write(
        library.join("lib.mf"),
        "> crate::internal::ready\n---\n# api\n> ready\nPublic specification.\n---\n> api\n",
    )
    .unwrap();
    fs::write(
        library.join("internal.mf"),
        "---\n# ready\nInternal specification.\n---\n> ready\n",
    )
    .unwrap();
    let packed = run(&library, &["pack"]);
    assert!(packed.status.success(), "{packed:?}");
    assert!(run(&root, &["init", "app"]).status.success());
    fs::create_dir_all(app.join(".mkd/vendor")).unwrap();
    fs::copy(
        library.join("dist/library.mjar"),
        app.join(".mkd/vendor/library.mjar"),
    )
    .unwrap();
    fs::write(app.join(".mkd/main.mf"),
        "> crate::vendor::*library::api as imported\n---\n# start\n> imported\nApplication.\n---\n> start\n").unwrap();
    let built = run(&app, &["build"]);
    assert!(built.status.success(), "{built:?}");
    let plan = fs::read_to_string(app.join("dist/plan.md")).unwrap();
    assert!(plan.contains("Internal specification."));
    assert!(plan.contains("Public specification."));
    fs::remove_dir_all(root).unwrap();
}

// --jar 无需本地项目，即可对包渲染终端、DOT 与 SVG 图。
#[test]
fn graph_jar_renders_standalone_package() {
    let root = temporary_directory();
    let library = root.join("library");
    assert!(run(&root, &["init", "--lib", "library"]).status.success());
    let packed = run(&library, &["pack"]);
    assert!(packed.status.success(), "{packed:?}");
    let jar = library.join("dist/library.mjar");
    let jar_arg = jar.to_str().unwrap();
    let terminal = run(&root, &["graph", "--jar", jar_arg]);
    assert!(terminal.status.success(), "{terminal:?}");
    assert!(String::from_utf8_lossy(&terminal.stdout).contains("start"));
    let dot = run(&root, &["graph", "--jar", jar_arg, "--format", "dot"]);
    assert!(dot.status.success(), "{dot:?}");
    assert!(String::from_utf8_lossy(&dot.stdout).contains("digraph"));
    let svg = run(&root, &["graph", "--jar", jar_arg, "--image", "graph.svg"]);
    assert!(svg.status.success(), "{svg:?}");
    assert!(
        fs::read_to_string(library.join("dist/graph.svg"))
            .unwrap()
            .contains("<svg")
    );
    let blocked = run(&root, &["graph", "--jar", jar_arg, "-o", jar_arg]);
    assert!(!blocked.status.success());
    assert!(jar.is_file());
    let corrupt = root.join("broken.mjar");
    fs::write(&corrupt, "not a zip").unwrap();
    let invalid = run(&root, &["graph", "--jar", corrupt.to_str().unwrap()]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("cannot analyze package"));
    fs::remove_dir_all(root).unwrap();
}

// 锁定包后哈希不符阻断构建，重新 lock 后可继续构建。
#[test]
fn lock_detects_changed_package_and_can_refresh() {
    let root = temporary_directory();
    let library = root.join("library");
    let app = root.join("app");
    assert!(run(&root, &["init", "--lib", "library"]).status.success());
    fs::write(library.join("lib.mf"), "---\n# api\nInitial.\n---\n> api\n").unwrap();
    assert!(run(&library, &["pack"]).status.success());
    assert!(run(&root, &["init", "app"]).status.success());
    fs::create_dir_all(app.join(".mkd/vendor")).unwrap();
    let installed = app.join(".mkd/vendor/library.mjar");
    fs::copy(library.join("dist/library.mjar"), &installed).unwrap();
    fs::write(
        app.join(".mkd/main.mf"),
        "> crate::vendor::*library::api as imported\n---\n# start\n> imported\n---\n> start\n",
    )
    .unwrap();
    assert!(run(&app, &["build"]).status.success());
    let locked = run(&app, &["lock"]);
    assert!(locked.status.success(), "{locked:?}");
    let lock = fs::read_to_string(app.join("mkd.lock")).unwrap();
    assert!(lock.contains(".mkd/vendor/library.mjar"));
    assert!(lock.contains("sha256:"));
    fs::write(library.join("lib.mf"), "---\n# api\nUpdated.\n---\n> api\n").unwrap();
    assert!(run(&library, &["pack"]).status.success());
    fs::copy(library.join("dist/library.mjar"), &installed).unwrap();
    let mismatch = run(&app, &["build"]);
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("hash mismatch"));
    assert!(run(&app, &["lock"]).status.success());
    assert!(run(&app, &["build"]).status.success());
    assert!(
        fs::read_to_string(app.join("dist/plan.md"))
            .unwrap()
            .contains("Updated.")
    );
    fs::remove_dir_all(root).unwrap();
}

// init 不创建会被现有入口遮蔽的第二套布局。
#[test]
fn init_rejects_competing_roots() {
    let root = temporary_directory();
    fs::write(root.join("main.mf"), "---\n# start\n---\n> start\n").unwrap();
    let conflict = run(&root, &["init"]);
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("competing project root"));
    assert!(!root.join(".mkd").exists());
    assert!(!run(&root, &["init", "--lib"]).status.success());
    fs::remove_dir_all(root).unwrap();
}

// 验证同一目录内四种根的优先级与向上搜索。
#[test]
fn root_discovery_prefers_documented_order() {
    let directory = temporary_directory();
    fs::create_dir_all(directory.join("work")).unwrap();
    fs::create_dir_all(directory.join(".mkd")).unwrap();
    fs::write(
        directory.join(".mkd/main.mf"),
        "---\n# hidden\n- hidden\n---\n> hidden\n",
    )
    .unwrap();
    fs::write(directory.join("lib.mf"), "---\n# lib\n- lib\n---\n> lib\n").unwrap();
    let result = run(&directory.join("work"), &["target", "lib", "--check"]);
    assert!(result.status.success(), "{result:?}");

    fs::write(
        directory.join("main.mf"),
        "---\n# flat\n- flat\n---\n> flat\n",
    )
    .unwrap();
    let result = run(&directory.join("work"), &["target", "flat", "--check"]);
    assert!(result.status.success(), "{result:?}");
    let result = run(&directory.join("work"), &["target", "lib", "--check"]);
    assert!(!result.status.success(), "{result:?}");
    fs::remove_dir_all(directory).unwrap();
}
