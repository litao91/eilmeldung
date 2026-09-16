use crate::prelude::*;
use crate::ui::articles_list::model::ArticleListModelData;

use getset::{Getters, MutGetters};
use news_flash::models::{Article, ArticleFilter, Marked, Read};
use ratatui::layout::Constraint;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Table,
    TableState, Widget,
};
use strum::IntoEnumIterator;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How many lines a summary may occupy underneath the title.
const SUMMARY_LINES: usize = 2;

/// Assumed table width before the first render has reported the real one.
const FALLBACK_TABLE_WIDTH: u16 = 80;

/// Share of the leftover width the title column gets relative to other flexible columns, when it
/// also carries the two summary lines.
const TITLE_FLEX_WEIGHT: u16 = 3;

#[derive(Getters, MutGetters)]
#[getset(get = "pub(super)")]
pub struct FilterState {
    augmented_article_filter: Option<AugmentedArticleFilter>,

    #[get_mut = "pub(super)"]
    article_scope: ArticleScope,

    #[get_mut = "pub(super)"]
    article_search_query: Option<ArticleQuery>,

    #[get_mut = "pub(super)"]
    article_adhoc_filter: Option<ArticleQuery>,

    #[get_mut = "pub(super)"]
    adhoc_sort_order: Option<SortOrder>,

    #[get_mut = "pub(super)"]
    reverse_sort_order: bool,

    #[get_mut = "pub(super)"]
    apply_article_adhoc_filter: bool,

    #[get_mut = "pub(super)"]
    sticky_adhoc_filter: bool,
}

impl FilterState {
    pub fn new(article_scope: ArticleScope) -> Self {
        Self {
            article_scope,
            augmented_article_filter: None,
            article_search_query: None,
            article_adhoc_filter: None,
            adhoc_sort_order: None,
            apply_article_adhoc_filter: false,
            reverse_sort_order: false,
            sticky_adhoc_filter: false,
        }
    }

    pub(super) fn generate_effective_filter(&self) -> Option<ArticleFilter> {
        let augmented_article_filter = self.augmented_article_filter.as_ref()?;

        let mut article_filter = augmented_article_filter.article_filter.clone();

        // read/unread/marked etc comes from query
        if !augmented_article_filter.defines_scope() {
            match self.article_scope {
                ArticleScope::All => {}
                ArticleScope::Unread => {
                    article_filter.unread = Some(Read::Unread);
                    article_filter.marked = None;
                }
                ArticleScope::Marked => {
                    article_filter.marked = Some(Marked::Marked);
                    article_filter.unread = None;
                }
            }
        }
        Some(article_filter)
    }

    pub fn get_effective_scope(&self) -> Option<ArticleScope> {
        if let Some(augmented_article_filter) = self.augmented_article_filter.as_ref()
            && augmented_article_filter.defines_scope()
        {
            return None;
        }
        Some(self.article_scope)
    }

    pub fn uses_default_sort_order(&self) -> bool {
        self.adhoc_sort_order.is_none()
            && (self
                .augmented_article_filter
                .as_ref()
                .is_none_or(|filter| filter.article_query.sort_order().is_none()))
            && !*self.reverse_sort_order()
    }

    pub fn get_effective_sort_order(&self, config: &Config) -> SortOrder {
        self.adhoc_sort_order
            .as_ref()
            .or_else(|| {
                self.article_adhoc_filter
                    .as_ref()
                    .and_then(|filter| filter.sort_order().as_ref())
            })
            .or_else(|| {
                self.augmented_article_filter
                    .as_ref()
                    .and_then(|filter| filter.article_query.sort_order().as_ref())
            })
            .unwrap_or(&config.default_sort_order)
            .to_owned()
            .reverse(self.reverse_sort_order)
    }

    pub fn on_new_article_filter(&mut self, article_filter: AugmentedArticleFilter) {
        self.augmented_article_filter = Some(article_filter);
        self.apply_article_adhoc_filter = self.sticky_adhoc_filter;
    }

