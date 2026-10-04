use caudra_agent::{
    PeerOutput,
    peers::{PeerHistoryPage, PeerSummary, PublishReceipt, TopicActivity, handle_address, literal},
};
use caudra_config::InboundPolicy;
use caudra_providers::PEER_SCRIPT_SENDER;
use jiff::Timestamp;
use jiff::tz::TimeZone;
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
const NAME_LABEL: &str = "Name: ";
const WORKSPACE_LABEL: &str = "Workspace: ";
const MESSAGE_LABEL: &str = "Message: ";
const REASON_LABEL: &str = "Reason: ";
const TO_LABEL: &str = "To: ";
const TOPICS_LABEL: &str = "Topics: ";
const AUDIENCE_LABEL: &str = "Audience: ";
const SKIPPED_LABEL: &str = "Not sent, over the fan-out limit: ";
const BROADCASTS: &str = "Receives broadcasts";
const NO_RECIPIENTS: &str = "No live recipients";
const PUBLISHED_MEANING: &str = "acceptance is not proof of delivery or completion";
const ACCEPTED: &[&str] = &["queued", "held"];
const SEPARATOR: &str = " · ";
const NO_TOPICS: &str = "No stored topic messages";
const NO_MESSAGES: &str = "No stored messages";
const BROADCAST: &str = "broadcast";
const MESSAGE_NOUN: &str = "message";
const MESSAGES_NOUN: &str = "messages";
const LAST_LABEL: &str = "last ";
const WITHHELD_LABEL: &str = "Withheld: ";
const WITHHELD_MEANING: &str = "senders this session would hold for review";
const OLDER: &str = "Older messages remain";
const SENT_FORMAT: &str = "%Y-%m-%d %H:%M";

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
                    peer.title.escape_debug().to_string(),
                    theme.tool_prefix,
                )));
                lines.push(match peer.handle_address() {
                    Some(address) => labelled(NAME_LABEL, &address, theme.tool_path),
                    None => labelled(TARGET_LABEL, &peer.target, theme.tool_path),
                });
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
                if !peer.topics.is_empty() {
                    lines.push(labelled(TOPICS_LABEL, &peer.topics.join(", "), theme.tool));
                }
                if peer.broadcasts {
                    lines.push(Line::from(Span::styled(BROADCASTS, theme.tool_dim)));
                }
            }
        }
        PeerOutput::Sent { target, receipt } => {
            let (state, meaning) = receipt_state(&receipt.status);
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
        PeerOutput::Published { receipt } => {
            let summary = if receipt.recipients.is_empty() {
                NO_RECIPIENTS.to_owned()
            } else {
                let accepted = receipt
                    .recipients
                    .iter()
                    .filter(|recipient| ACCEPTED.contains(&recipient.status.as_str()))
                    .count();
                format!("{accepted} of {} accepted", receipt.recipients.len())
            };
            lines.push(Line::from(vec![
                Span::styled(summary, publication_style(receipt)),
                Span::styled(format!("{SEPARATOR}{PUBLISHED_MEANING}"), theme.tool_dim),
            ]));
            lines.push(labelled(
                AUDIENCE_LABEL,
                &receipt.audience.to_string(),
                theme.tool_path,
            ));
            if !receipt.message_id.is_empty() {
                lines.push(labelled(MESSAGE_LABEL, &receipt.message_id, theme.tool));
            }
            for recipient in &receipt.recipients {
                let mut spans = vec![
                    Span::styled(
                        receipt_state(&recipient.status).0,
                        receipt_style(&recipient.status),
                    ),
                    Span::styled(
                        format!("{SEPARATOR}{}", recipient.title.escape_debug()),
                        theme.tool_prefix,
                    ),
                ];
                if let Some(handle) = &recipient.handle {
                    spans.push(Span::styled(
                        format!(" {}", handle_address(handle).escape_debug()),
                        theme.tool_path,
                    ));
                }
                lines.push(Line::from(spans));
                if recipient.handle.is_none() {
                    lines.push(labelled(TO_LABEL, &recipient.target, theme.tool_path));
                }
                if let Some(reason) = recipient
                    .reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                {
                    lines.push(labelled(REASON_LABEL, reason, theme.tool));
                }
            }
            if receipt.skipped > 0 {
                lines.push(Line::from(Span::styled(
                    format!("{SKIPPED_LABEL}{}", receipt.skipped),
                    theme.tool_warning,
                )));
            }
        }
        PeerOutput::Topics { topics } => {
            if topics.is_empty() {
                lines.push(Line::from(Span::styled(NO_TOPICS, theme.tool_dim)));
            }
            lines.extend(topics.iter().map(topic_line));
        }
        PeerOutput::History { page } => history_lines(page, &mut lines),
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

fn topic_line(activity: &TopicActivity) -> Line<'static> {
    let noun = if activity.messages == 1 {
        MESSAGE_NOUN
    } else {
        MESSAGES_NOUN
    };
    let theme = theme::current();
    Line::from(vec![
        Span::styled(activity.topic.escape_debug().to_string(), theme.tool_path),
        Span::styled(
            format!(
                "{SEPARATOR}{} {noun}{SEPARATOR}{LAST_LABEL}{}",
                activity.messages,
                sent_at(activity.last_ms)
            ),
            theme.tool_dim,
        ),
    ])
}

