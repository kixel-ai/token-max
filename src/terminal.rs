//! Terminal layout by display cells.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// How to wrap: without a width, lines are only indented.
#[derive(Clone, Copy, Debug, Default)]
pub struct WrapOptions {
    pub width: Option<usize>,
    /// Extra indent added to each line's own leading spaces.
    pub indent: usize,
    /// Indent for continuation rows, when narrower than the width; otherwise rows keep the first row's indent.
    pub continuation: Option<usize>,
}

/// The cell width of a non-emoji character: wide and fullwidth take two, ambiguous ones take one.
fn char_cells(c: char) -> usize {
    if c.width() == Some(2) { 2 } else { 1 }
}

/// Format characters that are not default-ignorable, such as Arabic number signs; they print nothing either.
fn visible_format(c: char) -> bool {
    matches!(c,
        '\u{600}'..='\u{605}'
        | '\u{6dd}'
        | '\u{70f}'
        | '\u{890}'..='\u{891}'
        | '\u{8e2}'
        | '\u{fff9}'..='\u{fffb}'
        | '\u{110bd}'
        | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}')
}

/// Hangul medial vowels and final consonants, which have no width of their own once joined.
fn jamo(c: char) -> bool {
    matches!(c, '\u{1160}'..='\u{11ff}' | '\u{d7b0}'..='\u{d7ff}')
}

/// Controls, format characters, non-spacing marks and default-ignorable characters print nothing.
fn non_printing(c: char) -> bool {
    c.is_control() || (c.width() == Some(0) && !jamo(c)) || visible_format(c)
}

/// Indic virama linkers, after which a consonant joins the same cluster as a conjunct.
fn linker(c: char) -> bool {
    matches!(
        c,
        '\u{94d}' | '\u{9cd}' | '\u{acd}' | '\u{b4d}' | '\u{c4d}' | '\u{d4d}'
    )
}

/// An approximation of `Extended_Pictographic`: the symbol and emoji blocks that join with ZWJ.
fn pictographic(c: char) -> bool {
    matches!(c,
        '\u{a9}' | '\u{ae}' | '\u{203c}' | '\u{2049}' | '\u{2122}' | '\u{2139}'
        | '\u{2190}'..='\u{2bff}'
        | '\u{3030}' | '\u{303d}' | '\u{3297}' | '\u{3299}'
        | '\u{1f000}'..='\u{1faff}')
}

/// Terminal cells for one grapheme cluster, following `string-width`.
fn cluster_cells(cluster: &str) -> usize {
    let chars: Vec<char> = cluster.chars().collect();
    let Some(start) = chars.iter().position(|c| !non_printing(*c)) else {
        return 0;
    };
    let base = chars[start];
    let regional = |c: &char| ('\u{1f1e6}'..='\u{1f1ff}').contains(c);
    // A cluster that is exactly an emoji renders as one double-width glyph:
    // a flag, a keycap, or a presentation sequence.
    let emoji = match chars.as_slice() {
        [a, b] if regional(a) && regional(b) => true,
        [key, '\u{20e3}'] | [key, '\u{fe0f}', '\u{20e3}'] => {
            key.is_ascii_digit() || *key == '#' || *key == '*'
        }
        [first, rest @ ..] => {
            *first == base
                && rest.contains(&'\u{fe0f}')
                && unicode_width::UnicodeWidthStr::width(cluster) >= 2
        }
        [] => false,
    };
    // ZWJ joins two pictographs into one glyph wherever they sit in the cluster.
    let joined =
        chars.contains(&'\u{200d}') && chars.iter().filter(|c| pictographic(**c)).count() >= 2;
    if emoji || joined {
        return 2;
    }
    // Spacing marks and halfwidth/fullwidth forms keep their own cells after
    // the base. Other printing characters in a cluster are conjunct consonants
    // after a virama, or Thai and Lao SARA AM, which share the base's cells.
    let mut cells = char_cells(base);
    let mut linked = false;
    for &c in &chars[start + 1..] {
        if ('\u{ff00}'..='\u{ffef}').contains(&c) {
            cells += char_cells(c);
        } else if linker(c) {
            linked = true;
        } else if c.width() == Some(1) {
            if !linked && c != '\u{e33}' && c != '\u{eb3}' {
                cells += 1;
            }
            linked = false;
        }
    }
    cells
}

