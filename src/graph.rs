use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use unicode_width::UnicodeWidthChar;

use ascii_dag::render::engine::{EdgeStyle, EdgeStyleCtx, SubgraphStyle, SubgraphStyleCtx};
use ascii_dag::{
    CellColor, Charset, ColorMode, CustomNode, Direction, Graph, RenderOptions, SubgraphBorder,
};

use crate::core::{ModulePath, Project, TargetId};

const ID_STYLE_BITS: usize = 16;
const MODULE_BITS: usize = 12;
const MODULE_MASK: usize = (1 << MODULE_BITS) - 1;
const SHADE_MASK: usize = (1 << (ID_STYLE_BITS - MODULE_BITS)) - 1;
const MAX_TARGETS: usize = usize::MAX >> ID_STYLE_BITS;

/// 选择项目图的文本流向。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphDirection {
    #[default]
    TopDown,
    BottomUp,
    LeftRight,
    RightLeft,
}

/// 选择项目图的终端字符集。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphCharset {
    #[default]
    Unicode,
    Ascii,
}

/// 描述项目图无法构造的原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphRenderError {
    TargetNotFound(TargetId),
    TooManyTargets(usize),
    Layout(String),
}

impl fmt::Display for GraphRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetNotFound(target) => {
                write!(formatter, "graph target `{target}` does not exist")
            }
            Self::TooManyTargets(count) => write!(
                formatter,
                "project has {count} targets; terminal graph supports at most {MAX_TARGETS}"
            ),
            Self::Layout(message) => write!(formatter, "cannot lay out project graph: {message}"),
        }
    }
}

impl Error for GraphRenderError {}

/// 将已验证项目渲染为终端 DAG、SVG 或 Graphviz DOT。
#[derive(Clone, Copy, Debug)]
pub struct ProjectGraphRenderer {
    direction: GraphDirection,
    charset: GraphCharset,
    color: bool,
}

impl Default for ProjectGraphRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectGraphRenderer {
    /// 创建采用自上而下 Unicode 布局的渲染器。
    pub fn new() -> Self {
        Self {
            direction: GraphDirection::TopDown,
            charset: GraphCharset::Unicode,
            color: false,
        }
    }

    /// 设置图的流向。
    pub fn with_direction(mut self, direction: GraphDirection) -> Self {
        self.direction = direction;
        self
    }

    /// 设置终端字符集。
    pub fn with_charset(mut self, charset: GraphCharset) -> Self {
        self.charset = charset;
        self
    }

    /// 控制终端输出是否包含 ANSI 真彩色。
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// 渲染整个项目或指定目标的前置依赖子图。
    pub fn render_terminal(
        &self,
        project: &Project,
        focus: Option<&TargetId>,
    ) -> Result<String, GraphRenderError> {
        let model = GraphModel::new(project, focus)?;
        Ok(self.render_model(&model)?.0)
    }

    /// 仅显示当前模块全部目标及其直接外部依赖，保留节点的布局坐标。
    pub fn render_module(
        &self,
        project: &Project,
        namespace: &ModulePath,
    ) -> Result<ModuleGraph, GraphRenderError> {
        let model = GraphModel::for_module(project, namespace)?;
        let (text, targets) = Self {
            color: true,
            ..*self
        }
        .render_model(&model)?;
        Ok(ModuleGraph {
            graph: StyledGraph::parse(&text),
            targets,
        })
    }

