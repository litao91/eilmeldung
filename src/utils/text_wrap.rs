//! Span-aware greedy word wrapping.
//!
//! ratatui does not expose the wrapper behind `Paragraph`, and both the article content pane (which
//! interleaves image rows) and the article list (which caps summaries at two lines) need to know
//! exact line boundaries, so the wrapping is done here instead.

use std::borrow::Cow;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A tab is expanded to this many columns before measuring.
const TAB_WIDTH: usize = 4;

/// One indivisible piece of a line: either a run of whitespace or a run of non-whitespace.
struct Atom {
    text: String,
    style: Style,
    width: usize,
    is_space: bool,
}

fn atoms_of(span: &Span<'static>) -> Vec<Atom> {
    let mut atoms = Vec::new();
    let mut text = String::new();
    let mut text_is_space: Option<bool> = None;

    let push = |atoms: &mut Vec<Atom>, text: &mut String, text_is_space: &mut Option<bool>| {
        if text.is_empty() {
            return;
        }
        atoms.push(Atom {
            width: UnicodeWidthStr::width(text.as_str()),
            is_space: text_is_space.unwrap_or(false),
            style: span.style,
            text: std::mem::take(text),
        });
        *text_is_space = None;
    };

    for character in span.content.chars() {
        // Newlines are hard breaks; callers do not normally emit them inside a line, but a stray
        // one must not silently join two rows.
        if character == '\n' {
            push(&mut atoms, &mut text, &mut text_is_space);
            atoms.push(Atom {
                text: "\n".to_owned(),
                style: span.style,
                width: 0,
                is_space: true,
            });
            continue;
        }

        let expanded = if character == '\t' {
            " ".repeat(TAB_WIDTH)
        } else {
            character.to_string()
        };
        let is_space = character.is_whitespace();

        if text_is_space == Some(!is_space) {
            push(&mut atoms, &mut text, &mut text_is_space);
        }

        text.push_str(&expanded);
        text_is_space = Some(is_space);
    }

    push(&mut atoms, &mut text, &mut text_is_space);
    atoms
}

