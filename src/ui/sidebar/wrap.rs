//! Wraps one resolved token row over a bounded number of sidebar lines.

use ratatui::{style::Style, text::Span};
use unicode_width::UnicodeWidthChar;

use super::tokens::{separator, ResolvedToken};

type Cell = (char, Style);

/// Continuation lines narrower than this drop the hanging indent instead.
const MIN_CONTINUATION_WIDTH: usize = 6;

/// Lays the untruncated `spans` of `resolved` over at most `max_lines` lines of `max_width`
/// columns. Continuation lines hang under the first text token and carry their indent as a
/// leading span; the last line ends in `…` when text is left over.
pub(crate) fn wrap_token_spans(
    resolved: &[ResolvedToken],
    spans: &[Span<'static>],
    state_icon: &str,
    max_width: usize,
    max_lines: usize,
) -> Vec<Vec<Span<'static>>> {
    let cells = spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect::<Vec<_>>();
    let lead = leading_width(resolved, state_icon);
    let prefix = fit_count(&cells, lead);
    let hang = if max_width.saturating_sub(lead) < MIN_CONTINUATION_WIDTH {
        0
    } else {
        lead
    };
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut rest = cells.as_slice();
    loop {
        let (width, floor) = if lines.is_empty() {
            (max_width, prefix)
        } else {
            (max_width - hang, 0)
        };
        if width_of(rest) <= width {
            lines.push(rest.to_vec());
            break;
        }
        if lines.len() + 1 >= max_lines.max(1) || width < 2 {
            lines.push(ellipsize(rest, width));
            break;
        }
        let split = break_point(rest, width, floor, max_width - hang);
        lines.push(trim_end(&rest[..split]).to_vec());
        rest = trim_start(&rest[split..]);
        if rest.is_empty() {
            break;
        }
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let indent = (index > 0 && hang > 0).then(|| Span::raw(" ".repeat(hang)));
            indent.into_iter().chain(merge(&line)).collect()
        })
        .collect()
}

/// Columns taken by the icon and status tokens before the first text token.
fn leading_width(resolved: &[ResolvedToken], state_icon: &str) -> usize {
    let Some(first_text) = resolved
        .iter()
        .position(|token| token.kind.text_value().is_some())
    else {
        return 0;
    };
    resolved[..first_text]
        .iter()
        .enumerate()
        .map(|(index, token)| {
            super::fixed_token_width(&token.kind, state_icon)
                + super::display_width(separator(token, &resolved[index + 1]))
        })
        .sum()
}

/// Picks where the first `width` columns of `rest` end: at a word boundary after `floor`
/// when the word pushed down fits the next line, otherwise at the last cell that fits.
fn break_point(rest: &[Cell], width: usize, floor: usize, next_width: usize) -> usize {
    let hard = fit_count(rest, width).clamp(1, rest.len() - 1);
    if rest[hard].0.is_whitespace() {
        return hard;
    }
    (floor + 1..=hard)
        .rev()
        .find(|&at| can_break(rest[at - 1].0, rest[at].0))
        .filter(|&at| width_of(&rest[at..at + word_len(&rest[at..])]) <= next_width)
        .unwrap_or(hard)
}

/// Breaks are allowed anywhere except inside a run of ASCII non-space characters,
/// where only a hyphen or slash ends a word.
fn can_break(before: char, after: char) -> bool {
    !(before.is_ascii_graphic() && after.is_ascii_graphic()) || matches!(before, '-' | '/')
}

fn word_len(cells: &[Cell]) -> usize {
    (1..cells.len())
        .find(|&at| can_break(cells[at - 1].0, cells[at].0))
        .unwrap_or(cells.len())
}

fn ellipsize(cells: &[Cell], width: usize) -> Vec<Cell> {
    if width == 0 {
        return Vec::new();
    }
    let keep = fit_count(cells, width - 1);
    let style = cells[keep.saturating_sub(1)].1;
    let mut line = trim_end(&cells[..keep]).to_vec();
    line.push(('…', style));
    line
}

