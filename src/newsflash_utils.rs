use crate::{messages::event::AsyncOperationError, prelude::*};
use std::{
    collections::HashMap, error::Error, hash::Hash, io::Cursor, path::Path, process::Stdio,
    str::FromStr, sync::Arc, time::Duration,
};

use htmd::HtmlToMarkdown;
use image::ImageReader;
use news_flash::{
    NewsFlash,
    error::NewsFlashError,
    models::{
        ArticleFilter, ArticleID, Category, CategoryID, CategoryMapping, Feed, FeedID, FeedMapping,
        LoginData, Marked, Read, Tag, TagID, Url,
    },
};

use log::{debug, error, info};
use ratatui::{
    style::Color,
    text::{Line, Span},
};
use reqwest::{Client, ClientBuilder};
use tokio::sync::{Mutex, RwLock, mpsc::UnboundedSender};

#[derive(Clone)]
pub struct NewsFlashUtils {
    pub news_flash_lock: Arc<RwLock<NewsFlash>>,
    client_lock: Arc<RwLock<Client>>,
    undo_stack_lock: Arc<RwLock<Vec<UndoOperation>>>,
    command_sender: UnboundedSender<Message>,

    async_operation_mutex: Arc<Mutex<()>>,
}

// macro to wrap news flash async calls into spawns and send messages at the beginning and end
macro_rules! gen_async_call {
    {
        method_name: $method_name:ident,
        params: ($($param:ident: $param_type:ty),*),
        news_flash_var: $news_flash_var:ident,
        client_var: $client_var:ident,
        undo_stack_var: $undo_stack_var:ident,
        start_event: $start_event:expr,
        operation: $operation:stmt,
        success_event: $success_event:expr,
    } => {
        pub fn $method_name(&self, $($param: $param_type),*) {
            let news_flash_lock = self.news_flash_lock.clone();
            let client_lock = self.client_lock.clone();
            let undo_stack_lock = self.undo_stack_lock.clone();
            let command_sender = self.command_sender.clone();
            let async_operation_mutex = self.async_operation_mutex.clone();

            tokio::spawn(async move {
                let _lock = async_operation_mutex.lock().await;

                if let Err(e) = async {
                    command_sender.send(Message::Event($start_event)).map_err(|send_error|
                        color_eyre::eyre::eyre!(send_error))?;

                    let $news_flash_var = news_flash_lock.read().await;
                    let $client_var = client_lock.read().await;
                    let mut $undo_stack_var = undo_stack_lock.write().await;

                    $operation

                    command_sender.send(Message::Event($success_event)).map_err(|send_error|
                        color_eyre::eyre::eyre!(send_error))?;
                    Ok::<(), AsyncOperationError>(())
                }.await{
                    error!("Async call {} failed: {}", stringify!(&method_name), e,);
                    let _ = command_sender.send(Message::Event(Event::AsyncOperationFailed( e,
                                Box::new($start_event),)));
                }
            });
        }

    }


}

pub fn build_client(timeout: Duration) -> color_eyre::Result<Client> {
    let user_agent = format!(
        "eilmeldung/{} (RSS reader; +https://github.com/christo-auer/eilmeldung",
        env!("CARGO_PKG_VERSION")
    );
    let builder = ClientBuilder::new()
        .user_agent(user_agent.as_str())
        .hickory_dns(false)
        .gzip(true)
        .brotli(true)
        .timeout(timeout);

    Ok(builder.build()?)
}

/// Extract the readable article body out of a fetched page.
///
/// Kept separate from the async fetch so that it can be tested without a network. The returned
/// `content` is cleaned article HTML with absolute image URLs, ready for the existing
/// HTML → markdown → renderer chain; `text_content` feeds the plain-text path.
fn extract_article(
    article_id: ArticleID,
    url: &str,
    html: &str,
) -> Result<ExtractedArticle, AsyncOperationError> {
    let extracted = libreadability::extract(html, Some(url)).map_err(|error| {
        AsyncOperationError::Report(color_eyre::eyre::eyre!(
            "could not extract readable content from {url}: {error}"
        ))
    })?;

    // Readability can report success while finding nothing worth reading; fail so that the caller
    // falls back to news-flash's scraper instead of showing an empty article.
    if extracted.length == 0 {
        return Err(AsyncOperationError::Report(color_eyre::eyre::eyre!(
            "no readable content could be extracted from {url}"
        )));
    }

    Ok(ExtractedArticle {
        article_id,
        content: extracted.content,
        text_content: extracted.text_content,
        title: Some(extracted.title).filter(|title| !title.trim().is_empty()),
        byline: Some(extracted.byline).filter(|byline| !byline.trim().is_empty()),
    })
}

/// Largest content image that will be downloaded.
const CONTENT_IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many content images are downloaded concurrently.
const CONTENT_IMAGE_CONCURRENCY: usize = 3;

/// Resolve a possibly-relative image reference against the article's page URL.
///
/// `libreadability` already absolutizes the URLs it returns, but content from news-flash's
/// scraper or from a user-defined pipe may not be. Only `http`/`https` are supported, which
/// deliberately excludes `data:` URIs.
fn resolve_image_url(base_url: Option<&str>, url: &str) -> color_eyre::Result<::url::Url> {
    let absolute = match ::url::Url::parse(url) {
        Ok(absolute) => absolute,
        Err(::url::ParseError::RelativeUrlWithoutBase) => {
            let base = base_url.ok_or_else(|| {
                color_eyre::eyre::eyre!("{url} is relative but no base URL is known")
            })?;
            ::url::Url::parse(base)?.join(url)?
        }
        Err(error) => return Err(color_eyre::eyre::eyre!("{url} is not a valid URL: {error}")),
    };

    if absolute.scheme() != "http" && absolute.scheme() != "https" {
        return Err(color_eyre::eyre::eyre!(
            "unsupported URL scheme `{}`",
            absolute.scheme()
        ));
    }

    Ok(absolute)
}

