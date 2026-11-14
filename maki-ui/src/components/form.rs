use crate::components::modal::BORDER;
use crate::theme::Theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

pub(crate) const FORM_WRAP: Wrap = Wrap { trim: false };
pub(crate) const CLIPPED_CONTENT: &str = " more content below ";

pub(crate) fn content_rows(paragraph: &Paragraph<'_>, width: u16) -> u16 {
    u16::try_from(paragraph.line_count(width.saturating_sub(BORDER))).unwrap_or(u16::MAX)
}

pub(crate) fn render_form<'a>(
    t: &Theme,
    title: &'a str,
    frame: &mut Frame,
    area: Rect,
    paragraph: Paragraph<'a>,
    scroll: (u16, u16),
    total: u16,
) -> Rect {
    let clipped = total > area.height.saturating_sub(BORDER).saturating_add(scroll.0);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .title_top(Line::from(title).left_aligned())
        .title_style(t.panel_title);
    if clipped {
        block = block.title_bottom(Line::from(CLIPPED_CONTENT).right_aligned());
    }

    let inner = block.inner(area);
    let paragraph = paragraph
        .style(Style::new().fg(t.foreground))
        .wrap(FORM_WRAP)
        .scroll(scroll);
    frame.render_widget(paragraph.block(block), area);
    inner
}

pub(crate) fn selected_prefix(t: &Theme, is_selected: bool) -> (&'static str, Style) {
    if is_selected {
        ("▸ ", t.active)
    } else {
        ("  ", Style::new().fg(t.foreground))
    }
}
