/// Command families whose lexical shape misleads the prefix heuristic.
///
/// The heuristic keeps every leading token that looks like a subcommand word,
/// which is wrong in both directions: `rg foo src/` bakes the search term into
/// the pattern, and `npm run build` consumes the whole command so no pattern is
/// offered at all. Each entry names the token prefix that identifies the family
/// and the number of literals the pattern keeps.
///
/// An entry may name a flag, as `sed -n` does, and the heuristic may not: a
/// named flag was decided on, where a kept one is only guessed at, and guessing
/// turns `git -C /repo commit` into `git -C *`.
///
/// Entries are ordered by their prefix so the table reads as documentation;
/// lookup takes the longest match, not the first.
const CURATED_PREFIXES: &[(&[&str], usize)] = &[
    (&["aws"], 3),
    (&["basename"], 1),
    (&["brew"], 2),
    (&["bun"], 2),
    (&["bun", "run"], 3),
    (&["cat"], 1),
    (&["df"], 1),
    (&["diff"], 1),
    (&["dirname"], 1),
    (&["docker"], 2),
    (&["docker", "compose"], 3),
    (&["du"], 1),
    (&["fd"], 1),
    (&["file"], 1),
    (&["find"], 1),
    (&["gh"], 3),
    (&["git", "remote"], 3),
    (&["git", "stash"], 3),
    (&["git", "submodule"], 3),
    (&["git", "worktree"], 3),
    (&["go"], 2),
    (&["go", "run"], 3),
    (&["grep"], 1),
    (&["head"], 1),
    (&["herdr"], 3),
    (&["jq"], 1),
    (&["just"], 2),
    (&["kubectl"], 2),
    (&["kubectl", "rollout"], 3),
    (&["ls"], 1),
    (&["make"], 2),
    (&["npm"], 2),
    (&["npm", "run"], 3),
    (&["pip"], 2),
    (&["pnpm"], 2),
    (&["pnpm", "run"], 3),
    (&["podman"], 2),
    (&["readlink"], 1),
    (&["realpath"], 1),
    (&["rg"], 1),
    (&["sed", "-n"], 2),
    (&["sort"], 1),
    (&["stat"], 1),
    (&["systemctl"], 2),
    (&["tail"], 1),
    (&["tree"], 1),
    (&["uniq"], 1),
    (&["uv"], 2),
    (&["uv", "run"], 3),
    (&["wc"], 1),
    (&["which"], 1),
    (&["yarn"], 2),
    (&["yarn", "run"], 3),
    (&["yq"], 1),
];

/// The length of the matched entry's own prefix, and the literals the pattern
/// keeps. The caller needs both: the tokens the entry named are decided, and
/// only the ones past them still have to look like subcommands.
pub(super) fn curated_literals(tokens: &[String]) -> Option<(usize, usize)> {
    CURATED_PREFIXES
        .iter()
        .filter(|(prefix, _)| starts_with(tokens, prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(prefix, literals)| (prefix.len(), *literals))
}

fn starts_with(tokens: &[String], prefix: &[&str]) -> bool {
    tokens.len() >= prefix.len()
        && prefix
            .iter()
            .zip(tokens)
            .all(|(entry, token)| *entry == token.as_str())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::command_pattern::MAX_PATTERN_TOKENS;
    use super::{CURATED_PREFIXES, curated_literals};

    fn tokens(command: &str) -> Vec<String> {
        command.split_whitespace().map(String::from).collect()
    }

    #[test_case("rg foo src/", Some((1, 1)); "data first tool")]
    #[test_case("npm install react", Some((1, 2)); "subcommand tool")]
    #[test_case("npm run build", Some((2, 3)); "longest prefix wins")]
    #[test_case("uv run pytest tests", Some((2, 3)))]
    #[test_case("docker compose up -d", Some((2, 3)))]
    #[test_case("git stash pop", Some((2, 3)); "namespaced git subcommand")]
    #[test_case("sed -n 1,140p f.rs", Some((2, 2)); "an entry may name a flag")]
    #[test_case("herdr pane read w1:p2 --lines 40", Some((1, 3)); "herdr keeps the pane id out")]
    #[test_case("git commit -m x", None; "uncurated git subcommand")]
    #[test_case("cargo nextest run", None; "heuristic handles cargo")]
    #[test_case("npm", Some((1, 2)); "prefix matches without enough operands")]
    fn curated_lookup_takes_the_longest_matching_prefix(
        command: &str,
        expected: Option<(usize, usize)>,
    ) {
        assert_eq!(curated_literals(&tokens(command)), expected);
    }

    #[test]
    fn curated_prefixes_are_ordered_unique_and_within_bounds() {
        let keys: Vec<&[&str]> = CURATED_PREFIXES.iter().map(|(prefix, _)| *prefix).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(keys, sorted);

        for (prefix, literals) in CURATED_PREFIXES {
            assert!(
                !prefix.is_empty() && *literals >= prefix.len() && *literals < MAX_PATTERN_TOKENS,
                "{prefix:?} keeps {literals} literals"
            );
        }
    }
}
