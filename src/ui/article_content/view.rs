use super::content_layout::{
    ContentImageRef, ContentRow, IMAGE_SENTINEL, build_rows, image_cell_size,
};
use super::model::ArticleContentModelData;
use crate::prelude::*;

use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    sync::{Arc, Mutex},
};

use getset::{Getters, MutGetters};
use image::ImageReader;
use log::info;
use news_flash::models::Enclosure;
use ratatui::layout::{Flex, Size};
use ratatui_image::{
    FilterType, FontSize, Resize, StatefulImage,
    picker::Picker,
    protocol::StatefulProtocol,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use the_other_tui_markdown::RendererBuilder;
use throbber_widgets_tui::{Throbber, ThrobberState, WhichUse};

const NO_THUMB_PLACEHOLDER: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/no-thumb.png"));

/// Everything a cached row layout was derived from. Any difference means the rows are stale.
///
/// Config changes that affect rendering are deliberately not part of the key: `ArticleContent`
/// calls [`ArticleContentViewData::invalidate_layout`] on `ConfigReloaded` instead, which avoids
/// comparing a whole `Config` or keying on a pointer that could be reused after a free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutKey {
    content_generation: u64,
    width: u16,
    font_width: u16,
    font_height: u16,
    max_image_rows: u16,
    show_images: bool,
}

/// A content image encoded for one specific cell size and picker generation.
struct BuiltImage {
    protocol: SlicedProtocol,
    columns: u16,
    rows: u16,
    picker_generation: u64,
}

/// Scratch space shared with the markdown renderer's image hook.
struct ImageHookState {
    /// URLs that are already downloaded, with their pixel dimensions.
    loaded: HashMap<String, (u32, u32)>,
    /// Images the hook decided to draw, in document order.
    ordered: Vec<ContentImageRef>,
    /// Every image URL the document referred to, in document order.
    discovered: Vec<String>,
}

#[derive(Getters, MutGetters)]
pub struct ArticleContentViewData {
    // Scroll state
    #[getset(get = "pub(super)", get_mut = "pub(super)")]
    vertical_scroll: u16,
    #[getset(get = "pub(super)")]
    max_scroll: u16,

    #[getset(get = "pub(super), get_mut = "pub(super))]
    scrollbar_state: ScrollbarState,

    // Image rendering state
    image: Option<StatefulProtocol>,
    placeholder_image: StatefulProtocol,
    picker: Picker,

    // Throbber state for loading animations
    thumbnail_fetching_throbber: ThrobberState,

    #[getset(get = "pub(super)")]
    url_for_hint: HashMap<String, String>,

    // Cached row layout. Rendering the markdown used to happen on every frame; it now happens
    // only when `layout_key` changes, which makes steady-state frames cheaper than before.
    rows: Vec<ContentRow>,
    total_height: u16,
    layout_key: Option<LayoutKey>,

    // Inline image bookkeeping belonging to the cached layout.
    content_image_order: Vec<ContentImageRef>,
    discovered_image_urls: Vec<String>,
    content_image_protocols: HashMap<String, BuiltImage>,
    /// URLs whose encoding failed, so that they are not retried on every frame.
    failed_protocols: HashSet<String>,
    /// Sizes recorded while painting, drained and encoded between frames so that the blocking
    /// encode never happens on the render path.
    protocol_requests: HashSet<(String, u16, u16)>,
    picker_generation: u64,
    rendered_inline_images: bool,
}