fn fit_count(cells: &[Cell], width: usize) -> usize {
    let mut used = 0;
    cells
        .iter()
        .take_while(|(ch, _)| {
            used += char_width(*ch);
            used <= width
        })
        .count()
}

fn width_of(cells: &[Cell]) -> usize {
    cells.iter().map(|(ch, _)| char_width(*ch)).sum()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn trim_end(cells: &[Cell]) -> &[Cell] {
    let end = cells
        .iter()
        .rposition(|(ch, _)| !ch.is_whitespace())
        .map_or(0, |index| index + 1);
    &cells[..end]
}

fn trim_start(cells: &[Cell]) -> &[Cell] {
    let start = cells
        .iter()
        .position(|(ch, _)| !ch.is_whitespace())
        .unwrap_or(cells.len());
    &cells[start..]
}

fn merge(cells: &[Cell]) -> Vec<Span<'static>> {
    cells
        .chunk_by(|left, right| left.1 == right.1)
        .map(|run| Span::styled(run.iter().map(|(ch, _)| ch).collect::<String>(), run[0].1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::sidebar::tokens::ResolvedTokenKind;

    fn text(lines: &[Vec<Span<'static>>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    fn wrap(tokens: &[ResolvedTokenKind], width: usize) -> Vec<String> {
        let resolved = tokens
            .iter()
            .cloned()
            .map(ResolvedToken::unstyled)
            .collect::<Vec<_>>();
        let style = Style::default();
        let spans = super::super::resolved_token_spans(
            &resolved,
            ("●", style),
            style,
            style,
            style,
            style,
            &super::super::Palette::catppuccin(),
            usize::MAX,
        );
        let lines = wrap_token_spans(&resolved, &spans, "●", width, 2);
        for line in &lines {
            let width_used = line
                .iter()
                .map(|span| super::super::display_width(&span.content))
                .sum::<usize>();
            assert!(width_used <= width, "{line:?} exceeds {width}");
        }
        text(&lines)
    }

    #[test]
    fn short_rows_stay_on_one_line() {
        assert_eq!(
            wrap(
                &[
                    ResolvedTokenKind::StateIcon,
                    ResolvedTokenKind::Pane("修 sidebar".into())
                ],
                20
            ),
            ["● 修 sidebar"]
        );
    }

    #[test]
    fn chinese_names_wrap_under_the_first_text_token() {
        assert_eq!(
            wrap(
                &[
                    ResolvedTokenKind::StateIcon,
                    ResolvedTokenKind::Pane("侧边栏长名字折成两行显示".into())
                ],
                16
            ),
            ["● 侧边栏长名字折", "  成两行显示"]
        );
    }

    #[test]
    fn ascii_words_move_whole_to_the_next_line() {
        assert_eq!(
            wrap(
                &[ResolvedTokenKind::Pane("fix herdr sidebar wrap".into())],
                14
            ),
            ["fix herdr", "sidebar wrap"]
        );
        assert_eq!(
            wrap(&[ResolvedTokenKind::Pane("修复herdr-namer".into())], 10),
            ["修复herdr-", "namer"]
        );
    }

    #[test]
    fn words_longer_than_a_line_break_hard() {
        assert_eq!(
            wrap(&[ResolvedTokenKind::Pane("abcdefghijklmnop".into())], 10),
            ["abcdefghij", "klmnop"]
        );
    }

    #[test]
    fn overflow_ends_the_second_line_with_an_ellipsis() {
        assert_eq!(
            wrap(
                &[
                    ResolvedTokenKind::StateIcon,
                    ResolvedTokenKind::Pane("给侧边栏的长会话名做折行并且第二行也放不下".into())
                ],
                16
            ),
            ["● 给侧边栏的长会", "  话名做折行并…"]
        );
    }

    #[test]
    fn narrow_rows_drop_the_hanging_indent() {
        assert_eq!(
            wrap(
                &[
                    ResolvedTokenKind::StateIcon,
                    ResolvedTokenKind::Pane("abcdefgh".into())
                ],
                7
            ),
            ["● abcde", "fgh"]
        );
    }
}
