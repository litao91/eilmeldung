use crate::prelude::*;

use std::{
    io::Cursor,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use getset::Getters;
use htmd::HtmlToMarkdown;
use image::ImageReader;
use indexmap::IndexMap;
use news_flash::models::{Article, ArticleID, Enclosure, FatArticle, Feed, Tag, Thumbnail};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

/// How many extracted articles are kept in memory.
///
/// news-flash's own scraper caches scraped content on disk, so revisiting an article is free
/// there. The `libreadability` path has no such cache, so without this an article would be
/// re-fetched and re-extracted every time the selection returns to it.
const EXTRACTED_CACHE_LIMIT: usize = 16;

/// Content images smaller than this in either dimension are icons or tracking pixels and are left
/// as text hints rather than drawn.
const CONTENT_IMAGE_MIN_PIXELS: u32 = 64;

/// How many content images are remembered across articles, keyed by URL.
const CONTENT_IMAGE_CACHE_LIMIT: usize = 64;

/// Combined size of the remembered content image bytes.
const CONTENT_IMAGE_CACHE_BYTES: usize = 48 * 1024 * 1024;

/// How many content image downloads are started for one article at a time.
const CONTENT_IMAGE_BATCH_LIMIT: usize = 24;

/// What is known about one content image URL.
///
/// Keyed by the URL exactly as it appeared in the markdown. Every state other than `Pending` is
/// final, so a URL is never downloaded twice: that keeps a hotlink-protected or 404ing image from
/// being retried on every tick.
#[derive(Debug)]
pub(super) enum ContentImageState {
    Pending,
    Loaded {
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Download or decode failed, or the image is too small to be worth drawing.
    Unusable,
}

impl ContentImageState {
    fn byte_len(&self) -> usize {
        match self {
            ContentImageState::Loaded { data, .. } => data.len(),
            ContentImageState::Pending | ContentImageState::Unusable => 0,
        }
    }
}

#[derive(Getters)]
#[getset(get = "pub(super)")]
pub struct ArticleContentModelData {
    #[getset(skip)]
    news_flash_utils: Arc<NewsFlashUtils>,

    // Core article data
    article: Option<Article>,
    feed: Option<Feed>,
    tags: Option<Vec<Tag>>,
    fat_article: Option<FatArticle>,
    enclosures: Option<Vec<Enclosure>>,

    #[getset(skip)]
    extracted_cache: IndexMap<ArticleID, FatArticle>,
    #[getset(skip)]
    extract_fallback_notified: bool,

    // Content images, keyed by the URL as it appeared in the markdown.
    #[getset(skip)]
    content_images: IndexMap<String, ContentImageState>,
    #[getset(skip)]
    content_image_bytes: usize,
    content_image_fetch_running: bool,
    #[getset(skip)]
    image_fetch_abort: Option<tokio::task::AbortHandle>,

    /// Bumped whenever anything that affects the rendered rows changes, so that the view can tell
    /// that its cached layout is stale.
    content_generation: u64,

    /// Whether the current article's images are shown, and therefore downloaded at all.
    ///
    /// Per article on purpose: it is reset from `config.content_show_images` on every selection, so
    /// images stay opt-in per article rather than following you through the whole list.
    content_images_enabled: bool,

    // Processed content
    markdown_content: Option<String>,

    // Filtered content
    filtered_markdown_content: Option<String>,

    // Thumbnail data and state
    thumbnail_fetch_successful: Option<bool>,
    thumbnail_fetch_running: bool,
    thumbnail: Option<Thumbnail>,

    // Timing for debouncing fetches
    instant_since_article_selected: Option<Instant>,
    duration_since_last_article_change: Option<Duration>,
}

impl ArticleContentModelData {
    pub(super) fn new(news_flash_utils: Arc<NewsFlashUtils>) -> Self {
        Self {
            news_flash_utils,

            article: None,
            feed: None,
            tags: None,
            fat_article: None,
            extracted_cache: IndexMap::new(),
            extract_fallback_notified: false,
            content_images: IndexMap::new(),
            content_image_bytes: 0,
            content_image_fetch_running: false,
            image_fetch_abort: None,
            content_generation: 0,
            content_images_enabled: false,
            markdown_content: None,
            filtered_markdown_content: None,
            thumbnail_fetch_successful: None,
            thumbnail_fetch_running: false,
            thumbnail: None,
            instant_since_article_selected: None,
            duration_since_last_article_change: None,
            enclosures: None,
        }
    }

    pub(super) async fn on_article_selected(
        &mut self,
        article_id: Option<&ArticleID>,
        config: &Config,
    ) -> color_eyre::Result<bool> {
        if self.article.as_ref().map(|article| &article.article_id) == article_id {
            return Ok(false);
        }

        let current_instant = Instant::now();
        if let Some(last_article_selected) = self.instant_since_article_selected {
            self.duration_since_last_article_change =
                Some(current_instant.duration_since(last_article_selected));
        }
        self.instant_since_article_selected = Some(current_instant);
        self.thumbnail_fetch_successful = None;
        self.thumbnail = None;
        self.abort_content_image_fetch();
        // Restore previously extracted content instead of always dropping it, so that returning
        // to an article does not trigger another fetch.
        let cached =
            article_id.and_then(|article_id| self.extracted_cache.get(article_id).cloned());
        self.fat_article = cached;
        self.markdown_content = None;
        self.filtered_markdown_content = None;
        self.feed = None;
        self.tags = None;
        self.content_generation += 1;
        self.content_images_enabled = config.content_show_images;

        match article_id {
            Some(article_id) => {
                let article = {
                    let news_flash = self.news_flash_utils.news_flash_lock.read().await;
                    let article = news_flash.get_article(article_id)?;
                    self.feed = news_flash
                        .get_feeds()?
                        .0
                        .into_iter()
                        .find(|feed| feed.feed_id == article.feed_id);
                    self.enclosures = Some(news_flash.get_enclosures(article_id)?);
                    article
                };

                self.update_article_tags().await?;

                self.article = Some(article);
            }
            None => {
                self.article = None;
            }
        }

        Ok(true)
    }

    pub(super) async fn update_article_tags(&mut self) -> color_eyre::Result<()> {
        if let Some(article_id) = self.article.as_ref().map(|article| &article.article_id) {
            let news_flash = self.news_flash_utils.news_flash_lock.read().await;
            let (tags, taggings) = news_flash.get_tags()?;
            let mut tag_for_tag_id =
                NewsFlashUtils::generate_id_map(&tags, |tag| tag.tag_id.clone());
            self.tags = Some(
                taggings
                    .into_iter()
                    .filter(|tagging| tagging.article_id == *article_id)
                    .filter_map(|tagging| tag_for_tag_id.remove(&tagging.tag_id))
                    .collect::<Vec<Tag>>(),
            );
        }
        Ok(())
    }

    pub(super) fn prepare_thumbnail(
        &mut self,
        thumbnail: &Thumbnail,
        picker: &Picker,
    ) -> color_eyre::Result<Option<StatefulProtocol>> {
        if let Some(article) = self.article.as_ref()
            && article.article_id == thumbnail.article_id
            && let Some(data) = thumbnail.data.as_ref()
        {
            self.thumbnail = Some(thumbnail.to_owned());
            Ok(Some(Self::gen_stateful_protocol(data, picker)?))
        } else {
            Ok(None)
        }
    }

    pub(super) fn update_thumbnail(
        &self,
        picker: &Picker,
    ) -> color_eyre::Result<Option<StatefulProtocol>> {
        if let Some(thumbnail) = self.thumbnail.as_ref()
            && let Some(data) = thumbnail.data.as_ref()
        {
            Ok(Some(Self::gen_stateful_protocol(data, picker)?))
        } else {
            Ok(None)
        }
    }

    fn gen_stateful_protocol(data: &[u8], picker: &Picker) -> color_eyre::Result<StatefulProtocol> {
        let cursor = Cursor::new(data);
        let image = ImageReader::new(cursor).with_guessed_format()?.decode()?;
        Ok(picker.new_resize_protocol(image))
    }

    pub(super) fn scrape_article(&mut self, config: &Config) -> color_eyre::Result<()> {
        let Some(article) = self.article.as_ref() else {
            return Ok(());
        };

        if self.fat_article.is_some() {
            return Ok(());
        }

        let article_id = article.article_id.clone();

        // Without a source URL there is nothing to fetch; news-flash handles that case itself.
        if config.content_fetcher == ContentFetcher::Readability
            && let Some(url) = article.url.as_ref()
        {
            self.news_flash_utils
                .fetch_article_html(article_id, url.as_str().to_owned());
        } else {
            self.news_flash_utils.fetch_fat_article(article_id);
        }

        Ok(())
    }

    /// Retry with news-flash's own scraper after the `libreadability` path failed.
    /// Returns whether a fetch was actually started.
    pub(super) fn scrape_article_with_newsflash(&mut self) -> bool {
        if self.fat_article.is_some() {
            return false;
        }

        let Some(article) = self.article.as_ref() else {
            return false;
        };

        self.news_flash_utils
            .fetch_fat_article(article.article_id.clone());
        true
    }

    /// Whether the user has already been told that extraction fell back to news-flash's scraper.
    /// Returns `true` exactly once so that a failing extractor does not produce a tooltip per
    /// article.
    pub(super) fn take_extract_fallback_notification(&mut self) -> bool {
        let notified = self.extract_fallback_notified;
        self.extract_fallback_notified = true;
        !notified
    }

    pub(super) fn on_extract_finished(&mut self, extracted: &ExtractedArticle) {
        // Guard against a response for an article the user has already navigated away from.
        if self.article.as_ref().map(|article| &article.article_id) != Some(&extracted.article_id) {
            return;
        }

        let Some(article) = self.article.as_ref() else {
            return;
        };

        // Mirror the article's own metadata, filling in title/author the way news-flash's scraper
        // does, so that switching extractors does not change the header.
        let fat_article = FatArticle {
            article_id: article.article_id.clone(),
            title: article
                .title
                .clone()
                .or_else(|| extracted.title.clone())
                .filter(|title| !title.trim().is_empty()),
            author: article
                .author
                .clone()
                .or_else(|| extracted.byline.clone())
                .filter(|author| !author.trim().is_empty()),
            feed_id: article.feed_id.clone(),
            url: article.url.clone(),
            date: article.date,
            synced: article.synced,
            html: None,
            summary: article.summary.clone(),
            direction: article.direction,
            unread: article.unread,
            marked: article.marked,
            scraped_content: Some(extracted.content.clone()),
            plain_text: Some(extracted.text_content.clone()),
            thumbnail_url: article.thumbnail_url.clone(),
            updated: article.updated,
        };

        self.cache_extracted(fat_article.clone());
        self.set_fat_article(fat_article);
    }

    fn cache_extracted(&mut self, fat_article: FatArticle) {
        // shift_remove first so that a re-extraction moves the entry to the back rather than
        // keeping its old position and getting evicted early.
        self.extracted_cache.shift_remove(&fat_article.article_id);
        self.extracted_cache
            .insert(fat_article.article_id.clone(), fat_article);

        while self.extracted_cache.len() > EXTRACTED_CACHE_LIMIT {
            self.extracted_cache.shift_remove_index(0);
        }
    }

    pub(super) fn set_fat_article(&mut self, fat_article: FatArticle) {
        self.fat_article = Some(fat_article);
        self.markdown_content = None; // Reset processed content
        self.content_generation += 1;
    }

    /// Flip whether the current article's images are shown, returning the new state.
    ///
    /// Turning them on is also what starts the downloads: nothing is fetched for an article whose
    /// images were never asked for.
    pub(super) fn toggle_content_images(&mut self) -> bool {
        self.content_images_enabled = !self.content_images_enabled;
        self.content_images_enabled
    }

    /// Every content image that is ready to be drawn, with its pixel dimensions.
    ///
    /// The view snapshots this before rendering markdown so that the image hook can tell a
    /// drawable image from one that still has to fall back to a hint link. Snapshotting the whole
    /// set rather than only the URLs seen last time is what makes a cached article draw its images
    /// on the very first layout pass after being revisited.
    pub(super) fn loaded_content_images(&self) -> impl Iterator<Item = (&str, u32, u32)> {
        self.content_images
            .iter()
            .filter_map(|(url, state)| match state {
                ContentImageState::Loaded { width, height, .. } => {
                    Some((url.as_str(), *width, *height))
                }
                ContentImageState::Pending | ContentImageState::Unusable => None,
            })
    }

    /// Raw bytes of a loaded content image, kept so that a terminal resize or a revisit only has
    /// to re-encode rather than re-download.
    pub(super) fn content_image_data(&self, url: &str) -> Option<&[u8]> {
        match self.content_images.get(url) {
            Some(ContentImageState::Loaded { data, .. }) => Some(data),
            _ => None,
        }
    }

    /// Of the URLs the rendered content refers to, those nothing is known about yet.
    ///
    /// Anything already `Pending`, `Loaded` or `Unusable` is skipped, which is what stops a
    /// failing image from being retried on every tick.
    pub(super) fn pending_content_image_urls(&self, candidates: &[String]) -> Vec<String> {
        candidates
            .iter()
            .filter(|url| !self.content_images.contains_key(url.as_str()))
            .take(CONTENT_IMAGE_BATCH_LIMIT)
            .cloned()
            .collect()
    }

    /// Whether the debounce for content image downloads has elapsed, so that quickly browsing
    /// through articles does not start a batch for each one.
    pub(super) fn content_image_debounce_elapsed(&self, config: &Config) -> bool {
        match self.instant_since_article_selected {
            Some(selected) => {
                selected.elapsed() >= Duration::from_millis(config.content_image_debounce_millis)
            }
            None => true,
        }
    }

    pub(super) fn start_fetch_content_images(
        &mut self,
        urls: Vec<String>,
        base_url: Option<String>,
    ) {
        let Some(article_id) = self
            .article
            .as_ref()
            .map(|article| article.article_id.clone())
        else {
            return;
        };

        // Marking them pending up front keeps the next tick from selecting the same URLs again.
        for url in &urls {
            self.insert_content_image(url.clone(), ContentImageState::Pending);
        }

        self.content_image_fetch_running = true;
        self.image_fetch_abort = Some(
            self.news_flash_utils
                .fetch_content_images(article_id, base_url, urls),
        );
    }

    pub(super) fn on_content_image_finished(&mut self, image: &ContentImage) {
        // The user may have moved on while the batch was in flight.
        if self.article.as_ref().map(|article| &article.article_id) != Some(&image.article_id) {
            return;
        }

        let state = match image.data.as_ref() {
            Some(data)
                if image.width >= CONTENT_IMAGE_MIN_PIXELS
                    && image.height >= CONTENT_IMAGE_MIN_PIXELS =>
            {
                ContentImageState::Loaded {
                    data: data.clone(),
                    width: image.width,
                    height: image.height,
                }
            }
            _ => ContentImageState::Unusable,
        };

        self.insert_content_image(image.url.clone(), state);
        self.content_generation += 1;
    }

    pub(super) fn on_content_images_finished(&mut self, article_id: &ArticleID) {
        if self.article.as_ref().map(|article| &article.article_id) != Some(article_id) {
            return;
        }

        self.content_image_fetch_running = false;
        self.image_fetch_abort = None;
    }

    /// Cancel an in-flight batch and forget the URLs it had claimed, so that they can be picked up
    /// again by whichever article references them next.
    pub(super) fn abort_content_image_fetch(&mut self) {
        if let Some(abort) = self.image_fetch_abort.take() {
            abort.abort();
        }
        self.content_image_fetch_running = false;

        let pending = self
            .content_images
            .iter()
            .filter(|(_, state)| matches!(state, ContentImageState::Pending))
            .map(|(url, _)| url.clone())
            .collect::<Vec<String>>();

        for url in pending {
            self.content_images.shift_remove(&url);
        }
    }

    fn insert_content_image(&mut self, url: String, state: ContentImageState) {
        self.content_image_bytes += state.byte_len();
        if let Some((_, evicted)) = self.content_images.shift_remove_entry(&url) {
            self.content_image_bytes = self.content_image_bytes.saturating_sub(evicted.byte_len());
        }
        self.content_images.insert(url, state);

        while self.content_images.len() > CONTENT_IMAGE_CACHE_LIMIT
            || self.content_image_bytes > CONTENT_IMAGE_CACHE_BYTES
        {
            let Some((_, evicted)) = self.content_images.shift_remove_index(0) else {
                break;
            };
            self.content_image_bytes = self.content_image_bytes.saturating_sub(evicted.byte_len());
        }
    }

    pub(super) fn get_or_create_markdown_content(
        &mut self,
        config: &Config,
    ) -> color_eyre::Result<()> {
        if self.markdown_content.is_none()
            && config.content_preferred_type == ArticleContentType::Markdown
            && let Some(fat_article) = self.fat_article.as_ref()
            && let Some(html) = fat_article.scraped_content.as_deref()
        {
            // create html to markdown converter
            let html2markdown = HtmlToMarkdown::builder().build();

            self.markdown_content = Some(html2markdown.convert(html)?);
        }
        Ok(())
    }

    pub(super) fn update_should_fetch_thumbnail(&mut self, config: &Config) -> bool {
        if !config.thumbnail_show || self.thumbnail_fetch_running {
            return false;
        }

        let Some(article) = self.article.as_ref() else {
            return false;
        };

        if article.thumbnail_url.is_none() {
            self.thumbnail_fetch_successful = Some(false);
            return false;
        }

        if !self.thumbnail_fetch_successful.unwrap_or(true) {
            return false;
        }

        let current_instant = Instant::now();
        let long_enough_current_article = match self.instant_since_article_selected {
            Some(article_selected_instant) => {
                let duration = current_instant.duration_since(article_selected_instant);
                duration >= Duration::from_millis(config.thumbnail_fetch_debounce_millis)
            }
            None => true,
        };

        match self.duration_since_last_article_change {
            None => true,
            Some(duration) => {
                duration > Duration::from_millis(config.thumbnail_fetch_debounce_millis)
                    || long_enough_current_article
            }
        }
    }

    pub(super) fn start_fetch_thumbnail(&mut self) -> color_eyre::Result<()> {
        let Some(article) = self.article.as_ref() else {
            self.thumbnail_fetch_successful = Some(false);
            return Ok(());
        };

        if article.thumbnail_url.is_none() {
            self.thumbnail_fetch_successful = Some(false);
            return Ok(());
        }

        let article_id = article.article_id.clone();
        self.news_flash_utils.fetch_thumbnail(article_id);
        self.thumbnail_fetch_running = true;

        Ok(())
    }

    pub(super) fn on_thumbnail_fetch_finished(&mut self, thumbnail: Option<&Thumbnail>) {
        self.thumbnail_fetch_running = false;
        match thumbnail {
            Some(_) => {
                self.thumbnail_fetch_successful = Some(true);
            }
            None => {
                log::debug!("fetching thumbnail not successful");
                self.thumbnail_fetch_successful = Some(false);
            }
        }
    }

    pub(super) fn on_thumbnail_fetch_failed(&mut self) {
        self.thumbnail_fetch_successful = Some(false);
        self.thumbnail_fetch_running = false;
    }

    pub(super) fn clean_string(string: &str) -> String {
        string.replace("\r", "").replace("\n", "")
    }

    pub(crate) async fn open_enclosure(
        &self,
        config: &Config,
        enclosure: &Enclosure,
    ) -> color_eyre::Result<String> {
        let command = match <EnclosureType>::from(enclosure) {
            EnclosureType::Audio => config.audio_enclosure_command.as_ref(),
            EnclosureType::Image => config.image_enclosure_command.as_ref(),
            EnclosureType::Video => config.video_enclosure_command.as_ref(),
        }
        .unwrap_or(&config.enclosure_command)
        .replace("{url}", enclosure.url.as_ref())
        .replace("{type}", <EnclosureType>::from(enclosure).as_ref())
        .replace("{mime}", enclosure.mime_type.as_deref().unwrap_or("*/*"));

        let (cmd, args) = prepare_command(&command)?;

        let mut command = std::process::Command::new(&cmd);

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(args)
            .spawn()?;

        Ok(cmd.to_owned())
    }

    pub(crate) fn pipe(
        &self,
        config: &Config,
        in_target: PipeTarget,
        out_target: PipeTarget,
        command: &str,
    ) -> color_eyre::Result<()> {
        let Some(article) = self.article.as_ref() else {
            return Err(color_eyre::eyre::eyre!("no article selected"));
        };

        let command = command
            .replace(
                "{date}",
                &article
                    .date
                    .with_timezone(&chrono::Local)
                    .format(&config.date_format)
                    .to_string(),
            )
            .replace("{title}", article.title.as_deref().unwrap_or_default())
            .replace(
                "{url}",
                article
                    .url
                    .as_ref()
                    .map(|url| url.as_str())
                    .unwrap_or_default(),
            )
            .replace("{author}", article.author.as_deref().unwrap_or_default())
            .replace(
                "{feed}",
                self.feed
                    .as_ref()
                    .map(|feed| &*feed.label)
                    .unwrap_or_default(),
            );

        self.news_flash_utils.pipe(
            article.to_owned(),
            self.fat_article.to_owned(),
            in_target,
            out_target,
            command.to_string(),
        );

        Ok(())
    }

    pub(crate) fn on_pipe_finished(
        &mut self,
        article_id: &ArticleID,
        exit_status: std::process::ExitStatus,
        markdown: &Option<String>,
        error: &Option<String>,
    ) {
        if self.article.as_ref().map(|article| &article.article_id) != Some(article_id) {
            return;
        }

        if markdown.is_none()
            && exit_status.success()
            && error.as_ref().map(|error| error.is_empty()).unwrap_or(true)
        {
            return;
        }

        let mut markdown_content = markdown.as_deref().unwrap_or("").to_string();

        if !exit_status.success() {
            markdown_content.push_str(&format!(
                r#"
---

Process returned with error ({})
                "#,
                exit_status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown error code".to_string())
            ));
        }

        if let Some(error) = error.as_deref() {
            markdown_content.push_str(&format!(
                r#"
---
 
# stderr output

{error}
                "#
            ));
        }

        self.filtered_markdown_content.replace(markdown_content);
        self.content_generation += 1;
    }
}