impl Default for ArticleContentViewData {
    fn default() -> Self {
        let picker = Picker::from_query_stdio().unwrap();
        let cursor = Cursor::new(NO_THUMB_PLACEHOLDER);
        let placeholder_image = picker.new_resize_protocol(
            ImageReader::new(cursor)
                .with_guessed_format()
                .unwrap() // OK as content is checked
                .decode()
                .unwrap(), // OK as content is checked
        );

        Self {
            vertical_scroll: 0,
            max_scroll: 0,
            image: None,
            placeholder_image,
            picker, // TODO gracefully handle errors
            thumbnail_fetching_throbber: ThrobberState::default(),
            scrollbar_state: ScrollbarState::default(),
            url_for_hint: Default::default(),
            rows: Vec::new(),
            total_height: 0,
            layout_key: None,
            content_image_order: Vec::new(),
            discovered_image_urls: Vec::new(),
            content_image_protocols: HashMap::new(),
            failed_protocols: HashSet::new(),
            protocol_requests: HashSet::new(),
            picker_generation: 0,
            rendered_inline_images: false,
        }
    }
}

impl ArticleContentViewData {
    pub(super) fn update(&mut self, model_data: &ArticleContentModelData, _config: Arc<Config>) {
        if model_data.article().is_some() && self.vertical_scroll > self.max_scroll {
            self.vertical_scroll = 0;
        }
    }

    pub(super) fn set_image(&mut self, image: Option<StatefulProtocol>) {
        self.image = image;
    }

    pub(super) fn clear_image(&mut self) {
        self.image = None;
    }

    pub(super) fn reset_thumbnail_throbber(&mut self) {
        self.thumbnail_fetching_throbber.calc_next();
    }

    // Public accessors for private fields
    pub(super) fn picker(&self) -> &Picker {
        &self.picker
    }

    pub(super) fn image(&self) -> &Option<StatefulProtocol> {
        &self.image
    }

    pub(super) fn tick_throbber(&mut self) {
        self.thumbnail_fetching_throbber.calc_next();
    }

    pub(super) fn scroll_up(&mut self) {
        self.vertical_scroll = self.vertical_scroll.saturating_sub(1);
    }

    pub(super) fn scroll_down(&mut self) {
        self.vertical_scroll = (self.vertical_scroll + 1).min(self.max_scroll);
    }

    pub(super) fn scroll_page_up(&mut self, scroll_amount: u16) {
        self.vertical_scroll = self.vertical_scroll.saturating_sub(scroll_amount);
    }

    pub(super) fn scroll_page_down(&mut self, scroll_amount: u16) {
        self.vertical_scroll = (self.vertical_scroll + scroll_amount).min(self.max_scroll);
    }

    pub(super) fn scroll_to_top(&mut self) {
        self.vertical_scroll = 0;
    }

    pub(super) fn scroll_to_bottom(&mut self) {
        self.vertical_scroll = self.max_scroll;
    }

    pub(super) fn render_block(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        config: &Config,
        is_focused: bool,
    ) -> Rect {
        let block = Block::default()
            .borders(Borders::all())
            .border_type(config.border_theme.eff_type(is_focused))
            .merge_borders(config.border_theme.framing.eff_merge_strategy())
            .border_style(if is_focused {
                config.theme.border_focused()
            } else {
                config.theme.border()
            })
            .title_bottom(
                if self.max_scroll > 0 && config.content_show_position {
                    // u32 because a long article can have more than 655 wrapped rows, which would
                    // overflow the u16 multiplication.
                    let percent =
                        (u32::from(self.vertical_scroll) * 100) / u32::from(self.max_scroll);
                    Line::styled(format!(" {percent}% "), config.theme.header())
                } else {
                    "".into()
                }
                .alignment(HorizontalAlignment::Right),
            );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .symbols(config.border_theme.scrollbar_set(is_focused))
            .style(config.theme.eff_border(is_focused));

        self.scrollbar_state = self
            .scrollbar_state
            .position(self.vertical_scroll as usize)
            .content_length(self.max_scroll as usize);

        let inner_area = block.inner(area);
        block.render(area, buf);
        StatefulWidget::render(
            scrollbar,
            area.inner(Margin {
                horizontal: 0,
                vertical: 1,
            }),
            buf,
            &mut self.scrollbar_state,
        );
        inner_area
    }

