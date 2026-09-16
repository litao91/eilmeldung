//! Row-based layout for article content that mixes wrapped text with inline images.
//!
//! `Paragraph` can only scroll text, so the article pane builds its own list of [`ContentRow`]s:
//! a text row is always exactly one terminal row, while an image row occupies as many rows as the
//! image needs. That makes `max_scroll` exact and lets an image sit at a known offset in the
//! document rather than being an overlay.

use crate::utils::text_wrap::wrap_spans;

use ratatui::text::{Line, Span, Text};
use ratatui_image::FontSize;

/// Emitted by the markdown image hook in place of an image that is ready to be drawn.
///
/// The hook's spans are pushed into the line verbatim by the markdown renderer, so an exact match
/// on the span content is enough to find them, and [`build_rows`] splits them out *before*
/// wrapping — the character never reaches a rendered row. NUL is used because it cannot occur in
/// article text.
pub(super) const IMAGE_SENTINEL: &str = "\u{0}";

#[derive(Debug)]
pub(super) enum ContentRow {
    Text(Line<'static>),
    /// `index` refers to the caller's ordered list of [`ContentImageRef`].
    Image {
        index: usize,
        height: u16,
    },
}

impl ContentRow {
    pub(super) fn height(&self) -> u16 {
        match self {
            ContentRow::Text(_) => 1,
            ContentRow::Image { height, .. } => *height,
        }
    }
}

/// An image the markdown hook asked to be drawn, in document order.
#[derive(Debug, Clone)]
pub(super) struct ContentImageRef {
    /// The URL exactly as it appeared in the markdown; this is the cache key everywhere.
    pub(super) url: String,
    pub(super) px_width: u32,
    pub(super) px_height: u32,
}

/// Turn rendered markdown into rows of known height.
///
/// `image_height` must be derived from [`image_cell_size`] with the same arguments the caller uses
/// when building the image protocol, or the reserved height and the drawn height will disagree.
pub(super) fn build_rows(
    text: Text<'static>,
    images: &[ContentImageRef],
    width: u16,
    image_height: impl Fn(&ContentImageRef) -> u16,
) -> (Vec<ContentRow>, u16) {
    let mut rows: Vec<ContentRow> = Vec::new();
    let mut image_index = 0usize;

    for line in text.lines {
        let mut pending: Vec<Span<'static>> = Vec::new();
        let mut saw_image = false;

        for span in line.spans {
            if span.content != IMAGE_SENTINEL {
                pending.push(span);
                continue;
            }

            saw_image = true;
            if !pending.is_empty() {
                flush_text(&mut rows, &mut pending, width);
            }

            // An image without a known size is dropped rather than reserving an empty gap; the
            // hook only emits sentinels for images that decoded successfully.
            if let Some(image) = images.get(image_index) {
                let height = image_height(image);
                if height > 0 {
                    rows.push(ContentRow::Image {
                        index: image_index,
                        height,
                    });
                }
            }
            image_index += 1;
        }

        // A line with no sentinel must still produce a row so that blank lines are preserved.
        if !pending.is_empty() || !saw_image {
            flush_text(&mut rows, &mut pending, width);
        }
    }

    let total_height = rows
        .iter()
        .fold(0u32, |total, row| {
            total.saturating_add(u32::from(row.height()))
        })
        .min(u32::from(u16::MAX)) as u16;

    (rows, total_height)
}

/// Append the wrapped form of `pending` to `rows`, clearing it.
///
/// An empty `pending` still yields one empty row, which is what preserves blank lines.
fn flush_text(rows: &mut Vec<ContentRow>, pending: &mut Vec<Span<'static>>, width: u16) {
    let spans = std::mem::take(pending);
    rows.extend(wrap_spans(&spans, width).into_iter().map(ContentRow::Text));
}

/// Size an image into terminal cells.
///
/// Returns `(columns, rows)`, or `(0, 0)` when the image should not be drawn at all. Images are
/// never scaled up past their intrinsic size, and the height cap shrinks the width too so that the
/// aspect ratio survives.
pub(super) fn image_cell_size(
    px_width: u32,
    px_height: u32,
    available_columns: u16,
    font: FontSize,
    max_rows: u16,
) -> (u16, u16) {
    if px_width == 0 || px_height == 0 || available_columns == 0 || max_rows == 0 {
        return (0, 0);
    }

    // Some terminals do not report a cell size; fall back to the near-universal 1:2 aspect ratio.
    let (cell_w, cell_h) = if font.width == 0 || font.height == 0 {
        (1u32, 2u32)
    } else {
        (u32::from(font.width), u32::from(font.height))
    };

    let available_columns = u32::from(available_columns);
    let max_rows = u32::from(max_rows);

    // Intrinsic width in columns, capped by the pane: images are never scaled up.
    let columns = px_width.div_ceil(cell_w).max(1).min(available_columns);

    let rows =
        (columns as f32 * cell_w as f32 * px_height as f32) / (px_width as f32 * cell_h as f32);
    let rows = (rows.ceil() as u32).max(1);

    if rows <= max_rows {
        return (columns as u16, rows as u16);
    }

    // Height-limited: derive the width from the capped height instead.
    let target_px_height = max_rows.saturating_mul(cell_h);
    let target_px_width = (target_px_height as f32 * px_width as f32 / px_height as f32) as u32;
    let columns = (target_px_width / cell_w).clamp(1, available_columns);

    (columns as u16, max_rows as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(url: &str, px_width: u32, px_height: u32) -> ContentImageRef {
        ContentImageRef {
            url: url.to_owned(),
            px_width,
            px_height,
        }
    }

    fn row_texts(rows: &[ContentRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                ContentRow::Text(line) => line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect(),
                ContentRow::Image { index, height } => format!("<image {index} {height}>"),
            })
            .collect()
    }

    #[test]
    fn image_cell_size_scales_to_the_available_width() {
        // A 1600x900 image with 8x16 pixel cells in an 80 column pane: width-limited to 80
        // columns, and 900 * (80*8 / 1600) = 360 px tall, which is 360/16 = 22.5 -> 23 rows.
        assert_eq!(
            (80, 23),
            image_cell_size(1600, 900, 80, FontSize::new(8, 16), 40)
        );
    }

    #[test]
    fn image_cell_size_never_scales_up() {
        // 80x32 px at 8x16 per cell is intrinsically 10 columns by 2 rows.
        assert_eq!(
            (10, 2),
            image_cell_size(80, 32, 60, FontSize::new(8, 16), 40)
        );
    }

    #[test]
    fn image_cell_size_caps_the_height_and_keeps_the_aspect_ratio() {
        // A very tall image: 400x4000 px, intrinsic 50 columns and 250 rows, capped at 10 rows.
        // 10 rows * 16 px = 160 px tall, so the width becomes 400 * 160 / 4000 = 16 px = 2 columns.
        assert_eq!(
            (2, 10),
            image_cell_size(400, 4000, 80, FontSize::new(8, 16), 10)
        );
    }

    #[test]
    fn image_cell_size_falls_back_to_a_two_to_one_cell_aspect() {
        // With no reported cell size a cell is assumed to be 1 px wide and 2 px tall, so a
        // 200x100 image at 40 columns is 40 px wide, 20 px tall, i.e. 10 rows.
        assert_eq!(
            (40, 10),
            image_cell_size(200, 100, 40, FontSize::new(0, 0), 30)
        );
    }

    #[test]
    fn image_cell_size_rejects_degenerate_input() {
        assert_eq!(
            (0, 0),
            image_cell_size(0, 100, 80, FontSize::new(8, 16), 20)
        );
        assert_eq!(
            (0, 0),
            image_cell_size(100, 0, 80, FontSize::new(8, 16), 20)
        );
        assert_eq!(
            (0, 0),
            image_cell_size(100, 100, 0, FontSize::new(8, 16), 20)
        );
        assert_eq!(
            (0, 0),
            image_cell_size(100, 100, 80, FontSize::new(8, 16), 0)
        );
    }

    #[test]
    fn build_rows_places_an_image_between_the_surrounding_text() {
        let text = Text::from(vec![
            Line::from("before the picture"),
            Line::from(vec![Span::from(IMAGE_SENTINEL)]),
            Line::from("after the picture"),
        ]);
        let images = vec![image("https://example.com/a.png", 100, 100)];

        let (rows, total_height) = build_rows(text, &images, 40, |image| {
            image_cell_size(
                image.px_width,
                image.px_height,
                40,
                FontSize::new(8, 16),
                20,
            )
            .1
        });

        assert_eq!(
            vec!["before the picture", "<image 0 7>", "after the picture"],
            row_texts(&rows)
        );
        assert_eq!(1 + 7 + 1, total_height);
    }

    #[test]
    fn build_rows_splits_a_paragraph_around_an_inline_image() {
        let text = Text::from(vec![Line::from(vec![
            Span::raw("words before ".to_owned()),
            Span::from(IMAGE_SENTINEL),
            Span::raw(" words after".to_owned()),
        ])]);
        let images = vec![image("https://example.com/a.png", 80, 32)];

        let (rows, _) = build_rows(text, &images, 40, |image| {
            image_cell_size(
                image.px_width,
                image.px_height,
                40,
                FontSize::new(8, 16),
                20,
            )
            .1
        });

        assert_eq!(
            vec!["words before", "<image 0 2>", "words after"],
            row_texts(&rows)
        );
    }

    #[test]
    fn build_rows_preserves_blank_lines() {
        let text = Text::from(vec![Line::from("one"), Line::from(""), Line::from("two")]);

        let (rows, total_height) = build_rows(text, &[], 40, |_| 0);

        assert_eq!(vec!["one", "", "two"], row_texts(&rows));
        assert_eq!(3, total_height);
    }

    #[test]
    fn build_rows_wraps_text_and_counts_the_wrapped_rows() {
        let text = Text::from(vec![Line::from("the quick brown fox jumps")]);

        let (rows, total_height) = build_rows(text, &[], 9, |_| 0);

        assert_eq!(vec!["the quick", "brown fox", "jumps"], row_texts(&rows));
        assert_eq!(3, total_height);
    }

    #[test]
    fn build_rows_numbers_images_in_document_order() {
        let text = Text::from(vec![
            Line::from(vec![Span::from(IMAGE_SENTINEL)]),
            Line::from("between"),
            Line::from(vec![Span::from(IMAGE_SENTINEL)]),
        ]);
        let images = vec![
            image("https://example.com/a.png", 80, 32),
            image("https://example.com/b.png", 80, 32),
        ];

        let (rows, _) = build_rows(text, &images, 40, |image| {
            image_cell_size(
                image.px_width,
                image.px_height,
                40,
                FontSize::new(8, 16),
                20,
            )
            .1
        });

        assert_eq!(
            vec!["<image 0 2>", "between", "<image 1 2>"],
            row_texts(&rows)
        );
    }

    #[test]
    fn build_rows_drops_an_image_that_has_no_size() {
        let text = Text::from(vec![
            Line::from("before"),
            Line::from(vec![Span::from(IMAGE_SENTINEL)]),
            Line::from("after"),
        ]);

        let (rows, _) = build_rows(text, &[], 40, |_| 0);

        assert_eq!(vec!["before", "after"], row_texts(&rows));
    }

    #[test]
    fn build_rows_leaves_an_unknown_image_index_unrendered_but_harmless() {
        let text = Text::from(vec![Line::from(vec![Span::from(IMAGE_SENTINEL)])]);

        // No images at all, so index 0 does not resolve; the sentinel must not reach a row.
        let (rows, total_height) = build_rows(text, &[], 40, |_| 5);

        assert!(rows.is_empty());
        assert_eq!(0, total_height);
    }

    /// The sentinel is matched by exact span content, and the markdown renderer pushes the hook's
    /// spans into the line verbatim, so it can sit between ordinary text spans.
    #[test]
    fn the_sentinel_never_reaches_a_rendered_row() {
        let text = Text::from(vec![Line::from(vec![
            Span::raw("a".to_owned()),
            Span::from(IMAGE_SENTINEL),
            Span::raw("b".to_owned()),
        ])]);
        let images = vec![image("https://example.com/a.png", 80, 32)];

        for height in [0u16, 3] {
            let (rows, _) = build_rows(text.clone(), &images, 40, move |_| height);
            let rendered = row_texts(&rows);
            assert!(
                !rendered.iter().any(|row| row.contains(IMAGE_SENTINEL)),
                "sentinel leaked at height {height}: {rendered:?}"
            );
        }
    }

    /// End-to-end check of the chain the article pane actually uses: extracted HTML, through the
    /// real `htmd` and `the-other-tui-markdown` crates, into rows with an image between the
    /// paragraphs that surround it.
    #[test]
    fn extracted_html_becomes_text_rows_and_an_image_row_in_place() {
        let page = r#"<html><body><article>
<h1>Headline</h1>
<p>First paragraph of the story, long enough that the readability scoring treats this
   container as the dominant text block on the page rather than any surrounding chrome.</p>
<img src="/media/photo.jpg" alt="A photograph">
<p>Second paragraph of the story, which continues after the picture and is also long
   enough to keep the extraction anchored on the article body rather than the sidebar.</p>
</article></body></html>"#;

        let extracted =
            libreadability::extract(page, Some("https://example.com/story")).expect("extractable");
        assert!(
            extracted
                .content
                .contains("https://example.com/media/photo.jpg"),
            "the image URL should have been made absolute: {}",
            extracted.content
        );

        let markdown = htmd::HtmlToMarkdown::builder()
            .build()
            .convert(&extracted.content)
            .expect("convertible");
        assert!(markdown.contains("![A photograph]"), "got {markdown:?}");

        // Stand in for the pane's image hook: an image that is ready to draw becomes a sentinel.
        let renderer = the_other_tui_markdown::RendererBuilder::new()
            .with_image(|_alt, _url| vec![Span::from(IMAGE_SENTINEL)])
            .build();
        let text = the_other_tui_markdown::into_text_with_renderer(&markdown, &renderer);

        let images = vec![image("https://example.com/media/photo.jpg", 400, 200)];
        let font = FontSize::new(8, 16);
        let (rows, total_height) = build_rows(text, &images, 60, |image| {
            image_cell_size(image.px_width, image.px_height, 60, font, 20).1
        });

        let rendered = row_texts(&rows);
        let position_of = |needle: &str| {
            rendered
                .iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from {rendered:?}"))
        };

        let image_row = position_of("<image 0 ");
        assert!(
            position_of("First paragraph") < image_row
                && image_row < position_of("Second paragraph"),
            "image should sit between the paragraphs: {rendered:?}"
        );
        assert!(
            total_height > rendered.len() as u16,
            "the image should occupy several rows: {rendered:?} total {total_height}"
        );
        assert!(
            !rendered.iter().any(|row| row.contains(IMAGE_SENTINEL)),
            "sentinel leaked: {rendered:?}"
        );
    }
}
