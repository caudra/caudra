//! Config files that failed to load while the session runs. The event loop
//! records them and every session's status bar reads them, so a file left
//! broken keeps saying so after the reload's flash has faded.

use std::sync::Arc;

use arc_swap::ArcSwap;

const DETAIL_SEPARATOR: &str = " \u{b7} ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigProblem {
    pub(crate) file: &'static str,
    pub(crate) error: String,
    pub(crate) consequence: &'static str,
}

/// At most one problem per file, in the order the files first failed.
#[derive(Default)]
pub(crate) struct ConfigProblems(ArcSwap<Vec<ConfigProblem>>);

impl ConfigProblems {
    /// Replaces the problem recorded for `file`.
    pub(crate) fn set(&self, file: &'static str, error: String, consequence: &'static str) {
        let problem = ConfigProblem {
            file,
            error,
            consequence,
        };
        self.0.rcu(|problems| {
            let mut next = Vec::clone(problems);
            match next.iter_mut().find(|known| known.file == file) {
                Some(known) => *known = problem.clone(),
                None => next.push(problem.clone()),
            }
            next
        });
    }

    /// Clears `file` once it loads again.
    pub(crate) fn clear(&self, file: &'static str) {
        self.0.rcu(|problems| {
            problems
                .iter()
                .filter(|problem| problem.file != file)
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    pub(crate) fn load(&self) -> Arc<Vec<ConfigProblem>> {
        self.0.load_full()
    }

    /// Every problem on one line, `{file}: {error}; {consequence}`, for a
    /// flash. `None` once nothing is broken.
    pub(crate) fn details(&self) -> Option<String> {
        let problems = self.0.load();
        (!problems.is_empty()).then(|| {
            problems
                .iter()
                .map(|problem| {
                    format!(
                        "{}: {}; {}",
                        problem.file,
                        one_line(&problem.error),
                        problem.consequence
                    )
                })
                .collect::<Vec<_>>()
                .join(DETAIL_SEPARATOR)
        })
    }
}

/// A parser's error spans lines to point at the column, which a one-line
/// flash cannot draw.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PERMISSIONS: &str = "permissions.toml";
    const MCP: &str = "mcp.toml";
    const SYNTAX_ERROR: &str = "expected `=`";
    const MISSING_FIELD: &str = "missing field `command`";
    const DENIES_TOOLS: &str = "every tool call is denied";
    const KEEPS_SERVERS: &str = "the running servers stay";
    const MULTILINE_ERROR: &str = "TOML parse error at line 1\n  |\n1 | x\n  |  ^\nexpected `=`";
    const ONE_LINE_ERROR: &str = "TOML parse error at line 1 | 1 | x | ^ expected `=`";

    fn files(problems: &ConfigProblems) -> Vec<&'static str> {
        problems.load().iter().map(|problem| problem.file).collect()
    }

    #[test]
    fn setting_a_file_again_replaces_its_problem_in_place() {
        let problems = ConfigProblems::default();
        problems.set(PERMISSIONS, SYNTAX_ERROR.into(), DENIES_TOOLS);
        problems.set(MCP, SYNTAX_ERROR.into(), KEEPS_SERVERS);

        problems.set(PERMISSIONS, MISSING_FIELD.into(), DENIES_TOOLS);

        assert_eq!(files(&problems), [PERMISSIONS, MCP]);
        assert_eq!(problems.load()[0].error, MISSING_FIELD);
    }

    #[test_case(PERMISSIONS, &[MCP]; "a_recorded_file")]
    #[test_case("caudra.toml", &[PERMISSIONS, MCP]; "a_file_never_recorded")]
    fn clearing_removes_only_that_file(cleared: &'static str, remaining: &[&str]) {
        let problems = ConfigProblems::default();
        problems.set(PERMISSIONS, SYNTAX_ERROR.into(), DENIES_TOOLS);
        problems.set(MCP, SYNTAX_ERROR.into(), KEEPS_SERVERS);

        problems.clear(cleared);

        assert_eq!(files(&problems), remaining);
    }

    #[test]
    fn details_name_every_file_its_error_and_what_it_costs() {
        let problems = ConfigProblems::default();
        assert_eq!(problems.details(), None);
        problems.set(PERMISSIONS, MULTILINE_ERROR.into(), DENIES_TOOLS);
        problems.set(MCP, MISSING_FIELD.into(), KEEPS_SERVERS);

        assert_eq!(
            problems.details().unwrap(),
            format!(
                "{PERMISSIONS}: {ONE_LINE_ERROR}; {DENIES_TOOLS}{DETAIL_SEPARATOR}\
                 {MCP}: {MISSING_FIELD}; {KEEPS_SERVERS}"
            )
        );
    }
}
