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
        // A tool row's kind crosses between `ToolInline` and `ToolBlock` every
        // time its logical line count crosses one, which streaming output does
        // repeatedly within a single call. Both therefore spell the same
        // horizontal box, so the wrap width never moves under text already on
        // screen; only the rail and the vertical padding still tell them apart.
        let right_inset = card || kind == SegmentKind::ToolInline;
        let vertical_padding = u16::from(card && width >= 24);

        Self {
            margin_top,
            left: inset.min(width),
            right: u16::from(right_inset && width >= 32),
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
    use test_case::test_case;

    const EXPECT_TOOL_BOX_AGREES: &str =
        "a tool row's horizontal box must not depend on how many lines it holds";
    const EXPECT_INLINE_STAYS_FLAT: &str = "an inline row keeps no rail and no blank separator";
    const EXPECT_BLOCK_STAYS_A_CARD: &str = "a block row keeps its rail and its padding";
    const EXPECT_FIXED_ROLE_CHROME: &str =
        "a kind that cannot flip mid-stream keeps the chrome it always painted";

    /// Every step of the inset ladder and both sides of each threshold the
    /// chrome branches on, so a width-dependent regression cannot hide between
    /// the cases.
    const CHROME_WIDTHS: [u16; 9] = [80, 40, 32, 31, 24, 23, 20, 16, 10];

    #[test_case(CHROME_WIDTHS[0] ; "width_80")]
    #[test_case(CHROME_WIDTHS[1] ; "width_40")]
    #[test_case(CHROME_WIDTHS[2] ; "width_32")]
    #[test_case(CHROME_WIDTHS[3] ; "width_31")]
    #[test_case(CHROME_WIDTHS[4] ; "width_24")]
    #[test_case(CHROME_WIDTHS[5] ; "width_23")]
    #[test_case(CHROME_WIDTHS[6] ; "width_20")]
    #[test_case(CHROME_WIDTHS[7] ; "width_16")]
    #[test_case(CHROME_WIDTHS[8] ; "width_10")]
    fn tool_kinds_wrap_to_one_content_width(width: u16) {
        let inline = SegmentChrome::for_kind(SegmentKind::ToolInline, width, 0);
        let block = SegmentChrome::for_kind(SegmentKind::ToolBlock, width, 0);

        assert_eq!(
            (inline.left, inline.right),
            (block.left, block.right),
            "{EXPECT_TOOL_BOX_AGREES} at width {width}"
        );
        assert_eq!(
            inline.content_width(width),
            block.content_width(width),
            "{EXPECT_TOOL_BOX_AGREES} at width {width}"
        );
    }

    /// `block_padding` is spelled out per row rather than recomputed from the
    /// width, so moving the threshold or the padding breaks a case here instead
    /// of being copied into the expectation.
    #[test_case(CHROME_WIDTHS[0], 1 ; "width_80")]
    #[test_case(CHROME_WIDTHS[1], 1 ; "width_40")]
    #[test_case(CHROME_WIDTHS[4], 1 ; "width_24")]
    #[test_case(CHROME_WIDTHS[5], 0 ; "width_23")]
    #[test_case(CHROME_WIDTHS[6], 0 ; "width_20")]
    fn sharing_the_box_leaves_the_vertical_shapes_apart(width: u16, block_padding: u16) {
        let inline = SegmentChrome::for_kind(SegmentKind::ToolInline, width, 0);
        let block = SegmentChrome::for_kind(SegmentKind::ToolBlock, width, 0);

        assert_eq!(
            (inline.top, inline.bottom, inline.rail),
            (0, 0, false),
            "{EXPECT_INLINE_STAYS_FLAT} at width {width}"
        );
        assert!(block.rail, "{EXPECT_BLOCK_STAYS_A_CARD} at width {width}");
        assert_eq!(
            (block.top, block.bottom),
            (block_padding, block_padding),
            "{EXPECT_BLOCK_STAYS_A_CARD} at width {width}"
        );
    }

    /// Only the tool pair flips kind mid-stream, so only the tool pair had to
    /// change. A message's role is fixed once it exists, which keeps every
    /// other kind on the chrome it already painted.
    #[test_case(SegmentKind::User, 1, true ; "user_is_still_a_card")]
    #[test_case(SegmentKind::Instruction, 1, true ; "instruction_is_still_a_card")]
    #[test_case(SegmentKind::Error, 1, true ; "error_is_still_a_card")]
    #[test_case(SegmentKind::Assistant, 0, false ; "assistant_is_still_flat")]
    #[test_case(SegmentKind::Thinking, 0, false ; "thinking_is_still_flat")]
    #[test_case(SegmentKind::Done, 0, false ; "done_is_still_flat")]
    fn kinds_that_cannot_flip_keep_their_chrome(kind: SegmentKind, right: u16, rail: bool) {
        let chrome = SegmentChrome::for_kind(kind, CHROME_WIDTHS[0], 0);

        assert_eq!(
            (chrome.right, chrome.rail),
            (right, rail),
            "{EXPECT_FIXED_ROLE_CHROME}"
        );
    }

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
