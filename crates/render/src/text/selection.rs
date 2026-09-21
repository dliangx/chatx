
use crate::text::TextLine;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct HitResult {
    pub line: usize,
    pub char_index: u32,
    pub after: bool,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    start: f32,
    end: f32,
    char_index: u32,
}

fn segments(line: &TextLine) -> Vec<Segment> {
    let mut segs = Vec::with_capacity(line.glyphs.len() + line.images.len());
    for g in &line.glyphs {
        segs.push(Segment { start: g.pen_x, end: g.pen_x + g.advance, char_index: g.char_index });
    }
    for im in &line.images {
        segs.push(Segment { start: im.x, end: im.x + im.w, char_index: im.char_index });
    }
    segs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    segs
}

pub fn hit_test(lines: &[TextLine], x: f32, y: f32) -> Option<HitResult> {
    let mut top = 0.0f32;
    let mut line_idx = lines.len().checked_sub(1)?;
    let mut found = false;
    for (i, line) in lines.iter().enumerate() {
        if y < top + line.height {
            line_idx = i;
            found = true;
            break;
        }
        top += line.height;
    }
    if !found {
        line_idx = lines.len() - 1;
    }

    let line = &lines[line_idx];
    let segs = segments(line);
    let first = *segs.first()?;
    let last = *segs.last()?;

    if x < first.start {
        return Some(HitResult { line: line_idx, char_index: first.char_index, after: false });
    }
    if x >= last.end {
        return Some(HitResult { line: line_idx, char_index: last.char_index, after: true });
    }
    for s in &segs {
        if x >= s.start && x < s.end {
            let after = x >= (s.start + s.end) / 2.0;
            return Some(HitResult { line: line_idx, char_index: s.char_index, after });
        }
    }
    let mut prev = first;
    for s in &segs {
        if s.start > x {
            break;
        }
        prev = *s;
    }
    Some(HitResult { line: line_idx, char_index: prev.char_index, after: true })
}

pub fn selection_rects(lines: &[TextLine], start: u32, end: u32) -> Vec<Rect> {
    let mut rects = Vec::new();
    let mut top = 0.0f32;
    for line in lines {
        let mut spans: Vec<(f32, f32)> = Vec::new();
        for s in segments(line) {
            if s.char_index >= start && s.char_index < end {
                spans.push((s.start, s.end));
            }
        }
        if !spans.is_empty() {
            spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut cur = spans[0];
            for &(s, e) in &spans[1..] {
                if s <= cur.1 {
                    cur.1 = cur.1.max(e);
                } else {
                    rects.push(Rect { x: cur.0, y: top, w: cur.1 - cur.0, h: line.height });
                    cur = (s, e);
                }
            }
            rects.push(Rect { x: cur.0, y: top, w: cur.1 - cur.0, h: line.height });
        }
        top += line.height;
    }
    rects
}