fn history_lines(page: &PeerHistoryPage, lines: &mut Vec<Line<'static>>) {
    let theme = theme::current();
    if page.messages.is_empty() && page.withheld == 0 {
        lines.push(Line::from(Span::styled(NO_MESSAGES, theme.tool_dim)));
    }
    for message in &page.messages {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let mut heading = vec![
            Span::styled(
                message
                    .topic
                    .as_deref()
                    .unwrap_or(BROADCAST)
                    .escape_debug()
                    .to_string(),
                theme.tool_path,
            ),
            Span::styled(
                format!("{SEPARATOR}{}", message.sender_name.escape_debug()),
                theme.tool_prefix,
            ),
        ];
        if let Some(handle) = &message.sender_handle {
            heading.push(Span::styled(
                format!(" {}", handle_address(handle).escape_debug()),
                theme.tool_path,
            ));
        }
        if message.external {
            heading.push(Span::styled(
                format!(" ({PEER_SCRIPT_SENDER})"),
                theme.tool_dim,
            ));
        }
        heading.push(Span::styled(
            format!("{SEPARATOR}{}", sent_at(message.sent_ms)),
            theme.tool_dim,
        ));
        lines.push(Line::from(heading));
        lines.extend(
            message
                .text
                .lines()
                .map(|line| Line::from(Span::styled(literal(line, false), theme.tool))),
        );
    }
    if page.withheld > 0 {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{WITHHELD_LABEL}{}", page.withheld),
                theme.tool_warning,
            ),
            Span::styled(format!("{SEPARATOR}{WITHHELD_MEANING}"), theme.tool_dim),
        ]));
    }
    if page.before.is_some() {
        lines.push(Line::from(Span::styled(OLDER, theme.tool_dim)));
    }
}

fn sent_at(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map(|at| {
            at.to_zoned(TimeZone::system())
                .strftime(SENT_FORMAT)
                .to_string()
        })
        .unwrap_or_default()
}

fn receipt_state(status: &str) -> (&'static str, &'static str) {
    match status {
        "queued" => ("Queued", "accepted, not proof of delivery or completion"),
        "held" => ("Held", "awaiting review, not delivered to the model"),
        "refused" => ("Refused", "not accepted"),
        "rate_limited" => ("Rate-limited", "not accepted"),
        "unavailable" => ("Unavailable", "peer could not be reached"),
        _ => ("Unknown", "acceptance unconfirmed; do not assume delivery"),
    }
}

fn receipt_style(status: &str) -> Style {
    let theme = theme::current();
    match status {
        "queued" => theme.tool_success,
        "refused" | "unavailable" => theme.tool_error,
        _ => theme.tool_warning,
    }
}

