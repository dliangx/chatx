//! Bubble model, message parsing, layout and drawing.

pub mod draw;
pub mod layout;
pub mod parse;

use crate::canvas::Image;

/// A parsed piece of message content.
pub enum Segment {
    Text(String),
    Emoji(char),
    /// A block image (attachment).
    Image(Image),
    Mention(String),
    Link(String),
}

/// Which side of the conversation a bubble is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    SelfSide,
    Other,
}

/// Position of a message within a group of consecutive messages from the same
/// sender. Controls avatar/name display and corner radius.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupPos {
    First,
    Middle,
    Last,
    Single,
}

/// A message to render as a bubble.
pub struct Bubble<'a> {
    pub segments: &'a [Segment],
    pub sender: &'a str,
    pub time: &'a str,
    pub side: Side,
    pub group: GroupPos,
    /// Optional avatar image. `None` renders a solid placeholder circle.
    pub avatar: Option<&'a Image>,
}

/// The final RGBA texture for a bubble.
#[derive(Clone)]
pub struct RenderedBubble {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl RenderedBubble {
    /// Returns the texture as straight (non-premultiplied) RGBA, suitable for
    /// display frameworks that expect unpremultiplied pixels.
    pub fn to_straight_rgba(&self) -> Vec<u8> {
        crate::canvas::unpremultiply(&self.rgba)
    }
}
