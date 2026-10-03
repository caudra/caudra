use caudra_agent::{PeerOutput, peers::PeerSummary};
use caudra_config::InboundPolicy;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::components::code_view::{
    UNCONSTRAINED_WIDTH, WrappedRows, body_window, truncation_line,
};
use crate::components::tool_display::clamp_to_row;
use crate::theme;

const EMPTY: &str = "No reachable peers. Both sessions must enable cross-session messaging.";
const TARGET_LABEL: &str = "Target: ";
const WORKSPACE_LABEL: &str = "Workspace: ";
const MESSAGE_LABEL: &str = "Message: ";
const REASON_LABEL: &str = "Reason: ";
const TO_LABEL: &str = "To: ";
const SEPARATOR: &str = " · ";

pub(crate) fn render(output: &PeerOutput, budget: usize, width: u16) -> (Vec<Line<'static>>, bool) {
    let theme = theme::current();
    let mut lines = Vec::new();
    match output {
        PeerOutput::Sessions { sessions } => {
            if sessions.is_empty() {
                lines.push(Line::from(Span::styled(EMPTY, theme.tool_dim)));
            }
            for peer in sessions {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(
                    peer.name.escape_debug().to_string(),
                    theme.tool_prefix,
                )));
                lines.push(labelled(TARGET_LABEL, &peer.target, theme.tool_path));
                lines.push(Line::from(vec![
                    Span::styled(session_state(peer), theme.tool),
                    Span::styled(
                        format!("{SEPARATOR}inbound {}", inbound_label(&peer.inbound)),
                        theme.tool_dim,
                    ),
                ]));
                lines.push(labelled(
                    WORKSPACE_LABEL,
                    &peer.cwd.to_string_lossy(),
                    theme.tool_path,
                ));
            }
        }
        PeerOutput::Sent { target, receipt } => {
            let (state, meaning) = match receipt.status.as_str() {
                "queued" => ("Queued", "accepted, not proof of delivery or completion"),
                "held" => ("Held", "awaiting review, not delivered to the model"),
                "refused" => ("Refused", "not accepted"),
                "rate_limited" => ("Rate-limited", "not accepted"),
                "unavailable" => ("Unavailable", "peer could not be reached"),
                _ => ("Unknown", "acceptance unconfirmed; do not assume delivery"),
            };
            lines.push(Line::from(vec![
                Span::styled(state, receipt_style(&receipt.status)),
                Span::styled(format!("{SEPARATOR}{meaning}"), theme.tool_dim),
            ]));
            lines.push(labelled(TO_LABEL, target, theme.tool_path));
            if !receipt.message_id.is_empty() {
                lines.push(labelled(MESSAGE_LABEL, &receipt.message_id, theme.tool));
            }
            if let Some(reason) = receipt
                .reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                lines.push(labelled(REASON_LABEL, reason, theme.tool));
            }
        }
    }
    let mut lines = WrappedRows::new(lines, 0, width).lines();
    if budget == 0 {
        return (Vec::new(), !lines.is_empty());
    }
    let (shown, hidden) = body_window(lines.len(), budget);
    lines.truncate(shown);
    if hidden > 0 {
        let mut notice = truncation_line(hidden);
        if width != UNCONSTRAINED_WIDTH {
            notice.spans = clamp_to_row(notice.spans, width);
        }
        lines.push(notice);
    }
    (lines, hidden > 0)
}

fn receipt_style(status: &str) -> Style {
    let theme = theme::current();
    match status {
        "queued" => theme.tool_success,
        "refused" | "unavailable" => theme.tool_error,
        _ => theme.tool_warning,
    }
}

fn labelled(label: &'static str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, theme::current().tool_dim),
        Span::styled(value.escape_debug().to_string(), style),
    ])
}

fn session_state(peer: &PeerSummary) -> &'static str {
    if peer.blocked {
        "blocked"
    } else if peer.busy {
        "busy"
    } else {
        "idle"
    }
}

