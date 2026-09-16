use crate::prelude::*;

use ratatui::{layout::Flex, prelude::*};
use throbber_widgets_tui::Throbber;

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // render status bar
        let area = self.render_status_bar(area, buf);

        // render command line (if visible)
        let (panels_chunk, command_line_chunk) = self.compute_command_line_chunk(area.to_owned());

        // render the main panels
        self.render_panels(panels_chunk, buf);

        self.render_command_line(buf, command_line_chunk);

        // and finally the pop if visible
        if self.help_popup.is_visible() {
            self.help_popup.render(area, buf);
        }
    }
}

impl App {
    fn render_status_bar(&self, area: Rect, buf: &mut Buffer) -> Rect {
        // in zen mode we only lower status bar if the tooltip is Warning or Error
        let render_bottom_bar = !matches!(self.state, AppState::ArticleContentDistractionFree)
            || matches!(
                self.tooltip.flavor,
                TooltipFlavor::Warning | TooltipFlavor::Error
            );

        let [middle, bottom] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0), // Middle: takes remaining space
                Constraint::Length(if render_bottom_bar { 1 } else { 0 }), // Bottom: fixed 1 line
            ])
            .areas(area);

        if render_bottom_bar {
            // fill top line with status bar color
            Block::default()
                .style(self.config.theme.statusbar())
                .render(bottom, buf);

            let tooltip_line = self.tooltip.to_line(&self.config);
            let left_icon = self.config.icon_set.status_bar_left_icon();
            let right_icon = self.config.icon_set.status_bar_right_icon();

            let [bottom_left, bottom_main, status, bottom_right] = Layout::default()
                .direction(Direction::Horizontal)
                .flex(Flex::Center)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(bottom.width.saturating_sub(3)),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .areas::<4>(bottom);

            let status_span = if self.is_offline {
                // when offline display offline icon
                Span::styled(
                    format!("{} ", self.config.icon_set.offline_icon()),
                    tooltip_line.style,
                )
            } else {
                // when online display throbber
                if self.news_flash_utils.is_async_operation_running() {
                    Throbber::default()
                        .style(tooltip_line.style)
                        .throbber_style(tooltip_line.style)
                        .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
                        .use_type(throbber_widgets_tui::WhichUse::Spin)
                        .to_symbol_span(&self.async_operation_throbber)
                } else {
                    Span::styled(" ", tooltip_line.style)
                }
            };

            Span::styled(
                left_icon.to_string(),
                if left_icon != ' ' {
                    tooltip_line.style.not_reversed()
                } else {
                    tooltip_line.style.reversed()
                },
            )
            .render(bottom_left, buf);
            Span::styled(
                right_icon.to_string(),
                if right_icon != ' ' {
                    tooltip_line.style.not_reversed()
                } else {
                    tooltip_line.style.reversed()
                },
            )
            .render(bottom_right, buf);
            tooltip_line.render(bottom_main, buf);
            status_span.render(status, buf);
        }

        middle
    }

    fn render_command_line(&mut self, buf: &mut Buffer, command_line_chunk: Rect) {
        if self.command_input.is_active() {
            self.command_input.render(command_line_chunk, buf);
        } else if self.command_confirm.is_active() {
            self.command_confirm.render(command_line_chunk, buf);
        }
    }

    fn compute_command_line_chunk(&mut self, area: Rect) -> (Rect, Rect) {
        if self.command_input.is_active() || self.command_confirm.is_active() {
            let [panels_chunk, command_line_chunk] =
                Layout::vertical(vec![Constraint::Min(0), Constraint::Length(3)]).areas::<2>(area);

            (panels_chunk, command_line_chunk)
        } else {
            (area, Default::default())
        }
    }

    fn render_panels(&mut self, area: Rect, buf: &mut Buffer) {
        if self.state == AppState::ArticleContentDistractionFree {
            self.article_content.render(area, buf);
            *self.panel_areas.feed_list_mut() = Rect::default(); // 0 0 0 0
            *self.panel_areas.articles_list_mut() = Rect::default(); // 0 0 0 0
            *self.panel_areas.article_content_mut() = area;
            return;
        }

        // The focused panel takes its configured width; the other two share what is left in
        // proportion to their own configured widths, so a configuration whose widths add up to
        // 100% looks the same whichever panel is focused.
        let (feeds_constraint_width, articles_constraint_width, content_constraint_width) =
            match self.state {
                AppState::FeedSelection => (
                    self.config.feed_list_focused_width.as_constraint(),
                    Constraint::Fill(self.config.article_list_focused_width.as_fill_weight()),
                    Constraint::Fill(self.config.article_content_focused_width.as_fill_weight()),
                ),
                AppState::ArticleSelection => (
                    Constraint::Fill(self.config.feed_list_focused_width.as_fill_weight()),
                    self.config.article_list_focused_width.as_constraint(),
                    Constraint::Fill(self.config.article_content_focused_width.as_fill_weight()),
                ),
                _ => (
                    Constraint::Fill(self.config.feed_list_focused_width.as_fill_weight()),
                    Constraint::Fill(self.config.article_list_focused_width.as_fill_weight()),
                    self.config.article_content_focused_width.as_constraint(),
                ),
            };

        // Dragging the border between the article list and the content pins the former's width.
        let (articles_constraint_width, content_constraint_width) =
            match self.articles_width_override {
                Some(override_width) => (Constraint::Length(override_width), Constraint::Min(0)),
                None => (articles_constraint_width, content_constraint_width),
            };

        let [feeds_list_chunk, articles_list_chunk, article_content_chunk] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                feeds_constraint_width,
                articles_constraint_width,
                content_constraint_width,
            ])
            .areas::<3>(area);

        // store areas for mouse hit-testing
        *self.panel_areas.feed_list_mut() = feeds_list_chunk;
        *self.panel_areas.articles_list_mut() = articles_list_chunk;
        *self.panel_areas.article_content_mut() = article_content_chunk;

        if !self.feed_list.is_focused() && feeds_list_chunk.area() > 0 {
            self.feed_list.render(feeds_list_chunk, buf);
        }
        if !self.articles_list.is_focused() && articles_list_chunk.area() > 0 {
            self.articles_list.render(articles_list_chunk, buf);
        }
        if !self.article_content.is_focused() && article_content_chunk.area() > 0 {
            self.article_content.render(article_content_chunk, buf);
        }

        // render the focused panel last so that its border drawn over the other borders
        if self.feed_list.is_focused() && feeds_list_chunk.area() > 0 {
            self.feed_list.render(feeds_list_chunk, buf);
        } else if self.articles_list.is_focused() && articles_list_chunk.area() > 0 {
            self.articles_list.render(articles_list_chunk, buf);
        } else if self.article_content.is_focused() && article_content_chunk.area() > 0 {
            self.article_content.render(article_content_chunk, buf);
        }
    }
}
