//! Executables whose arguments can be code they run, so allowing one with any
//! arguments allows anything.

pub const SOURCE_DOT: &str = ".";
pub const PRIVILEGED_EXECUTABLES: &[&str] = &["sudo", "doas", "su"];
pub const INDIRECT_EXECUTABLES: &[&str] = &["eval", "source", SOURCE_DOT];
/// Shells whose `-c` code is Bash, so Workcell's parser reads it faithfully.
pub const PARSED_SHELLS: &[&str] = &["bash", "sh"];
pub const WRAPPERS: &[&str] = &["command", "builtin", "env", "xargs", "time", "coproc"];
pub const PAYLOAD_EXECUTABLES: &[&str] = &[
    "cd",
    "exec",
    "ash",
    "dash",
    "zsh",
    "ksh",
    "csh",
    "tcsh",
    "fish",
    "cmd",
    "powershell",
    "pwsh",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "jq",
    "yq",
    "ssh",
    "sshpass",
    "rsh",
    "mosh",
    "expect",
    "tclsh",
    "wish",
    "osascript",
    "deno",
    "bun",
    "npx",
    "ts-node",
    "tsx",
    "parallel",
    "bc",
    "dc",
    "sqlite3",
    "psql",
    "mysql",
    "r",
    "rscript",
];
const VERSIONED_INTERPRETERS: &[&str] = &[
    "python",
    "pypy",
    "ipython",
    "jython",
    "micropython",
    "node",
    "nodejs",
    "perl",
    "raku",
    "ruby",
    "irb",
    "lua",
    "luajit",
    "php",
    "julia",
];

/// The interpreter a name runs, reading `python3` and `python3.12` as `python`.
pub fn versioned_interpreter(name: &str) -> Option<&'static str> {
    VERSIONED_INTERPRETERS.iter().copied().find(|interpreter| {
        name.strip_prefix(interpreter).is_some_and(|version| {
            version.is_empty()
                || version.starts_with(|character: char| character.is_ascii_digit())
                    && version
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
    })
}

/// Whether the words after this executable can be code it runs: a shell, an
/// interpreter, a wrapper, or a privileged or indirect command.
pub fn runs_given_code(name: &str) -> bool {
    [
        PRIVILEGED_EXECUTABLES,
        INDIRECT_EXECUTABLES,
        PARSED_SHELLS,
        WRAPPERS,
        PAYLOAD_EXECUTABLES,
    ]
    .iter()
    .any(|names| names.contains(&name))
        || versioned_interpreter(name).is_some()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{runs_given_code, versioned_interpreter};

    #[test_case("python", Some("python"); "bare")]
    #[test_case("python3", Some("python"); "major")]
    #[test_case("python3.12", Some("python"); "minor")]
    #[test_case("python3x", None; "a suffix that is not a version")]
    #[test_case("pythonista", None; "a longer name")]
    #[test_case("rustfmt", None; "not an interpreter")]
    fn versioned_interpreter_reads_only_version_suffixes(name: &str, expected: Option<&str>) {
        assert_eq!(versioned_interpreter(name), expected);
    }

    #[test_case("sudo", true; "privileged")]
    #[test_case("source", true; "indirect")]
    #[test_case("bash", true; "parsed shell")]
    #[test_case("xargs", true; "wrapper")]
    #[test_case("sed", true; "payload")]
    #[test_case("node22", true; "versioned interpreter")]
    #[test_case("rustfmt", false; "formatter")]
    #[test_case("git", false; "version control")]
    fn runs_given_code_names_every_code_runner(name: &str, expected: bool) {
        assert_eq!(runs_given_code(name), expected);
    }
}