/// Download one content image and read its pixel dimensions.
///
/// Never fails: a problem is logged and reported as `data: None`, so that one broken image cannot
/// abort the rest of the batch. Only the image header is parsed here; the full decode happens when
/// the terminal image protocol is built.
async fn download_content_image(
    client: &Client,
    article_id: &ArticleID,
    base_url: Option<&str>,
    url: &str,
) -> ContentImage {
    let downloaded = async {
        let absolute = resolve_image_url(base_url, url)?;

        let response = client
            .get(absolute)
            .send()
            .await
            .map_err(|error| color_eyre::eyre::eyre!("request for {url} failed: {error}"))?;

        if !response.status().is_success() {
            return Err(color_eyre::eyre::eyre!(
                "{url} returned HTTP status {}",
                response.status()
            ));
        }

        if let Some(length) = response.content_length()
            && length > CONTENT_IMAGE_MAX_BYTES
        {
            return Err(color_eyre::eyre::eyre!(
                "{url} is {length} bytes, over the limit of {CONTENT_IMAGE_MAX_BYTES}"
            ));
        }

        let data = response.bytes().await.map_err(|error| {
            color_eyre::eyre::eyre!("could not read the body of {url}: {error}")
        })?;

        if data.len() as u64 > CONTENT_IMAGE_MAX_BYTES {
            return Err(color_eyre::eyre::eyre!(
                "{url} is {} bytes, over the limit of {CONTENT_IMAGE_MAX_BYTES}",
                data.len()
            ));
        }

        let (width, height) = ImageReader::new(Cursor::new(data.as_ref()))
            .with_guessed_format()
            .map_err(|error| color_eyre::eyre::eyre!("{url} has an unreadable format: {error}"))?
            .into_dimensions()
            .map_err(|error| color_eyre::eyre::eyre!("could not size {url}: {error}"))?;

        Ok::<(Vec<u8>, u32, u32), color_eyre::Report>((data.to_vec(), width, height))
    }
    .await;

    let (data, width, height) = match downloaded {
        Ok(downloaded) => (Some(downloaded.0), downloaded.1, downloaded.2),
        Err(error) => {
            log::debug!("not displaying content image {url}: {error}");
            (None, 0, 0)
        }
    };

    ContentImage {
        article_id: article_id.clone(),
        url: url.to_owned(),
        data,
        width,
        height,
    }
}

#[rustfmt::skip]        
impl NewsFlashUtils {
    pub fn new(
        news_flash: NewsFlash,
        client: Client,
        command_sender: UnboundedSender<Message>,
    ) -> Self {
        debug!("Creating NewsFlashUtils");

        Self {
            news_flash_lock: Arc::new(RwLock::new(news_flash)),
            client_lock: Arc::new(RwLock::new(client)),
            command_sender,
            undo_stack_lock: Default::default(),
            async_operation_mutex: Arc::new(Mutex::new(())),
        }
    }

    pub async fn rebuild_client(&self, config: &Config) -> color_eyre::Result<()>{
        info!("rebuilding reqwest client");
        let mut client = self.client_lock.write().await;
        *client = build_client(Duration::from_secs(config.network_timeout_seconds))?;
        Ok(())
    }

    /// Attempt to re-login using stored credentials. Returns true if successful.
    pub async fn relogin(&self) -> bool {
        let news_flash = self.news_flash_lock.read().await;
        let client = self.client_lock.read().await;
        
        if let Some(login_data) = news_flash.get_login_data().await {
            info!("Attempting re-login to refresh session");
            match news_flash.login(login_data, &client).await {
                Ok(()) => {
                    info!("Re-login successful");
                    true
                }
                Err(e) => {
                    error!("Re-login failed: {}", e);
                    false
                }
            }
        } else {
            error!("No login data available for re-login");
            false
        }
    }

    // for polling
    pub fn is_async_operation_running(&self) -> bool {
        self.async_operation_mutex.try_lock().is_err()
    }