    fn render_model(
        &self,
        model: &GraphModel<'_>,
    ) -> Result<(String, Vec<GraphTarget>), GraphRenderError> {
        let mut graph = Graph::new();
        graph.set_direction(self.direction.into_ascii_dag());

        for node in &model.nodes {
            graph.add_node(
                node.render_id,
                CustomNode {
                    label: node.label,
                    width: node
                        .label
                        .chars()
                        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
                        .sum::<usize>()
                        + 2,
                    height: 1,
                    painter: Some(paint_target),
                    payload: "",
                },
            );
        }
        for edge in &model.edges {
            graph.add_edge(edge.from, edge.to, None);
        }
        // ascii-dag 的画布一格存一个 char；为双宽字符预留后续格。
        let module_labels = model
            .modules
            .iter()
            .map(|module| canvas_label(&module.label))
            .collect::<Vec<_>>();
        for (module, label) in model.modules.iter().zip(&module_labels) {
            let subgraph = graph.add_subgraph(label);
            graph
                .put_nodes(&module.nodes)
                .inside(subgraph)
                .map_err(|error| GraphRenderError::Layout(error.to_string()))?;
        }

        let mut options = RenderOptions::plain();
        options.emit.charset = match self.charset {
            GraphCharset::Unicode => Charset::Unicode,
            GraphCharset::Ascii => Charset::Ascii,
        };
        options.emit.color_mode = if self.color {
            ColorMode::TrueColor
        } else {
            ColorMode::None
        };
        options.plan.edge_style_fn = themed_edge_style;
        options.plan.subgraph_style_fn = themed_subgraph_style;
        let layout = graph.compute_layout();
        let targets = model
            .nodes
            .iter()
            .filter_map(|node| {
                let position = layout.node_by_id(node.render_id)?;
                Some(GraphTarget {
                    id: node.id.clone(),
                    x: position.x,
                    y: position.y,
                    width: position.width,
                    height: position.height,
                })
            })
            .collect();
        Ok((
            normalize_canvas_text(&layout.render_string(&options)),
            targets,
        ))
    }

    /// 将终端布局导出为可独立查看的真彩色 SVG 图片。
    pub fn render_svg(
        &self,
        project: &Project,
        focus: Option<&TargetId>,
    ) -> Result<String, GraphRenderError> {
        let colored = Self {
            color: true,
            ..*self
        }
        .render_terminal(project, focus)?;
        Ok(StyledGraph::parse(&colored).to_svg())
    }

    /// 返回供交互视框消费的彩色单元格图。
    pub fn render_styled(
        &self,
        project: &Project,
        focus: Option<&TargetId>,
    ) -> Result<StyledGraph, GraphRenderError> {
        let colored = Self {
            color: true,
            ..*self
        }
        .render_terminal(project, focus)?;
        Ok(StyledGraph::parse(&colored))
    }

    /// 导出整个项目或指定目标的前置依赖子图为 Graphviz DOT。
    pub fn render_dot(
        &self,
        project: &Project,
        focus: Option<&TargetId>,
    ) -> Result<String, GraphRenderError> {
        let model = GraphModel::new(project, focus)?;
        let mut output = String::from("digraph makedown {\n");
        output.push_str(&format!(
            "  graph [rankdir={}, compound=true, newrank=true];\n",
            self.direction.dot_rankdir()
        ));
        output.push_str("  node [shape=box, style=rounded, fontname=\"monospace\"];\n");
        output.push_str("  edge [arrowsize=0.8];\n\n");

        for (module_index, module) in model.modules.iter().enumerate() {
            let color = module_color(theme_index(&module.label), 8);
            output.push_str(&format!("  subgraph cluster_{module_index} {{\n"));
            output.push_str(&format!(
                "    label=\"{}\"; color=\"{}\"; fontcolor=\"{}\";\n",
                dot_escape(&module.label),
                hex_color(color),
                hex_color(color)
            ));
            for node_id in &module.nodes {
                let node = model
                    .nodes
                    .iter()
                    .find(|node| node.render_id == *node_id)
                    .expect("module membership references a graph node");
                let color = target_color(node.render_id);
                output.push_str(&format!(
                    "    n{} [label=\"{}\", color=\"{}\", fontcolor=\"{}\"];\n",
                    node.render_id,
                    dot_escape(node.label),
                    hex_color(color),
                    hex_color(color)
                ));
            }
            output.push_str("  }\n\n");
        }
        for edge in &model.edges {
            let color = mix(target_color(edge.from), target_color(edge.to));
            output.push_str(&format!(
                "  n{} -> n{} [color=\"{}\"];\n",
                edge.from,
                edge.to,
                hex_color(color)
            ));
        }
        output.push_str("}\n");
        Ok(output)
    }
}

/// 模块局部图以及可选择的目标坐标。
#[derive(Clone, Debug)]
pub struct ModuleGraph {
    pub graph: StyledGraph,
    pub targets: Vec<GraphTarget>,
}

/// 目标的稳定标识及终端布局中的矩形。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphTarget {
    pub id: TargetId,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// 保存去除 ANSI 控制序列后的带色终端图，供 TUI 与 SVG 复用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledGraph {
    width: usize,
    rows: Vec<Vec<StyledGlyph>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledGlyph {
    pub x: usize,
    pub symbol: char,
    pub color: Option<(u8, u8, u8)>,
}

