use super::{
    LocalOutcome, Parser, RawInput, RepairError, RepairState, RepairedInput, Token, TokenKind,
    ValueSpan, local_repair, lock, tokenize, unwrap_document,
};
use crate::tools::{
    ToolContext,
    native::batch::{MAX_BATCH_SIZE, child_tool_use_id, dispatchable},
};
use caudra_providers::InvalidToolInput;
use serde_json::Value;

const CALLS: &str = "tool_calls";
const TOOL: &str = "tool";
const PARAMETERS: &str = "parameters";
pub(super) const ISOLATED: &str = "isolated batch children";

impl RepairState {
    pub(super) fn prepare_batch(
        &self,
        slot: &str,
        raw: &str,
        ctx: &ToolContext,
    ) -> Result<RepairedInput, RepairError> {
        let text = unwrap_document(raw)?;
        let tokens = tokenize(text)?;
        let mut parser = Parser {
            tokens: &tokens,
            position: 0,
            needs_model: false,
            spans: Vec::new(),
        };
        let effective = parser.value(0)?;
        if parser.position != tokens.len() {
            return Err(RepairError::Ambiguous);
        }
        if let Ok(strict) = serde_json::from_str::<Value>(raw) {
            return Ok(RepairedInput {
                effective: strict,
                method: "unchanged",
            });
        }
        let calls_span = parser.spans.iter().find(|span| {
            span.depth == 1 && span.member.as_ref().is_some_and(|(key, _)| key == CALLS)
        });
        let Some((calls, calls_span)) = effective
            .get(CALLS)
            .and_then(Value::as_array)
            .zip(calls_span)
        else {
            return Ok(RepairedInput {
                effective,
                method: "local",
            });
        };
        let children = parser.spans.iter().filter(|span| {
            span.depth == 2 && span.start > calls_span.start && span.end <= calls_span.end
        });
        let mut repairs = Vec::new();
        for (index, (child, span)) in calls.iter().zip(children).enumerate() {
            if index >= MAX_BATCH_SIZE {
                break;
            }
            let child_raw = source(text, &tokens, span.start, span.end);
            if serde_json::from_str::<Value>(child_raw).is_ok() {
                continue;
            }
            if !matches!(tokens[span.end - 1].kind, TokenKind::Punctuation(b'}'))
                || parser.spans.iter().any(|nested| {
                    nested.depth > span.depth && nested.start > span.start && nested.end == span.end
                })
            {
                return Err(RepairError::Ambiguous);
            }
            let Some((_, params)) = dispatchable(child, ctx) else {
                continue;
            };
            let members: Vec<_> = parser
                .spans
                .iter()
                .filter(|member| {
                    member.depth == span.depth + 1
                        && member.start > span.start
                        && member.end < span.end
                })
                .collect();
            let raw_params = parameter_source(text, &tokens, &members)?;
            let candidate = match local_repair(RawInput::Complete(&raw_params))? {
                LocalOutcome::Unchanged(value)
                | LocalOutcome::Repaired(value)
                | LocalOutcome::NeedsModel(value) => value,
            };
            if candidate != params {
                return Err(RepairError::Preservation);
            }
            repairs.push((child_tool_use_id(Some(slot), index), raw_params));
        }
        for (child_slot, raw) in repairs {
            lock(&self.wrapper_repairs).insert(child_slot.clone());
            self.register_invalid(
                &child_slot,
                InvalidToolInput {
                    raw,
                    complete: true,
                    clipped: false,
                },
            );
        }
        Ok(RepairedInput {
            effective,
            method: ISOLATED,
        })
    }
}

fn source<'a>(text: &'a str, tokens: &[Token], start: usize, end: usize) -> &'a str {
    &text[tokens[start].start..tokens[end - 1].end]
}

fn parameter_source(
    text: &str,
    tokens: &[Token],
    members: &[&ValueSpan],
) -> Result<String, RepairError> {
    let parameters = members.iter().find(|member| {
        member
            .member
            .as_ref()
            .is_some_and(|(key, _)| key == PARAMETERS)
    });
    let flat: Vec<_> = members
        .iter()
        .filter(|member| {
            member
                .member
                .as_ref()
                .is_some_and(|(key, _)| key != TOOL && key != PARAMETERS)
        })
        .collect();
    if let Some(parameters) = parameters {
        let nested = source(text, tokens, parameters.start, parameters.end);
        if flat.is_empty() {
            return Ok(nested.to_owned());
        }
        let nested = nested
            .strip_prefix('{')
            .and_then(|nested| nested.strip_suffix('}'))
            .ok_or(RepairError::Ambiguous)?;
        let nested = nested.trim().trim_end_matches(',');
        let mut fields = Vec::new();
        if !nested.is_empty() {
            fields.push(nested.to_owned());
        }
        fields.extend(flat.iter().filter_map(|member| {
            let (_, start) = member.member.as_ref()?;
            Some(source(text, tokens, *start, member.end).to_owned())
        }));
        return Ok(format!("{{{}}}", fields.join(",")));
    }
    let fields: Vec<_> = flat
        .iter()
        .filter_map(|member| {
            let (_, start) = member.member.as_ref()?;
            Some(source(text, tokens, *start, member.end))
        })
        .collect();
    Ok(format!("{{{}}}", fields.join(",")))
}
