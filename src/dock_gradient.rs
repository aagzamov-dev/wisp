//! Dock-only gradient colors shared by the renderer and Settings.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    TopToBottom,
    Diagonal,
}

impl Direction {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "left-to-right" => Some(Self::LeftToRight),
            "top-to-bottom" => Some(Self::TopToBottom),
            "diagonal" => Some(Self::Diagonal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gradient {
    pub start: [u8; 3],
    pub end: [u8; 3],
    pub direction: Direction,
    pub transparency: u32,
}

impl Gradient {
    pub const fn preset(start: [u8; 3], end: [u8; 3]) -> Self {
        Self {
            start,
            end,
            direction: Direction::LeftToRight,
            transparency: 15,
        }
    }

    pub fn parse(start: &str, end: &str, direction: &str, transparency: i64) -> Result<Self, &'static str> {
        Ok(Self {
            start: parse_color(start).ok_or("Enter the gradient start color as #RRGGBB.")?,
            end: parse_color(end).ok_or("Enter the gradient end color as #RRGGBB.")?,
            direction: Direction::parse(direction).ok_or("Choose a valid gradient direction.")?,
            transparency: u32::try_from(transparency)
                .ok()
                .filter(|value| *value <= 100)
                .ok_or("Choose a gradient transparency between 0 and 100%.")?,
        })
    }
}

pub fn parse_color(value: &str) -> Option<[u8; 3]> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let [_, red, green, blue] = u32::from_str_radix(hex, 16).ok()?.to_be_bytes();
    Some([red, green, blue])
}

impl Default for Gradient {
    fn default() -> Self {
        Self::preset([0x20, 0x29, 0x43], [0x38, 0x2B, 0x45])
    }
}