impl StyledGraph {
    pub(crate) fn parse(source: &str) -> Self {
        let mut rows = vec![Vec::new()];
        let mut x = 0usize;
        let mut width = 0usize;
        let mut color = None;
        let mut characters = source.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' && characters.peek() == Some(&'[') {
                characters.next();
                let mut sequence = String::new();
                for next in characters.by_ref() {
                    if next == 'm' {
                        break;
                    }
                    sequence.push(next);
                }
                color = parse_sgr_color(&sequence, color);
                continue;
            }
            if character == '\n' {
                width = width.max(x);
                rows.push(Vec::new());
                x = 0;
                continue;
            }
            if character.is_control() {
                continue;
            }
            rows.last_mut().expect("graph has a row").push(StyledGlyph {
                x,
                symbol: character,
                color,
            });
            x += UnicodeWidthChar::width(character).unwrap_or(0);
        }
        width = width.max(x);
        if rows.last().is_some_and(Vec::is_empty) {
            rows.pop();
        }
        Self { width, rows }
    }

    /// 返回图占用的终端列数。
    pub fn width(&self) -> usize {
        self.width
    }

    /// 返回图占用的终端行数。
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// 返回一行中带原始横坐标的可见字符。
    pub fn row(&self, y: usize) -> Option<&[StyledGlyph]> {
        self.rows.get(y).map(Vec::as_slice)
    }

    /// 将当前终端布局转换为无需外部依赖的 SVG 图片。
    pub fn to_svg(&self) -> String {
        const CELL_WIDTH: usize = 10;
        const CELL_HEIGHT: usize = 20;
        const PADDING: usize = 12;
        let width = self.width.max(1) * CELL_WIDTH + PADDING * 2;
        let height = self.height().max(1) * CELL_HEIGHT + PADDING * 2;
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" width=\"{width}\" height=\"{height}\">\n  <rect width=\"100%\" height=\"100%\" rx=\"8\" fill=\"#0b1020\"/>\n  <g font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\" font-size=\"15\" dominant-baseline=\"middle\">\n"
        );
        for (y, row) in self.rows.iter().enumerate() {
            let mut index = 0;
            while index < row.len() {
                if row[index].symbol == ' ' {
                    index += 1;
                    continue;
                }
                let start = &row[index];
                let mut text = String::new();
                let mut positions = Vec::new();
                let mut next_x = start.x;
                while index < row.len()
                    && row[index].symbol != ' '
                    && row[index].color == start.color
                    && row[index].x == next_x
                {
                    let glyph = &row[index];
                    positions.push((PADDING + glyph.x * CELL_WIDTH).to_string());
                    text.push_str(&xml_escape(glyph.symbol));
                    next_x += UnicodeWidthChar::width(glyph.symbol).unwrap_or(0);
                    index += 1;
                }
                let x = positions.join(" ");
                let y = PADDING + y * CELL_HEIGHT + CELL_HEIGHT / 2;
                let color = start
                    .color
                    .map(|(red, green, blue)| format!("#{red:02x}{green:02x}{blue:02x}"))
                    .unwrap_or_else(|| "#d8dee9".to_owned());
                svg.push_str(&format!(
                    "    <text x=\"{x}\" y=\"{y}\" fill=\"{color}\">{text}</text>\n"
                ));
            }
        }
        svg.push_str("  </g>\n</svg>\n");
        svg
    }
}

fn parse_sgr_color(sequence: &str, current: Option<(u8, u8, u8)>) -> Option<(u8, u8, u8)> {
    if sequence == "0" {
        return None;
    }
    let fields = sequence.split(';').collect::<Vec<_>>();
    match fields.as_slice() {
        ["38", "2", red, green, blue] => Some((
            red.parse().unwrap_or(255),
            green.parse().unwrap_or(255),
            blue.parse().unwrap_or(255),
        )),
        _ => current,
    }
}

fn xml_escape(character: char) -> String {
    match character {
        '&' => "&amp;".to_owned(),
        '<' => "&lt;".to_owned(),
        '>' => "&gt;".to_owned(),
        '\'' => "&apos;".to_owned(),
        '"' => "&quot;".to_owned(),
        _ => character.to_string(),
    }
}

