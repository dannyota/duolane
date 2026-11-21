use ratatui::{layout::Alignment, layout::Constraint};

pub const ID_WIDTH: u16 = 8;
pub const TIME_WIDTH: u16 = 8;
pub const CODE_WIDTH: u16 = 4;
pub const COUNT_WIDTH: u16 = 3;
pub const PROVIDER_WIDTH: u16 = 8;
pub const MODEL_NARROW_WIDTH: u16 = 18;
pub const MODEL_MEDIUM_WIDTH: u16 = 28;
pub const MODEL_WIDE_WIDTH: u16 = 36;
pub const PROJECT_MEDIUM_WIDTH: u16 = 12;
pub const PROJECT_WIDE_WIDTH: u16 = 16;
pub const EFFORT_WIDTH: u16 = 6;
pub const ENDPOINT_WIDTH: u16 = 12;
pub const STATUS_WIDTH: u16 = 11;
pub const RATE_WIDTH: u16 = 12;
pub const DURATION_WIDTH: u16 = 8;
pub const TOKEN_WIDTH: u16 = 7;
pub const ERROR_WIDTH: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutTier {
    Emergency,
    Narrow,
    Medium,
    Expanded,
    Wide,
}

impl LayoutTier {
    pub fn minimum_outer_width(self) -> u16 {
        match self {
            Self::Emergency => 0,
            Self::Narrow => 76 + 2,
            Self::Medium => 88 + 2,
            Self::Expanded => 118 + 2,
            Self::Wide => 152 + 2,
        }
    }

    pub fn for_outer_width(width: u16) -> Self {
        [Self::Wide, Self::Expanded, Self::Medium, Self::Narrow]
            .into_iter()
            .find(|tier| width >= tier.minimum_outer_width())
            .unwrap_or(Self::Emergency)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnWidth {
    Fixed(u16),
    Flex(u16),
}

#[derive(Clone, Copy, Debug)]
pub struct ColumnSpec<K> {
    pub key: K,
    pub header: &'static str,
    pub alignment: Alignment,
    pub width: ColumnWidth,
}

impl<K> ColumnSpec<K> {
    pub const fn fixed(key: K, header: &'static str, alignment: Alignment, width: u16) -> Self {
        Self {
            key,
            header,
            alignment,
            width: ColumnWidth::Fixed(width),
        }
    }

    pub const fn flex(key: K, header: &'static str, alignment: Alignment, weight: u16) -> Self {
        Self {
            key,
            header,
            alignment,
            width: ColumnWidth::Flex(weight),
        }
    }

    pub fn constraint(&self) -> Constraint {
        match self.width {
            ColumnWidth::Fixed(width) => Constraint::Length(width),
            ColumnWidth::Flex(weight) => Constraint::Fill(weight),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_tiers_cover_conventional_terminal_widths() {
        for tier in [
            LayoutTier::Narrow,
            LayoutTier::Medium,
            LayoutTier::Expanded,
            LayoutTier::Wide,
        ] {
            assert_eq!(
                LayoutTier::for_outer_width(tier.minimum_outer_width()),
                tier
            );
            assert_ne!(
                LayoutTier::for_outer_width(tier.minimum_outer_width() - 1),
                tier
            );
        }
        assert_eq!(LayoutTier::for_outer_width(77), LayoutTier::Emergency);
        assert_eq!(LayoutTier::for_outer_width(78), LayoutTier::Narrow);
        assert_eq!(LayoutTier::for_outer_width(90), LayoutTier::Medium);
        assert_eq!(LayoutTier::for_outer_width(120), LayoutTier::Expanded);
        assert_eq!(LayoutTier::for_outer_width(153), LayoutTier::Expanded);
        assert_eq!(LayoutTier::for_outer_width(154), LayoutTier::Wide);
    }
}