/// One recipient's colour when they all agree, a warning when they differ or
/// nobody was reached.
fn publication_style(receipt: &PublishReceipt) -> Style {
    let mut styles = receipt
        .recipients
        .iter()
        .map(|recipient| receipt_style(&recipient.status));
    match styles.next() {
        Some(first) if styles.all(|style| style == first) => first,
        _ => theme::current().tool_warning,
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
        peers::{
            PeerHistoryPage, PeerSummary, PublishReceipt, RecipientReceipt, SendReceipt,
            StoredPeerMessage, TopicActivity,
        },
        tools::{ToolEffect, native::peers::SEND_NAME},
    };
    use caudra_config::{InboundPolicy, ToolOutputLines};
    use caudra_providers::{PEER_SCRIPT_SENDER, PeerAudience};
    use ratatui::text::Line;
    use test_case::test_case;

    use super::{
        AUDIENCE_LABEL, BROADCAST, BROADCASTS, EMPTY, NAME_LABEL, NO_MESSAGES, NO_RECIPIENTS,
        NO_TOPICS, OLDER, SKIPPED_LABEL, TARGET_LABEL, TO_LABEL, TOPICS_LABEL, WITHHELD_LABEL,
        render,
    };
    use crate::components::code_view::{BatchViews, RenderLimits, RowTarget, render_tool_content};
    use crate::theme;

    const WIDTH: u16 = 120;
    const TARGET: &str = "calm-blue-wren";
    const MESSAGE: &str = "kind-amber-fox";
    const NAME: &str = "Review session";
    const HANDLE: &str = "review-agent";
    const WORKSPACE: &str = "/workspace/review";
    const REASON: &str = "The destination is waiting for the reader to review this message.";
    const HOSTILE: &str = "**literal**\x1b]8;;evil\x07\r\n\t\u{85}\u{202e}";
    const STALE_ANNOTATION: &str = "1 lines";
    const RAW_JSON: &str = "{\"status\":\"unknown\"}";
    const TOPIC: &str = "ci.failures";
    const OTHER_TOPIC: &str = "deploy.prod";
    const PATTERN: &str = "ci.**";
    const OTHER_PATTERN: &str = "deploy.*";
    const BODY_LINES: [&str; 2] = ["The linux build failed.", "See job 42."];
    const SENT_MS: u64 = 1_790_000_000_000;
    const SENT_YEAR: &str = "2026-";
    const HISTORY_SEQ: i64 = 7;

    fn stored(topic: Option<&str>, text: &str) -> StoredPeerMessage {
        StoredPeerMessage {
            seq: HISTORY_SEQ,
            topic: topic.map(str::to_owned),
            sender_name: NAME.into(),
            sender_handle: Some(HANDLE.into()),
            external: false,
            sent_ms: SENT_MS,
            text: text.into(),
        }
    }

    fn history(
        messages: Vec<StoredPeerMessage>,
        withheld: usize,
        before: Option<i64>,
    ) -> PeerOutput {
        PeerOutput::History {
            page: PeerHistoryPage {
                topic: Some(PATTERN.into()),
                messages,
                withheld,
                before,
            },
        }
    }

    fn session() -> PeerSummary {
        PeerSummary {
            target: TARGET.into(),
            title: NAME.into(),
            handle: None,
            cwd: PathBuf::from(WORKSPACE),
            busy: false,
            blocked: false,
            inbound: InboundPolicy::Auto,
            topics: Vec::new(),
            broadcasts: false,
        }
    }

    fn recipient(status: &str) -> RecipientReceipt {
        RecipientReceipt {
            target: TARGET.into(),
            title: NAME.into(),
            handle: Some(HANDLE.into()),
            status: status.into(),
            reason: Some(REASON.into()),
        }
    }

    fn publication(statuses: &[&str], skipped: usize) -> PeerOutput {
        PeerOutput::Published {
            receipt: PublishReceipt {
                message_id: MESSAGE.into(),
                audience: PeerAudience::Topic {
                    topic: TOPIC.into(),
                },
                recipients: statuses.iter().map(|status| recipient(status)).collect(),
                skipped,
            },
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

    #[test_case(Some(HANDLE); "named")]
    #[test_case(None; "unnamed")]
    fn peers_show_their_name_in_place_of_their_target(handle: Option<&str>) {
        let output = PeerOutput::Sessions {
            sessions: vec![PeerSummary {
                handle: handle.map(str::to_owned),
                ..session()
            }],
        };
        let drawn = text(&render(&output, usize::MAX, WIDTH).0);
        let named = handle.is_some();
        assert_eq!(drawn.contains(&format!("{NAME_LABEL}@{HANDLE}")), named);
        assert_eq!(drawn.contains(NAME_LABEL), named);
        assert_eq!(drawn.contains(&format!("{TARGET_LABEL}{TARGET}")), !named);
        assert_eq!(drawn.contains(TARGET), !named);
    }

    #[test_case(Some(HANDLE); "named")]
    #[test_case(None; "unnamed")]
    fn recipients_show_their_name_in_place_of_their_target(handle: Option<&str>) {
        let output = PeerOutput::Published {
            receipt: PublishReceipt {
                message_id: MESSAGE.into(),
                audience: PeerAudience::Broadcast,
                recipients: vec![RecipientReceipt {
                    handle: handle.map(str::to_owned),
                    ..recipient("queued")
                }],
                skipped: 0,
            },
        };
        let drawn = text(&render(&output, usize::MAX, WIDTH).0);
        let named = handle.is_some();
        assert_eq!(drawn.contains(&format!("@{HANDLE}")), named);
        assert_eq!(drawn.contains(&format!("{TO_LABEL}{TARGET}")), !named);
        assert_eq!(drawn.contains(TARGET), !named);
    }

    #[test_case(&[], false; "unsubscribed")]
    #[test_case(&[PATTERN, OTHER_PATTERN], false; "topics")]
    #[test_case(&[], true; "broadcasts")]
    fn peer_rows_show_subscriptions_only_when_present(topics: &[&str], broadcasts: bool) {
        let output = PeerOutput::Sessions {
            sessions: vec![PeerSummary {
                topics: topics.iter().copied().map(str::to_owned).collect(),
                broadcasts,
                ..session()
            }],
        };
        let drawn = text(&render(&output, usize::MAX, WIDTH).0);
        assert_eq!(
            drawn.contains(&format!("{TOPICS_LABEL}{}", topics.join(", "))),
            !topics.is_empty()
        );
        assert_eq!(drawn.contains(BROADCASTS), broadcasts);
    }

    #[test_case(&["queued", "queued"], 0, "2 of 2 accepted"; "all_queued")]
    #[test_case(&["queued", "refused"], 0, "1 of 2 accepted"; "mixed")]
    #[test_case(&["refused", "unavailable"], 0, "0 of 2 accepted"; "none_accepted")]
    #[test_case(&["held", "unknown"], 4, "1 of 2 accepted"; "skipped_over_fanout")]
    #[test_case(&[], 0, NO_RECIPIENTS; "nobody_subscribed")]
    fn publication_receipts_list_every_recipient_outcome(
        statuses: &[&str],
        skipped: usize,
        summary: &str,
    ) {
        let (lines, truncated) = render(&publication(statuses, skipped), usize::MAX, WIDTH);
        let drawn = text(&lines);
        assert!(!truncated);
        for expected in [summary, &format!("{AUDIENCE_LABEL}topic {TOPIC}"), MESSAGE] {
            assert!(drawn.contains(expected), "{drawn}");
        }
        assert_eq!(drawn.matches(&format!("@{HANDLE}")).count(), statuses.len());
        assert_eq!(drawn.matches(REASON).count(), statuses.len());
        assert_eq!(
            drawn.contains(&format!("{SKIPPED_LABEL}{skipped}")),
            skipped > 0
        );
        let theme = theme::current();
        let expected_style = match statuses {
            ["queued", "queued"] => theme.tool_success,
            ["refused", "unavailable"] => theme.tool_error,
            _ => theme.tool_warning,
        };
        assert_eq!(lines[0].spans[0].style, expected_style);
    }

    #[test_case(1, "1 message"; "singular")]
    #[test_case(3, "3 messages"; "plural")]
    fn topic_directories_show_each_topic_with_its_count_and_latest_time(
        messages: usize,
        count: &str,
    ) {
        let output = PeerOutput::Topics {
            topics: [TOPIC, OTHER_TOPIC]
                .map(|topic| TopicActivity {
                    topic: topic.into(),
                    messages,
                    last_ms: SENT_MS,
                })
                .into(),
        };
        let (lines, truncated) = render(&output, usize::MAX, WIDTH);
        assert!(!truncated);
        assert_eq!(lines.len(), 2);
        for (line, topic) in lines.iter().zip([TOPIC, OTHER_TOPIC]) {
            let drawn = line.to_string();
            assert!(
                drawn.starts_with(&format!("{topic} · {count} · last {SENT_YEAR}")),
                "{drawn}"
            );
        }
    }

    #[test_case(PeerOutput::Topics { topics: Vec::new() }, NO_TOPICS; "topics")]
    #[test_case(history(Vec::new(), 0, None), NO_MESSAGES; "message_history")]
    fn empty_history_reads_say_nothing_is_stored(output: PeerOutput, notice: &str) {
        let (lines, truncated) = render(&output, usize::MAX, WIDTH);
        assert_eq!(text(&lines), notice);
        assert!(!truncated);
    }

    #[test_case(Some(TOPIC), TOPIC; "topic")]
    #[test_case(None, BROADCAST; "broadcast")]
    fn stored_messages_show_their_audience_sender_time_and_every_body_line(
        topic: Option<&str>,
        audience: &str,
    ) {
        let output = history(vec![stored(topic, &BODY_LINES.join("\n"))], 0, None);
        let (lines, truncated) = render(&output, usize::MAX, WIDTH);
        assert!(!truncated);
        let drawn = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(
            drawn[0].starts_with(&format!("{audience} · {NAME} @{HANDLE} · {SENT_YEAR}")),
            "{drawn:?}"
        );
        assert_eq!(drawn[1..], BODY_LINES);
    }

    #[test]
    fn stored_script_messages_are_marked() {
        let script = StoredPeerMessage {
            sender_handle: None,
            external: true,
            ..stored(Some(TOPIC), BODY_LINES[0])
        };
        let (lines, _) = render(&history(vec![script], 0, None), usize::MAX, WIDTH);
        let heading = lines[0].to_string();
        assert!(
            heading.starts_with(&format!(
                "{TOPIC} · {NAME} ({PEER_SCRIPT_SENDER}) · {SENT_YEAR}"
            )),
            "{heading}"
        );
    }

    #[test_case(2, 0, None; "complete")]
    #[test_case(2, 3, None; "withheld")]
    #[test_case(0, 3, None; "everything_withheld")]
    #[test_case(2, 0, Some(HISTORY_SEQ); "older")]
    fn history_pages_say_what_was_withheld_and_whether_older_messages_remain(
        shown: usize,
        withheld: usize,
        before: Option<i64>,
    ) {
        let messages = BODY_LINES
            .iter()
            .take(shown)
            .map(|body| stored(Some(TOPIC), body))
            .collect();
        let drawn = text(&render(&history(messages, withheld, before), usize::MAX, WIDTH).0);
        assert_eq!(drawn.matches(&format!("@{HANDLE}")).count(), shown);
        assert_eq!(
            drawn.contains(&format!("{WITHHELD_LABEL}{withheld}")),
            withheld > 0
        );
        assert_eq!(drawn.contains(OLDER), before.is_some());
        assert!(!drawn.contains(NO_MESSAGES));
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

    fn hostile_sessions() -> PeerOutput {
        let named = PeerSummary {
            target: HOSTILE.into(),
            title: HOSTILE.into(),
            handle: Some(HOSTILE.into()),
            cwd: PathBuf::from(HOSTILE),
            topics: vec![HOSTILE.into()],
            ..session()
        };
        let unnamed = PeerSummary {
            handle: None,
            ..named.clone()
        };
        PeerOutput::Sessions {
            sessions: vec![named, unnamed],
        }
    }

    fn hostile_receipt() -> PeerOutput {
        PeerOutput::Sent {
            target: HOSTILE.into(),
            receipt: SendReceipt {
                status: HOSTILE.into(),
                message_id: HOSTILE.into(),
                reason: Some(HOSTILE.into()),
            },
        }
    }

    fn hostile_publication() -> PeerOutput {
        PeerOutput::Published {
            receipt: PublishReceipt {
                message_id: HOSTILE.into(),
                audience: PeerAudience::Topic {
                    topic: HOSTILE.into(),
                },
                recipients: [Some(HOSTILE.into()), None]
                    .map(|handle| RecipientReceipt {
                        target: HOSTILE.into(),
                        title: HOSTILE.into(),
                        handle,
                        status: HOSTILE.into(),
                        reason: Some(HOSTILE.into()),
                    })
                    .into(),
                skipped: 0,
            },
        }
    }

    fn hostile_topics() -> PeerOutput {
        PeerOutput::Topics {
            topics: vec![TopicActivity {
                topic: HOSTILE.into(),
                messages: 1,
                last_ms: SENT_MS,
            }],
        }
    }

    fn hostile_history() -> PeerOutput {
        history(
            vec![StoredPeerMessage {
                topic: Some(HOSTILE.into()),
                sender_name: HOSTILE.into(),
                sender_handle: Some(HOSTILE.into()),
                ..stored(None, HOSTILE)
            }],
            0,
            None,
        )
    }

    #[test_case(hostile_sessions(); "session_labels")]
    #[test_case(hostile_receipt(); "receipt_labels")]
    #[test_case(hostile_publication(); "publication_labels")]
    #[test_case(hostile_topics(); "topic_labels")]
    #[test_case(hostile_history(); "stored_messages")]
    fn hostile_values_remain_literal_without_terminal_controls(output: PeerOutput) {
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
