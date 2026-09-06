//! Message text parsing into segments.

use crate::bubble::Segment;

/// Splits a message string into text / emoji / mention / link segments.
pub fn parse_text(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '@' {
            // Mention: @ followed by word chars.
            let mut j = i + 1;
            while j < chars.len() && is_word_char(chars[j]) {
                j += 1;
            }
            if j > i + 1 {
                flush_text(&mut buf, &mut out);
                out.push(Segment::Mention(chars[i..j].iter().collect()));
                i = j;
                continue;
            }
        }

        if c == 'h' || c == 'w' {
            let rest: String = chars[i..].iter().collect();
            if rest.starts_with("http://")
                || rest.starts_with("https://")
                || rest.starts_with("www.")
            {
                let link: String = rest.chars().take_while(|&x| !x.is_whitespace()).collect();
                let len = link.chars().count();
                flush_text(&mut buf, &mut out);
                out.push(Segment::Link(link));
                i += len;
                continue;
            }
        }

        if is_emoji_char(c) {
            flush_text(&mut buf, &mut out);
            // Consume a variation selector if present.
            out.push(Segment::Emoji(c));
            i += 1;
            while i < chars.len() && chars[i] == '\u{FE0F}' {
                i += 1;
            }
            continue;
        }

        buf.push(c);
        i += 1;
    }
    flush_text(&mut buf, &mut out);
    out
}

fn flush_text(buf: &mut String, out: &mut Vec<Segment>) {
    if !buf.is_empty() {
        out.push(Segment::Text(std::mem::take(buf)));
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-' || c == '/'
}

fn is_emoji_char(c: char) -> bool {
    let cp = c as u32;
    (0x1F300..=0x1FAFF).contains(&cp)
        || (0x1F000..=0x1F0FF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x1F1E6..=0x1F1FF).contains(&cp) // regional indicators
        || matches!(cp, 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139)
}