    pub fn on_new_article_adhoc_filter(
        &mut self,
        article_adhoc_filter: ArticleQuery,
        sticky: bool,
    ) {
        self.article_adhoc_filter = Some(article_adhoc_filter);
        self.apply_article_adhoc_filter = true;
        self.sticky_adhoc_filter = sticky;
    }

    pub fn clear_sort_order(&mut self) {
        self.adhoc_sort_order = None;
        self.reverse_sort_order = false;
    }
}

impl Widget for &mut ArticlesList {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer) {
        let (block, area) =
            self.view_data
                .gen_block(&self.config, &self.filter_state, self.is_focused, area);
        let inner = block.inner(area);

        // Taken from `inner` rather than derived from the panel area, so that it stays correct
        // whichever borders the framing happens to open.
        *self.view_data.article_lines_mut() = Some(inner.height);
        *self.view_data.article_width_mut() = Some(inner.width);

        StatefulWidget::render(
            &self.view_data.table,
            inner,
            buf,
            &mut self.view_data.table_state,
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .symbols(self.config.border_theme.scrollbar_set(self.is_focused))
            .style(self.config.theme.eff_border(self.is_focused));

        let scrollbar_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: block.inner(area).height,
        };

        block.render(area, buf);

        StatefulWidget::render(
            scrollbar,
            scrollbar_area,
            buf,
            &mut self.view_data.scrollbar_state,
        );
    }
}

#[derive(Default, Getters, MutGetters)]
#[getset(get = "pub(super)")]
pub struct ArticleListViewData<'a> {
    table: Table<'a>,
    #[getset(get_mut = "pub(super)")]
    table_state: TableState,

    #[getset(get_mut = "pub(super)")]
    scrollbar_state: ScrollbarState,

    #[getset(get_mut = "pub(super)", get = "pub(super)")]
    article_lines: Option<u16>,

    /// Width of the table's inner area as of the last render.
    ///
    /// Summaries have to be wrapped to the width of the column they land in, which is not known
    /// until the table has been laid out, so this carries it back to the next [`Self::update`].
    /// The same one-frame lag already applies to `article_lines`.
    #[getset(get_mut = "pub(super)")]
    article_width: Option<u16>,

    /// Height in terminal rows of every article row; more than one when summaries are shown.
    #[getset(get = "pub(super)")]
    row_height: u16,

    article_count: usize,
}

impl<'a> ArticleListViewData<'a> {
    fn build_title(&self, filter_state: &FilterState, config: &Config) -> Line<'static> {
        let mut title = Line::styled("", config.theme.header());
        let spans = &mut title.spans;

        if let Some(article_scope) = filter_state.get_effective_scope() {
            for scope in ArticleScope::iter() {
                let style = if scope == article_scope {
                    config.theme.header()
                } else {
                    config.theme.inactive()
                };
                spans.push(" ".into());
                spans.push(Span::styled(scope.to_icon(config).to_string(), style));
            }
            spans.push(" ".into());
        }

        let filter_info = match filter_state.article_adhoc_filter {
            Some(_) if filter_state.apply_article_adhoc_filter => "  ",
            Some(_) => "  ",
            _ => "",
        };

        spans.push(Span::styled(filter_info, config.theme.header()));

        if !config.hide_default_sort_order || !filter_state.uses_default_sort_order() {
            let filter_text = &format!(
                " {} {} ",
                if *filter_state.reverse_sort_order() {
                    config.icon_set.sort_reversed_icon()
                } else {
                    config.icon_set.sort_normal_icon()
                },
                filter_state
                    .get_effective_sort_order(config)
                    .as_string(config)
            );
            spans.push(Span::styled(filter_text.to_owned(), config.theme.header()));
        }

