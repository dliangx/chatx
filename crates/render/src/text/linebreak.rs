//! Line break opportunities (UAX #14) and bidi reordering (UAX #9) helpers.

use unicode_bidi::{BidiInfo, Level};
use unicode_linebreak::{linebreaks, BreakOpportunity};

/// Break opportunities for a paragraph, as `(byte_index, mandatory)`.
///
/// `byte_index` is the byte offset of the character *after* the break; a line
/// may end immediately before that character.
pub fn break_opportunities(text: &str) -> Vec<(usize, bool)> {
    linebreaks(text)
        .map(|(i, opp)| (i, opp == BreakOpportunity::Mandatory))
        .collect()
}

/// Per-character bidi levels (logical order) for a paragraph with no newlines.
pub fn char_levels(text: &str) -> Vec<Level> {
    let info = BidiInfo::new(text, None);
    if info.paragraphs.is_empty() {
        return Vec::new();
    }
    let para = &info.paragraphs[0];
    info.reordered_levels_per_char(para, para.range.clone())
}

/// Returns the visual order permutation for a line given its per-char levels
/// (in logical order). `result[i]` is the logical char index to draw at visual
/// position `i`.
pub fn visual_order(levels: &[Level]) -> Vec<usize> {
    BidiInfo::reorder_visual(levels)
}
