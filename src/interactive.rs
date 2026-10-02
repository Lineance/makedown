use std::fs;
use std::io::{self, IsTerminal, Stdout, Write};
use std::path::PathBuf;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use unicode_width::UnicodeWidthChar;

mod details;

use details::{Popup, Rect};

use crate::core::{ModulePath, Project, TargetId};
use crate::graph::{
    GraphCharset, GraphRenderError, GraphTarget, ModuleGraph, ProjectGraphRenderer,
};

/// 判断当前标准输入输出能否承载交互界面。
pub fn terminal_available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// 按模块浏览、选择 Target、展开详情，并允许按 `e` 导出当前模块 SVG。
pub fn run(
    project: &Project,
    renderer: &ProjectGraphRenderer,
    focus: Option<&TargetId>,
    charset: GraphCharset,
    color: bool,
    export_path: PathBuf,
) -> io::Result<()> {
    let mut browser = Browser::new(project, renderer, focus).map_err(io::Error::other)?;
    browser.color = color;
    let mut terminal = TerminalSession::enter()?;
    let mut status = String::new();
    loop {
        let (width, height) = terminal::size()?;
        browser.follow_focus(width, height);
        browser.clamp_detail_scroll(project, width, height);
        draw(
            terminal.stdout(),
            project,
            &browser,
            charset,
            width,
            height,
            &status,
        )?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if should_quit(key) {
                    break;
                }
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    continue;
                }
                if key.code == KeyCode::Char('e') {
                    status = match fs::write(&export_path, browser.view.graph.to_svg()) {
                        Ok(()) => format!("exported {}", export_path.display()),
                        Err(error) => format!("export failed: {error}"),
                    };
                } else {
                    status.clear();
                    browser
                        .handle_event(project, renderer, key, width, height)
                        .map_err(io::Error::other)?;
                    browser.clamp_detail_scroll(project, width, height);
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    terminal.restore()
}

fn should_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

struct Browser {
    module: ModulePath,
    view: ModuleGraph,
    focus: Option<TargetId>,
    viewport: Viewport,
    offset: ViewOffset,
    details: bool,
    color: bool,
    detail_scroll: usize,
    history: Vec<(ModulePath, Option<TargetId>)>,
}

impl Browser {
    fn new(
        project: &Project,
        renderer: &ProjectGraphRenderer,
        focus: Option<&TargetId>,
    ) -> Result<Self, GraphRenderError> {
        if let Some(id) = focus
            && project
                .module(id.namespace())
                .and_then(|module| module.target(id.name()))
                .is_none()
        {
            return Err(GraphRenderError::TargetNotFound(id.clone()));
        }
        let module = focus
            .map(|id| id.namespace())
            .unwrap_or(project.root_module())
            .clone();
        let view = renderer.render_module(project, &module)?;
        let focus = focus.cloned().or_else(|| first_local(&view, &module));
        Ok(Self {
            module,
            view,
            focus,
            viewport: Viewport::default(),
            offset: ViewOffset::default(),
            details: false,
            color: true,
            detail_scroll: 0,
            history: Vec::new(),
        })
    }

    fn selected(&self) -> Option<&GraphTarget> {
        self.view
            .targets
            .iter()
            .find(|node| Some(&node.id) == self.focus.as_ref())
    }

    fn switch_module(
        &mut self,
        project: &Project,
        renderer: &ProjectGraphRenderer,
        module: ModulePath,
        focus: Option<TargetId>,
    ) -> Result<(), GraphRenderError> {
        let view = renderer.render_module(project, &module)?;
        let focus = focus
            .filter(|id| view.targets.iter().any(|node| &node.id == id))
            .or_else(|| first_local(&view, &module));
        self.module = module;
        self.view = view;
        self.focus = focus;
        self.viewport = Viewport::default();
        self.details = false;
        self.detail_scroll = 0;
        Ok(())
    }

    fn handle_event(
        &mut self,
        project: &Project,
        renderer: &ProjectGraphRenderer,
        key: KeyEvent,
        width: u16,
        height: u16,
    ) -> Result<(), GraphRenderError> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Ok(());
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let movement = match key.code {
            KeyCode::Char('H') => Some((-1, 0)),
            KeyCode::Char('J') => Some((0, 1)),
            KeyCode::Char('K') => Some((0, -1)),
            KeyCode::Char('L') => Some((1, 0)),
            KeyCode::Char('h') | KeyCode::Left if shift => Some((-1, 0)),
            KeyCode::Char('j') | KeyCode::Down if shift => Some((0, 1)),
            KeyCode::Char('k') | KeyCode::Up if shift => Some((0, -1)),
            KeyCode::Char('l') | KeyCode::Right if shift => Some((1, 0)),
            _ => None,
        };
        if let Some((dx, dy)) = movement {
            self.offset.x += dx;
            self.offset.y += dy;
            self.follow_focus(width, height);
            return Ok(());
        }
        self.handle_key(project, renderer, key.code)
    }

    fn handle_key(
        &mut self,
        project: &Project,
        renderer: &ProjectGraphRenderer,
        code: KeyCode,
    ) -> Result<(), GraphRenderError> {
        if self.details {
            match code {
                KeyCode::Enter | KeyCode::Esc => self.details = false,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                _ => {}
            }
            return Ok(());
        }
        match code {
            KeyCode::Enter => {
                if let Some(id) = self.focus.clone() {
                    if id.namespace() != &self.module {
                        self.history.push((self.module.clone(), self.focus.clone()));
                        self.switch_module(project, renderer, id.namespace().clone(), Some(id))?;
                    } else {
                        self.details = true;
                        self.detail_scroll = 0;
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Esc => {
                if let Some((module, focus)) = self.history.pop() {
                    self.switch_module(project, renderer, module, focus)?;
                }
            }
            KeyCode::Char('[' | ']') => {
                let modules = project
                    .modules()
                    .map(|module| module.namespace().clone())
                    .collect::<Vec<_>>();
                if let Some(index) = modules.iter().position(|module| module == &self.module) {
                    let next = if code == KeyCode::Char(']') {
                        (index + 1) % modules.len()
                    } else {
                        (index + modules.len() - 1) % modules.len()
                    };
                    if modules[next] != self.module {
                        self.history.push((self.module.clone(), self.focus.clone()));
                        self.switch_module(project, renderer, modules[next].clone(), None)?;
                    }
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let nodes = &self.view.targets;
                if !nodes.is_empty() {
                    let index = nodes
                        .iter()
                        .position(|node| Some(&node.id) == self.focus.as_ref())
                        .unwrap_or(0);
                    let next = if code == KeyCode::Tab {
                        (index + 1) % nodes.len()
                    } else {
                        (index + nodes.len() - 1) % nodes.len()
                    };
                    self.focus = Some(nodes[next].id.clone());
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.navigate(-1, 0),
            KeyCode::Char('l') | KeyCode::Right => self.navigate(1, 0),
            KeyCode::Char('k') | KeyCode::Up => self.navigate(0, -1),
            KeyCode::Char('j') | KeyCode::Down => self.navigate(0, 1),
            _ => {}
        }
        Ok(())
    }

    fn navigate(&mut self, dx: i64, dy: i64) {
        let Some(current) = self.selected() else {
            return;
        };
        let center = |node: &GraphTarget| {
            (
                node.x as i64 + node.width as i64 / 2,
                node.y as i64 + node.height as i64 / 2,
            )
        };
        let (x, y) = center(current);
        let next = self
            .view
            .targets
            .iter()
            .filter(|node| node.id != current.id)
            .filter_map(|node| {
                let (nx, ny) = center(node);
                let along = (nx - x) * dx + (ny - y) * dy;
                if along <= 0 {
                    return None;
                }
                let across = ((nx - x) * dy - (ny - y) * dx).abs();
                // 优先接近按键方向的节点，再比较距离，稳定标识打破平局。
                Some(((across * 2 + along, across, &node.id), node))
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, node)| node.id.clone());
        if let Some(next) = next {
            self.focus = Some(next);
        }
    }

    fn clamp_detail_scroll(&mut self, project: &Project, width: u16, height: u16) {
        if !self.details {
            return;
        }
        if let Some(popup) = self.popup(project, width, height) {
            self.detail_scroll = self.detail_scroll.min(popup.max_scroll());
        }
    }

    fn popup(&self, project: &Project, width: u16, height: u16) -> Option<Popup> {
        let id = self.focus.as_ref()?;
        let target = project.module(id.namespace())?.target(id.name())?;
        let focus = self
            .selected()
            .and_then(|node| self.viewport.screen_rect(node, width, height));
        details::layout(target, focus, width, height)
    }

    fn preview(&self, project: &Project, width: u16, height: u16) -> Option<Popup> {
        let id = self.focus.as_ref()?;
        let target = project.module(id.namespace())?.target(id.name())?;
        let focus = self
            .selected()
            .and_then(|node| self.viewport.screen_rect(node, width, height));
        details::preview_layout(target, focus, width, height)
    }

    fn follow_focus(&mut self, width: u16, height: u16) {
        let node = self.selected().cloned();
        self.offset.clamp(node.as_ref(), width, height);
        if let Some(node) = node {
            self.viewport.center(&node, self.offset, width, height);
        }
    }
}

fn first_local(view: &ModuleGraph, module: &ModulePath) -> Option<TargetId> {
    view.targets
        .iter()
        .find(|node| node.id.namespace() == module)
        .map(|node| node.id.clone())
}

/// 相对观察位置偏移；不随选择/模块切换归零。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ViewOffset {
    x: i64,
    y: i64,
}

impl ViewOffset {
    fn clamp(&mut self, node: Option<&GraphTarget>, width: u16, height: u16) {
        let limit = |available: u16, size: usize, cap: i64| {
            let available = i64::from(available.saturating_sub(2));
            (available / 4)
                .min((available - size as i64).max(0) / 2)
                .min(cap)
        };
        let x = limit(width, node.map_or(1, |node| node.width), 32);
        let y = limit(height, node.map_or(1, |node| node.height), 12);
        self.x = self.x.clamp(-x, x);
        self.y = self.y.clamp(-y, y);
    }
}

/// 允许负原点：靠近图边界的 Target 也能居中，空白区域正常留空。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Viewport {
    x: i64,
    y: i64,
}

impl Viewport {
    fn center(&mut self, node: &GraphTarget, offset: ViewOffset, width: u16, height: u16) {
        self.x = node.x as i64 + node.width as i64 / 2 - i64::from(width.saturating_sub(2)) / 2
            + offset.x;
        self.y = node.y as i64 + node.height as i64 / 2 - i64::from(height.saturating_sub(2)) / 2
            + offset.y;
    }

    fn screen_rect(&self, node: &GraphTarget, width: u16, height: u16) -> Option<Rect> {
        let x = 1 + node.x as i64 - self.x;
        let y = 1 + node.y as i64 - self.y;
        let left = x.max(1);
        let top = y.max(1);
        let right = (x + node.width as i64).min(i64::from(width.saturating_sub(1)));
        let bottom = (y + node.height as i64).min(i64::from(height.saturating_sub(1)));
        (right > left && bottom > top).then(|| Rect {
            x: left as u16,
            y: top as u16,
            width: (right - left) as u16,
            height: (bottom - top) as u16,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    stdout: &mut impl Write,
    project: &Project,
    browser: &Browser,
    charset: GraphCharset,
    width: u16,
    height: u16,
    status: &str,
) -> io::Result<()> {
    queue!(stdout, Hide, Clear(ClearType::All), ResetColor)?;
    if width < 4 || height < 4 {
        queue!(
            stdout,
            MoveTo(0, 0),
            Print(truncate("terminal too small", width as usize))
        )?;
        return stdout.flush();
    }
    let help = if status.is_empty() {
        "hjkl select | HJKL pan | Enter open | Esc back | [] modules | e export | Ctrl-C quit"
    } else {
        status
    };
    draw_frame(stdout, charset, 0, 0, width, height, help)?;
    let title = format!(
        "{} · {} · offset {:+},{:+}",
        module_label(&browser.module),
        browser
            .focus
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "no targets".into()),
        browser.offset.x,
        browser.offset.y,
    );
    queue!(
        stdout,
        MoveTo(2, 0),
        Print(truncate(&title, width.saturating_sub(4) as usize))
    )?;
    let viewport = browser.viewport;
    let right = viewport.x + i64::from(width.saturating_sub(2));
    let selected = browser.selected();
    for screen_y in 0..height.saturating_sub(2) as usize {
        let graph_y = viewport.y + screen_y as i64;
        let Some(row) = usize::try_from(graph_y)
            .ok()
            .and_then(|y| browser.view.graph.row(y))
        else {
            continue;
        };
        for glyph in row {
            let glyph_width = UnicodeWidthChar::width(glyph.symbol).unwrap_or(0);
            if (glyph.x as i64) < viewport.x
                || glyph.x as i64 >= right
                || (glyph.x + glyph_width) as i64 > right
            {
                continue;
            }
            let color = glyph
                .color
                .filter(|_| browser.color)
                .map(|(r, g, b)| Color::Rgb { r, g, b })
                .unwrap_or(Color::Reset);
            let highlight = selected.is_some_and(|node| {
                graph_y >= node.y as i64
                    && graph_y < (node.y + node.height) as i64
                    && glyph.x >= node.x
                    && glyph.x < node.x + node.width
            });
            queue!(
                stdout,
                MoveTo(
                    1 + (glyph.x as i64 - viewport.x) as u16,
                    1 + screen_y as u16
                ),
                SetBackgroundColor(if highlight {
                    Color::DarkGrey
                } else {
                    Color::Reset
                }),
                SetForegroundColor(if highlight { Color::White } else { color }),
                Print(glyph.symbol)
            )?;
        }
    }
    queue!(stdout, ResetColor)?;
    let popup = if browser.details {
        browser.popup(project, width, height)
    } else {
        browser.preview(project, width, height)
    };
    if let Some(popup) = popup {
        draw_details(
            stdout,
            &popup,
            charset,
            if browser.details {
                browser.detail_scroll
            } else {
                0
            },
            browser.color,
        )?;
    }
    stdout.flush()
}

fn module_label(module: &ModulePath) -> String {
    if module.is_root() {
        "main.mf".into()
    } else {
        format!("{}.mf", module.to_string().replace("::", "/"))
    }
}

fn draw_details(
    stdout: &mut impl Write,
    popup: &Popup,
    charset: GraphCharset,
    scroll: usize,
    color: bool,
) -> io::Result<()> {
    let Rect {
        x,
        y,
        width,
        height,
    } = popup.rect;
    let available = height.saturating_sub(2) as usize;
    let start = scroll.min(popup.max_scroll());
    queue!(stdout, ResetColor)?;
    for row in 0..height {
        queue!(
            stdout,
            MoveTo(x, y + row),
            Print(" ".repeat(width as usize))
        )?;
    }
    draw_frame(stdout, charset, x, y, width, height, popup.caption)?;
    for (row, line) in popup.lines.iter().skip(start).take(available).enumerate() {
        queue!(stdout, MoveTo(x + 1, y + 1 + row as u16))?;
        let mut index = 0;
        while index < line.len() {
            let syntax = line[index].syntax;
            let mut text = String::new();
            while index < line.len() && (!color || line[index].syntax == syntax) {
                text.push(line[index].symbol);
                index += 1;
            }
            queue!(
                stdout,
                SetForegroundColor(if color { syntax.color() } else { Color::Reset }),
                Print(text)
            )?;
        }
    }
    queue!(stdout, ResetColor)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    stdout: &mut impl Write,
    charset: GraphCharset,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    status: &str,
) -> io::Result<()> {
    let (tl, horizontal, tr, vertical, bl, br) = match charset {
        GraphCharset::Unicode => ('┌', '─', '┐', '│', '└', '┘'),
        GraphCharset::Ascii => ('+', '-', '+', '|', '+', '+'),
    };
    let line = horizontal
        .to_string()
        .repeat(width.saturating_sub(2) as usize);
    queue!(stdout, MoveTo(x, y), Print(tl), Print(&line), Print(tr))?;
    for row in 1..height.saturating_sub(1) {
        queue!(
            stdout,
            MoveTo(x, y + row),
            Print(vertical),
            MoveTo(x + width - 1, y + row),
            Print(vertical)
        )?;
    }
    queue!(
        stdout,
        MoveTo(x, y + height - 1),
        Print(bl),
        Print(&line),
        Print(br)
    )?;
    if width > 4 {
        queue!(
            stdout,
            MoveTo(x + 2, y + height - 1),
            Print(truncate(status, width.saturating_sub(4) as usize))
        )?;
    }
    Ok(())
}

fn truncate(value: &str, width: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take_while(|ch| {
            let size = UnicodeWidthChar::width(*ch).unwrap_or(0);
            if used + size > width {
                return false;
            }
            used += size;
            true
        })
        .collect()
}

struct TerminalSession {
    stdout: Stdout,
    restored: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = execute!(stdout, Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self {
            stdout,
            restored: false,
        })
    }
    fn stdout(&mut self) -> &mut Stdout {
        &mut self.stdout
    }
    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let screen = execute!(self.stdout, ResetColor, Show, LeaveAlternateScreen);
        let raw = disable_raw_mode();
        screen.and(raw)
    }
}
impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Module, Spec, Target, TargetName};

    fn id(module: &str, name: &str) -> TargetId {
        TargetId::new(
            if module.is_empty() {
                ModulePath::root()
            } else {
                ModulePath::parse(module).unwrap()
            },
            TargetName::parse(name).unwrap(),
        )
    }
    fn fixture() -> Project {
        let mut project = Project::new(ModulePath::root());
        for (module, targets) in [
            (
                "",
                vec![
                    ("one", vec![id("", "two")]),
                    ("two", vec![id("", "three"), id("B", "four")]),
                    ("three", vec![]),
                ],
            ),
            (
                "B",
                vec![("four", vec![id("C", "five")]), ("other", vec![])],
            ),
            ("C", vec![("five", vec![])]),
        ] {
            let namespace = id(module, "unused").namespace().clone();
            let mut group = Module::new(namespace.clone());
            for (name, dependencies) in targets {
                let mut target = Target::new(namespace.clone(), TargetName::parse(name).unwrap());
                for dependency in dependencies {
                    target.add_dependency(dependency);
                }
                target.set_description("这是详情描述。\nSecond line");
                target.add_specification(Spec::new("验收条目"));
                group.add_target(target).unwrap();
            }
            project.add_module(group).unwrap();
        }
        project
    }

    #[test]
    fn module_boundary_and_enter_preserve_external_target_focus() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        assert_eq!(browser.view.targets.len(), 4);
        assert!(
            !browser
                .view
                .targets
                .iter()
                .any(|node| node.id == id("C", "five"))
        );
        browser.focus = Some(id("B", "four"));
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        assert_eq!(browser.module, ModulePath::parse("B").unwrap());
        assert_eq!(browser.focus, Some(id("B", "four")));
        assert!(!browser.details);
        assert_eq!(browser.view.targets.len(), 3);
        assert!(
            browser
                .view
                .targets
                .iter()
                .any(|node| node.id == id("C", "five"))
        );
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        assert!(browser.details);
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        assert!(!browser.details);
        browser
            .handle_key(&project, &renderer, KeyCode::Esc)
            .unwrap();
        assert_eq!(browser.module, ModulePath::root());
        assert_eq!(browser.focus, Some(id("B", "four")));
    }

    #[test]
    fn preview_tracks_focus_and_details_replace_it_then_restore_it() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        let render = |browser: &Browser| {
            let mut output = Vec::new();
            draw(
                &mut output,
                &project,
                browser,
                GraphCharset::Unicode,
                120,
                40,
                "",
            )
            .unwrap();
            String::from_utf8(output).unwrap()
        };
        browser.follow_focus(120, 40);
        let initial = render(&browser);
        assert!(initial.contains("Second line"));
        assert!(!initial.contains("Specifications"));
        assert!(!initial.contains("Enter/Esc close"));
        browser
            .handle_key(&project, &renderer, KeyCode::Tab)
            .unwrap();
        browser.follow_focus(120, 40);
        let expected = browser.focus.as_ref().unwrap().to_string();
        let preview = browser.preview(&project, 120, 40).unwrap();
        let name = preview.lines[0]
            .iter()
            .map(|glyph| glyph.symbol)
            .collect::<String>();
        assert_eq!(name, expected);
        // 一个外部 Target 的 Enter 切模块后，缩略窗立即恢复。
        browser.focus = Some(id("B", "four"));
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        browser.follow_focus(120, 40);
        assert!(!browser.details);
        assert!(render(&browser).contains("Second line"));
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        let detail = render(&browser);
        assert!(detail.contains("Specifications"));
        assert!(detail.contains("Enter/Esc close"));
        for close in [KeyCode::Enter, KeyCode::Esc] {
            browser.handle_key(&project, &renderer, close).unwrap();
            let preview = render(&browser);
            assert!(preview.contains("Second line"));
            assert!(!preview.contains("Specifications"));
            browser
                .handle_key(&project, &renderer, KeyCode::Enter)
                .unwrap();
        }
    }

    #[test]
    fn initial_target_selects_its_module_not_its_transitive_subgraph() {
        let project = fixture();
        let browser = Browser::new(
            &project,
            &ProjectGraphRenderer::new(),
            Some(&id("B", "four")),
        )
        .unwrap();
        assert_eq!(browser.focus, Some(id("B", "four")));
        assert!(
            browser
                .view
                .targets
                .iter()
                .any(|node| node.id == id("B", "other"))
        );
    }

    #[test]
    fn arrow_navigation_selects_nodes_and_viewport_follows() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        browser.view.targets = vec![
            GraphTarget {
                id: id("", "one"),
                x: 0,
                y: 0,
                width: 5,
                height: 1,
            },
            GraphTarget {
                id: id("", "two"),
                x: 20,
                y: 0,
                width: 5,
                height: 1,
            },
            GraphTarget {
                id: id("", "three"),
                x: 20,
                y: 10,
                width: 7,
                height: 1,
            },
        ];
        browser.focus = Some(id("", "one"));
        browser
            .handle_key(&project, &renderer, KeyCode::Char('l'))
            .unwrap();
        assert_eq!(browser.focus, Some(id("", "two")));
        browser
            .handle_key(&project, &renderer, KeyCode::Char('j'))
            .unwrap();
        assert_eq!(browser.focus, Some(id("", "three")));
        let mut viewport = Viewport::default();
        viewport.center(browser.selected().unwrap(), ViewOffset::default(), 12, 6);
        assert_eq!(viewport, Viewport { x: 18, y: 8 });
        browser
            .handle_key(&project, &renderer, KeyCode::Char('j'))
            .unwrap();
        assert_eq!(browser.focus, Some(id("", "three")));
    }

    #[test]
    fn shift_pan_is_bounded_persistent_and_keeps_focus_centered_with_offset() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        for _ in 0..100 {
            browser
                .handle_event(
                    &project,
                    &renderer,
                    KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
                    80,
                    24,
                )
                .unwrap();
            browser
                .handle_event(
                    &project,
                    &renderer,
                    KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE),
                    80,
                    24,
                )
                .unwrap();
        }
        assert_eq!(browser.focus, Some(id("", "one")));
        assert_eq!(browser.offset, ViewOffset { x: 19, y: 5 });
        let offset = browser.offset;
        browser
            .handle_key(&project, &renderer, KeyCode::Tab)
            .unwrap();
        browser.follow_focus(80, 24);
        assert_eq!(browser.offset, offset);
        let node = browser.selected().unwrap();
        assert_eq!(
            node.x as i64 + node.width as i64 / 2 - browser.viewport.x,
            39 - offset.x
        );
        assert_eq!(node.y as i64 - browser.viewport.y, 11 - offset.y);
        browser
            .switch_module(
                &project,
                &renderer,
                ModulePath::parse("B").unwrap(),
                Some(id("B", "four")),
            )
            .unwrap();
        browser.follow_focus(80, 24);
        assert_eq!(browser.offset, offset);
        browser.follow_focus(20, 8);
        assert!(browser.offset.x <= 4 && browser.offset.y <= 1);
        let selected = browser
            .viewport
            .screen_rect(browser.selected().unwrap(), 20, 8)
            .unwrap();
        assert!(selected.width > 0 && selected.height > 0);
        browser
            .handle_event(
                &project,
                &renderer,
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::SHIFT),
                20,
                8,
            )
            .unwrap();
        assert_eq!(browser.offset.x, 3);
        // 屏幕足够大时硬上限仍然有效；从图原点也允许居中留白。
        browser.offset = ViewOffset { x: 1000, y: -1000 };
        browser.follow_focus(1000, 1000);
        assert_eq!(browser.offset, ViewOffset { x: 32, y: -12 });
        browser.offset = ViewOffset::default();
        browser.focus = Some(id("B", "four"));
        browser.follow_focus(80, 24);
        assert!(browser.viewport.x < 0);
    }

    #[test]
    fn details_wrap_unicode_and_are_modal() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, Some(&id("", "two"))).unwrap();
        let target = project.root().unwrap().target("two").unwrap();
        let lines = details::detail_lines(target, 12)
            .iter()
            .map(|line| line.iter().map(|glyph| glyph.symbol).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 12)
        );
        assert!(lines.join("\n").contains("验收条目"));
        assert!(lines.join("\n").contains("B::four"));
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        browser
            .handle_key(&project, &renderer, KeyCode::Char('j'))
            .unwrap();
        assert_eq!(browser.detail_scroll, 1);
        assert_eq!(browser.focus, Some(id("", "two")));
        let mut output = Vec::new();
        draw(
            &mut output,
            &project,
            &browser,
            GraphCharset::Unicode,
            80,
            24,
            "",
        )
        .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("38;5;"));
        let popup = browser.popup(&project, 80, 24).unwrap();
        let text = popup
            .lines
            .iter()
            .map(|line| line.iter().map(|glyph| glyph.symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Specifications"));
        browser
            .handle_key(&project, &renderer, KeyCode::Esc)
            .unwrap();
        assert!(!browser.details);
    }

    #[test]
    fn details_scroll_stops_at_end_and_moves_up_immediately() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, Some(&id("", "two"))).unwrap();
        browser
            .handle_key(&project, &renderer, KeyCode::Enter)
            .unwrap();
        for _ in 0..100 {
            browser
                .handle_key(&project, &renderer, KeyCode::Char('j'))
                .unwrap();
            browser.clamp_detail_scroll(&project, 30, 12);
        }
        let end = browser.detail_scroll;
        assert!(end > 0 && end < 100);
        browser
            .handle_key(&project, &renderer, KeyCode::Char('k'))
            .unwrap();
        browser.clamp_detail_scroll(&project, 30, 12);
        assert_eq!(browser.detail_scroll, end - 1);
        browser.clamp_detail_scroll(&project, 200, 100);
        assert_eq!(browser.detail_scroll, 0);
    }

    #[test]
    fn monochrome_display_keeps_focus_but_export_retains_theme() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        browser.color = false;
        let mut output = Vec::new();
        draw(
            &mut output,
            &project,
            &browser,
            GraphCharset::Ascii,
            80,
            24,
            "",
        )
        .unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("38;2;"));
        assert!(browser.view.graph.to_svg().contains("fill=\"#"));
        browser.details = true;
        browser.follow_focus(80, 24);
        let popup = browser.popup(&project, 80, 24).unwrap();
        let mut output = Vec::new();
        draw_details(&mut output, &popup, GraphCharset::Unicode, 0, false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Description"));
        assert!(!text.contains("38;5;") && !text.contains("38;2;"));
    }

    #[test]
    fn empty_module_and_small_terminal_are_safe() {
        let mut project = Project::new(ModulePath::root());
        project.add_module(Module::new(ModulePath::root())).unwrap();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        for key in [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Char('j'),
            KeyCode::Char(']'),
        ] {
            browser.handle_key(&project, &renderer, key).unwrap();
        }
        assert!(browser.focus.is_none());
        for (width, height) in [(1, 1), (4, 4), (80, 24)] {
            draw(
                &mut Vec::new(),
                &project,
                &browser,
                GraphCharset::Ascii,
                width,
                height,
                "",
            )
            .unwrap();
        }
    }

    #[test]
    fn module_cycle_and_back_navigation_reach_disconnected_targets() {
        let project = fixture();
        let renderer = ProjectGraphRenderer::new();
        let mut browser = Browser::new(&project, &renderer, None).unwrap();
        browser
            .handle_key(&project, &renderer, KeyCode::Char(']'))
            .unwrap();
        assert_eq!(browser.module, ModulePath::parse("B").unwrap());
        browser
            .handle_key(&project, &renderer, KeyCode::Backspace)
            .unwrap();
        assert_eq!(browser.module, ModulePath::root());
    }

    #[test]
    fn ctrl_c_is_the_only_quit_key() {
        assert!(should_quit(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(should_quit(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!should_quit(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn status_truncation_keeps_the_frame_bounded_and_safe() {
        assert_eq!(truncate("abcdef", 4), "abcd");
        assert_eq!(truncate("界面x", 4), "界面");
        assert_eq!(truncate("ok\u{1b}[2J", 20), "ok[2J");
    }
}