fn inbound_label(policy: &InboundPolicy) -> &'static str {
    match policy {
        InboundPolicy::Auto => "auto",
        InboundPolicy::Accept => "accept",
        InboundPolicy::Hold => "hold",
        InboundPolicy::Refuse => "refuse",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use caudra_agent::{
        BatchToolEntry, BatchToolStatus, PeerOutput, ToolOutput,
        peers::{PeerSummary, SendReceipt},
        tools::{ToolEffect, native::peers::SEND_NAME},
    };
    use caudra_config::{InboundPolicy, ToolOutputLines};
    use ratatui::text::Line;
    use test_case::test_case;

    use super::{EMPTY, render};
    use crate::components::code_view::{BatchViews, RenderLimits, RowTarget, render_tool_content};
    use crate::theme;

    const WIDTH: u16 = 120;
    const TARGET: &str = "calm-blue-wren";
    const MESSAGE: &str = "kind-amber-fox";
    const NAME: &str = "Review session";
    const WORKSPACE: &str = "/workspace/review";
    const REASON: &str = "The destination is waiting for the reader to review this message.";
    const HOSTILE: &str = "**literal**\x1b]8;;evil\x07\r\n\t\u{85}\u{202e}";
    const STALE_ANNOTATION: &str = "1 lines";
    const RAW_JSON: &str = "{\"status\":\"unknown\"}";

    fn session() -> PeerSummary {
        PeerSummary {
            target: TARGET.into(),
            name: NAME.into(),
            cwd: PathBuf::from(WORKSPACE),
            busy: false,
            blocked: false,
            inbound: InboundPolicy::Auto,
        }
    }

    fn receipt(status: &str) -> PeerOutput {
        PeerOutput::Sent {
            target: TARGET.into(),
            receipt: SendReceipt {
                status: status.into(),
                message_id: MESSAGE.into(),
                reason: Some(REASON.into()),
            },
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test_case("queued", "Queued", "not proof of delivery or completion"; "queued_is_acceptance_only")]
    #[test_case("held", "Held", "not delivered to the model"; "held_is_not_delivery")]
    #[test_case("refused", "Refused", "not accepted"; "refused")]
    #[test_case("rate_limited", "Rate-limited", "not accepted"; "rate_limited")]
    #[test_case("unknown", "Unknown", "acceptance unconfirmed"; "unknown")]
    #[test_case("unavailable", "Unavailable", "could not be reached"; "unavailable")]
    #[test_case("future_status", "Unknown", "acceptance unconfirmed"; "unrecognized_is_not_success")]
    fn receipt_states_explain_acceptance_without_claiming_completion(
        status: &str,
        label: &str,
        meaning: &str,
    ) {
        let (lines, truncated) = render(&receipt(status), usize::MAX, WIDTH);
        let drawn = text(&lines);
        assert!(!truncated);
        for expected in [label, meaning, TARGET, MESSAGE, REASON] {
            assert!(drawn.contains(expected), "{drawn}");
        }
        let theme = theme::current();
        let expected_style = match status {
            "queued" => theme.tool_success,
            "refused" | "unavailable" => theme.tool_error,
            _ => theme.tool_warning,
        };
        assert_eq!(lines[0].spans[0].style, expected_style);
        assert!(!drawn.contains("message_id"));
    }

    #[test_case(false, false, InboundPolicy::Auto, "idle", "auto"; "idle_auto")]
    #[test_case(true, false, InboundPolicy::Accept, "busy", "accept"; "busy_accept")]
    #[test_case(false, true, InboundPolicy::Hold, "blocked", "hold"; "blocked_hold")]
    #[test_case(true, true, InboundPolicy::Refuse, "blocked", "refuse"; "blocked_overrides_busy")]
    fn peer_rows_show_a_title_address_workspace_and_policy(
        busy: bool,
        blocked: bool,
        inbound: InboundPolicy,
        state: &str,
        policy: &str,
    ) {
        let output = PeerOutput::Sessions {
            sessions: vec![PeerSummary {
                busy,
                blocked,
                inbound,
                ..session()
            }],
        };
        let drawn = text(&render(&output, usize::MAX, WIDTH).0);
        for expected in [
            NAME,
            TARGET,
            WORKSPACE,
            &format!("{state} · inbound {policy}"),
        ] {
            assert!(drawn.contains(expected), "{drawn}");
        }
        assert!(!drawn.contains("session_id"));
    }

    #[test]
    fn empty_discovery_explains_how_to_make_a_peer_reachable() {
        let (lines, truncated) = render(
            &PeerOutput::Sessions {
                sessions: Vec::new(),
            },
            usize::MAX,
            WIDTH,
        );
        assert_eq!(text(&lines), EMPTY);
        assert!(!truncated);
    }

    #[test]
    fn multiple_peers_can_be_expanded_beyond_the_collapsed_budget() {
        const OTHER: &str = "brave-gold-owl";
        const BUDGET: usize = 4;
        let output = PeerOutput::Sessions {
            sessions: vec![
                session(),
                PeerSummary {
                    target: OTHER.into(),
                    ..session()
                },
            ],
        };
        let (collapsed, truncated) = render(&output, BUDGET, WIDTH);
        assert!(truncated);
        assert!(collapsed.len() <= BUDGET);
        assert!(text(&collapsed).contains(TARGET));
        let (expanded, truncated) = render(&output, usize::MAX, WIDTH);
        assert!(!truncated);
        assert!(expanded.len() > collapsed.len());
        assert!(text(&expanded).contains(OTHER));
        assert_eq!(text(&expanded).matches(WORKSPACE).count(), 2);
    }

    #[test_case(0; "unconstrained")]
    #[test_case(1; "one_column")]
    #[test_case(2; "two_columns")]
    #[test_case(8; "narrow")]
    #[test_case(24; "wrapped")]
    fn receipt_widths_wrap_or_clip_without_terminal_overflow(width: u16) {
        const BUDGET: usize = 4;
        let output = receipt("held");
        let (expanded, truncated) = render(&output, usize::MAX, width);
        assert!(!truncated);
        if width == 0 {
            assert!(text(&expanded).contains(REASON));
            assert!(
                expanded
                    .iter()
                    .any(|line| line.width() > usize::from(WIDTH / 2))
            );
        } else {
            assert!(
                expanded
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            let (collapsed, _) = render(&output, BUDGET, width);
            assert!(collapsed.len() <= BUDGET);
            assert!(
                collapsed
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            if width >= 24 {
                for word in REASON.split_whitespace() {
                    assert!(text(&expanded).contains(word));
                }
            }
        }
        let (hidden, truncated) = render(&output, 0, width);
        assert!(hidden.is_empty());
        assert!(truncated);
    }

    #[test_case(false; "session_labels")]
    #[test_case(true; "receipt_labels")]
    fn hostile_values_remain_literal_without_terminal_controls(sent: bool) {
        let output = if sent {
            PeerOutput::Sent {
                target: HOSTILE.into(),
                receipt: SendReceipt {
                    status: HOSTILE.into(),
                    message_id: HOSTILE.into(),
                    reason: Some(HOSTILE.into()),
                },
            }
        } else {
            PeerOutput::Sessions {
                sessions: vec![PeerSummary {
                    target: HOSTILE.into(),
                    name: HOSTILE.into(),
                    cwd: PathBuf::from(HOSTILE),
                    ..session()
                }],
            }
        };
        let (lines, _) = render(&output, usize::MAX, 0);
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| !span.content.chars().any(char::is_control))
        );
        let drawn = text(&lines);
        assert!(drawn.contains("**literal**"));
        assert!(!drawn.contains('\u{202e}'));
        assert!(drawn.contains("\\u{1b}]8;;evil\\u{7}\\r\\n\\t\\u{85}"));
    }

    #[test_case(BatchToolStatus::Success; "successful_tool_receipt")]
    #[test_case(BatchToolStatus::Error; "failed_tool_receipt")]
    fn batch_children_keep_structured_receipts_and_actionable_row_targets(status: BatchToolStatus) {
        let output = ToolOutput::Batch {
            entries: vec![BatchToolEntry {
                tool: SEND_NAME.into(),
                summary: "send".into(),
                effect: ToolEffect::Unknown,
                status,
                annotation: Some(STALE_ANNOTATION.into()),
                input: None,
                output: Some(ToolOutput::Peers(receipt("unknown"))),
                raw_input: None,
                model_suffix: None,
                refused: false,
            }],
            text: RAW_JSON.into(),
        };
        let limits = RenderLimits::new(
            true,
            usize::MAX,
            BatchViews::new([0]),
            ToolOutputLines::DEFAULT,
        )
        .with_width(WIDTH);
        let content = render_tool_content(None, Some(&output), false, limits);
        let drawn = text(&content.lines);
        for expected in [TARGET, MESSAGE, REASON, "acceptance unconfirmed"] {
            assert!(drawn.contains(expected), "{drawn}");
        }
        assert!(!drawn.contains(STALE_ANNOTATION));
        assert!(!drawn.contains(RAW_JSON));
        assert!(
            content
                .rows
                .iter()
                .all(|row| *row == Some(RowTarget::Item(0)))
        );
        assert_eq!(content.rows.len(), content.lines.len());
        assert!(content.highlights.is_empty());
    }
}
