use std::borrow::Cow;
use std::env;
use std::path::Path;

use caudra_storage::checkout::{self, Checkout};
use jiff::Timestamp;

use crate::herdr::HerdrEnv;
use crate::prompt::{CHECKOUT_SLOT, HERDR_SLOT};

const DETACHED_HEAD: &str = "detached HEAD";

pub fn env_vars() -> Vars {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    let date = Timestamp::now().strftime("%Y-%m-%d").to_string();
    let (checkout, herdr) = if crate::scratch::tools_run_locally() {
        (
            checkout_section(Path::new(&cwd)),
            herdr_section(HerdrEnv::detect()),
        )
    } else {
        Default::default()
    };
    Vars::new()
        .set("{cwd}", cwd)
        .set("{platform}", env::consts::OS)
        .set("{date}", date)
        .set(CHECKOUT_SLOT, checkout)
        .set(HERDR_SLOT, herdr)
        .set("{scratch}", crate::scratch::environment_section())
        .set(
            "{task_system_prompt_profiles}",
            "- `builtin`: Caudra's built-in task prompt",
        )
}

/// Said only in a verified linked worktree, where the working directory alone
/// does not tell which repository the checkout belongs to or that sibling
/// checkouts share its memory and plans.
fn checkout_section(cwd: &Path) -> String {
    let Some(checkout) = checkout::discover(cwd).filter(Checkout::is_linked) else {
        return String::new();
    };
    let head = checkout.branch.map_or_else(
        || DETACHED_HEAD.to_owned(),
        |branch| format!("branch {branch}"),
    );
    format!(
        "- Git worktree: {head}, linked to {}\n",
        checkout.main_root.display()
    )
}

fn herdr_section(herdr: Option<HerdrEnv>) -> String {
    herdr.map_or_else(String::new, |herdr| {
        format!("- Herdr: pane {}\n", herdr.pane_id)
    })
}

#[derive(Clone, Default)]
pub struct Vars(Vec<(&'static str, String)>);

impl Vars {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn set(mut self, key: &'static str, val: impl Into<String>) -> Self {
        let val = val.into();
        if let Some((_, current)) = self.0.iter_mut().find(|(candidate, _)| *candidate == key) {
            *current = val;
        } else {
            self.0.push((key, val));
        }
        self
    }

    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if self.0.is_empty() || !s.contains('{') {
            return Cow::Borrowed(s);
        }
        let mut out = s.to_string();
        for (k, v) in &self.0 {
            out = out.replace(k, v);
        }
        Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    use crate::worktree::git::fixture::{Repository, git};

    const BRANCH: &str = "feature/login";
    const LINKED: &str = "linked";
    const PANE: &str = "w1:p2";
    const HERDR_BINARY: &str = "herdr";
    const HERDR_SOCKET: &str = "/tmp/herdr.sock";

    fn format_date(ts: Timestamp) -> String {
        ts.strftime("%Y-%m-%d").to_string()
    }

    #[test_case("{cwd} on {platform}", "/home on linux" ; "multiple_keys")]
    #[test_case("{x} and {x}", "42 and 42" ; "repeated_key")]
    #[test_case("no placeholders", "no placeholders" ; "no_placeholders")]
    fn apply(input: &str, expected: &str) {
        let vars = Vars::new()
            .set("{cwd}", "/home")
            .set("{platform}", "linux")
            .set("{x}", "42");
        assert_eq!(vars.apply(input).as_ref(), expected);
    }

    #[test_case(0,             "1970-01-01" ; "unix_epoch")]
    #[test_case(1_000_000_000, "2001-09-09" ; "billion_seconds")]
    #[test_case(1_740_700_800, "2025-02-28" ; "feb_28_non_leap")]
    #[test_case(1_709_164_800, "2024-02-29" ; "leap_day_2024")]
    fn format_date_cases(secs: i64, expected: &str) {
        let ts = Timestamp::from_second(secs).unwrap();
        assert_eq!(format_date(ts), expected);
    }

    #[test]
    fn env_vars_includes_date() {
        let vars = env_vars();
        let result = vars.apply("{date}");
        assert_ne!(result.as_ref(), "{date}");
    }

    #[test_case(true ; "linked_worktree")]
    #[test_case(false ; "main_checkout")]
    fn only_a_linked_worktree_is_named(linked: bool) {
        let repository = Repository::new();
        let worktree = repository.sibling(LINKED);
        git(
            &repository.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                BRANCH,
                &worktree.to_string_lossy(),
            ],
        );

        let section = checkout_section(if linked { &worktree } else { &repository.root });

        assert_eq!(!section.is_empty(), linked);
        if linked {
            assert!(section.contains(BRANCH));
            assert!(section.contains(repository.root.to_string_lossy().as_ref()));
        }
    }

    #[test]
    fn a_detached_worktree_says_so() {
        let repository = Repository::new();
        let worktree = repository.sibling(LINKED);
        git(
            &repository.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &worktree.to_string_lossy(),
            ],
        );

        assert!(checkout_section(&worktree).contains(DETACHED_HEAD));
    }

    #[test_case(Some(PANE) ; "inside_herdr")]
    #[test_case(None ; "outside_herdr")]
    fn only_a_herdr_pane_is_named(pane: Option<&str>) {
        let herdr = pane.map(|pane| HerdrEnv {
            binary: HERDR_BINARY.into(),
            socket_path: HERDR_SOCKET.into(),
            pane_id: pane.into(),
            workspace_id: None,
        });

        let section = herdr_section(herdr);

        assert_eq!(section.contains(PANE), pane.is_some());
        assert_eq!(section.is_empty(), pane.is_none());
    }
}