    pub(super) fn generate_header<'a>(
        &'a self,
        model_data: &'a ArticleContentModelData,
        config: &'a Config,
    ) -> Vec<Line<'a>> {
        let Some(article) = model_data.article() else {
            return vec![];
        };

        let title = html_sanitize(article.title.as_deref().unwrap_or("unknown title"));
        let feed_label: String = if let Some(feed) = model_data.feed() {
            html_sanitize(&feed.label)
        } else {
            article.feed_id.as_str().into()
        };

        let tags = model_data.tags().as_deref().unwrap_or_default();
        let mut tags_and_enclosures = tags
            .iter()
            .flat_map(|tag| {
                let mut line = NewsFlashUtils::tag_to_line(tag, config, None);
                line.spans.push(Span::from(" "));
                line
            })
            .collect::<Vec<Span>>();

        let enclosures = model_data.enclosures().clone().unwrap_or_default();

        tags_and_enclosures.append(&mut to_enclosure_bubble(
            config,
            &enclosures,
            |enclosure| enclosure.is_video(),
            config.icon_set.enclosure_video_icon(),
        ));

        tags_and_enclosures.append(&mut to_enclosure_bubble(
            config,
            &enclosures,
            |enclosure| enclosure.is_image(),
            config.icon_set.enclosure_image_icon(),
        ));

        tags_and_enclosures.append(&mut to_enclosure_bubble(
            config,
            &enclosures,
            |enclosure| enclosure.is_audio(),
            config.icon_set.enclosure_audio_icon(),
        ));

        let author = html_sanitize(
            article
                .author
                .as_deref()
                .map(|author| format!(" by {author}"))
                .as_deref()
                .unwrap_or(""),
        );

        let date_string: String = article
            .date
            .with_timezone(&chrono::Local)
            .format(&config.date_format)
            .to_string();

        let mut title_line = Line::from(vec![
            Span::from(date_string).style(config.theme.header()),
            Span::from("  ").style(config.theme.header()),
            Span::from(feed_label).style(config.theme.header()),
        ]);

        if model_data.filtered_markdown_content().is_some() {
            title_line.spans.push(" ".into());
            title_line.spans.append(
                &mut to_bubble(
                    Span::styled(
                        config.icon_set.piped_icon().to_string(),
                        config.theme.highlighted(&Default::default()),
                    ),
                    config,
                )
                .spans,
            );
        }

        let summary_lines = vec![
            title_line,
            Line::styled(title, config.theme.paragraph()),
            Line::styled(author, config.theme.paragraph()),
            Line::from(tags_and_enclosures),
        ];

        summary_lines
    }

    pub(super) fn render_header(
        &mut self,
        model_data: &ArticleContentModelData,
        config: &Config,
        inner_area: Rect,
        buf: &mut Buffer,
    ) {
        let thumbnail_constraint = if config.thumbnail_show {
            config.thumbnail_width.as_constraint()
        } else {
            Constraint::Length(0)
        };

        let [thumbnail_chunk, header_chunk] = Layout::default()
            .direction(Direction::Horizontal)
            .flex(ratatui::layout::Flex::Start)
            .constraints(vec![thumbnail_constraint, Constraint::Min(1)])
            // .margin(1)
            .spacing(1)
            .areas::<2>(inner_area);

        if config.thumbnail_show {
            self.render_thumbnail(model_data, config, thumbnail_chunk, buf);
        }

        let header_lines = self.generate_header(model_data, config);
        let paragraph = Paragraph::new(header_lines).wrap(Wrap { trim: true });
        paragraph.render(header_chunk, buf);
    }

    pub(super) fn render_summary(
        &mut self,
        model_data: &ArticleContentModelData,
        config: &Config,
        inner_area: Rect,
        buf: &mut Buffer,
    ) {
        let Some(article) = model_data.article() else {
            return;
        };

        let [header_chunk, summary_chunk] = Layout::default()
            .direction(Direction::Vertical)
            .flex(ratatui::layout::Flex::Start)
            .constraints([
                config.thumbnail_height.as_constraint(),
                config
                    .thumbnail_height
                    .as_complementary_constraint(inner_area.width.saturating_sub(3)),
            ])
            .horizontal_margin(2)
            .vertical_margin(1)
            .spacing(1)
            .areas::<2>(inner_area);

        self.scrollbar_state = ScrollbarState::default();
        self.render_header(model_data, config, header_chunk, buf);

        let mut summary = article.summary.clone().unwrap_or("".into());
        summary = ArticleContentModelData::clean_string(&summary);
        let summary_paragraph = Paragraph::new(Line::from(
            Span::from(summary).style(config.theme.paragraph()),
        ))
        .wrap(Wrap { trim: true });
        summary_paragraph.render(summary_chunk, buf);
    }

    pub(super) fn render_thumbnail(
        &mut self,
        model_data: &ArticleContentModelData,
        config: &Config,
        thumbnail_chunk: Rect,
        buf: &mut Buffer,
    ) {
        let centered_layout = Layout::default()
            .direction(Direction::Horizontal)
            .flex(Flex::Center);

        match &mut self.image {
            Some(image) => {
                let mut stateful_image = StatefulImage::new();
                if config.thumbnail_resize {
                    stateful_image =
                        stateful_image.resize(Resize::Scale(Some(FilterType::Lanczos3)));
                }
                let [centered_chunk] = centered_layout
                    .constraints([Constraint::Fill(1)])
                    .areas(thumbnail_chunk);
                stateful_image.render(centered_chunk, buf, image);
            }
            None if *model_data.thumbnail_fetch_running()
                || model_data.thumbnail_fetch_successful().is_none() =>
            {
                let throbber = Throbber::default()
                    .throbber_style(config.theme.header())
                    .throbber_set(throbber_widgets_tui::BRAILLE_EIGHT_DOUBLE)
                    .use_type(WhichUse::Spin);
                let [centered_chunk] = centered_layout
                    .constraints([Constraint::Length(1)])
                    .areas(thumbnail_chunk);
                StatefulWidget::render(
                    throbber,
                    centered_chunk,
                    buf,
                    &mut self.thumbnail_fetching_throbber,
                );
            }
            _ => {
                let mut stateful_image = StatefulImage::new();
                if config.thumbnail_resize {
                    stateful_image = stateful_image.resize(Resize::Fit(Some(FilterType::Lanczos3)))
                }
                let [centered_chunk] = centered_layout
                    .constraints([Constraint::Fill(1)])
                    .areas(thumbnail_chunk);
                stateful_image.render(centered_chunk, buf, &mut self.placeholder_image);
            }
        }
    }

    pub(super) fn render_fat_article(
        &mut self,
        model_data: &ArticleContentModelData,
        distraction_free: bool,
        config: &Config,
        inner_area: Rect,
        buf: &mut Buffer,
    ) {
        let show_header = !distraction_free || config.zen_mode_show_header;

        let vertical_scroll = self.vertical_scroll;

        let [summary_area, content_area] = Layout::default()
            .direction(Direction::Vertical)
            .flex(Flex::Start)
            .constraints([
                Constraint::Length(if show_header { 5 } else { 0 }),
                Constraint::Fill(1),
            ])
            .horizontal_margin(2)
            .vertical_margin(1)
            .spacing(1)
            .areas::<2>(inner_area);

        let text_constraint = if distraction_free {
            Constraint::Max(config.text_max_width)
        } else {
            Constraint::Percentage(100)
        };

        if show_header {
            let [header_area] = Layout::default()
                .direction(Direction::Horizontal)
                .flex(ratatui::layout::Flex::Center)
                .constraints([text_constraint])
                .areas(summary_area);
            self.render_header(model_data, config, header_area, buf);
        }

        let [paragraph_area] = Layout::default()
            .direction(Direction::Horizontal)
            .flex(ratatui::layout::Flex::Center)
            .constraints([text_constraint])
            .areas(content_area);

        self.ensure_rows(model_data, config, paragraph_area.width);

        // `total_height` counts image rows as well as text rows, so content below a tall image is
        // still reachable.
        let max_scroll = self.total_height.saturating_sub(paragraph_area.height);
        let vertical_scroll = vertical_scroll.min(max_scroll);

        self.paint_rows(
            paragraph_area,
            vertical_scroll,
            self.picker.font_size(),
            config,
            buf,
        );

        self.max_scroll = max_scroll;
        self.vertical_scroll = vertical_scroll;
    }

    /// Rebuild the row layout if anything it depends on has changed.
    ///
    /// This is the only place the markdown is rendered, so the per-frame cost of parsing and
    /// wrapping the whole article is paid once per change rather than once per frame.
    fn ensure_rows(&mut self, model_data: &ArticleContentModelData, config: &Config, width: u16) {
        let font = self.picker.font_size();
        let key = LayoutKey {
            content_generation: *model_data.content_generation(),
            width,
            font_width: font.width,
            font_height: font.height,
            max_image_rows: config.content_image_max_height,
            show_images: *model_data.content_images_enabled(),
        };

        if self.layout_key == Some(key) {
            return;
        }

        let text = self.content_as_text(model_data, config);
        let draw_images = *model_data.content_images_enabled();
        let max_image_rows = config.content_image_max_height;

        let (rows, total_height) = build_rows(text, &self.content_image_order, width, |image| {
            if !draw_images {
                return 0;
            }
            image_cell_size(image.px_width, image.px_height, width, font, max_image_rows).1
        });

        self.rows = rows;
        self.total_height = total_height;
        self.layout_key = Some(key);
    }

    /// Pick the content to render and turn it into styled text.
    ///
    /// Also refreshes `content_image_order`, `discovered_image_urls` and `url_for_hint` as a side
    /// effect of the markdown renderer's hooks.
    fn content_as_text(
        &mut self,
        model_data: &ArticleContentModelData,
        config: &Config,
    ) -> Text<'static> {
        // Snapshot of every image already downloaded, so the hook can tell a drawable image from
        // one that still has to fall back to a hint link. Images whose encoding failed are left
        // out so that they become a hint link again rather than a blank gap.
        let loaded_images = model_data
            .loaded_content_images()
            .filter(|(url, _, _)| !self.failed_protocols.contains(*url))
            .map(|(url, width, height)| (url.to_owned(), (width, height)))
            .collect::<HashMap<String, (u32, u32)>>();

        let draw_images = *model_data.content_images_enabled();

        // prefer filtered content
        if let Some(filtered_markdown_content) = model_data.filtered_markdown_content().as_deref() {
            return self.markdown_to_text(
                filtered_markdown_content,
                config,
                loaded_images,
                draw_images,
            );
        }

        if config.content_preferred_type == ArticleContentType::Markdown
            && let Some(scraped) = model_data
                .fat_article()
                .as_ref()
                .and_then(|fat_article| fat_article.scraped_content.as_deref())
        {
            // Use the cached markdown content from model
            if let Some(markdown) = model_data.markdown_content().as_deref() {
                info!("markdown available");
                return self.markdown_to_text(markdown, config, loaded_images, draw_images);
            }

            info!("no markdown available, falling back to html2text");
            // Fallback - convert to plain text instead of markdown to avoid lifetime issues
            return Text::from(news_flash::util::html2text::html2text(scraped));
        }

        if let Some(plain_text) = model_data
            .fat_article()
            .as_ref()
            .and_then(|fat_article| fat_article.plain_text.as_deref())
        {
            info!("rendering plain text content");
            return Text::from(plain_text.to_owned());
        }

        info!("no content available");
        Text::from("no content available")
    }

    /// Paint the cached rows into the viewport.
    ///
    /// Image rows whose protocol is not encoded yet are left blank and their size is recorded, so
    /// that the encoding happens between frames; `SlicedImage` blocks the calling thread while it
    /// encodes, and rendering runs on the UI thread.
    fn paint_rows(
        &mut self,
        area: Rect,
        vertical_scroll: u16,
        font: FontSize,
        config: &Config,
        buf: &mut Buffer,
    ) {
        let max_image_rows = config.content_image_max_height;
        let scroll = i32::from(vertical_scroll);
        let bottom = scroll.saturating_add(i32::from(area.height));
        let picker_generation = self.picker_generation;

        let mut row_top = 0i32;
        let mut requests: Vec<(String, u16, u16)> = Vec::new();
        let mut drew_image = false;

        for row in &self.rows {
            let height = i32::from(row.height());

            if row_top.saturating_add(height) <= scroll {
                row_top += height;
                continue;
            }
            if row_top >= bottom {
                break;
            }

            let offset = row_top.saturating_sub(scroll);

            match row {
                ContentRow::Text(line) => {
                    let row_area =
                        Rect::new(area.x, area.y.saturating_add(offset as u16), area.width, 1);
                    line.render(row_area, buf);
                }
                ContentRow::Image {
                    index,
                    height: image_rows,
                } => {
                    let Some(image) = self.content_image_order.get(*index) else {
                        row_top += height;
                        continue;
                    };

                    let (columns, _) = image_cell_size(
                        image.px_width,
                        image.px_height,
                        area.width,
                        font,
                        max_image_rows,
                    );
                    if columns == 0 {
                        row_top += height;
                        continue;
                    }

                    let ready = self
                        .content_image_protocols
                        .get(&image.url)
                        .is_some_and(|built| {
                            built.columns == columns
                                && built.rows == *image_rows
                                && built.picker_generation == picker_generation
                        });

                    if !ready {
                        requests.push((image.url.clone(), columns, *image_rows));
                        row_top += height;
                        continue;
                    }

                    // Safe: `ready` was only true because the lookup succeeded.
                    let built = &self.content_image_protocols[&image.url];
                    let x = area.width.saturating_sub(columns) / 2;
                    // Unlike a text row, an image can be scrolled partly off the top, so this is
                    // deliberately signed: `SlicedImage` skips the hidden rows instead of
                    // re-encoding or overdrawing the pane.
                    let y = row_top - scroll;
                    let position = SignedPosition::from((x as i16, y as i16));
                    SlicedImage::new(&built.protocol, position).render(area, buf);
                    drew_image = true;
                }
            }

            row_top += height;
        }

        self.protocol_requests.extend(requests);
        self.rendered_inline_images = drew_image;
    }

    /// Encode the images requested during the last paint. Returns whether anything changed, so
    /// that the caller can ask for a redraw.
    pub(super) fn build_requested_protocols(
        &mut self,
        model_data: &ArticleContentModelData,
    ) -> bool {
        if self.protocol_requests.is_empty() {
            return false;
        }

        let requests = std::mem::take(&mut self.protocol_requests);
        let mut built_any = false;

        for (url, columns, rows) in requests {
            if self.content_image_protocols.contains_key(&url)
                || self.failed_protocols.contains(&url)
            {
                continue;
            }

            let Some(data) = model_data.content_image_data(&url) else {
                continue;
            };

            match Self::build_sliced_protocol(&self.picker, data, columns, rows) {
                Ok(protocol) => {
                    self.content_image_protocols.insert(
                        url,
                        BuiltImage {
                            protocol,
                            columns,
                            rows,
                            picker_generation: self.picker_generation,
                        },
                    );
                    built_any = true;
                }
                Err(error) => {
                    log::warn!("could not prepare content image {url} for display: {error}");
                    self.failed_protocols.insert(url);
                    // The layout committed to drawing this image, so it has to be rebuilt to put
                    // the hint link back instead of leaving a blank gap.
                    self.invalidate_layout();
                }
            }
        }

        built_any
    }

    fn build_sliced_protocol(
        picker: &Picker,
        data: &[u8],
        columns: u16,
        rows: u16,
    ) -> color_eyre::Result<SlicedProtocol> {
        let image = ImageReader::new(Cursor::new(data))
            .with_guessed_format()?
            .decode()?;
        SlicedProtocol::new(picker, image, Some(Size::new(columns, rows))).map_err(|error| {
            color_eyre::eyre::eyre!("could not encode the image for the terminal: {error}")
        })
    }

    /// Every image URL the rendered content refers to, in document order.
    pub(super) fn discovered_image_urls(&self) -> &[String] {
        &self.discovered_image_urls
    }

    /// Whether the last paint actually drew an image.
    ///
    /// Graphics protocols leave pixels that ratatui's cell diffing cannot erase, so this gates the
    /// full-terminal clears.
    pub(super) fn rendered_inline_images(&self) -> bool {
        self.rendered_inline_images
    }

    /// Drop the cached layout so that the next paint rebuilds it.
    pub(super) fn invalidate_layout(&mut self) {
        self.layout_key = None;
    }

    /// Forget everything about the previous article's inline images.
    ///
    /// The downloaded bytes stay in the model, keyed by URL, so revisiting an article only has to
    /// re-encode rather than re-download.
    pub(super) fn clear_content_images(&mut self) {
        self.content_image_order.clear();
        self.discovered_image_urls.clear();
        self.content_image_protocols.clear();
        self.failed_protocols.clear();
        self.protocol_requests.clear();
        self.rendered_inline_images = false;
        self.invalidate_layout();
    }

    fn markdown_to_text(
        &mut self,
        markdown: &str,
        config: &Config,
        loaded_images: HashMap<String, (u32, u32)>,
        draw_images: bool,
    ) -> Text<'static> {
        let url_for_hint = Arc::new(Mutex::new(HashMap::<String, String>::new()));
        // unwrap is safe here: at least one symbol passed save here
        let iterator = config.hint_type.iter();
        let hint_iterator = Arc::new(Mutex::new(iterator));
        let show_url = config.content_show_urls;
        let hook_state = Arc::new(Mutex::new(ImageHookState {
            loaded: loaded_images,
            ordered: Vec::new(),
            discovered: Vec::new(),
        }));

        let inner_link_url_for_hint = url_for_hint.clone();
        let inner_link_hint_iterator = hint_iterator.clone();
        let link_alt_text_style = Style::new()
            .fg(*config.theme.color_palette().accent_primary())
            .add_modifier(Modifier::UNDERLINED);
        let link_url_text_style = Style::new().fg(*config.theme.color_palette().foreground());
        let link_hint_style = Style::new()
            .fg(*config.theme.color_palette().highlight())
            .add_modifier(Modifier::BOLD);

        let inner_image_url_for_hint = url_for_hint.clone();
        let inner_image_hint_iterator = hint_iterator.clone();
        let image_hook_state = Arc::clone(&hook_state);
        let image_alt_text_style = Style::new()
            .fg(*config.theme.color_palette().accent_primary())
            .add_modifier(Modifier::UNDERLINED);
        let image_url_text_style = Style::new().fg(*config.theme.color_palette().foreground());
        let image_hint_style = link_hint_style;

        let image_icon = config.icon_set.image_icon();
        let url_icon = config.icon_set.url_icon();

        let text = {
            let renderer = RendererBuilder::new()
                .with_link(move |alt, url| {
                    let mut url_for_hint = inner_link_url_for_hint.lock().unwrap(); // unwrap is save here: locking with sync calls
                    let hint = inner_link_hint_iterator.lock().unwrap().next().unwrap(); // unwrap is save here: locking with sync calls

                    url_for_hint
                        .entry(hint.to_owned())
                        .or_insert(url.to_owned());
                    let mut spans = vec![
                        Span::styled(format!("{hint}{url_icon}"), link_hint_style),
                        Span::styled(alt.to_owned(), link_alt_text_style),
                    ];
                    if show_url {
                        spans.push(Span::styled(format!("({url})"), link_url_text_style));
                    }

                    spans
                })
                .with_image(move |alt, url| {
                    let dimensions = {
                        let mut hook_state = image_hook_state.lock().unwrap(); // unwrap is safe here: locking with sync calls
                        if draw_images {
                            hook_state.discovered.push(url.to_owned());
                            let dimensions = hook_state.loaded.get(url).copied();
                            if let Some((px_width, px_height)) = dimensions {
                                hook_state.ordered.push(ContentImageRef {
                                    url: url.to_owned(),
                                    px_width,
                                    px_height,
                                });
                            }
                            dimensions
                        } else {
                            None
                        }
                    };

                    if dimensions.is_some() {
                        // Replaced by the drawn image: `build_rows` turns this span into an image
                        // row at exactly this position in the document.
                        return vec![Span::from(IMAGE_SENTINEL)];
                    }

                    // Not available to draw, so keep the hint link the reader can open externally.
                    let mut url_for_hint = inner_image_url_for_hint.lock().unwrap();
                    let hint = inner_image_hint_iterator.lock().unwrap().next().unwrap(); // unwrap is save here: locking with sync calls
                    url_for_hint
                        .entry(hint.to_owned())
                        .or_insert(url.to_owned());
                    let mut spans = vec![
                        Span::styled(format!("{hint}{image_icon}"), image_hint_style),
                        Span::styled(alt.to_owned(), image_alt_text_style),
                    ];

                    if show_url {
                        spans.push(Span::styled(format!("({url})"), image_url_text_style));
                    }
                    spans
                })
                .build();

            the_other_tui_markdown::into_text_with_renderer(markdown, &renderer)
        };

        {
            let mut hook_state = hook_state.lock().unwrap(); // unwrap is safe here: locking with sync calls
            self.content_image_order = std::mem::take(&mut hook_state.ordered);
            self.discovered_image_urls = std::mem::take(&mut hook_state.discovered);
        }
        self.url_for_hint = url_for_hint.lock().unwrap().to_owned();

        text
    }

    pub fn picker_updated(&mut self, picker: &Picker) -> color_eyre::Result<()> {
        self.picker = picker.to_owned();

        // Encoded images are tied to the cell size the picker reported, and that cell size also
        // feeds the row heights, so both the protocols and the layout have to be rebuilt.
        self.picker_generation += 1;
        self.content_image_protocols.clear();
        self.failed_protocols.clear();
        self.protocol_requests.clear();
        self.invalidate_layout();

        let cursor = Cursor::new(NO_THUMB_PLACEHOLDER);
        self.placeholder_image = self.picker.new_resize_protocol(
            ImageReader::new(cursor)
                .with_guessed_format()
                .unwrap() // OK as content is checked
                .decode()
                .unwrap(), // OK as content is checked
        );

        Ok(())
    }
}

fn to_enclosure_bubble<P>(
    config: &Config,
    enclosures: &'_ [Enclosure],
    predicate: P,
    icon: char,
) -> Vec<Span<'static>>
where
    P: FnMut(&Enclosure) -> bool,
{
    let any_enclosures = enclosures.iter().any(predicate);
    if any_enclosures {
        to_bubble(
            Span::styled(format!("{}", icon), config.theme.paragraph()),
            config,
        )
        .spans
    } else {
        Default::default()
    }
}

impl Widget for &mut ArticleContent {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer) {
        let inner_area = self
            .view_data
            .render_block(area, buf, &self.config, self.is_focused);

        if !self.model_data.article().is_some() {
            return;
        }

        if self.model_data.fat_article().is_some()
            || self.model_data.filtered_markdown_content().is_some()
        {
            self.view_data.render_fat_article(
                &self.model_data,
                self.is_distraction_free,
                &self.config,
                inner_area,
                buf,
            );
        } else if self.model_data.article().is_some() {
            self.view_data
                .render_summary(&self.model_data, &self.config, inner_area, buf);
        }
    }
}