/// Wrap plain text by terminal cells without normalizing Unicode or shortening words.
///
/// Input must already be free of terminal escapes, such as text passed through `safe::terminal_text`.
pub fn wrap_text(text: &str, options: WrapOptions) -> String {
    text.split('\n')
        .map(|input| wrap_line(input, options))
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_line(input: &str, options: WrapOptions) -> String {
    let leading = input.len() - input.trim_start_matches(' ').len();
    let Some(width) = options.width else {
        return format!("{}{input}", " ".repeat(options.indent));
    };
    let width = width.max(1);
    let indent = leading + options.indent;
    let first = " ".repeat(indent.min(width.saturating_sub(2)));
    let continuation = match options.continuation {
        Some(n) if n < width => " ".repeat(n),
        _ => first.clone(),
    };
    let mut padding = first;
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    let mut spaces = 0;
    let mut flush = |padding: &mut String, row: &mut String, cells: &mut usize| {
        rows.push(format!("{padding}{row}"));
        padding.clone_from(&continuation);
        row.clear();
        *cells = 0;
    };
    for word in input[leading..].split(' ') {
        // Each separator contributes one space; runs of spaces collapse into the pending gap.
        if word.is_empty() {
            spaces += 1;
            continue;
        }
        let clusters: Vec<(&str, usize)> = word
            .graphemes(true)
            .map(|g| (g, cluster_cells(g)))
            .collect();
        let size: usize = clusters.iter().map(|(_, n)| n).sum();
        let columns = width - padding.len();
        if !row.is_empty() && cells + spaces + size > columns {
            flush(&mut padding, &mut row, &mut cells);
            spaces = 0;
        }
        row.push_str(&" ".repeat(spaces));
        cells += spaces;
        spaces = 1;
        for (cluster, size) in clusters {
            if cells > 0 && cells + size > width - padding.len() {
                flush(&mut padding, &mut row, &mut cells);
            }
            row.push_str(cluster);
            cells += size;
        }
    }
    flush(&mut padding, &mut row, &mut cells);
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, width: usize) -> String {
        wrap_text(
            text,
            WrapOptions {
                width: Some(width),
                ..Default::default()
            },
        )
    }

    #[test]
    fn widths_follow_terminal_cells() {
        assert_eq!(cluster_cells("a"), 1);
        assert_eq!(cluster_cells("e\u{301}"), 1);
        assert_eq!(cluster_cells("漢"), 2);
        assert_eq!(cluster_cells("😀"), 2);
        assert_eq!(cluster_cells("👨\u{200d}👩\u{200d}👧"), 2);
        assert_eq!(cluster_cells("🇯🇵"), 2);
        assert_eq!(cluster_cells("❤\u{fe0f}"), 2);
        assert_eq!(cluster_cells("1\u{20e3}"), 2);
        assert_eq!(cluster_cells("ｶﾞ"), 2);
        assert_eq!(cluster_cells("\u{200b}"), 0);
        assert_eq!(cluster_cells("é"), 1);
    }

    #[test]
    fn words_wrap_at_cells_and_long_words_hard_wrap() {
        assert_eq!(wrap("aaa bbb ccc", 7), "aaa bbb\nccc");
        assert_eq!(wrap("abcdefghij", 4), "abcd\nefgh\nij");
        assert_eq!(wrap("漢字漢字", 5), "漢字\n漢字");
        assert_eq!(wrap("a  b", 10), "a  b");
        assert_eq!(wrap("a b   ", 10), "a b");
        assert_eq!(wrap("", 10), "");
        assert_eq!(wrap("one\ntwo", 10), "one\ntwo");
    }

    #[test]
    fn indents_keep_leading_spaces_and_continuations() {
        assert_eq!(wrap("  aaa bbb", 7), "  aaa\n  bbb");
        assert_eq!(
            wrap_text(
                "x aaa bbb",
                WrapOptions {
                    width: Some(6),
                    indent: 1,
                    continuation: Some(3),
                }
            ),
            " x aaa\n   bbb"
        );
        // Indents never leave fewer than two columns.
        assert_eq!(
            wrap_text(
                "abc",
                WrapOptions {
                    width: Some(4),
                    indent: 10,
                    continuation: None,
                }
            ),
            "  ab\n  c"
        );
        // A continuation at or beyond the width keeps the first row's indent.
        assert_eq!(
            wrap_text(
                "aa bb",
                WrapOptions {
                    width: Some(3),
                    indent: 1,
                    continuation: Some(3),
                }
            ),
            " aa\n bb"
        );
        assert_eq!(wrap("ab", 0), "a\nb");
        assert_eq!(
            wrap_text(
                "  a",
                WrapOptions {
                    indent: 2,
                    ..Default::default()
                }
            ),
            "    a"
        );
    }

    // Ported from report.test.ts: narrow output never exceeds its width.
    #[test]
    fn every_row_fits_its_width() {
        let text = "Change claude-cache-ttl for /home/user/.claude/projects/very-long-directory-name 漢字😀 to 1h; saves $12.34 per month.";
        for width in [1, 2, 3, 8, 28, 40, 80] {
            for indent in [0, 2, 6] {
                let out = wrap_text(
                    text,
                    WrapOptions {
                        width: Some(width),
                        indent,
                        continuation: None,
                    },
                );
                for row in out.split('\n') {
                    let cells: usize = row.graphemes(true).map(cluster_cells).sum();
                    assert!(cells <= width.max(2), "{width} {indent} {row:?}");
                }
            }
        }
    }
}
