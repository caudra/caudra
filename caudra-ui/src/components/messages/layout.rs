#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum SegmentKind {
    User,
    #[default]
    Assistant,
    Thinking,
    ToolInline,
    ToolBlock,
    Instruction,
    Error,
    Done,
}

impl SegmentKind {
    /// Names the segment in a grab's component stack, so a transcript row
    /// reports what kind of thing was painted there rather than only that the
    /// transcript painted it.
    #[cfg(debug_assertions)]
    pub fn grab_name(self) -> &'static str {
        match self {
            Self::User => "transcript_user",
            Self::Assistant => "transcript_assistant",
            Self::Thinking => "transcript_thinking",
            Self::ToolInline | Self::ToolBlock => "transcript_tool",
            Self::Instruction => "transcript_instruction",
            Self::Error => "transcript_error",
            Self::Done => "transcript_done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SegmentChrome {
    pub margin_top: u16,
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
    pub rail: bool,
}

impl SegmentChrome {
    pub fn for_kind(kind: SegmentKind, width: u16, margin_top: u16) -> Self {
        let inset = if width >= 32 {
            3
        } else if width >= 16 {
            2
        } else {
            1
        };
        let card = matches!(
            kind,
            SegmentKind::User
                | SegmentKind::ToolBlock
                | SegmentKind::Instruction
                | SegmentKind::Error
        );
        let vertical_padding = u16::from(card && width >= 24);

        Self {
            margin_top,
            left: inset.min(width),
            right: u16::from(card && width >= 32),
            top: vertical_padding,
            bottom: vertical_padding,
            rail: card,
        }
    }

    pub fn content_width(self, width: u16) -> u16 {
        width.saturating_sub(self.left.saturating_add(self.right))
    }

    pub fn content_start(self) -> u16 {
        self.margin_top.saturating_add(self.top)
    }

    pub fn action_offset(self) -> Option<u16> {
        (self.left > 0).then_some(self.left / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_user_and_assistant_content_align() {
        let user = SegmentChrome::for_kind(SegmentKind::User, 80, 0);
        let assistant = SegmentChrome::for_kind(SegmentKind::Assistant, 80, 0);

        assert_eq!(user.left, 3);
        assert_eq!(assistant.left, user.left);
        assert_eq!((user.top, user.bottom, user.rail), (1, 1, true));
        assert_eq!(
            (assistant.top, assistant.bottom, assistant.rail),
            (0, 0, false)
        );
    }

    #[test]
    fn narrow_layout_drops_optional_padding() {
        let user = SegmentChrome::for_kind(SegmentKind::User, 20, 0);
        let assistant = SegmentChrome::for_kind(SegmentKind::Assistant, 20, 0);

        assert_eq!(user.left, 2);
        assert_eq!(assistant.left, 2);
        assert_eq!((user.right, user.top, user.bottom), (0, 0, 0));
    }

    #[test]
    fn action_handle_stays_inside_the_existing_gutter() {
        assert_eq!(
            SegmentChrome::for_kind(SegmentKind::Assistant, 80, 0).action_offset(),
            Some(1)
        );
        assert_eq!(
            SegmentChrome::for_kind(SegmentKind::Assistant, 20, 0).action_offset(),
            Some(1)
        );
        assert_eq!(
            SegmentChrome::for_kind(SegmentKind::Assistant, 10, 0).action_offset(),
            Some(0)
        );
    }
}
