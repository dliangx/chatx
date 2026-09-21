
use unicode_bidi::{BidiInfo, Level};
use unicode_linebreak::{linebreaks, BreakOpportunity};

pub fn break_opportunities(text: &str) -> Vec<(usize, bool)> {
    linebreaks(text)
        .map(|(i, opp)| (i, opp == BreakOpportunity::Mandatory))
        .collect()
}

pub fn char_levels(text: &str) -> Vec<Level> {
    let info = BidiInfo::new(text, None);
    if info.paragraphs.is_empty() {
        return Vec::new();
    }
    let para = &info.paragraphs[0];
    info.reordered_levels_per_char(para, para.range.clone())
}

pub fn visual_order(levels: &[Level]) -> Vec<usize> {
    BidiInfo::reorder_visual(levels)
}
