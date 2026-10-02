# 从 Markfile 生成实施计划

Markfile 是扩展名为 `.mf` 的工作说明文件。文件里的一个目标（Target）描述一项要完成的工作，可以写明它依赖哪些工作，以及怎样判断工作已经完成。`mkd` 会读取目标及其依赖，生成按阶段排列的 Markdown 实施计划：先安排基础工作，再安排依赖这些工作的后续任务。同一阶段的目标之间没有依赖关系，可以由实施者并行安排。

第一次使用时，可以让 `mkd` 帮你创建一个项目。在准备存放项目的目录里运行：

```sh
mkd init
mkd build
```

在当前目录创建应用型项目时，第一条命令创建 `.mkd/main.mf` 和外层的 `mkd.toml`，并执行 `git init` 初始化或复用 Git 仓库。`.mkd/` 收纳全部 Markfile 模块，不占用编程项目根目录；`mkd.toml` 保存构建设置。第二条命令生成外层 `dist/plan.md`。如果想让工具新建一个项目目录，可以改用以下步骤：

```sh
mkd init my-project
cd my-project
mkd build
```

## 目标和构建配方

一个项目通常有多个目标。例如 `main.mf` 中可以有一个名为 `start` 的目标。`mkd target start -o plan.md` 会直接生成这个目标的实施计划，并写入 `plan.md`。这里的 `-o` 指定输出文件。检查内容是否能成功分析、暂时不生成文件时，可以运行 `mkd target start --check`。

如果经常要生成同一份计划，可以把目标、输出文件以及可选的排版模板写进 `mkd.toml`。这组可重复使用的设置叫构建配方（build profile）。例如配方 `main` 可以指定目标 `start`，将计划写到 `dist/plan.md`。此时运行 `mkd build` 就会使用默认配方；运行 `mkd build main` 则明确指定名为 `main` 的配方。`mkd target` 按命令行给出的目标编译，不读取 `mkd.toml`，适合临时指定输出；`mkd build` 读取配方，适合重复生成固定文档。

目标名来自 `.mf` 文件，配方名来自 `mkd.toml`。两者可以相同：要编译名为 `build` 的目标，写 `mkd target build -o plan.md`；要执行名为 `build` 的配方，写 `mkd build build`。

## 项目根目录和初始化

`mkd` 从当前目录向上寻找最近的项目入口；同一目录按 `main.mf`、`lib.mf`、`.mkd/main.mf`、`.mkd/lib.mf` 顺序选择。平铺项目的模块与配置都在入口文件旁；隐藏项目的模块位于 `.mkd/`，`mkd.toml`、`mkd.lock` 和默认产物位于其外层。`crate::` 始终锚定模块目录。可用 `--root FILE` 显式指定入口，隐藏布局的配置仍在外层。

`mkd init` 默认创建 `.mkd/main.mf`；`mkd init --flat` 创建平铺的 `main.mf`（适合纯规划项目）；`mkd init --lib` 创建平铺的 `lib.mf`（可打包的库项目）。三者都创建 `mkd.toml`、名为 `start` 的公开目标和默认配方。可指定目录与 `--target launch` 自定义初始目标。

若入口文件已存在但没有 `mkd.toml`，用对应布局的 init 参数和 `--target launch` 创建配置，例如已有平铺 `main.mf` 时运行 `mkd init --flat --target launch`；已有 `.mkd/main.mf` 时运行 `mkd init --target launch`。工具会验证目标、保留原文件，不覆盖已有配置。

## 编写 mkd.toml

假设项目中已有 `platform` 和 `content/guide` 等模块文件，并准备了 `templates/release.md.j2` 模板，就可以使用下面这份配置。它提供两份常用计划：`release` 是默认配方，`guide` 是另一份可单独构建的配方。只想先体验完整流程时，使用前面的 `mkd init` 即可，它会创建可以直接运行的简单配置。

```toml
version = 1

[build]
default = "release"

[build.release]
target = "platform::launch"
template = "templates/release.md.j2"
output = "dist/release.md"

[build.guide]
target = "content::guide::publish"
output = "dist/guide.md"
```

`version = 1` 声明配置格式版本。`[build]` 中的 `default` 指定不带配方名运行 `mkd build` 时使用哪一个配方。`[build.release]` 和 `[build.guide]` 分别保存两个配方，方括号中的后半部分就是配方名。每个配方需要一个 `target` 和一个 `output`；`template` 可以省略，省略后会使用内置的 Markdown 排版。配置了配方时，`default` 必须指向其中一个。`default` 用作设置名称，不能再作为配方名。

`platform::launch` 表示 `platform` 模块文件中的 `launch` 目标；`content::guide::publish` 表示更深一层模块中的 `publish` 目标。模块可以理解为项目中的另一份 `.mf` 文件，用来分组保存目标。入口文件 `main.mf` 中的目标只需写本地名称，例如 `start`。文件组织和跨文件引用的实例可以从 [`examples/`](../examples/README.md) 开始查看。