#[derive(Debug)]
struct GraphModel<'a> {
    nodes: Vec<GraphNode<'a>>,
    edges: Vec<GraphEdge>,
    modules: Vec<GraphModule>,
}

#[derive(Debug)]
struct GraphNode<'a> {
    id: TargetId,
    render_id: usize,
    label: &'a str,
}

#[derive(Debug)]
struct GraphEdge {
    from: usize,
    to: usize,
}

#[derive(Debug)]
struct GraphModule {
    label: String,
    nodes: Vec<usize>,
}

impl<'a> GraphModel<'a> {
    fn new(project: &'a Project, focus: Option<&TargetId>) -> Result<Self, GraphRenderError> {
        let selected = match focus {
            Some(target) => collect_dependencies(project, target)?,
            None => project
                .modules()
                .flat_map(|module| module.targets().map(|target| target.id().clone()))
                .collect(),
        };
        Self::from_selected(project, &selected, None)
    }

    fn for_module(project: &'a Project, namespace: &ModulePath) -> Result<Self, GraphRenderError> {
        let mut selected = BTreeSet::new();
        if let Some(module) = project.module(namespace) {
            for target in module.targets() {
                selected.insert(target.id().clone());
                selected.extend(target.dependencies().iter().cloned());
            }
        }
        Self::from_selected(project, &selected, Some(namespace))
    }

    fn from_selected(
        project: &'a Project,
        selected: &BTreeSet<TargetId>,
        local: Option<&ModulePath>,
    ) -> Result<Self, GraphRenderError> {
        let modules = project
            .modules()
            .filter(|module| {
                module
                    .targets()
                    .any(|target| selected.contains(target.id()))
            })
            .collect::<Vec<_>>();
        if selected.len() > MAX_TARGETS {
            return Err(GraphRenderError::TooManyTargets(selected.len()));
        }

        let mut ids = BTreeMap::new();
        let mut nodes = Vec::new();
        let mut graph_modules = Vec::new();
        for module in &modules {
            let label = module_label(module.namespace());
            let theme = theme_index(&label);
            let mut module_nodes = Vec::new();
            for target in module
                .targets()
                .filter(|target| selected.contains(target.id()))
            {
                let unique = nodes.len() + 1;
                let shade = shade_index(target.name());
                let render_id = encode_node_id(unique, theme, shade);
                ids.insert(target.id().clone(), render_id);
                module_nodes.push(render_id);
                nodes.push(GraphNode {
                    id: target.id().clone(),
                    render_id,
                    label: target.name(),
                });
            }
            graph_modules.push(GraphModule {
                label,
                nodes: module_nodes,
            });
        }

        let mut edges = Vec::new();
        for module in &modules {
            for target in module
                .targets()
                .filter(|target| selected.contains(target.id()))
            {
                // 外部节点是模块入口，不展开外部节点自身的依赖边。
                if local.is_some_and(|namespace| target.namespace() != namespace) {
                    continue;
                }
                let to = ids[target.id()];
                for dependency in target.dependencies() {
                    if let Some(&from) = ids.get(dependency) {
                        edges.push(GraphEdge { from, to });
                    }
                }
            }
        }
        Ok(Self {
            nodes,
            edges,
            modules: graph_modules,
        })
    }
}

fn collect_dependencies(
    project: &Project,
    start: &TargetId,
) -> Result<BTreeSet<TargetId>, GraphRenderError> {
    if target(project, start).is_none() {
        return Err(GraphRenderError::TargetNotFound(start.clone()));
    }
    let mut selected = BTreeSet::new();
    let mut pending = vec![start.clone()];
    while let Some(id) = pending.pop() {
        if !selected.insert(id.clone()) {
            continue;
        }
        if let Some(target) = target(project, &id) {
            pending.extend(target.dependencies().iter().cloned());
        }
    }
    Ok(selected)
}

fn target<'a>(project: &'a Project, id: &TargetId) -> Option<&'a crate::core::Target> {
    project
        .module(id.namespace())
        .and_then(|module| module.target(id.name()))
}

fn module_label(namespace: &ModulePath) -> String {
    if namespace.is_root() {
        "main.mf".to_owned()
    } else {
        format!("{}.mf", namespace.to_string().replace("::", "/"))
    }
}