        title
    }

    fn build_position(&self, config: &Config) -> Line<'static> {
        if self.article_count > 0 && config.article_list_show_position {
            let selected = self.table_state.selected().unwrap_or(0).saturating_add(1);
            let all = self.article_count;
            Line::styled(format!(" {selected}/{all} ",), config.theme.header())
        } else {
            "".into()
        }
    }

    pub fn update(
        &mut self,
        config: &Config,
        model_data: &ArticleListModelData,
        filter_state: &FilterState,
        _is_focused: bool,
    ) {
        let selected_style = config.theme.selected(&Default::default());

        let read_icon = config.icon_set.read_icon().to_string();
        let unread_icon = config.icon_set.unread_icon().to_string();
        let marked_icon = config.icon_set.marked_icon().to_string();
        let unmarked_icon = config.icon_set.unmarked_icon().to_string();

        let configured: Vec<&str> = config
            .article_table
            .split(",")
            .map(|placeholder| placeholder.trim())
            .collect();

        // `{summary}` is not a column of its own: it adds two lines underneath the title. It only
        // falls back to being a column when there is no `{title}` to attach it to.
        let show_summary = configured.contains(&"{summary}");
        let summary_is_column = show_summary && !configured.contains(&"{title}");
        let summary_in_title = show_summary && !summary_is_column;
        let placeholders: Vec<&str> = configured
            .into_iter()
            .filter(|placeholder| *placeholder != "{summary}" || summary_is_column)
            .collect();

        // The tag icon column is sized by the widest tag set, and the summary needs the resulting
        // column widths to know how far it may run, so both are settled before any row is built.
        let max_tags = model_data
            .articles()
            .iter()
            .filter_map(|article| model_data.tags_for_article().get(&article.article_id))
            .map(Vec::len)
            .max()
            .unwrap_or(0) as u16;

        let fixed_width = |placeholder: &str| -> Option<u16> {
            if placeholder == "{read}"
                || placeholder == "{marked}"
                || (placeholder == "{flagged}" && !model_data.flagged_articles().is_empty())
            {
                Some(2)
            } else if placeholder == "{flagged}" {
                Some(0)
            } else if placeholder == "{age}" {
                Some(4)
            } else if placeholder == "{date}" {
                Some(config.date_format.len() as u16)
            } else if placeholder == "{tag_icons}" {
                Some(max_tags)
            } else {
                None
            }
        };

        // Relative share of the leftover width for a column that has no fixed width. The title
        // column also carries the two summary lines, so it needs a bigger share than a column
        // holding a single line; without this a long `{url}` beside it squeezes the summary.
        let flex_weight = |placeholder: &str| -> u16 {
            if summary_in_title && placeholder == "{title}" {
                TITLE_FLEX_WEIGHT
            } else {
                1
            }
        };

        let summary_width = if show_summary {
            self.summary_width(
                &placeholders,
                &fixed_width,
                &flex_weight,
                if summary_in_title {
                    "{title}"
                } else {
                    "{summary}"
                },
            )
        } else {
            0
        };

        // Every row gets the same height so that a screen line can be turned back into a row index
        // by division; walking per-row heights would be needed otherwise. A list in which no
        // article has a summary keeps the compact single-line rows.
        let row_height = if summary_width > 0
            && model_data
                .articles()
                .iter()
                .any(|article| article.title.is_some() && !Self::summary_of(article).is_empty())
        {
            SUMMARY_LINES as u16 + 1
        } else {
            1
        };
        self.row_height = row_height;

        let summary_style = Style::new().add_modifier(Modifier::DIM);

        let entries: Vec<Row> = model_data
            .articles()
            .iter()
            .map(|article| {
                let row_vec: Vec<Text> = placeholders
                    .iter()
                    .map(|placeholder| match *placeholder {
                        "{title}" => {
                            let mut lines = vec![Line::from(html_sanitize(
                                article
                                    .title
                                    .as_deref()
                                    .or(article.summary.as_deref())
                                    .unwrap_or("no title and summary"),
                            ))];
                            // The title falls back to the summary when an article has none, so
                            // showing the summary underneath as well would repeat it.
                            if show_summary && !summary_is_column && article.title.is_some() {
                                lines.extend(Self::summary_lines(
                                    article,
                                    summary_width,
                                    summary_style,
                                ));
                            }
                            Text::from(lines)
                        }
                        "{summary}" => {
                            Text::from(Self::summary_lines(article, summary_width, summary_style))
                        }
                        "{tag_icons}" => Text::from(Line::from(
                            match model_data.tags_for_article().get(&article.article_id) {
                                Some(tag_ids) => tag_ids
                                    .iter()
                                    .map(|tag_id| {
                                        let Some(tag) = model_data.tag_map().get(tag_id) else {
                                            return Span::from("");
                                        };

                                        let style = match NewsFlashUtils::tag_color(tag) {
                                            Some(color) => config.theme.tag().fg(color),
                                            None => config.theme.tag(),
                                        };
                                        Span::styled(config.icon_set.tag_icon().to_string(), style)
                                    })
                                    .collect::<Vec<Span>>(),
                                None => vec![Span::from("")],
                            },
                        )),
                        "{author}" => {
                            html_sanitize(article.author.as_deref().unwrap_or("no author")).into()
                        }
                        "{feed}" => html_sanitize(
                            model_data
                                .feed_map()
                                .get(&article.feed_id)
                                .map(|feed| feed.label.as_str())
                                .unwrap_or("unknown feed"),
                        )
                        .into(),
                        "{date}" => article
                            .date
                            .with_timezone(&chrono::Local)
                            .format(&config.date_format)
                            .to_string()
                            .into(),
                        "{age}" => {
                            let now = chrono::Utc::now();
                            let duration = now.signed_duration_since(article.date);

                            let weeks = duration.num_weeks();
                            let days = duration.num_days();
                            let hours = duration.num_hours();
                            let minutes = duration.num_minutes();
                            let seconds = duration.num_seconds();

                            if weeks > 0 {
                                format!("{:>2}w", weeks)
                            } else if days > 0 {
                                format!("{:>2}d", days)
                            } else if hours > 0 {
                                format!("{:>2}h  ", hours)
                            } else if minutes > 0 {
                                format!("{:>2}m", minutes)
                            } else {
                                format!("{:>2}s", seconds)
                            }
                        }
                        .into(),
                        "{read}" => if article.unread == Read::Read {
                            format!(" {}", read_icon)
                        } else {
                            format!(" {}", unread_icon)
                        }
                        .into(),
                        "{marked}" => if article.marked == Marked::Marked {
                            format!(" {}", marked_icon)
                        } else {
                            format!(" {}", unmarked_icon)
                        }
                        .into(),
                        "{url}" => article
                            .url
                            .as_ref()
                            .map(|url| url.to_string())
                            .unwrap_or("?".into())
                            .into(),
                        "{flagged}" => if model_data.flagged_articles().is_empty() {
                            "".to_string()
                        } else if model_data.flagged_articles().contains(&article.article_id) {
                            format!(" {}", config.icon_set.flagged_icon())
                        } else {
                            "  ".to_string()
                        }
                        .into(),
                        _ => format!("{placeholder}?").into(),
                    })
                    .collect();

                let mut style = match filter_state.article_search_query.as_ref() {
                    Some(query)
                        if query.test(
                            article,
                            &ArticleQueryContext {
                                feed_map: model_data.feed_map(),
                                category_for_feed: model_data.category_for_feed(),
                                tags_for_article: model_data.tags_for_article(),
                                tag_map: model_data.tag_map(),
                                last_sync: model_data.last_sync(),
                                flagged: model_data.flagged_articles(),
                            },
                        ) =>
                    {
                        config.theme.highlighted(&config.theme.article())
                    }
                    _ => config.theme.article(),
                };

                style = if article.unread == Read::Read {
                    config.theme.read(&style)
                } else {
                    config.theme.unread(&style)
                };

                if model_data.flagged_articles().contains(&article.article_id) {
                    style = config.theme.flagged(&style);
                }

                Row::new(row_vec).style(style).height(row_height)
            })
            .collect();

        let constraint_for_placeholder = |placeholder: &str| match fixed_width(placeholder) {
            Some(width) => Constraint::Length(width),
            // `Fill` rather than `Min` so that the shares are proportional to `flex_weight`.
            None if summary_in_title => Constraint::Fill(flex_weight(placeholder)),
            None => Constraint::Min(1),
        };

        self.scrollbar_state = self
            .scrollbar_state
            .content_length(entries.len())
            .position(0);

        self.article_count = entries.len();

        self.table = Table::new(
            entries,
            placeholders
                .iter()
                .map(|placeholder| constraint_for_placeholder(placeholder))
                .collect::<Vec<Constraint>>(),
        )
        .row_highlight_style(selected_style);
    }

    /// Width of the column the summary is rendered into.
    ///
    /// The table is only laid out when it renders, so this mirrors ratatui's own distribution: the
    /// fixed-width columns take their length and what is left is shared in proportion to
    /// `flex_weight`.
    fn summary_width(
        &self,
        placeholders: &[&str],
        fixed_width: &impl Fn(&str) -> Option<u16>,
        flex_weight: &impl Fn(&str) -> u16,
        target: &str,
    ) -> u16 {
        let mut used = 0u32;
        let mut total_weight = 0u32;

        for placeholder in placeholders {
            match fixed_width(placeholder) {
                Some(width) => used += u32::from(width),
                None => total_weight += u32::from(flex_weight(placeholder)),
            }
        }

        let remaining =
            u32::from(self.article_width.unwrap_or(FALLBACK_TABLE_WIDTH)).saturating_sub(used);
        let share = remaining * u32::from(flex_weight(target)) / total_weight.max(1);

        // One column short so that a wide character or the ellipsis is never clipped at the edge.
        u16::try_from(share).unwrap_or(u16::MAX).saturating_sub(1)
    }

    /// The article's summary with markup decoded and all whitespace collapsed onto one line.
    fn summary_of(article: &Article) -> String {
        let Some(summary) = article.summary.as_deref() else {
            return String::new();
        };

        html_sanitize(summary)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The summary wrapped to at most [`SUMMARY_LINES`] lines, the last one ellipsized if the text
    /// did not fit.
    fn summary_lines(article: &Article, width: u16, style: Style) -> Vec<Line<'static>> {
        let summary = Self::summary_of(article);
        if summary.is_empty() {
            return Vec::new();
        }

        let mut lines = wrap_spans(&[Span::styled(summary, style)], width);
        if lines.len() <= SUMMARY_LINES {
            return lines;
        }

        lines.truncate(SUMMARY_LINES);
        if let Some(last) = lines.last_mut() {
            Self::ellipsize(last, style, width);
        }
        lines
    }

    /// Cut a line back to `width` columns, ending it with an ellipsis.
    fn ellipsize(line: &mut Line<'static>, style: Style, width: u16) {
        let budget = (width as usize).saturating_sub(UnicodeWidthStr::width("…"));
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        let mut kept = String::new();
        let mut used = 0usize;
        for character in text.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used + character_width > budget {
                break;
            }
            kept.push(character);
            used += character_width;
        }
        kept.push('…');

        line.spans = vec![Span::styled(kept, style)];
    }

    pub(super) fn gen_block(
        &self,
        config: &Config,
        filter_state: &FilterState,
        is_focused: bool,
        area: Rect,
    ) -> (Block<'static>, Rect) {
        let borders = config.border_theme.framing.eff_borders_open(Borders::RIGHT);

        let enlarged_area = config.border_theme.framing.eff_area(Borders::RIGHT, area);

        (
            Block::default()
                .borders(borders)
                .title_top(
                    self.build_title(filter_state, config)
                        .alignment(HorizontalAlignment::Left),
                )
                .title_top(
                    self.build_position(config)
                        .alignment(HorizontalAlignment::Right),
                )
                .title_alignment(ratatui::layout::Alignment::Left)
                .border_type(config.border_theme.eff_type(is_focused))
                .merge_borders(config.border_theme.framing.eff_merge_strategy())
                .border_style(config.theme.eff_border(is_focused)),
            enlarged_area,
        )
    }

    pub(super) fn get_table_state_mut(&mut self) -> &mut TableState {
        &mut self.table_state
    }

    pub(super) fn get_table_state(&self) -> &TableState {
        &self.table_state
    }
}