上述 `release` 配方会把文件写到项目目录内的 `dist/release.md`，并用 `templates/release.md.j2` 排版；`guide` 配方会使用内置排版，写到 `dist/guide.md`。这些文件路径以 `mkd.toml` 所在目录为起点（隐藏布局即 `.mkd/` 的外层）。配置中的路径只能指向项目内部，不能是绝对路径，也不能通过上一级路径或符号链接指向项目外部。`mkd` 会检查配置格式、目标名称和路径；只写 `version = 1` 也能得到有效的空配置，但要执行 `mkd build`，还需要配方。

## 分发 .mjar 包与哈希锁

包项目使用 `lib.mf` 作为根，其**公开区**（第二个 `---` 后的 `> 目标`）是包的入口接口：

```sh
mkd init --lib shared
cd shared
# 编辑 lib.mf 和其他 .mf 模块
mkd pack                        # dist/shared.mjar
mkd graph --jar dist/shared.mjar # 不必身处项目中也能查看包图
```

在应用项目中将包复制到 `.mkd/vendor/shared.mjar`（平铺项目为 `vendor/shared.mjar`），在 `main.mf` 导入：

```mf
> crate::vendor::*shared::start as shared_start
---
# start
> shared_start
完成应用目标。
---
> start
```

`*shared` 明确选择 `shared.mjar`，普通 `shared` 仍选择 `shared.mf`；同名包与模块不能占用同一命名空间。包内 `crate::` 指向包自身，`self`/`super` 保持相对含义。只能从外部导入包的 `lib.mf` 公开目标；包保留依赖模块和规格原文，分发等同开源，也不能阻止规格文本中的提示词注入。`mkd pack -o FILE` 可指定输出；打包从公开目标出发按模块剪枝，并将已导入包**展开**，产物内不嵌套 `.mjar`，写出后自校验。

运行 `mkd lock` 遍历项目模块中的包导入，生成配置目录下的 `mkd.lock`（路径与 SHA-256 哈希）。之后分析/构建时，**已记录**的包哈希不符会报错；没有锁文件或新增包尚未记录时仍可构建。更新包后重新运行 `mkd lock`。由于打包会展开依赖包，锁记录项目直接引用的 `.mjar` 文件；锁定的是字节内容，不是版本号。

## 查看 Target 依赖图

`mkd graph` 会扫描入口文件所在项目的全部 Markfile，在标准输出中绘制 Target DAG。Markfile 以分组框显示，依赖箭头从前置 Target 指向依赖它的 Target。模块拥有各自的主题色；同一模块内的 Target 采用相近色，边取起点与终点颜色各 50% 的中间色。

```sh
mkd graph
mkd graph platform::launch
mkd graph --direction left-right
mkd graph --charset ascii --color never
mkd graph --interactive
mkd graph --image dist/project.svg
mkd graph --format dot -o dist/project.dot
```

非交互模式不指定 Target 时显示整个项目，包括没有从入口 Target 可达的模块；指定 Target 时只显示该 Target 和递归前置依赖。`--direction` 接受 `top-down`、`bottom-up`、`left-right`、`right-left`，默认 `top-down`。终端格式默认使用 Unicode 线条；`--charset ascii` 改为纯 ASCII。

`--interactive` 以模块为页面，在随终端尺寸变化的固定边框内显示当前模块的全部 Target 和这些 Target 直接依赖的外部 Target，不递归展开外部节点。默认从入口模块开始；指定 Target 时从它所在的模块开始并选中它，该模块其他 Target 也会显示。交互模式始终分析全部模块，Target 参数只决定初始焦点，不限制可切换的模块范围。

- `h/j/k/l` 或方向键：选择图中对应方向的 Target，节点高亮，视框以该节点居中为基准。`Tab` / `Shift-Tab` 依次选择节点。
- `Shift+H/J/K/L`（大写 `H/J/K/L`）或 Shift+方向键：每次按一格调整相对观察位置，H 左、J 下、K 上、L 右，图向相反方向移动。偏移在更换焦点和模块时保留，不归零；顶部显示偏移。横向范围最多 ±32 列，纵向最多 ±12 行，同时不超过视框内部对应尺寸的四分之一，并按节点尺寸收紧以尽量保留整个焦点。终端缩小时超出新范围的偏移会被截断。靠近图边缘可以留白，不限制为只显示图内部。
- 聚焦 Target 时自动显示缩略浮窗，只包含绝对逻辑路径名和 description，共用语法高亮与动态避让布局。缩略窗最多 48 列、10 行，长内容以 `…` 标记截断；空 description 只显示路径名。缩略窗不拦截导航键、不包含规格和依赖，也不进入导出图片。
- `Enter`：选中本模块 Target 时，缩略窗消失并展开详情浮窗；收起详情后缩略窗立即恢复。选中外部 Target 时切换到其所属模块，焦点保留在同一 Target，新页面立即恢复其缩略窗，再按一次才展开详情。
- 浮窗内 `j/k` 或上下键滚动描述、规格和依赖；`Enter` / `Esc` 收起。浮窗最多 72 列、24 行，内容按显示宽度换行；按标题、依赖、规格、代码和强调高亮，遵循 `--color`。优先放在焦点的左右或上下可用区域，空间不足时缩小；无法避让时选择尽量减少遮挡的角落。Shift 导航在浮窗打开时仍可使用，位置和滚动范围同步更新。
- 浮窗关闭时 `Esc` / `Backspace` 返回上一个模块及焦点；`[` / `]` 切换上一个/下一个模块，可访问孤立模块。
- `e` 导出当前模块局部图，不包含焦点高亮或浮窗；`Ctrl-C` 退出。

