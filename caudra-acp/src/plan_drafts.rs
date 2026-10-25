//! The plan a client is shown while `todo_write` is still being written.
//!
//! ACP's plan update replaces the whole plan, so whatever is sent reads as the
//! agent's plan. Only items that have closed are sent: an item still being
//! written would be sent again on every token. A draft the call never stores
//! is withdrawn by sending the last plan a call did store, or an empty plan
//! when none has in this process.

use agent_client_protocol_schema::v1::SessionUpdate;
use caudra_agent::types::{TodoItem, ToolOutput};

use crate::translate;

/// The list one streaming call has written, and how much of it was sent.
struct Draft {
    id: String,
    items: Vec<TodoItem>,
    sent: usize,
}

#[derive(Default)]
pub(crate) struct PlanDrafts {
    stored: Vec<TodoItem>,
    draft: Option<Draft>,
}

impl PlanDrafts {
    /// The plan one argument fragment calls for, `None` unless an item closed
    /// since the last one sent. Every item but the last has closed, and the
    /// last closes with the arguments.
    pub(crate) fn advance(
        &mut self,
        id: &str,
        todos: Option<Vec<TodoItem>>,
        complete: bool,
    ) -> Option<SessionUpdate> {
        let draft = match (self.draft.as_mut(), todos) {
            (Some(draft), todos) if draft.id == id => {
                if let Some(items) = todos {
                    draft.items = items;
                }
                draft
            }
            (_, Some(items)) => self.draft.insert(Draft {
                id: id.to_owned(),
                items,
                sent: 0,
            }),
            (_, None) => return None,
        };
        let closed = match complete {
            true => draft.items.len(),
            false => draft.items.len().saturating_sub(1),
        };
        (closed > draft.sent).then(|| {
            draft.sent = closed;
            translate::plan(&draft.items[..closed])
        })
    }

    /// The plan a finished call leaves: the list it stored, or the last one
    /// stored when it was the draft and failed.
    pub(crate) fn finished(&mut self, id: &str, output: &ToolOutput) -> Option<SessionUpdate> {
        let drafted = self.draft.as_ref().is_some_and(|draft| draft.id == id);
        if let ToolOutput::TodoList(items) = output {
            if drafted {
                self.draft = None;
            }
            self.stored.clone_from(items);
            return Some(translate::plan(items));
        }
        drafted.then(|| self.abandon()).flatten()
    }

    /// Withdraws a draft its run will never store, `None` when the client was
    /// never shown any of it.
    pub(crate) fn abandon(&mut self) -> Option<SessionUpdate> {
        let draft = self.draft.take()?;
        (draft.sent > 0).then(|| translate::plan(&self.stored))
    }
}

#[cfg(test)]
mod tests {
    use super::PlanDrafts;
    use agent_client_protocol_schema::v1::SessionUpdate;
    use caudra_agent::types::{TodoItem, TodoPriority, TodoStatus, ToolOutput};
    use test_case::test_case;

    const CALL: &str = "toolu_todos";
    const OTHER_CALL: &str = "toolu_other";
    const STORED: &str = "the plan a call stored";
    const FIRST: &str = "first";
    const SECOND: &str = "second";

    fn todo(content: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: TodoStatus::Pending,
            priority: TodoPriority::default(),
        }
    }

    fn todos(contents: &[&str]) -> Option<Vec<TodoItem>> {
        Some(contents.iter().copied().map(todo).collect())
    }

    /// What a plan update names, in order.
    fn entries(update: Option<SessionUpdate>) -> Option<Vec<String>> {
        match update? {
            SessionUpdate::Plan(plan) => Some(
                plan.entries
                    .into_iter()
                    .map(|entry| entry.content)
                    .collect(),
            ),
            other => panic!("not a plan: {other:?}"),
        }
    }

    fn sent(contents: &[&str]) -> Option<Vec<String>> {
        Some(
            contents
                .iter()
                .map(|content| (*content).to_owned())
                .collect(),
        )
    }

    #[test]
    fn an_item_is_sent_once_the_next_one_begins() {
        let mut drafts = PlanDrafts::default();
        assert_eq!(entries(drafts.advance(CALL, todos(&[FIRST]), false)), None);
        assert_eq!(
            entries(drafts.advance(CALL, todos(&[FIRST, SECOND]), false)),
            sent(&[FIRST])
        );
        assert_eq!(
            entries(drafts.advance(CALL, todos(&[FIRST, "second, longer"]), false)),
            None,
            "the open item is not resent per token"
        );
    }

    #[test]
    fn the_last_item_is_sent_when_the_arguments_close() {
        let mut drafts = PlanDrafts::default();
        drafts.advance(CALL, todos(&[FIRST, SECOND]), false);
        assert_eq!(
            entries(drafts.advance(CALL, None, true)),
            sent(&[FIRST, SECOND])
        );
    }

    #[test]
    fn another_calls_fragments_leave_the_draft_alone() {
        let mut drafts = PlanDrafts::default();
        drafts.advance(CALL, todos(&[FIRST]), false);
        assert_eq!(entries(drafts.advance(OTHER_CALL, None, true)), None);
        assert_eq!(entries(drafts.advance(CALL, None, true)), sent(&[FIRST]));
    }

    #[test]
    fn a_stored_list_replaces_the_draft() {
        let mut drafts = PlanDrafts::default();
        drafts.advance(CALL, todos(&[FIRST, SECOND]), false);
        let stored = ToolOutput::TodoList(vec![todo(STORED)]);
        assert_eq!(entries(drafts.finished(CALL, &stored)), sent(&[STORED]));
        assert_eq!(
            entries(drafts.abandon()),
            None,
            "nothing is left to withdraw"
        );
    }

    #[test_case(true ; "after_a_stored_plan")]
    #[test_case(false ; "with_no_plan_stored")]
    fn a_failed_draft_falls_back_to_the_last_stored_plan(stored_first: bool) {
        let mut drafts = PlanDrafts::default();
        if stored_first {
            drafts.finished(OTHER_CALL, &ToolOutput::TodoList(vec![todo(STORED)]));
        }
        drafts.advance(CALL, todos(&[FIRST, SECOND]), false);
        let fallback = if stored_first {
            sent(&[STORED])
        } else {
            sent(&[])
        };
        assert_eq!(
            entries(drafts.finished(CALL, &ToolOutput::Plain("invalid".into()))),
            fallback
        );
    }

    #[test]
    fn an_abandoned_draft_is_withdrawn_only_when_some_of_it_was_sent() {
        let mut drafts = PlanDrafts::default();
        drafts.advance(CALL, todos(&[FIRST]), false);
        assert_eq!(entries(drafts.abandon()), None);
        drafts.advance(CALL, todos(&[FIRST, SECOND]), false);
        assert_eq!(entries(drafts.abandon()), sent(&[]));
    }

    #[test]
    fn another_calls_failure_keeps_the_draft() {
        let mut drafts = PlanDrafts::default();
        drafts.advance(CALL, todos(&[FIRST, SECOND]), false);
        assert_eq!(
            entries(drafts.finished(OTHER_CALL, &ToolOutput::Plain("x".into()))),
            None
        );
    }
}