// 将用户文字扩成 ascii-dag 的单字符画布格；输出时去除双宽字的占位空格。
fn canvas_label(label: &str) -> String {
    let mut result = String::new();
    for ch in label.chars() {
        result.push(ch);
        result.extend(std::iter::repeat_n(
            ' ',
            UnicodeWidthChar::width(ch).unwrap_or(0).saturating_sub(1),
        ));
    }
    result
}

fn normalize_canvas_text(text: &str) -> String {
    let mut result = String::new();
    let mut characters = text.chars().peekable();
    let mut padding = 0;
    while let Some(ch) = characters.next() {
        // ANSI SGR 不占画布格，也不能打断占位空格的删除。
        if ch == '\u{1b}' && characters.peek() == Some(&'[') {
            result.push(ch);
            for next in characters.by_ref() {
                result.push(next);
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        if padding > 0 && ch == ' ' {
            padding -= 1;
            continue;
        }
        result.push(ch);
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        // 组合符/零宽字仍占 ascii-dag 一格；保留字符并补足显示格，避免后续列左移。
        if width == 0 && !ch.is_control() {
            result.push(' ');
        }
        padding = width.saturating_sub(1);
    }
    result
}

fn encode_node_id(unique: usize, module: usize, shade: usize) -> usize {
    (unique << ID_STYLE_BITS) | ((module & MODULE_MASK) << 4) | (shade & SHADE_MASK)
}

fn decoded_style(id: usize) -> (usize, usize) {
    ((id >> 4) & MODULE_MASK, id & SHADE_MASK)
}

fn paint_target(
    region: &mut ascii_dag::render::engine::NodeRegion<'_, '_>,
    context: ascii_dag::render::engine::NodePaintCtx<'_>,
) {
    let color = cell_color(target_color(context.node_id));
    region.set_colored(0, 0, '[', color);
    let mut x = 1;
    for character in context.label.chars() {
        region.set_colored(x, 0, character, color);
        let width = UnicodeWidthChar::width(character).unwrap_or(0).max(1);
        for offset in 1..width {
            region.set_colored(x + offset, 0, ' ', color);
        }
        x += width;
    }
    region.set_colored(context.width.saturating_sub(1), 0, ']', color);
}

fn themed_edge_style(context: EdgeStyleCtx<'_>) -> EdgeStyle {
    let color = mix(target_color(context.from_id), target_color(context.to_id));
    EdgeStyle {
        color: cell_color(color),
        ..EdgeStyle::default()
    }
}

fn themed_subgraph_style(context: SubgraphStyleCtx<'_>) -> SubgraphStyle {
    SubgraphStyle {
        border: SubgraphBorder::Double,
        color: cell_color(module_color(
            theme_index(&normalize_canvas_text(context.label)),
            8,
        )),
        ..SubgraphStyle::default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rgb(u8, u8, u8);

fn cell_color(color: Rgb) -> CellColor {
    CellColor::rgb(color.0, color.1, color.2)
}

fn target_color(id: usize) -> Rgb {
    let (module, shade) = decoded_style(id);
    module_color(module, shade)
}

fn theme_index(module_label: &str) -> usize {
    stable_hash(module_label) & MODULE_MASK
}

fn shade_index(target_name: &str) -> usize {
    stable_hash(target_name) & SHADE_MASK
}

fn stable_hash(value: &str) -> usize {
    value.bytes().fold(0x811c9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
    }) as usize
}

fn module_color(module: usize, shade: usize) -> Rgb {
    let hue = ((module as f64 * 137.508) + shade as f64 * 1.8) % 360.0;
    let lightness = 0.58 + (shade % 5) as f64 * 0.025;
    hsl_to_rgb(hue, 0.68, lightness.min(0.70))
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let section = hue / 60.0;
    let secondary = chroma * (1.0 - (section % 2.0 - 1.0).abs());
    let (red, green, blue) = match section.floor() as usize {
        0 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let match_value = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + match_value) * 255.0).round() as u8;
    Rgb(channel(red), channel(green), channel(blue))
}

fn mix(left: Rgb, right: Rgb) -> Rgb {
    let average = |left: u8, right: u8| ((left as u16 + right as u16) / 2) as u8;
    Rgb(
        average(left.0, right.0),
        average(left.1, right.1),
        average(left.2, right.2),
    )
}

fn hex_color(color: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.0, color.1, color.2)
}

fn dot_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl GraphDirection {
    fn into_ascii_dag(self) -> Direction {
        match self {
            Self::TopDown => Direction::TopDown,
            Self::BottomUp => Direction::BottomUp,
            Self::LeftRight => Direction::LeftRight,
            Self::RightLeft => Direction::RightLeft,
        }
    }

    fn dot_rankdir(self) -> &'static str {
        match self {
            Self::TopDown => "TB",
            Self::BottomUp => "BT",
            Self::LeftRight => "LR",
            Self::RightLeft => "RL",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Module, Target, TargetName};

    fn id(namespace: ModulePath, name: &str) -> TargetId {
        TargetId::new(namespace, TargetName::parse(name).unwrap())
    }

    fn fixture() -> Project {
        let root = ModulePath::root();
        let service = ModulePath::parse("service").unwrap();
        let mut project = Project::new(root.clone());

        let mut root_module = Module::new(root.clone());
        let mut release = Target::new(root, TargetName::parse("release").unwrap());
        release.add_dependency(id(service.clone(), "build"));
        root_module.add_target(release).unwrap();
        project.add_module(root_module).unwrap();

        let mut service_module = Module::new(service.clone());
        let prepare = Target::new(service.clone(), TargetName::parse("prepare").unwrap());
        let mut build = Target::new(service, TargetName::parse("build").unwrap());
        build.add_dependency(prepare.id().clone());
        service_module.add_target(prepare).unwrap();
        service_module.add_target(build).unwrap();
        project.add_module(service_module).unwrap();
        project
    }

    #[test]
    fn terminal_graph_groups_modules_and_points_from_dependencies_to_consumers() {
        let output = ProjectGraphRenderer::new()
            .render_terminal(&fixture(), None)
            .unwrap();

        assert!(output.contains("main.mf"));
        assert!(output.contains("service.mf"));
        assert!(output.contains("[prepare]"));
        assert!(output.contains("[build]"));
        assert!(output.contains("[release]"));
        assert!(output.contains('↓'));
    }

    #[test]
    fn focused_graph_only_contains_the_targets_dependencies() {
        let project = fixture();
        let output = ProjectGraphRenderer::new()
            .with_charset(GraphCharset::Ascii)
            .render_terminal(
                &project,
                Some(&id(ModulePath::parse("service").unwrap(), "build")),
            )
            .unwrap();

        assert!(output.contains("[prepare]"));
        assert!(output.contains("[build]"));
        assert!(!output.contains("[release]"));
        assert!(!output.contains('↓'));
    }

    #[test]
    fn dot_graph_carries_module_target_and_mixed_edge_colors() {
        let output = ProjectGraphRenderer::new()
            .with_direction(GraphDirection::LeftRight)
            .render_dot(&fixture(), None)
            .unwrap();

        assert!(output.contains("rankdir=LR"));
        assert!(output.contains("subgraph cluster_0"));
        assert!(output.contains("label=\"service.mf\""));
        assert!(output.contains(" -> "));
        assert!(output.contains("color=\"#"));
    }

    #[test]
    fn module_targets_are_near_in_color_and_cross_module_edges_are_mixed() {
        let same_module_a = encode_node_id(1, 2, 0);
        let same_module_b = encode_node_id(2, 2, 1);
        let other_module = encode_node_id(3, 3, 0);
        let a = target_color(same_module_a);
        let b = target_color(same_module_b);
        let other = target_color(other_module);

        assert!(color_distance(a, b) < color_distance(a, other));
        assert_eq!(mix(a, other).0, ((a.0 as u16 + other.0 as u16) / 2) as u8);
    }

    #[test]
    fn every_direction_maps_to_the_expected_dot_rank() {
        assert_eq!(GraphDirection::TopDown.dot_rankdir(), "TB");
        assert_eq!(GraphDirection::BottomUp.dot_rankdir(), "BT");
        assert_eq!(GraphDirection::LeftRight.dot_rankdir(), "LR");
        assert_eq!(GraphDirection::RightLeft.dot_rankdir(), "RL");
    }

    #[test]
    fn styled_graph_preserves_true_color_and_display_width() {
        let graph = StyledGraph::parse("\x1b[38;2;12;34;56m界\x1b[0mX\n");

        assert_eq!(graph.width(), 3);
        assert_eq!(graph.height(), 1);
        assert_eq!(graph.row(0).unwrap()[0].x, 0);
        assert_eq!(graph.row(0).unwrap()[0].color, Some((12, 34, 56)));
        assert_eq!(graph.row(0).unwrap()[1].x, 2);
        assert_eq!(graph.row(0).unwrap()[1].color, None);
    }

    #[test]
    fn svg_export_is_standalone_colored_xml_without_ansi() {
        let svg = ProjectGraphRenderer::new()
            .render_svg(&fixture(), None)
            .unwrap();

        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains("<rect width=\"100%\""));
        assert!(svg.contains("fill=\"#"));
        assert!(svg.contains("[prepare]"));
        assert!(!svg.contains('\u{1b}'));
    }

    #[test]
    fn svg_positions_every_glyph_on_the_terminal_grid() {
        let svg = StyledGraph::parse("abc  界X\n").to_svg();
        assert!(svg.contains("<text x=\"12 22 32\" y=\"22\""));
        assert!(svg.contains("<text x=\"62 82\" y=\"22\""));
        assert!(svg.contains(">abc</text>"));
        assert!(svg.contains(">界X</text>"));
    }

    #[test]
    fn module_view_does_not_expand_edges_between_external_targets() {
        let root = ModulePath::root();
        let external = ModulePath::parse("external").unwrap();
        let mut project = Project::new(root.clone());
        let mut local = Module::new(root.clone());
        let mut release = Target::new(root.clone(), TargetName::parse("release").unwrap());
        release.add_dependency(id(external.clone(), "one"));
        release.add_dependency(id(external.clone(), "two"));
        local.add_target(release).unwrap();
        project.add_module(local).unwrap();
        let mut other = Module::new(external.clone());
        let mut one = Target::new(external.clone(), TargetName::parse("one").unwrap());
        one.add_dependency(id(external.clone(), "two"));
        other.add_target(one).unwrap();
        other
            .add_target(Target::new(
                external.clone(),
                TargetName::parse("two").unwrap(),
            ))
            .unwrap();
        project.add_module(other).unwrap();
        let model = GraphModel::for_module(&project, &root).unwrap();
        assert_eq!(model.nodes.len(), 3);
        assert_eq!(model.edges.len(), 2);
    }

    #[test]
    fn selectable_bounds_match_rendered_labels_in_every_direction_and_charset() {
        let mut project = fixture();
        let namespace = ModulePath::parse("中文模块").unwrap();
        let mut module = Module::new(namespace.clone());
        for name in ["构建任务", "e\u{301}build", "😀build"] {
            module
                .add_target(Target::new(
                    namespace.clone(),
                    TargetName::parse(name).unwrap(),
                ))
                .unwrap();
        }
        project.add_module(module).unwrap();
        for direction in [
            GraphDirection::TopDown,
            GraphDirection::BottomUp,
            GraphDirection::LeftRight,
            GraphDirection::RightLeft,
        ] {
            for charset in [GraphCharset::Ascii, GraphCharset::Unicode] {
                for module in project.modules() {
                    let view = ProjectGraphRenderer::new()
                        .with_direction(direction)
                        .with_charset(charset)
                        .render_module(&project, module.namespace())
                        .unwrap();
                    for node in &view.targets {
                        let rendered = view
                            .graph
                            .row(node.y)
                            .unwrap()
                            .iter()
                            .filter(|glyph| glyph.x >= node.x && glyph.x < node.x + node.width)
                            .map(|glyph| glyph.symbol)
                            .collect::<String>();
                        assert_eq!(
                            rendered,
                            normalize_canvas_text(&canvas_label(&format!("[{}]", node.id.name()))),
                            "{direction:?} {charset:?} {} at {},{}",
                            node.id,
                            node.x,
                            node.y
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn module_and_target_themes_are_stable_identifiers() {
        assert_eq!(theme_index("service.mf"), theme_index("service.mf"));
        assert_ne!(theme_index("service.mf"), theme_index("main.mf"));
        assert_eq!(shade_index("build"), shade_index("build"));
        assert_eq!(
            target_color(encode_node_id(1, 42, 7)),
            target_color(encode_node_id(99, 42, 7))
        );
    }

    fn color_distance(left: Rgb, right: Rgb) -> u32 {
        let square = |left: u8, right: u8| {
            let delta = left as i32 - right as i32;
            (delta * delta) as u32
        };
        square(left.0, right.0) + square(left.1, right.1) + square(left.2, right.2)
    }
}
