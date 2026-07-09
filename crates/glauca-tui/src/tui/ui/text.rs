//! Styled-text helpers: filter-match highlighting and width-aware span wrapping.

use super::*;

/// Build styled spans for `text`, highlighting every filter-token match
/// (ranges computed by `FilterQuery::highlight_ranges`). Fuzzy matches are
/// non-contiguous, so there may be several highlighted runs.
pub(super) fn highlight_spans<'a>(
    query: &FilterQuery,
    text: &'a str,
    normal: Style,
    highlight: Style,
) -> Vec<Span<'a>> {
    let ranges = query.highlight_ranges(text);
    if ranges.is_empty() {
        return vec![Span::styled(text, normal)];
    }
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (start, end) in ranges {
        if start > cursor {
            spans.push(Span::styled(&text[cursor..start], normal));
        }
        spans.push(Span::styled(&text[start..end], highlight));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(&text[cursor..], normal));
    }
    spans
}

/// Wrap styled `spans` (e.g. a title's highlight fragments) to `max_cols`
/// display columns, returning one span vector per visual line. Breaks on the
/// last whitespace that fits (word wrap); a single word wider than `max_cols`
/// is hard-broken at the column limit. Display width is measured with
/// unicode-width so CJK (full-width) characters count as two columns. Each
/// character keeps its original span style.
pub(super) fn wrap_spans(spans: &[Span], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    let max_cols = max_cols.max(1);

    // Flatten into (char, style) so we can re-break independently of the
    // original fragment boundaries while preserving per-character styling.
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_cols = 0usize;
    // Column index (into `cur`) just after the last whitespace, i.e. where the
    // next line would resume if we break on a word boundary.
    let mut last_break: Option<usize> = None;

    for (c, style) in chars {
        let char_cols = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_cols + char_cols > max_cols && !cur.is_empty() {
            // A space that overflows is the word boundary itself: end the line
            // here and consume the space (no leading space on the next line).
            if c == ' ' {
                lines.push(std::mem::take(&mut cur));
                cur_cols = 0;
                last_break = None;
                continue;
            }
            match last_break {
                // Break at the last whitespace: carry the trailing word to the
                // next line, dropping the breaking space.
                Some(brk) if brk > 0 && brk < cur.len() => {
                    let carry: Vec<(char, Style)> = cur.split_off(brk);
                    if cur.last().map(|(c, _)| *c) == Some(' ') {
                        cur.pop();
                    }
                    lines.push(std::mem::take(&mut cur));
                    cur_cols = carry
                        .iter()
                        .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
                        .sum();
                    cur = carry;
                }
                // No usable break point: hard-break before this char.
                _ => {
                    lines.push(std::mem::take(&mut cur));
                    cur_cols = 0;
                }
            }
            last_break = None;
        }
        if c == ' ' {
            last_break = Some(cur.len() + 1);
        }
        cur.push((c, style));
        cur_cols += char_cols;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }

    // Coalesce consecutive same-style chars back into owned spans.
    lines
        .into_iter()
        .map(|line| {
            let mut out: Vec<Span<'static>> = Vec::new();
            for (c, style) in line {
                match out.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => out.push(Span::styled(c.to_string(), style)),
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate a wrapped line's spans back into plain text.
    fn line_text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_width(spans: &[Span]) -> usize {
        line_text(spans)
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let spans = vec![Span::raw("hello world foo bar")];
        let lines = wrap_spans(&spans, 11);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(texts, vec!["hello world", "foo bar"]);
        // No line exceeds the limit.
        assert!(lines.iter().all(|l| line_width(l) <= 11));
    }

    #[test]
    fn hard_breaks_a_word_longer_than_width() {
        let spans = vec![Span::raw("supercalifragilistic")];
        let lines = wrap_spans(&spans, 5);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| line_width(l) <= 5));
        // Nothing is dropped.
        let joined: String = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(joined, "supercalifragilistic");
    }

    #[test]
    fn counts_full_width_chars_as_two_columns() {
        // Each CJK char is 2 columns wide; width 4 fits exactly two per line.
        let spans = vec![Span::raw("あいうえお")];
        let lines = wrap_spans(&spans, 4);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(texts, vec!["あい", "うえ", "お"]);
        assert!(lines.iter().all(|l| line_width(l) <= 4));
    }

    #[test]
    fn preserves_per_fragment_styles() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let hl = Style::default().fg(Color::Black).bg(Color::Yellow);
        let spans = vec![Span::styled("foo ", bold), Span::styled("bar", hl)];
        let lines = wrap_spans(&spans, 100);
        assert_eq!(lines.len(), 1);
        // The highlighted "bar" keeps its own style as a distinct span.
        assert!(lines[0].iter().any(|s| s.content == "bar" && s.style == hl));
    }
}
