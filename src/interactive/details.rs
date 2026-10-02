//! Target 详情的轻量语法高亮、按终端列换行及焦点避让布局。
use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;

use crate::core::Target;

const MAX_WIDTH: u16 = 72;
const MAX_HEIGHT: u16 = 24;
const PREVIEW_MAX_WIDTH: u16 = 48;
const PREVIEW_MAX_HEIGHT: u16 = 10;

#[derive(Clone, Copy)]
enum PopupKind {
    Details,
    Preview,
}

impl PopupKind {
    fn source(self, target: &Target) -> Vec<(String, Syntax)> {
        match self {
            Self::Details => detail_source(target),
            Self::Preview => {
                let mut source = vec![(target.id().to_string(), Syntax::Heading)];
                if !target.description().is_empty() {
                    source.push((String::new(), Syntax::Plain));
                    source.extend(
                        target
                            .description()
                            .lines()
                            .map(|line| (line.to_owned(), Syntax::Plain)),
                    );
                }
                source
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn right(self) -> u16 {
        self.x + self.width
    }
    pub fn bottom(self) -> u16 {
        self.y + self.height
    }
    #[cfg(test)]
    pub fn intersects(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Syntax {
    Plain,
    Heading,
    Reference,
    Specification,
    Code,
    Emphasis,
    Muted,
}

impl Syntax {
    pub fn color(self) -> Color {
        match self {
            Self::Plain => Color::Reset,
            Self::Heading => Color::Cyan,
            Self::Reference => Color::Magenta,
            Self::Specification => Color::Green,
            Self::Code => Color::Yellow,
            Self::Emphasis => Color::Blue,
            Self::Muted => Color::DarkGrey,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct DetailGlyph {
    pub symbol: char,
    pub syntax: Syntax,
}

pub(super) type DetailLine = Vec<DetailGlyph>;

pub(super) struct Popup {
    pub rect: Rect,
    pub lines: Vec<DetailLine>,
    pub caption: &'static str,
}

impl Popup {
    pub fn max_scroll(&self) -> usize {
        self.lines
            .len()
            .saturating_sub(self.rect.height.saturating_sub(2) as usize)
    }
}

/// 优先选取不覆盖焦点的区域。窄屏可缩小浮窗；无可用区域时才最小重叠。
pub(super) fn layout(
    target: &Target,
    focus: Option<Rect>,
    width: u16,
    height: u16,
) -> Option<Popup> {
    layout_kind(target, focus, width, height, PopupKind::Details)
}

pub(super) fn preview_layout(
    target: &Target,
    focus: Option<Rect>,
    width: u16,
    height: u16,
) -> Option<Popup> {
    layout_kind(target, focus, width, height, PopupKind::Preview)
}

fn layout_kind(
    target: &Target,
    focus: Option<Rect>,
    width: u16,
    height: u16,
    kind: PopupKind,
) -> Option<Popup> {
    if width < 6 || height < 6 {
        return None;
    }
    let source = kind.source(target);
    let (max_width, max_height, caption) = match kind {
        PopupKind::Details => (MAX_WIDTH, MAX_HEIGHT, "Enter/Esc close · j/k scroll"),
        PopupKind::Preview => {
            let natural = source
                .iter()
                .map(|(line, _)| {
                    line.chars()
                        .filter(|ch| !ch.is_control())
                        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0);
            (
                (natural.saturating_add(2).min(PREVIEW_MAX_WIDTH as usize) as u16).max(6),
                PREVIEW_MAX_HEIGHT,
                "",
            )
        }
    };
    let interior = Rect {
        x: 1,
        y: 1,
        width: width - 2,
        height: height - 2,
    };
    let build = |area: Rect| {
        let popup_width = area.width.min(max_width);
        let mut lines = highlight_and_wrap(&source, popup_width.saturating_sub(2) as usize);
        let popup_height = area
            .height
            .min(max_height)
            .min(lines.len().saturating_add(2).min(u16::MAX as usize) as u16);
        if matches!(kind, PopupKind::Preview) {
            let visible = popup_height.saturating_sub(2) as usize;
            if lines.len() > visible {
                lines.truncate(visible);
                if let Some(last) = lines.last_mut() {
                    *last = vec![DetailGlyph {
                        symbol: '…',
                        syntax: Syntax::Muted,
                    }];
                }
            }
        }
        Popup {
            rect: Rect {
                x: area.x,
                y: area.y,
                width: popup_width,
                height: popup_height,
            },
            lines,
            caption,
        }
    };
    if let Some(focus) = focus {
        let left_width = focus.x.saturating_sub(interior.x + 1);
        let right_x = focus.right().saturating_add(1).min(interior.right());
        let top_height = focus.y.saturating_sub(interior.y + 1);
        let bottom_y = focus.bottom().saturating_add(1).min(interior.bottom());
        // 稳定偏好右、左、下、上；在这些区域里比较可读宽度和可见行数。
        let areas = [
            Rect {
                x: right_x,
                width: interior.right() - right_x,
                ..interior
            },
            Rect {
                width: left_width,
                ..interior
            },
            Rect {
                y: bottom_y,
                height: interior.bottom() - bottom_y,
                ..interior
            },
            Rect {
                height: top_height,
                ..interior
            },
        ];
        let mut best = None;
        let mut best_score = (0, 0, 0);
        for (index, area) in areas.into_iter().enumerate() {
            if area.width < 6 || area.height < 4 {
                continue;
            }
            let mut popup = build(area);
            // 紧邻焦点，并沿另一轴对齐，所有坐标均保持在所选区域内。
            if index < 2 {
                popup.rect.y = focus.y.min(area.bottom() - popup.rect.height).max(area.y);
                if index == 1 {
                    popup.rect.x = area.right() - popup.rect.width;
                }
            } else {
                popup.rect.x = focus.x.min(area.right() - popup.rect.width).max(area.x);
                if index == 3 {
                    popup.rect.y = area.bottom() - popup.rect.height;
                }
            }
            let score = (
                popup.rect.width.min(24),
                popup.rect.width as usize * popup.rect.height as usize,
                4 - index,
            );
            if score > best_score {
                best_score = score;
                best = Some(popup);
            }
        }
        if best.is_some() {
            return best;
        }
    }
    let mut popup = build(interior);
    // 无法避让时选角落，尽可能减少和焦点的重叠。
    let positions = [
        (interior.x, interior.y),
        (interior.right() - popup.rect.width, interior.y),
        (interior.x, interior.bottom() - popup.rect.height),
        (
            interior.right() - popup.rect.width,
            interior.bottom() - popup.rect.height,
        ),
    ];
    let (x, y) = positions.into_iter().min_by_key(|&(x, y)| {
        let Some(focus) = focus else {
            return 0;
        };
        let right = (x + popup.rect.width).min(focus.right());
        let bottom = (y + popup.rect.height).min(focus.bottom());
        right.saturating_sub(x.max(focus.x)) as usize
            * bottom.saturating_sub(y.max(focus.y)) as usize
    })?;
    popup.rect.x = x;
    popup.rect.y = y;
    Some(popup)
}

/// 不执行或解释用户内容，只给结构标记、代码和强调着色；换行保留字形样式。
#[cfg(test)]
pub(super) fn detail_lines(target: &Target, width: usize) -> Vec<DetailLine> {
    highlight_and_wrap(&detail_source(target), width)
}

fn detail_source(target: &Target) -> Vec<(String, Syntax)> {
    let mut source = vec![
        (format!("# {}", target.id()), Syntax::Heading),
        (
            if target.is_public() {
                "public"
            } else {
                "private"
            }
            .into(),
            Syntax::Muted,
        ),
        (String::new(), Syntax::Plain),
        ("Description".into(), Syntax::Heading),
    ];
    source.extend(
        target
            .description()
            .lines()
            .map(|line| (line.to_owned(), Syntax::Plain)),
    );
    source.push((String::new(), Syntax::Plain));
    source.push(("Specifications".into(), Syntax::Heading));
    for spec in target.specifications() {
        source.extend(
            format!("- {}", spec.content())
                .lines()
                .map(|line| (line.to_owned(), Syntax::Specification)),
        );
    }
    source.push((String::new(), Syntax::Plain));
    source.push(("Dependencies".into(), Syntax::Heading));
    for dependency in target.dependencies() {
        source.push((format!("> {dependency}"), Syntax::Reference));
    }
    source
}

fn highlight_and_wrap(source: &[(String, Syntax)], width: usize) -> Vec<DetailLine> {
    let mut result = Vec::new();
    let mut fenced = false;
    for (line, base) in source {
        let base = *base;
        let trimmed = line.trim_start();
        let syntax = if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            Syntax::Code
        } else if fenced {
            Syntax::Code
        } else if base != Syntax::Plain {
            base
        } else if trimmed.starts_with('#') {
            Syntax::Heading
        } else if trimmed.starts_with("> ") {
            Syntax::Reference
        } else if trimmed.starts_with("- ") {
            Syntax::Specification
        } else {
            base
        };
        let mut wrapped = Vec::new();
        let mut used = 0;
        let mut inline_code = false;
        let mut emphasis = false;
        for ch in line.chars().filter(|ch| !ch.is_control()) {
            let size = UnicodeWidthChar::width(ch).unwrap_or(0);
            if size > width {
                continue;
            }
            if used + size > width {
                result.push(std::mem::take(&mut wrapped));
                used = 0;
            }
            let style = if syntax == Syntax::Code {
                Syntax::Code
            } else if ch == '`' {
                inline_code = !inline_code;
                Syntax::Code
            } else if inline_code {
                Syntax::Code
            } else if ch == '*' {
                emphasis = !emphasis;
                Syntax::Emphasis
            } else if emphasis {
                Syntax::Emphasis
            } else {
                syntax
            };
            wrapped.push(DetailGlyph {
                symbol: ch,
                syntax: style,
            });
            used += size;
        }
        result.push(wrapped);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ModulePath, TargetName};

    fn target() -> Target {
        let mut target = Target::new(ModulePath::root(), TargetName::parse("build").unwrap());
        target.set_description(
            "标题 `code` 与 *强调*\n```rust\nlet x = 1;\n```\n> crate::other\n- ready",
        );
        target
    }

    fn text(popup: &Popup) -> String {
        popup
            .lines
            .iter()
            .map(|line| line.iter().map(|glyph| glyph.symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn preview_contains_only_absolute_target_id_and_description() {
        use crate::core::{Spec, TargetId};
        let namespace = ModulePath::parse("module::nested").unwrap();
        let mut target = Target::new(namespace, TargetName::parse("build").unwrap());
        target.set_description("这是 `code` 与 *强调*。");
        target.add_specification(Spec::new("SPEC_SENTINEL"));
        target.add_dependency(TargetId::new(
            ModulePath::root(),
            TargetName::parse("DEPENDENCY_SENTINEL").unwrap(),
        ));
        let focus = Rect {
            x: 60,
            y: 15,
            width: 7,
            height: 1,
        };
        let popup = preview_layout(&target, Some(focus), 120, 40).unwrap();
        let text = text(&popup);
        assert_eq!(text, "module::nested::build\n\n这是 `code` 与 *强调*。");
        assert!(!popup.rect.intersects(focus));
        assert!(popup.rect.width <= PREVIEW_MAX_WIDTH && popup.rect.height <= PREVIEW_MAX_HEIGHT);
        assert!(
            popup
                .lines
                .iter()
                .flatten()
                .any(|glyph| glyph.syntax == Syntax::Code)
        );
        assert!(
            popup
                .lines
                .iter()
                .flatten()
                .any(|glyph| glyph.syntax == Syntax::Emphasis)
        );
        assert!(popup.caption.is_empty());
    }

    #[test]
    fn preview_is_compact_and_marks_truncated_description() {
        let mut target = target();
        target.set_description("a very long description line ".repeat(100));
        let popup = preview_layout(&target, None, 120, 40).unwrap();
        assert!(popup.rect.width <= PREVIEW_MAX_WIDTH && popup.rect.height <= PREVIEW_MAX_HEIGHT);
        assert!(text(&popup).ends_with('…'));
        assert_eq!(popup.max_scroll(), 0);
        target.set_description("");
        let popup = preview_layout(&target, None, 120, 40).unwrap();
        assert_eq!(text(&popup), "build");
        assert_eq!(popup.rect.height, 3);
    }

    #[test]
    fn highlighting_survives_unicode_wrapping() {
        let lines = detail_lines(&target(), 12);
        for line in &lines {
            assert!(
                line.iter()
                    .map(|glyph| UnicodeWidthChar::width(glyph.symbol).unwrap_or(0))
                    .sum::<usize>()
                    <= 12
            );
        }
        for syntax in [
            Syntax::Heading,
            Syntax::Code,
            Syntax::Emphasis,
            Syntax::Reference,
            Syntax::Specification,
        ] {
            assert!(lines.iter().flatten().any(|glyph| glyph.syntax == syntax));
        }
        assert!(
            lines
                .iter()
                .flatten()
                .any(|glyph| glyph.symbol == 'c' && glyph.syntax == Syntax::Code)
        );
    }

    #[test]
    fn popup_avoids_focus_at_center_and_each_edge() {
        for (width, height) in [(120, 40), (80, 24), (40, 12), (20, 8)] {
            for (x, y) in [(1, 1), (width / 2, height / 2), (width - 8, height - 3)] {
                let focus = Rect {
                    x,
                    y,
                    width: 6,
                    height: 1,
                };
                for popup in [
                    layout(&target(), Some(focus), width, height),
                    preview_layout(&target(), Some(focus), width, height),
                ] {
                    let popup = popup.unwrap();
                    assert!(
                        !popup.rect.intersects(focus),
                        "{width}x{height}: {:?} covers {focus:?}",
                        popup.rect
                    );
                    assert!(popup.rect.width <= MAX_WIDTH);
                    assert!(popup.rect.x >= 1 && popup.rect.right() < width);
                    assert!(popup.rect.y >= 1 && popup.rect.bottom() < height);
                }
            }
        }
    }

    #[test]
    fn tiny_screens_and_unavoidable_overlap_are_bounded() {
        for width in 0_u16..25 {
            for height in 0_u16..12 {
                let focus = (width >= 6 && height >= 6).then(|| Rect {
                    x: 1,
                    y: 1,
                    width: width.saturating_sub(2),
                    height: height.saturating_sub(2),
                });
                if let Some(popup) = layout(&target(), focus, width, height) {
                    assert!(popup.rect.right() < width && popup.rect.bottom() < height);
                }
            }
        }
    }
}