例如 A 模块的 Target 依赖 `B::four`，`B::four` 又依赖 `C::five`：A 页只显示 `B::four`，选中它并按 `Enter` 进入 B 页后才显示 `C::five`。默认导出路径是配置目录下的 `graph.svg`，`--interactive --image dist/project.svg` 可以改变热键的导出路径。交互模式要求标准输入和标准输出都连接终端。

`--image [FILE]` 用内置导出器生成带配色的独立 SVG 图片，不需要 Graphviz；省略文件名时使用 `graph.svg`，相对路径以配置目录为起点（`--jar` 时以包所在目录为起点）。SVG 按终端格为每个字符指定横坐标，避免字体自然字距导致线条与标签错位；中文双宽字符预留两格。组合符等零宽字符保留原字符，并补足内部画布占位格以保持后续列位置。`--format dot` 则输出 Graphviz DOT，可交给 `dot -Tpng` 等命令继续处理。`-o/--output` 写入终端文本或 DOT 文件，否则写入标准输出；自动颜色仅在直接输出到终端时启用。图命令要求项目分析成功，语法、引用或依赖环错误会阻止绘图并返回诊断。

## 归档项目变化

运行 `mkd archive` 时，平铺项目暂存整个项目目录；隐藏布局仅暂存 `.mkd/`、`mkd.toml` 和已有的 `mkd.lock`，不会提交宿主项目的源码，并创建一次 Git 提交。平铺项目等价于在配置目录执行 `git add --all -- .`；隐藏布局按上述文件范围提交。从子目录运行时，`mkd` 同样向上查找最近的入口。

提交消息由概括数量的主题行和分组清单组成，Target 按名称排序：

```text
Archive targets: add 2, update 1, remove 1

Add:
- launch
- catalog

Update:
- quality::test

Remove:
- draft
```

主题行按动作汇总数量，正文用 `Add:`、`Update:`、`Remove:` 分组列出 Target 的限定名（如 `quality::test`）。`add`、`update`、`remove` 分别表示 Target 新增、语义内容变化和删除。语义内容包括描述、依赖、规格、导入及公开状态。若暂存变化不涉及可识别的 Target（例如只改了源代码或 Markfile 当前无法解析），消息为单行 `Archive project changes`。没有变化时命令失败并提示 `nothing to archive`；Git 不可用、提交身份未配置或提交钩子拒绝提交时，命令保留 Git 的错误说明并返回失败。提交身份仍由 Git 配置管理，例如：

```sh
git config user.name "Your Name"
git config user.email "you@example.com"
```

## 常用命令

在上面的配置和项目结构下，可以运行：

```sh
mkd build                         # 生成默认的 release 计划
mkd build guide                   # 生成 guide 计划
mkd build --check                 # 检查目标及配方中指定的模板，不生成文件
mkd build guide -o guide-copy.md  # 临时改用另一个输出文件
mkd build --no-template           # 使用内置 Markdown 排版
mkd target platform::launch -o launch.md  # 不用配置，直接生成计划
mkd target platform::launch --check       # 检查目标，不生成文件
mkd graph platform::launch                 # 在终端查看目标及其前置依赖
mkd graph --interactive                    # 按模块浏览 Target 和详情浮窗
mkd graph --image dist/project.svg         # 直接导出全项目 SVG 图片
mkd graph --format dot -o dist/project.dot # 导出全项目 Graphviz DOT
mkd graph --jar dist/shared.mjar          # 查看独立包图
mkd pack                                  # 打包 lib.mf 项目
mkd lock                                  # 冻结被引用包的 SHA-256
mkd archive                                # 暂存全部项目变化并创建自动命名的提交
```

`mkd build --template alternate.md.j2` 可在本次构建中改用另一个排版模板；`mkd target start --template alternate.md.j2 -o plan.md` 也可以直接指定模板。模板的写法见[自定义 Markdown 模板](templates.md)。命令行中的 `-o` 和 `--template` 接受绝对路径；写相对路径时，以运行命令时所在的目录为起点。配置文件中的 `output` 和 `template` 则只能写项目内部的相对路径。`--check` 只检查，不创建输出文件或目录；当前配方使用了模板时，还会尝试用实际计划渲染它。

生成计划时，工具会为输出文件创建所需的父目录。为了保护输入内容，输出不能覆盖入口 `.mf`、构建时读取的其他 `.mf`、正在使用的模板或 `mkd.toml`。检查单份文件可以使用 `mkd lint .mkd/main.mf`（平铺布局直接使用 `main.mf`），检查整个项目用 `mkd lint --all`；格式化文件使用 `mkd fmt .mkd/main.mf`。这些维护命令不需要构建配方。