/// Greedy word-wrap of a single styled line into rows of at most `width` display columns.
///
/// Mirrors `Paragraph`'s `Wrap { trim: true }`: whitespace at the start of a row is dropped, and
/// the whitespace run at a break point is consumed by the break. An empty input yields one empty
/// row, so that blank lines survive. A `width` of 0 yields nothing.
pub fn wrap_spans(spans: &[Span<'static>], width: u16) -> Vec<Line<'static>> {
    let width = width as usize;

    // A collapsed area has no room for anything; wrapping here would emit one row per character.
    if width == 0 {
        return Vec::new();
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut row_widths: Vec<usize> = vec![0];

    for atom in spans.iter().flat_map(atoms_of) {
        if atom.text == "\n" {
            trim_trailing_space(&mut rows, &mut row_widths);
            rows.push(Vec::new());
            row_widths.push(0);
            continue;
        }

        if atom.is_space {
            // Leading whitespace of a row is trimmed.
            if rows.last().is_some_and(|row| row.is_empty()) {
                continue;
            }
            push_text(
                &mut rows,
                &mut row_widths,
                atom.text,
                atom.style,
                atom.width,
            );
            continue;
        }

        if *row_widths.last().unwrap_or(&0) > 0
            && *row_widths.last().unwrap_or(&0) + atom.width > width
        {
            trim_trailing_space(&mut rows, &mut row_widths);
            rows.push(Vec::new());
            row_widths.push(0);
        }

        if atom.width > width {
            push_broken_word(&mut rows, &mut row_widths, atom, width);
        } else {
            push_text(
                &mut rows,
                &mut row_widths,
                atom.text,
                atom.style,
                atom.width,
            );
        }
    }

    trim_trailing_space(&mut rows, &mut row_widths);
    rows.into_iter().map(Line::from).collect()
}

fn push_text(
    rows: &mut Vec<Vec<Span<'static>>>,
    row_widths: &mut [usize],
    text: String,
    style: Style,
    width: usize,
) {
    let row = rows.last_mut().expect("there is always at least one row");

    // Merge into the previous span when the style matches, so that a long paragraph does not turn
    // into hundreds of single-word spans.
    if let Some(last) = row.last_mut()
        && last.style == style
    {
        last.content = Cow::Owned(format!("{}{}", last.content, text));
    } else {
        row.push(Span::styled(text, style));
    }

    let row_width = row_widths.last_mut().expect("rows and widths stay in step");
    *row_width += width;
}

/// Drop the whitespace run at the end of the current row, as `Wrap { trim: true }` does.
fn trim_trailing_space(rows: &mut Vec<Vec<Span<'static>>>, row_widths: &mut [usize]) {
    let (Some(row), Some(row_width)) = (rows.last_mut(), row_widths.last_mut()) else {
        return;
    };

    loop {
        let Some(index) = row.len().checked_sub(1) else {
            return;
        };

        let content = row[index].content.as_ref().to_owned();
        let trimmed_end = content.trim_end().len();
        if trimmed_end == content.len() {
            return;
        }

        let removed = UnicodeWidthStr::width(&content[trimmed_end..]);
        if trimmed_end == 0 {
            row.pop();
        } else {
            let style = row[index].style;
            row[index] = Span::styled(content[..trimmed_end].to_owned(), style);
        }
        *row_width = row_width.saturating_sub(removed);
    }
}

/// Split a single word that is wider than the row, hard-breaking it across rows.
fn push_broken_word(
    rows: &mut Vec<Vec<Span<'static>>>,
    row_widths: &mut Vec<usize>,
    atom: Atom,
    width: usize,
) {
    let Atom { text, style, .. } = atom;
    let mut chunk = String::new();
    let mut chunk_width = 0usize;

    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);

        // Combining marks and other zero-width characters belong to what precedes them.
        if character_width == 0 && !chunk.is_empty() {
            chunk.push(character);
            continue;
        }

        if chunk_width + character_width > width && !chunk.is_empty() {
            let taken = std::mem::take(&mut chunk);
            push_text(rows, row_widths, taken, style, chunk_width);
            chunk_width = 0;
            trim_trailing_space(rows, row_widths);
            rows.push(Vec::new());
            row_widths.push(0);
        }

        chunk.push(character);
        chunk_width += character_width;
    }

    if !chunk.is_empty() {
        let row_width = *row_widths.last().unwrap_or(&0);
        if row_width > 0 && row_width + chunk_width > width {
            trim_trailing_space(rows, row_widths);
            rows.push(Vec::new());
            row_widths.push(0);
        }
        push_text(rows, row_widths, chunk, style, chunk_width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn plain(content: &str) -> Vec<Span<'static>> {
        vec![Span::raw(content.to_owned())]
    }

    fn wrapped(content: &str, width: u16) -> Vec<String> {
        wrap_spans(&plain(content), width)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn wraps_greedily_at_word_boundaries() {
        assert_eq!(
            vec!["the quick", "brown fox"],
            wrapped("the quick brown fox", 9)
        );
    }

    #[test]
    fn a_word_that_exactly_fills_the_row_does_not_break() {
        assert_eq!(vec!["abcd", "efgh"], wrapped("abcd efgh", 4));
    }

    #[test]
    fn trims_leading_whitespace_of_continuation_rows() {
        assert_eq!(vec!["one", "two"], wrapped("one     two", 3));
    }

    #[test]
    fn trims_trailing_whitespace_before_a_break() {
        assert_eq!(vec!["one", "two"], wrapped("one   \ntwo", 10));
    }

    #[test]
    fn keeps_single_line_content_on_one_row() {
        assert_eq!(vec!["short"], wrapped("short", 80));
    }

    #[test]
    fn empty_input_yields_one_empty_row() {
        assert_eq!(vec![""], wrapped("", 10));
    }

    #[test]
    fn zero_width_yields_nothing() {
        assert!(wrap_spans(&plain("anything"), 0).is_empty());
    }

    #[test]
    fn hard_breaks_a_word_wider_than_the_row() {
        assert_eq!(
            vec!["https://", "example.", "com/long"],
            wrapped("https://example.com/long", 8)
        );
    }

    #[test]
    fn counts_wide_characters_as_two_columns() {
        // Each CJK character is two columns wide, so only one fits per row of width 2.
        assert_eq!(vec!["日本", "語"], wrapped("日本語", 4));
        assert_eq!(vec!["日", "本", "語"], wrapped("日本語", 2));
    }

    #[test]
    fn a_two_column_character_does_not_straddle_the_boundary() {
        assert_eq!(vec!["ab", "日"], wrapped("ab日", 2));
    }

    #[test]
    fn expands_tabs() {
        assert_eq!(vec!["a", "b"], wrapped("a\tb", 4));
    }

    #[test]
    fn preserves_span_styles_across_a_break() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let spans = vec![
            Span::styled("bold text here".to_owned(), bold),
            Span::styled(" and plain".to_owned(), Style::new()),
        ];

        let rows = wrap_spans(&spans, 12);
        let texts: Vec<String> = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert_eq!(vec!["bold text", "here and", "plain"], texts);
        // "here" was bold in the source and must stay bold on the row it wrapped onto.
        assert_eq!(bold, rows[1].spans[0].style);
    }

    #[test]
    fn merges_adjacent_spans_with_the_same_style() {
        let spans = vec![Span::raw("one ".to_owned()), Span::raw("two".to_owned())];

        let rows = wrap_spans(&spans, 40);
        assert_eq!(1, rows.len());
        assert_eq!(1, rows[0].spans.len());
        assert_eq!("one two", rows[0].spans[0].content.as_ref());
    }

    #[test]
    fn coloured_spans_are_not_merged() {
        let spans = vec![
            Span::styled("red".to_owned(), Style::new().fg(Color::Red)),
            Span::styled("blue".to_owned(), Style::new().fg(Color::Blue)),
        ];

        let rows = wrap_spans(&spans, 40);
        assert_eq!(2, rows[0].spans.len());
    }
}