    gen_async_call! {
        method_name: set_offline,
        params: (offline: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncSetOffline,
        operation: news_flash.set_offline(offline, &client).await?,
        success_event: Event::AsyncSetOfflineFinished(offline),
    }

    gen_async_call! {
        method_name: sync,
        params: (),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncSync,
        operation: let new_articles = news_flash.sync(&client, Default::default()).await?,
        success_event: Event::AsyncSyncFinished(new_articles),
    }

    gen_async_call! {
        method_name: fetch_thumbnail,
        params: (article_id: ArticleID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncArticleThumbnailFetch,
        operation: let thumbnail = news_flash.get_article_thumbnail(&article_id, &client).await?,
        success_event: Event::AsyncArticleThumbnailFetchFinished(thumbnail),
    }

    gen_async_call! {
        method_name: fetch_fat_article,
        params: (article_id: ArticleID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncArticleFatFetch,
        operation: let fat_article = {
            // Temporarily redirect stderr to suppress libxml xpath errors that would mess up the TUI
            let _stderr_redirect = crate::utils::prelude::StderrRedirect::new();
            
            news_flash
                .scrap_content_article(&article_id, &client)
                .await?
        },
        success_event: Event::AsyncArticleFatFetchFinished(fat_article),
    }

    gen_async_call! {
        method_name: set_article_status,
        params: (article_ids: Vec<ArticleID>, read: Read, undoable: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncArticlesSetRead,
        operation: {
            news_flash.set_article_read(&article_ids, read, &client).await?;

            if undoable {
                undo_stack.push(UndoOperation::ChangeRead(article_ids, read));
            }
        },

        success_event: Event::AsyncArticlesSetReadFinished,
    }

    gen_async_call! {
        method_name: set_article_marked,
        params: (article_ids: Vec<ArticleID>, marked: Marked, undoable: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncArticlesSetRead,
        operation: {
            news_flash.set_article_marked(&article_ids, marked, &client).await?;

            if undoable {
                undo_stack.push(
                    UndoOperation::ChangeMarked(article_ids, marked)
                    );
            }

        },

        success_event: Event::AsyncArticlesMarkFinished,
    }

    gen_async_call! {
        method_name: tag_articles,
        params: (article_ids: Vec<ArticleID>, tag_id: TagID, undoable: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncArticleTag,
        operation: {
            let mut tagged_articles: Vec<ArticleID> = Default::default();
            for article_id in article_ids {
                    news_flash.tag_article(&article_id, &tag_id, &client).await?;
                    tagged_articles.push(article_id);
            }

            if undoable {
                undo_stack.push(UndoOperation::AddTag(tagged_articles, tag_id));
            }

        },
        success_event: Event::AsyncArticleTagFinished,
    }

    gen_async_call! {
        method_name: untag_articles,
        params: (article_ids: Vec<ArticleID>, tag_id: TagID, undoable: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncArticleUntag,
        operation:{
            let mut untagged_articles: Vec<ArticleID> = Default::default();
            for article_id in article_ids {
                    news_flash.untag_article(&article_id, &tag_id, &client).await?;
                    untagged_articles.push(article_id);
            }

            if undoable {
                undo_stack.push(UndoOperation::RemoveTag(untagged_articles, tag_id));
            }

        },
        success_event: Event::AsyncArticleUntagFinished,
    }

    gen_async_call! {
        method_name: add_tag,
        params: (tag_title: String, color: Option<Color>),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncTagAdd,
        operation: let tag = news_flash.add_tag( tag_title.as_str(), color.map(|color| color.to_string()), &client).await?,
        success_event: Event::AsyncTagAddFinished(tag),
    }

    gen_async_call! {
        method_name: remove_tag,
        params: (tag_id: TagID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncTagRemove,
        operation: news_flash.remove_tag(&tag_id, &client).await?,
        success_event: Event::AsyncTagRemoveFinished,
    }

    gen_async_call! {
        method_name: edit_tag,
        params: (tag_id: TagID, new_tag_title: String, color: Option<Color>),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncTagEdit,
        operation:
            let tag = news_flash.edit_tag( &tag_id, new_tag_title.as_str(), &color.map(|color| color.to_string()), &client).await?,
        success_event: Event::AsyncTagEditFinished(tag),
    }

    gen_async_call! {
        method_name: set_all_read,
        params: (),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncFeedSetRead,
        operation: {
            let article_ids = news_flash.get_articles(
                        ArticleFilter::all_unread())?.into_iter().map(|article| article.article_id).collect();
            news_flash.set_all_read(&client).await?;
            undo_stack.push(UndoOperation::ChangeRead(article_ids, Read::Read));
        },
        success_event: Event::AsyncSetAllReadFinished,
    }

    gen_async_call! {
        method_name: set_feed_read,
        params: (feed_id: FeedID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncFeedSetRead,
        operation: {
            let feed_id = [feed_id];
            let article_ids = news_flash.get_articles(
                        ArticleFilter::feed_unread(&feed_id[0]))?
                .into_iter().map(|article| article.article_id).collect();
            news_flash.set_feed_read(&feed_id, &client).await?;
            undo_stack.push(UndoOperation::ChangeRead(
                    article_ids, Read::Read,));
        }, 
        success_event: Event::AsyncFeedSetReadFinished,
    }

    gen_async_call! {
        method_name: set_category_read,
        params: (category_id: CategoryID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncCategorySetRead,
        operation: {
            let category_id = [category_id];
            let article_ids = news_flash.get_articles(
                        ArticleFilter::category_unread(&category_id[0]))?.into_iter().map(|article| article.article_id).collect();
            news_flash.set_category_read(&category_id, &client).await?;
            undo_stack.push(UndoOperation::ChangeRead(
                    article_ids, Read::Read,));

        },
        success_event: Event::AsyncCategorySetReadFinished,
    }

    gen_async_call! {
        method_name: set_tag_read,
        params: (tag_id: TagID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: undo_stack,
        start_event: Event::AsyncTagSetRead,
        operation: {

            let tag_id = [tag_id];
            let article_ids = news_flash.get_articles(
                        ArticleFilter::tag_unread(&tag_id[0]))?.into_iter().map(|article| article.article_id).collect();
            news_flash.set_tag_read(&tag_id, &client).await?;

            undo_stack.push(UndoOperation::ChangeRead(
                    article_ids, Read::Read,));


        },

        success_event: Event::AsyncTagSetReadFinished,
    }

    gen_async_call! {
        method_name: add_feed,
        params: (url: Url, title: Option<String>, category_id: Option<CategoryID>),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedAdd,
        operation: let (feed, .. ) = news_flash.add_feed(&url, title, category_id, &client).await?,
        success_event: Event::AsyncFeedAddFinished(feed),
    }

    gen_async_call! {
        method_name: fetch_feed,
        params: (feed_id: FeedID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedFetch,
        operation: let fetched = news_flash.fetch_feed(&feed_id, &client, Default::default()).await?,
        success_event: Event::AsyncFeedFetchFinished(feed_id, fetched),
    }

    gen_async_call! {
        method_name: add_category,
        params: (title: String, parent : Option<CategoryID>),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncCategoryAdd,
        operation: let (category, .. ) = news_flash.add_category(&title, parent.as_ref(), &client).await?,
        success_event: Event::AsyncCategoryAddFinished(category),
    }

    gen_async_call! {
        method_name: rename_feed,
        params: (feed_id: FeedID, title: String),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedRename,
        operation: let feed = news_flash.rename_feed(&feed_id, title.as_str(), &client).await?,
        success_event: Event::AsyncRenameFeedFinished(feed),
    }

    gen_async_call! {
        method_name: rename_category,
        params: (category_id: CategoryID, title: String),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncCategoryRename,
        operation: let category = news_flash.rename_category(&category_id, title.as_str(), &client).await?,
        success_event: Event::AsyncCategoryRenameFinished(category),
    }

    gen_async_call! {
        method_name: remove_category,
        params: (category_id: CategoryID, remove_children: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncCategoryRemove,
        operation: news_flash.remove_category(&category_id, remove_children, &client).await?,
        success_event: Event::AsyncCategoryRemoveFinished,
    }

    gen_async_call! {
        method_name: remove_feed,
        params: (feed_id: FeedID),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedRemove,
        operation: news_flash.remove_feed(&feed_id, &client).await?,
        success_event: Event::AsyncFeedRemoveFinished,
    }

    gen_async_call! {
        method_name: edit_feed_url,
        params: (feed_id: FeedID, new_url: String),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedUrlChange,
        operation: news_flash.edit_feed_url(&feed_id, &new_url, &client).await?,
        success_event: Event::AsyncFeedUrlChangeFinished,
    }

    gen_async_call! {
        method_name: move_feed,
        params: (from_feed_mapping: FeedMapping, to_feed_mapping: FeedMapping),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncFeedMove,
        operation: news_flash.move_feed(&from_feed_mapping, &to_feed_mapping, &client).await?,
        success_event: Event::AsyncFeedMoveFinished,
    }

    gen_async_call! {
        method_name: move_category,
        params: (category_mapping: CategoryMapping),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncCategoryMove,
        operation: news_flash.move_category(&category_mapping, &client).await?,
        success_event: Event::AsyncCategoryMoveFinished,
    }

    gen_async_call! {
        method_name: import_opml,
        params: (opml: String, parse_all_feeds: bool),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncImportOpml,
        operation: news_flash.import_opml(&opml, parse_all_feeds, &client).await?,
        success_event: Event::AsyncImportOpmlFinished,
    }

    gen_async_call! {
        method_name: logout,
        params: (),
        news_flash_var: news_flash,
        client_var: client,
        undo_stack_var: _undo_stack,
        start_event: Event::AsyncLogout,
        operation: news_flash.logout(&client).await?,
        success_event: Event::AsyncLogoutFinished,
    }

    pub fn pipe(&self, article: news_flash::models::Article, fat_article: Option<news_flash::models::FatArticle>, in_target: PipeTarget, out_target: PipeTarget, command: String)  {
        let news_flash_lock = self.news_flash_lock.clone();
        let client_lock = self.client_lock.clone();
        let command_sender = self.command_sender.clone();
        let async_operation_mutex = self.async_operation_mutex.clone();
            tokio::spawn(async move {
                let _lock = async_operation_mutex.lock().await;
                if let Err(e) = async {
                     command_sender.send(Message::Event(Event::AsyncPipeArticle)).map_err(|send_error|
                         color_eyre::eyre::eyre!(send_error))?;
                
                     let client = client_lock.read().await;

                     let (command, args) = prepare_command(&command)?;

                     let input = match in_target {
                         PipeTarget::Null => "".into(),
                         target @ (PipeTarget::Html | PipeTarget::Markdown) => {
                             let fat_article = match fat_article {
                                 Some(fat_article) => fat_article,
                                 None => {
                                     let news_flash = news_flash_lock.read().await;
                                     let _stderr_redirect = crate::utils::prelude::StderrRedirect::new();
                                     news_flash.scrap_content_article(&article.article_id, &client).await?
                                 }

                             };

                             let html = fat_article.scraped_content.as_deref().unwrap_or("no scraped content available").to_string();

                             if matches!(target, PipeTarget::Html) {
                                 html
                             } else {
                                 htmd::HtmlToMarkdown::builder().build().convert(&html).map_err(|e| AsyncOperationError::Report(color_eyre::eyre::eyre!("Unable to convert HTML to Markdown: {e}")))?
                             }
                         },
                     };

                     let mut child = tokio::process::Command::new(command)
                         .args(args)
                         .stdin(Stdio::piped())
                         .stdout(Stdio::piped())
                         .stderr(Stdio::piped())
                         .spawn().map_err(|e| AsyncOperationError::Report(color_eyre::eyre::eyre!("Could not execute pipe command: {e}")))?;

                     // Write input to stdin
                     if let Some(mut stdin) = child.stdin.take() {
                         use tokio::io::AsyncWriteExt;
                         stdin.write_all(input.as_bytes()).await.map_err(|e| AsyncOperationError::Report(color_eyre::eyre::eyre!("Failed to write to stdin: {e}")))?;
                         // Drop stdin to signal EOF
                         drop(stdin);
                     }

                     // Wait for the command to finish and capture output
                     let result = child.wait_with_output().await.map_err(|e| AsyncOperationError::Report(color_eyre::eyre::eyre!("Failed to wait for command: {e}")))?;

                     let output = String::from_utf8_lossy(&result.stdout).to_string();
                     let error = String::from_utf8_lossy(&result.stderr).to_string();

                     let error = if error.is_empty() {
                         None
                     } else {
                         Some(error)
                     };

                     let markdown = match out_target {
                         PipeTarget::Null => None,
                         PipeTarget::Html => Some(HtmlToMarkdown::new().convert(&output).map_err(|conversion_error| color_eyre::eyre::eyre!(conversion_error))?),
                         PipeTarget::Markdown => Some(output),
                     };
                
                     command_sender.send(Message::Event(Event::AsyncPipeArticleFinished(article.article_id, result.status, markdown, error))).map_err(|send_error| color_eyre::eyre::eyre!(send_error))?;
                     Ok::<(), AsyncOperationError>(())
                }.await{
                     error!("Async call pipe failed: {e}");
                     let _ = command_sender.send(Message::Event(Event::AsyncOperationFailed( e,
                                 Box::new(Event::AsyncPipeArticle),)));
                }
            });

    }

    /// Fetch an article's page with the built-in HTTP client and extract its body with
    /// `libreadability`.
    ///
    /// This is the `ContentFetcher::Readability` counterpart of `fetch_fat_article`: it replaces
    /// news-flash's own scraper rather than wrapping it. Deliberately hand-written instead of
    /// using `gen_async_call!` because it needs an HTTP round-trip plus a blocking extraction
    /// step, not a single news-flash call. It still takes `async_operation_mutex` so that the
    /// status bar throbber behaves exactly as it does for a news-flash scrape.
    pub fn fetch_article_html(&self, article_id: ArticleID, url: String) {
        let client_lock = self.client_lock.clone();
        let command_sender = self.command_sender.clone();
        let async_operation_mutex = self.async_operation_mutex.clone();

        tokio::spawn(async move {
            let _lock = async_operation_mutex.lock().await;

            if let Err(e) = async {
                command_sender
                    .send(Message::Event(Event::AsyncArticleExtract))
                    .map_err(|send_error| color_eyre::eyre::eyre!(send_error))?;

                let (status, html) = {
                    let client = client_lock.read().await;
                    let response = client.get(url.as_str()).send().await.map_err(|e| {
                        AsyncOperationError::Report(color_eyre::eyre::eyre!(
                            "could not fetch {url}: {e}"
                        ))
                    })?;
                    let status = response.status();
                    let html = response.text().await.map_err(|e| {
                        AsyncOperationError::Report(color_eyre::eyre::eyre!(
                            "could not read response body of {url}: {e}"
                        ))
                    })?;
                    (status, html)
                };

                if !status.is_success() {
                    return Err(AsyncOperationError::Report(color_eyre::eyre::eyre!(
                        "{url} returned HTTP status {status}"
                    )));
                }

                // Extraction walks the whole DOM and is CPU-bound, so keep it off the async
                // worker; the client lock is already released at this point.
                let page_url = url.clone();
                let extracted =
                    tokio::task::spawn_blocking(move || extract_article(article_id, &page_url, &html))
                        .await
                        .map_err(|e| {
                            AsyncOperationError::Report(color_eyre::eyre::eyre!(
                                "extraction task for {url} failed: {e}"
                            ))
                        })??;

                command_sender
                    .send(Message::Event(Event::AsyncArticleExtractFinished(extracted)))
                    .map_err(|send_error| color_eyre::eyre::eyre!(send_error))?;

                Ok::<(), AsyncOperationError>(())
            }
            .await
            {
                error!("Async call fetch_article_html failed: {e}");
                let _ = command_sender.send(Message::Event(Event::AsyncOperationFailed(
                    e,
                    Box::new(Event::AsyncArticleExtract),
                )));
            }
        });
    }

    /// Download the images referred to by an article's content so they can be drawn inline.
    ///
    /// Deliberately does *not* take `async_operation_mutex` the way every `gen_async_call!`
    /// operation does: that mutex serialises all async work, so a batch of image downloads would
    /// block syncing and keep the status bar throbber spinning for its whole duration. Images are
    /// decoration rather than a user-visible operation, and each one is reported individually as
    /// it lands so that they can appear progressively.
    ///
    /// Returns an `AbortHandle` so the caller can cancel the batch when the user moves on to
    /// another article.
    pub fn fetch_content_images(
        &self,
        article_id: ArticleID,
        base_url: Option<String>,
        urls: Vec<String>,
    ) -> tokio::task::AbortHandle {
        let client_lock = self.client_lock.clone();
        let command_sender = self.command_sender.clone();

        tokio::spawn(async move {
            // Clone the client and release the lock straight away so that a slow batch cannot
            // block `rebuild_client`.
            let client = client_lock.read().await.clone();
            let semaphore = Arc::new(tokio::sync::Semaphore::new(CONTENT_IMAGE_CONCURRENCY));

            let downloads = urls.into_iter().map(|url| {
                let client = client.clone();
                let semaphore = Arc::clone(&semaphore);
                let article_id = article_id.clone();
                let base_url = base_url.clone();
                let command_sender = command_sender.clone();

                async move {
                    let Ok(_permit) = semaphore.acquire().await else {
                        return;
                    };

                    let image =
                        download_content_image(&client, &article_id, base_url.as_deref(), &url).await;

                    let _ = command_sender
                        .send(Message::Event(Event::AsyncContentImageFetchFinished(image)));
                }
            });

            futures::future::join_all(downloads).await;

            let _ = command_sender
                .send(Message::Event(Event::AsyncContentImagesFetchFinished(article_id)));
        })
        .abort_handle()
    }

    pub async fn undo_last_operation(&self) -> Option<UndoOperation> {

        let last_operation = {
            let mut undo_stack = self.undo_stack_lock.write().await;
            undo_stack.pop()
        };

        if let Some(last_operation) = last_operation.clone() {

            use UndoOperation as O;
            match last_operation {
                O::ChangeRead(article_ids, read) => self.set_article_status(article_ids, read.invert(), false),
                O::ChangeMarked(article_ids, marked) => self.set_article_marked(article_ids, marked.invert(), false),
                O::AddTag(article_ids, tag_id) => self.untag_articles(article_ids, tag_id, false),
                O::RemoveTag(article_ids, tag_id) => self.tag_articles(article_ids, tag_id, false),
            }

        }

        last_operation
    }

    pub fn generate_id_map<V, I: Hash + Eq + Clone>(
        items: &[V],
        id_extractor: impl Fn(&V) -> I,
    ) -> HashMap<I, V>
    where
        V: Clone,
    {
        items
            .iter()
            .map(|item| (id_extractor(item), item.clone()))
            .collect()
    }

    pub fn generate_one_to_many<E, I: Hash + Eq + Clone, V>(
        mappings: &[E],
        id_extractor: impl Fn(&E) -> I,
        value_extractor: impl Fn(&E) -> V,
    ) -> HashMap<I, Vec<V>>
    where
        V: Clone,
    {
        mappings.iter().fold(HashMap::new(), |mut acc, mapping| {
            acc.entry(id_extractor(mapping).clone())
                .or_default()
                .push(value_extractor(mapping).clone());
            acc
        })
    }

    pub fn tag_color(tag: &Tag) -> Option<Color> {
        if let Some(color_str) = tag.color.clone()
            && let Ok(tag_color) = Color::from_str(color_str.as_str())
        {
            return Some(tag_color);
        }

        None
    }

    pub fn tag_to_line<'a>(tag: &Tag, config: &Config, override_color: Option<Color>) -> Line<'a> {
        let color = override_color
            .or(Self::tag_color(tag))
            .or(config.theme.tag().fg)
            .unwrap_or_default();
        let style = config.theme.tag().fg(color);
        to_bubble(Span::styled(tag.label.to_owned(), style), config)
    }

    fn get_root_cause_message(error: &dyn Error) -> String {
        let mut current_error = error;
        while let Some(source) = current_error.source() {
            current_error = source;
        }
        current_error.to_string()
    }

    pub fn error_to_message(news_flash_error: &NewsFlashError) -> String {
        match news_flash_error {
            NewsFlashError::Database(database_error) => {
                format!("Database error ({}).", Self::get_root_cause_message(&database_error))
            }
            
            NewsFlashError::API(feed_api_error) => {
                format!("API error ({})", Self::get_root_cause_message(&feed_api_error))
            }
            
            NewsFlashError::IO(error) => {
                format!("IO error ({})", Self::get_root_cause_message(&error))
            }
            
            NewsFlashError::LoadBackend => {
                "Failed to load NewsFlash backend.".to_string()
            }
            
            NewsFlashError::Icon(fav_icon_error) => {
                format!("Favicon error: {}.", fav_icon_error)
            }
            
            NewsFlashError::Url(parse_error) => {
                format!("Invalid URL format: {}", parse_error)
            }
            
            NewsFlashError::NotLoggedIn => {
                "You need be logged in to perform this action. Please log in first.".to_string()
            }
            
            NewsFlashError::Thumbnail => {
                "Failed to load or generate thumbnail image for the article.".to_string()
            }
            
            NewsFlashError::OPML(error) => {
                format!("OPML file processing failed: {}. The file may be corrupted or invalid.", error)
            }
            
            NewsFlashError::ImageDownload(image_download_error) => {
                format!("Failed to download images for article: {}", image_download_error)
            }
            
            NewsFlashError::GrabContent => {
                "Failed to download full article content.".to_string()
            }
            
            NewsFlashError::Semaphore(acquire_error) => {
                format!("Unable to start concurrent operation: {}", acquire_error)
            }
            
            NewsFlashError::Syncing => {
                "Cannot perform this operation while syncing feeds. Please wait for sync to complete.".to_string()
            }
            
            NewsFlashError::Offline => {
                "Cannot perform this operation while offline.".to_string()
            }
            
            NewsFlashError::Unknown => {
                "An unknown error occurred.".to_string()
            }
        }
    }
}

pub async fn login_news_flash(
    client: &reqwest::Client,
    cli_args: &CliArgs,
    config: &Config,
) -> color_eyre::Result<NewsFlash> {
    let news_flash_config_dir = cli_args
        .news_flash_config_dir()
        .as_ref()
        .map(Path::new)
        .unwrap_or(PROJECT_DIRS.config_dir());

    let state_dir = cli_args
        .news_flash_state_dir()
        .as_ref()
        .map(Path::new)
        .unwrap_or(PROJECT_DIRS.state_dir().unwrap_or(PROJECT_DIRS.data_dir()));

    info!("newsflash config dir: {news_flash_config_dir:?}");
    info!("state dir: {state_dir:?}");

    let news_flash_attempt = NewsFlash::builder()
        .config_dir(news_flash_config_dir)
        .data_dir(state_dir)
        .try_load();

    Ok(match news_flash_attempt {
        Ok(news_flash) => {
            // Re-login to refresh session token
            if let Some(login_data) = news_flash.get_login_data().await {
                info!("Re-logging in to refresh session");
                if let Err(e) = news_flash.login(login_data, client).await {
                    error!("Failed to re-login: {}. Session may have expired.", e);
                }
            }
            news_flash
        }
        Err(_) => {
            // this is the initial setup => setup login data
            info!("no profile found => ask user or try config");
            let mut logged_in = false;
            // skip if login configuration is given
            let mut skip_asking_for_login = config.login_setup.is_some();

            let mut login_data: Option<LoginData> = config
                .login_setup
                .as_ref()
                .inspect(|_| info!("login configuration found"))
                .map(|login_configuration| login_configuration.to_login_data())
                .transpose()?;
            let login_setup = LoginSetup::new();
            let mut news_flash: Option<NewsFlash> = None;
            while !logged_in {
                login_data = if login_data.is_none() || !skip_asking_for_login {
                    skip_asking_for_login = false;
                    Some(login_setup.inquire_login_data(&login_data).await?)
                } else {
                    login_data
                };
                news_flash = Some(
                    NewsFlash::builder()
                        .data_dir(state_dir)
                        .config_dir(news_flash_config_dir)
                        .plugin(login_data.as_ref().unwrap().id())
                        .create()?,
                );
                logged_in = login_setup
                    .login_and_initial_sync(
                        news_flash.as_ref().unwrap(),
                        login_data.as_ref().unwrap(),
                        client,
                    )
                    .await?;
            }
            news_flash.unwrap()
        }
    })
}

#[allow(clippy::type_complexity)]
pub fn get_feeds_and_categories(
    news_flash: &NewsFlash,
) -> Result<
    (
        Vec<Feed>,
        std::collections::HashMap<news_flash::models::FeedID, Feed>,
        std::collections::HashMap<news_flash::models::FeedID, news_flash::models::FeedMapping>,
        Vec<Category>,
        std::collections::HashMap<CategoryID, Category>,
        std::collections::HashMap<CategoryID, news_flash::models::CategoryMapping>,
    ),
    color_eyre::eyre::Error,
> {
    let (feeds, feed_mapping) = news_flash.get_feeds()?;
    let feed_for_feed_id = NewsFlashUtils::generate_id_map(&feeds, |feed| feed.feed_id.to_owned());
    let feed_mapping_for_feed_id =
        NewsFlashUtils::generate_id_map(&feed_mapping, |mapping| mapping.feed_id.to_owned());
    let (categories, category_mapping) = news_flash.get_categories()?;
    let category_for_category_id =
        NewsFlashUtils::generate_id_map(&categories, |category| category.category_id.to_owned());
    let category_mapping_for_category_id =
        NewsFlashUtils::generate_id_map(&category_mapping, |category_mapping| {
            category_mapping.category_id.to_owned()
        });
    Ok((
        feeds,
        feed_for_feed_id,
        feed_mapping_for_feed_id,
        categories,
        category_for_category_id,
        category_mapping_for_category_id,
    ))
}

pub fn sort_feeds_and_categories(
    feeds: &mut [Feed],
    categories: &mut [Category],
    feed_mapping_for_feed_id: &std::collections::HashMap<
        news_flash::models::FeedID,
        news_flash::models::FeedMapping,
    >,
    category_mapping_for_category_id: &std::collections::HashMap<
        news_flash::models::CategoryID,
        news_flash::models::CategoryMapping,
    >,
) {
    let category_cmp = |c1: Option<&CategoryID>, c2: Option<&CategoryID>| {
        let sort_index_for_c1 = c1.and_then(|c_id| {
            category_mapping_for_category_id
                .get(c_id)
                .map(|mapping| &mapping.sort_index)
        });
        let sort_index_for_c2 = c2.and_then(|c_id| {
            category_mapping_for_category_id
                .get(c_id)
                .map(|mapping| &mapping.sort_index)
        });

        sort_index_for_c1.cmp(&sort_index_for_c2)
    };

    categories.sort_by(|c1, c2| category_cmp(Some(&c1.category_id), Some(&c2.category_id)));

    feeds.sort_by(|f1, f2| {
        let feed_mapping_for_f1 = feed_mapping_for_feed_id.get(&f1.feed_id);
        let feed_mapping_for_f2 = feed_mapping_for_feed_id.get(&f2.feed_id);

        category_cmp(
            feed_mapping_for_f1.map(|mapping| &mapping.category_id),
            feed_mapping_for_f2.map(|mapping| &mapping.category_id),
        )
        .then(
            feed_mapping_for_f1
                .map(|feed_mapping| feed_mapping.sort_index)
                .cmp(&feed_mapping_for_f2.map(|feed_mapping| feed_mapping.sort_index)),
        )
    });
}

#[cfg(test)]
mod extract_article_test {
    use super::*;

    const PAGE_URL: &str = "https://example.com/news/story";

    /// A realistic news page: article body surrounded by navigation, ads, a teaser sidebar,
    /// a footer, inline styles and tracking scripts.
    const ARTICLE_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><title>Site Name - Article Title</title>
<style>body{font-family:sans-serif}.ad{background:#eee}</style>
<script>window.tracking = {id: 42};</script></head>
<body>
<header><nav><a href="/">Home</a><a href="/world">World</a><a href="/tech">Tech</a>
<form action="/search"><input name="q"><button>Search</button></form></nav></header>
<div class="ad">ADVERTISEMENT - buy things now</div>
<article>
  <h1>The Real Article Headline</h1>
  <p class="byline">By Jane Reporter</p>
  <p>First paragraph of the actual story, which needs to be long enough that the
     readability scoring algorithm considers this container to be the dominant
     text block on the page rather than the navigation or the sidebar.</p>
  <figure><img src="/media/photo-1234.jpg" alt="A protester holds a sign">
    <figcaption>Protesters gathered outside the ministry. Photograph: AP</figcaption></figure>
  <p>Second paragraph continues the story with more detail about what happened
     and who was involved, adding enough weight for the extraction to lock on.</p>
  <blockquote><p>A quote from an official who spoke on condition of anonymity.</p></blockquote>
  <p>Third paragraph wraps up the reporting and points to what happens next in
     the coming weeks, according to people familiar with the matter.</p>
  <img src="https://cdn.example.com/inline-chart.png" alt="Chart of results">
  <p>Final paragraph of the story proper.</p>
</article>
<aside><h2>Related articles</h2><ul><li><a href="/a">Teaser one</a></li>
<li><a href="/b">Teaser two</a></li><li><a href="/c">Teaser three</a></li></ul></aside>
<div class="ad">MORE ADVERTISEMENTS</div>
<footer><p>Copyright 2026 Site Name</p><nav><a href="/privacy">Privacy</a>
<a href="/terms">Terms</a></nav></footer>
<script>console.log("analytics");</script>
</body></html>"#;

    fn extract(html: &str) -> Result<ExtractedArticle, AsyncOperationError> {
        extract_article(ArticleID::new("article-1"), PAGE_URL, html)
    }

    #[test]
    fn extracts_the_story_and_drops_site_chrome() {
        let extracted = extract(ARTICLE_PAGE).expect("extraction succeeds");

        assert!(
            extracted
                .content
                .contains("First paragraph of the actual story")
        );
        assert!(
            extracted
                .content
                .contains("Final paragraph of the story proper")
        );

        for chrome in [
            "ADVERTISEMENT",
            "Teaser one",
            "Copyright 2026",
            "window.tracking",
            "console.log",
            "font-family",
            "<nav",
            "<form",
            "<aside",
            "<script",
            "<style",
        ] {
            assert!(
                !extracted.content.contains(chrome),
                "{chrome:?} should have been stripped, content was {}",
                extracted.content
            );
        }
    }

    #[test]
    fn keeps_images_and_makes_their_urls_absolute() {
        let extracted = extract(ARTICLE_PAGE).expect("extraction succeeds");

        // Two images in the body: one relative, one already absolute.
        assert_eq!(2, extracted.content.matches("<img").count());
        assert!(
            extracted
                .content
                .contains("https://example.com/media/photo-1234.jpg"),
            "relative src should be resolved against the page URL, content was {}",
            extracted.content
        );
        assert!(
            extracted
                .content
                .contains("https://cdn.example.com/inline-chart.png")
        );
        assert!(
            extracted
                .content
                .contains(r#"alt="A protester holds a sign""#)
        );
    }

    #[test]
    fn extracts_byline_and_plain_text() {
        let extracted = extract(ARTICLE_PAGE).expect("extraction succeeds");

        assert_eq!(Some("By Jane Reporter".to_owned()), extracted.byline);
        assert!(
            extracted
                .text_content
                .contains("First paragraph of the actual story")
        );
        assert!(
            !extracted.text_content.contains('<'),
            "plain text had markup"
        );
        assert_eq!(ArticleID::new("article-1"), extracted.article_id);
    }

    /// A page whose body is built client-side yields nothing over plain HTTP. Extraction must
    /// fail rather than show an empty article, so that the caller falls back to news-flash.
    #[test]
    fn fails_on_a_client_side_rendered_shell() {
        let shell = r#"<html><head><title>Empty</title></head><body><div id="app"></div>
<script>renderApp();</script></body></html>"#;

        claims::assert_matches!(extract(shell), Err(_));
    }

    /// Readability discards a gallery with no accompanying text. Failing here hands the article
    /// to news-flash's scraper, which may do better.
    #[test]
    fn fails_when_nothing_readable_is_found() {
        let gallery = r#"<html><head><title>Gallery</title></head><body>
<div id="content"><img src="https://cdn.example.com/1.jpg" alt="One">
<img src="https://cdn.example.com/2.jpg" alt="Two"></div></body></html>"#;

        claims::assert_matches!(extract(gallery), Err(_));
    }

    #[test]
    fn fails_on_unparseable_input() {
        claims::assert_matches!(extract(""), Err(_));
    }
}
