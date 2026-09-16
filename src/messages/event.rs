use std::{collections::HashMap, process::ExitStatus, sync::Arc};

use news_flash::{
    error::NewsFlashError,
    models::{ArticleID, Category, FatArticle, Feed, FeedID, Tag, Thumbnail},
};
use ratatui::text::Text;
use ratatui_image::picker::Picker;

use crate::prelude::*;

#[derive(thiserror::Error, Debug)]
pub enum AsyncOperationError {
    #[error("news flash error")]
    NewsFlashError(#[from] NewsFlashError),

    #[error("error report")]
    Report(#[from] color_eyre::Report),
}

/// The result of fetching an article's page and extracting its body with `libreadability`.
///
/// `content` is cleaned article HTML (junk stripped, image URLs made absolute) which feeds the
/// existing HTML → markdown → renderer chain; `text_content` feeds the plain-text path.
#[derive(Debug)]
pub struct ExtractedArticle {
    pub article_id: ArticleID,
    pub content: String,
    pub text_content: String,
    pub title: Option<String>,
    pub byline: Option<String>,
}

/// A single image referenced by an article's content, downloaded for inline display.
///
/// `url` is the key form: exactly the string that appeared in the markdown, which is what the
/// model's cache and the view's protocol cache are keyed by.
pub struct ContentImage {
    pub article_id: ArticleID,
    pub url: String,
    /// `None` means the download or decode failed.
    pub data: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

// Hand-written because a derived `Debug` would print up to several megabytes of image bytes
// per event into the log.
impl std::fmt::Debug for ContentImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentImage")
            .field("article_id", &self.article_id)
            .field("url", &self.url)
            .field(
                "data",
                &self
                    .data
                    .as_ref()
                    .map(|data| format!("{} bytes", data.len())),
            )
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

#[derive(Debug)]
pub enum Event {
    ArticlesSelected(AugmentedArticleFilter),
    ArticleSelected(Option<ArticleID>),

    AsyncSync,
    AsyncSyncFinished(HashMap<FeedID, i64>),

    AsyncArticleThumbnailFetch,
    AsyncArticleThumbnailFetchFinished(Option<Thumbnail>),

    AsyncArticleFatFetch,
    AsyncArticleFatFetchFinished(FatArticle),

    AsyncArticleExtract,
    AsyncArticleExtractFinished(ExtractedArticle),

    AsyncContentImageFetchFinished(ContentImage),
    AsyncContentImagesFetchFinished(ArticleID),

    AsyncPipeArticle,
    AsyncPipeArticleFinished(ArticleID, ExitStatus, Option<String>, Option<String>),

    AsyncArticlesMark,
    AsyncArticlesMarkFinished,

    AsyncArticleTag,
    AsyncArticleTagFinished,

    AsyncArticleUntag,
    AsyncArticleUntagFinished,

    AsyncTagAdd,
    AsyncTagAddFinished(Tag),

    AsyncTagRemove,
    AsyncTagRemoveFinished,

    AsyncFeedAdd,
    AsyncFeedAddFinished(Feed),

    AsyncFeedFetch,
    AsyncFeedFetchFinished(FeedID, i64),

    AsyncCategoryAdd,
    AsyncCategoryAddFinished(Category),

    AsyncFeedRename,
    AsyncRenameFeedFinished(Feed),

    AsyncCategoryRename,
    AsyncCategoryRenameFinished(Category),

    AsyncCategoryRemove,
    AsyncCategoryRemoveFinished,

    AsyncFeedRemove,
    AsyncFeedRemoveFinished,

    AsyncFeedUrlChange,
    AsyncFeedUrlChangeFinished,

    AsyncTagEdit,
    AsyncTagEditFinished(Tag),

    AsyncOperationFailed(AsyncOperationError, Box<Event>),

    AsyncSetOffline,
    AsyncSetOfflineFinished(bool),

    AsyncSetAllRead,
    AsyncSetAllReadFinished,

    AsyncFeedSetRead,
    AsyncFeedSetReadFinished,

    AsyncCategorySetRead,
    AsyncCategorySetReadFinished,

    AsyncTagSetRead,
    AsyncTagSetReadFinished,

    AsyncArticlesSetRead,
    AsyncArticlesSetReadFinished,

    AsyncFeedMove,
    AsyncFeedMoveFinished,

    AsyncCategoryMove,
    AsyncCategoryMoveFinished,

    AsyncImportOpml,
    AsyncImportOpmlFinished,

    AsyncLogout,
    AsyncLogoutFinished,

    ConfigFileChanged,
    ConfigReloaded(Arc<Config>),

    Tick, // general tick for animations and regular updates

    // messaging/status
    Tooltip(Tooltip<'static>),

    // help popup
    ShowHelpPopup(String, Text<'static>),
    ShowModalHelpPopup(String, Text<'static>),
    HideHelpPopup,

    // application
    ApplicationStarted,
    ApplicationStateChanged(AppState),

    // mouse click on article list at row offset from top of inner area
    MouseArticleClick(u16),

    // mouse click on feed list at screen position (col, row)
    MouseFeedClick(u16, u16),

    // mouse scroll viewport without moving selection (panel, lines)
    MouseScrollUp(Panel),
    MouseScrollDown(Panel),

    // terminal resized
    Resized(u16, u16),

    // new image protocol picker
    ImageProtocolPickerUpdated(Picker),

    // connectivity
    ConnectionAvailable,
    ConnectionLost(ConnectionLostReason),
}

impl Event {
    pub fn caused_model_update(&self) -> bool {
        use Event::*;

        matches!(
            self,
            AsyncSyncFinished(_)
                | AsyncFeedAddFinished(_)
                | AsyncFeedFetchFinished(..)
                | AsyncPipeArticleFinished(..)
                | AsyncRenameFeedFinished(_)
                | AsyncCategoryRenameFinished(_)
                | AsyncArticleFatFetchFinished(_)
                | AsyncArticleExtractFinished(_)
                | AsyncArticlesMarkFinished
                | AsyncArticleTagFinished
                | AsyncArticleUntagFinished
                | AsyncTagAddFinished(_)
                | AsyncTagRemoveFinished
                | AsyncTagEditFinished(_)
                | AsyncOperationFailed(..)
                | AsyncSetOfflineFinished(_)
                | AsyncSetAllReadFinished
                | AsyncFeedSetReadFinished
                | AsyncCategorySetReadFinished
                | AsyncTagSetReadFinished
                | AsyncArticlesSetReadFinished
                | AsyncImportOpmlFinished,
        )
    }
}
