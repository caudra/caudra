//! The rows of a plain tab or of a filter across every plain tab: the keys of each table in
//! order, with a heading before every table but a tab's first.

use caudra_config::ConfigField;

use super::fields::{TABLE_SEPARATOR, editable_fields, key_path};
use crate::components::docs_link::DocsTopic;
use crate::event_loop::config_reload::{ApplyTiming, apply_timing};

/// One `caudra.toml` table a tab edits, and the section of the reference that documents it.
pub(super) struct Group {
    pub(super) table: &'static [&'static str],
    pub(super) fields: &'static [ConfigField],
    pub(super) docs: DocsTopic,
}

#[derive(Clone, Copy)]
pub(super) enum PlainRow {
    Heading(&'static [&'static str]),
    Key {
        table: &'static [&'static str],
        field: &'static ConfigField,
        docs: DocsTopic,
    },
}

impl PlainRow {
    pub(super) fn is_key(&self) -> bool {
        matches!(self, Self::Key { .. })
    }
}

fn keys(group: &Group) -> impl Iterator<Item = PlainRow> {
    editable_fields(group.fields).map(|field| PlainRow::Key {
        table: group.table,
        field,
        docs: group.docs,
    })
}

/// A tab's rows: its first table's keys, then each later table under its heading.
pub(super) fn tab_rows(groups: &[Group]) -> Vec<PlainRow> {
    groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| {
            let heading = (index > 0).then_some(PlainRow::Heading(group.table));
            heading.into_iter().chain(keys(group))
        })
        .collect()
}

/// The keys whose `table.key` path holds `query`, ignoring case, each table under its heading.
pub(super) fn filtered_rows<'a>(
    groups: impl IntoIterator<Item = &'a Group>,
    query: &str,
) -> Vec<PlainRow> {
    let query = query.to_lowercase();
    let mut rows = Vec::new();
    for group in groups {
        let matched: Vec<PlainRow> = keys(group)
            .filter(|row| match row {
                PlainRow::Key { table, field, .. } => {
                    key_path(table, field).to_lowercase().contains(&query)
                }
                PlainRow::Heading(_) => false,
            })
            .collect();
        if !matched.is_empty() {
            rows.push(PlainRow::Heading(group.table));
            rows.extend(matched);
        }
    }
    rows
}

/// A heading as a table header, `[agent.messaging]`, or nothing at the top level.
pub(super) fn heading(table: &[&str]) -> String {
    match table.is_empty() {
        true => String::new(),
        false => format!("[{}]", table.join(TABLE_SEPARATOR)),
    }
}

/// When a change to `key` of the table at `table` applies.
pub(super) fn timing_of(table: &[&str], key: &str) -> ApplyTiming {
    match table.split_first() {
        None => apply_timing(key, ""),
        Some((top, rest)) => {
            let path = rest
                .iter()
                .copied()
                .chain([key])
                .collect::<Vec<_>>()
                .join(TABLE_SEPARATOR);
            apply_timing(top, &path)
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::{AgentConfig, MessagingConfig};
    use test_case::test_case;

    use super::*;

    const AGENT: &[&str] = &["agent"];
    const MESSAGING: &[&str] = &["agent", "messaging"];
    const UI: &[&str] = &["ui"];
    const GROUPS: [Group; 2] = [
        Group {
            table: AGENT,
            fields: AgentConfig::FIELDS,
            docs: DocsTopic::ConfigAgent,
        },
        Group {
            table: MESSAGING,
            fields: MessagingConfig::FIELDS,
            docs: DocsTopic::ConfigMessaging,
        },
    ];

    #[test]
    fn only_a_later_table_gets_a_heading() {
        let rows = tab_rows(&GROUPS);
        assert!(rows[0].is_key());
        let headings: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                PlainRow::Heading(table) => Some(*table),
                PlainRow::Key { .. } => None,
            })
            .collect();
        assert_eq!(headings, [MESSAGING]);
    }

    #[test]
    fn a_filter_matches_the_table_path_and_heads_each_table() {
        let rows = filtered_rows(&GROUPS, "MESSAGING.IN");
        assert!(matches!(rows[0], PlainRow::Heading(MESSAGING)));
        assert!(rows.len() > 1);
        assert!(rows[1..].iter().all(|row| matches!(
            row,
            PlainRow::Key { table: MESSAGING, field, .. } if field.name.starts_with("in")
        )));
    }

    #[test_case(UI, "show_decisions", ApplyTiming::Now; "a_key_that_applies_at_once")]
    #[test_case(UI, "theme", ApplyTiming::Reload; "a_ui_key")]
    #[test_case(MESSAGING, "inbound", ApplyTiming::NextTurn; "a_nested_agent_table")]
    #[test_case(&[], "always_yolo", ApplyTiming::Reload; "a_top_level_key")]
    fn timing_follows_the_table(table: &[&str], key: &str, expected: ApplyTiming) {
        assert_eq!(timing_of(table, key), expected);
    }
}
