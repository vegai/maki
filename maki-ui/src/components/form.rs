use crate::theme::Theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

pub(crate) const CLIPPED_CONTENT: &str = " more content below ";

pub(crate) fn render_form(
    t: &Theme,
    title: &str,
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: (u16, u16),
) {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .title_top(Line::from(title.to_string()).left_aligned())
        .title_style(t.panel_title);

    let paragraph = Paragraph::new(lines)
        .style(Style::new().fg(t.foreground))
        .wrap(Wrap { trim: false })
        .scroll(scroll);
    let visible_rows = usize::from(area.height.saturating_sub(2)) + usize::from(scroll.0);
    if paragraph.line_count(area.width.saturating_sub(2)) > visible_rows {
        block = block.title_bottom(Line::from(CLIPPED_CONTENT).right_aligned());
    }
    frame.render_widget(paragraph.block(block), area);
}

pub(crate) fn selected_prefix(t: &Theme, is_selected: bool) -> (&'static str, Style) {
    if is_selected {
        ("▸ ", t.active)
    } else {
        ("  ", Style::new().fg(t.foreground))
    }
}
