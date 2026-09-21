
pub mod draw;
pub mod layout;
pub mod parse;

use crate::canvas::Image;

pub enum Segment {
    Text(String),
    Emoji(char),
    Image(Image),
    Mention(String),
    Link(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    SelfSide,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupPos {
    First,
    Middle,
    Last,
    Single,
}

pub struct Bubble<'a> {
    pub segments: &'a [Segment],
    pub sender: &'a str,
    pub time: &'a str,
    pub side: Side,
    pub group: GroupPos,
    pub avatar: Option<&'a Image>,
}

#[derive(Clone)]
pub struct RenderedBubble {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl RenderedBubble {
    pub fn to_straight_rgba(&self) -> Vec<u8> {
        crate::canvas::unpremultiply(&self.rgba)
    }
}
