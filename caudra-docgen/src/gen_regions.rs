//! Generated regions of hand-written pages. Each region sits between
//! `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->`, and
//! everything outside the markers stays as written.

use std::fmt::{self, Display, Formatter, Write};
use std::fs;
use std::path::{Path, PathBuf};

use caudra_agent::automation::catalog::needed_features;
use caudra_automation::meta::parse_meta;
use caudra_config::example::{Document, mcp, permissions, sandboxes, workcell};
use caudra_config::files::{self, ConfigFile};

use crate::gen_config::write_entries;
use crate::gen_reference_configs;

const MARKER_OPEN: &str = "<!-- caudra-docgen:";
const MARKER_CLOSE: &str = "<!-- /caudra-docgen:";
const MARKER_END: &str = " -->";
/// The path of the keys that come before any table header.
const TOP_LEVEL: &str = "";
/// Where `caudra-automation` keeps the examples its tests replay, from this
/// crate's directory.
const EXAMPLES_DIR: &str = "../caudra-automation/tests/examples";
const EXAMPLE_EXTENSION: &str = "rhai";
/// Every example in [`EXAMPLES_DIR`], in teaching order: one session first,
/// then unattended sessions and other services, then a swarm.
const AUTOMATION_EXAMPLES: [&str; 17] = [
    "retry-overload",
    "keep-going",
    "timebox",
    "goal-chain",
    "standup",
    "spend-guard",
    "page-me",
    "goal-webhook",
    "nightly-review",
    "ci-watch",
    "join-swarm",
    "status-beacon",
    "status-desk",
    "work-nudge",
    "task-tracker",
    "ci-triage",
    "research-desk",
];
const PATTERN_HEADING: &str = "### ";
const SENTENCE_END: char = '.';
const NEEDS_SEPARATOR: &str = " and ";

#[derive(Debug, PartialEq, Eq)]
pub enum MarkerError {
    Missing(String),
    Repeated(String),
    /// The closing marker comes first.
    Reversed,
}

impl Display for MarkerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(marker) => write!(f, "`{marker}` is missing"),
            Self::Repeated(marker) => write!(f, "`{marker}` appears more than once"),
            Self::Reversed => f.write_str("the closing marker comes before the opening one"),
        }
    }
}

/// One table of a region: its heading, when the region has more than one
/// table, and the tables of the file's reference whose keys it lists.
struct RegionTable {
    heading: Option<&'static str>,
    paths: &'static [&'static str],
}

pub struct Region {
    /// The docs section of the page, such as `mcp`.
    pub page: &'static str,
    pub name: &'static str,
    source: Source,
}

/// What a region is generated from.
enum Source {
    /// Keys of a config file's reference.
    Keys {
        file: &'static ConfigFile,
        tables: &'static [RegionTable],
        /// Ends with the ways to get the whole reference. One region per
        /// page does.
        footer: bool,
    },
    /// The automation examples of these names, in this order.
    Examples(&'static [&'static str]),
}

pub const REGIONS: &[Region] = &[
    Region {
        page: "mcp",
        name: "mcp-server-fields",
        source: Source::Keys {
            file: &files::MCP,
            tables: &[RegionTable {
                heading: None,
                paths: &[mcp::STDIO_SERVER, mcp::HTTP_SERVER],
            }],
            footer: false,
        },
    },
    Region {
        page: "mcp",
        name: "mcp-top-level",
        source: Source::Keys {
            file: &files::MCP,
            tables: &[RegionTable {
                heading: None,
                paths: &[TOP_LEVEL],
            }],
            footer: true,
        },
    },
    Region {
        page: "mcp",
        name: "mcp-oauth",
        source: Source::Keys {
            file: &files::MCP,
            tables: &[RegionTable {
                heading: None,
                paths: &[mcp::OAUTH_CLIENT],
            }],
            footer: false,
        },
    },
    Region {
        page: "remote-workspaces",
        name: "workcell-profile-fields",
        source: Source::Keys {
            file: &files::WORKCELL,
            tables: &[RegionTable {
                heading: None,
                paths: &[workcell::PROFILE],
            }],
            footer: true,
        },
    },
    Region {
        page: "sandboxes",
        name: "sandbox-records",
        source: Source::Keys {
            file: &files::SANDBOXES,
            tables: &[
                RegionTable {
                    heading: Some("`[sandbox.providers.NAME]`"),
                    paths: &[sandboxes::PROVIDER],
                },
                RegionTable {
                    heading: Some("`[sandbox.networks.NAME]`"),
                    paths: &[sandboxes::NETWORK],
                },
                RegionTable {
                    heading: Some("`[sandbox.transfers.NAME]`"),
                    paths: &[sandboxes::TRANSFER],
                },
                RegionTable {
                    heading: Some("`[sandbox.profiles.NAME]`"),
                    paths: &[sandboxes::PROFILE],
                },
            ],
            footer: true,
        },
    },
    Region {
        page: "permissions",
        name: "permissions-keys",
        source: Source::Keys {
            file: &files::PERMISSIONS,
            tables: &[
                RegionTable {
                    heading: Some("Top level"),
                    paths: &[TOP_LEVEL],
                },
                RegionTable {
                    heading: Some("`[TOOL]`"),
                    paths: &[permissions::TOOL],
                },
                RegionTable {
                    heading: Some("`[mcp.SERVER]`"),
                    paths: &[permissions::MCP_SERVER],
                },
            ],
            footer: true,
        },
    },
    Region {
        page: "automations",
        name: "automation-patterns",
        source: Source::Examples(&AUTOMATION_EXAMPLES),
    },
];

impl Region {
    fn generate(&self) -> String {
        match self.source {
            Source::Keys {
                file,
                tables,
                footer,
            } => keys_region(file, tables, footer),
            Source::Examples(names) => patterns(names),
        }
    }

    /// `page` with the text between this region's markers generated again.
    pub fn splice(&self, page: &str) -> Result<String, MarkerError> {
        let open = format!("{MARKER_OPEN}{}{MARKER_END}", self.name);
        let close = format!("{MARKER_CLOSE}{}{MARKER_END}", self.name);
        let start = find(page, &open)? + open.len();
        let end = find(page, &close)?;
        if end < start {
            return Err(MarkerError::Reversed);
        }
        Ok(format!(
            "{}\n\n{}\n\n{}",
            &page[..start],
            self.generate().trim(),
            &page[end..]
        ))
    }
}

fn find(page: &str, marker: &str) -> Result<usize, MarkerError> {
    let mut found = page.match_indices(marker).map(|(at, _)| at);
    match (found.next(), found.next()) {
        (Some(at), None) => Ok(at),
        (None, _) => Err(MarkerError::Missing(marker.to_owned())),
        (Some(_), Some(_)) => Err(MarkerError::Repeated(marker.to_owned())),
    }
}

fn keys_region(file: &ConfigFile, tables: &[RegionTable], with_footer: bool) -> String {
    let document = reference(file);
    let mut out = String::new();
    for (index, table) in tables.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if let Some(heading) = table.heading {
            writeln!(out, "#### {heading}\n").unwrap();
        }
        out.push_str(&keys_table(&document, table.paths));
    }
    if with_footer {
        writeln!(out, "\n{}", footer(file)).unwrap();
    }
    out
}

pub fn reference(file: &ConfigFile) -> Document {
    let document = file
        .example
        .unwrap_or_else(|| panic!("{} has no reference", file.name));
    document()
}

/// One docs table with the keys of the reference tables at `paths`, in
/// order.
pub fn keys_table(document: &Document, paths: &[&str]) -> String {
    let entries = paths.iter().flat_map(|path| {
        &document
            .table(path)
            .unwrap_or_else(|| panic!("the reference has no [{path}] table"))
            .entries
    });
    let mut out = String::new();
    write_entries(&mut out, entries);
    out
}

/// The ways to get the whole reference of `file`.
pub fn footer(file: &ConfigFile) -> String {
    format!(
        "`{command}` prints every `{name}` key with its default, all commented out. \
         [Reference configs]({link}) shows the same text.",
        command = file.example_command(),
        name = file.name,
        link = gen_reference_configs::link(file),
    )
}

/// The automation examples of `names`, in order, as the tests replay them.
fn patterns(names: &[&str]) -> String {
    let mut out = String::new();
    for name in names {
        let path = examples_dir().join(name).with_extension(EXAMPLE_EXTENSION);
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        write_pattern(&mut out, name, &source);
    }
    out
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(EXAMPLES_DIR)
}

/// One example under its name: its description, the experimental switches
/// it needs besides `automations`, and its source.
fn write_pattern(out: &mut String, name: &str, source: &str) {
    let meta = parse_meta(source).unwrap_or_else(|error| panic!("the {name} example: {error}"));
    writeln!(
        out,
        "{PATTERN_HEADING}{name}\n\n{}{SENTENCE_END}\n",
        meta.description.trim_end_matches(SENTENCE_END)
    )
    .unwrap();
    let needs: Vec<String> = needed_features(&meta)
        .iter()
        .map(|feature| format!("`{feature}`"))
        .collect();
    if !needs.is_empty() {
        writeln!(out, "Needs {}{SENTENCE_END}\n", needs.join(NEEDS_SEPARATOR)).unwrap();
    }
    writeln!(out, "```rhai\n{}\n```\n", source.trim_end()).unwrap();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use caudra_config::files::{self, ConfigFile};
    use test_case::test_case;

    use super::{
        AUTOMATION_EXAMPLES, EXAMPLE_EXTENSION, MarkerError, PATTERN_HEADING, REGIONS, Region,
        Source, examples_dir, reference, write_pattern,
    };
    use crate::page_path;

    const OPEN: &str = "<!-- caudra-docgen:sample -->";
    const CLOSE: &str = "<!-- /caudra-docgen:sample -->";
    const BEFORE: &str = "Hand-written text before.\n\n";
    const AFTER: &str = "\n\nHand-written text after.\n";
    const STALE: &str = "| stale | table |";
    const SAMPLE: Region = Region {
        page: "sample",
        name: "sample",
        source: Source::Keys {
            file: &files::MCP,
            tables: &[],
            footer: true,
        },
    };
    const SAMPLE_PATTERNS: Region = Region {
        page: "sample",
        name: "sample",
        source: Source::Examples(&[]),
    };
    const NUDGE: &str = "nudge";
    const NUDGE_SOURCE: &str = r#"let meta = #{
    name: "nudge",
    description: "Ask for the tests when a turn ends",
    triggers: [#{ kind: "idle" }],
};
message("Run the tests.");"#;
    const DESK: &str = "desk";
    const DESK_SOURCE: &str = r#"let meta = #{
    name: "desk",
    description: "Hand review requests to a workflow.",
    triggers: [#{ kind: "message_received", consume: true }],
    workflows: ["review-changes"],
};
start_workflow("review-changes", #{ scope: "main" });
"#;

    #[test]
    fn a_splice_replaces_only_the_text_between_the_markers() {
        let page = format!("{BEFORE}{OPEN}\n{STALE}\n{CLOSE}{AFTER}");
        let spliced = SAMPLE.splice(&page).unwrap();
        assert!(
            spliced.starts_with(&format!("{BEFORE}{OPEN}\n\n`")),
            "{spliced}"
        );
        assert!(
            spliced.ends_with(&format!("\n\n{CLOSE}{AFTER}")),
            "{spliced}"
        );
        assert!(!spliced.contains(STALE));
        assert_eq!(SAMPLE.splice(&spliced).unwrap(), spliced);
    }

    #[test_case(&[CLOSE], MarkerError::Missing(OPEN.into()) ; "missing_opening_marker")]
    #[test_case(&[OPEN], MarkerError::Missing(CLOSE.into()) ; "missing_closing_marker")]
    #[test_case(&[OPEN, CLOSE, OPEN, CLOSE], MarkerError::Repeated(OPEN.into()) ; "repeated_markers")]
    #[test_case(&[CLOSE, OPEN], MarkerError::Reversed ; "reversed_markers")]
    fn a_page_without_one_ordered_pair_of_markers_is_refused(
        parts: &[&str],
        expected: MarkerError,
    ) {
        for region in [&SAMPLE, &SAMPLE_PATTERNS] {
            assert_eq!(
                region.splice(&parts.concat()).err().as_ref(),
                Some(&expected)
            );
        }
    }

    #[test]
    fn every_region_has_one_pair_of_markers_in_its_page() {
        for region in REGIONS {
            let page = fs::read_to_string(page_path(region.page)).unwrap();
            if let Err(error) = region.splice(&page) {
                panic!("{}: {error}", region.page);
            }
        }
    }

    #[test]
    fn the_regions_of_a_file_list_every_key_of_its_reference() {
        let mut generated: BTreeMap<&str, (&ConfigFile, String)> = BTreeMap::new();
        for region in REGIONS {
            let Source::Keys { file, .. } = region.source else {
                continue;
            };
            generated
                .entry(file.name)
                .or_insert_with(|| (file, String::new()))
                .1
                .push_str(&region.generate());
        }
        for (file, text) in generated.values() {
            for table in reference(file).tables {
                for entry in table.entries {
                    let row = format!("| `{}` |", entry.name);
                    assert!(text.contains(&row), "{}: {row}", file.name);
                }
            }
        }
    }

    #[test]
    fn the_automation_examples_are_the_files_of_their_directory() {
        let mut on_disk: Vec<String> = fs::read_dir(examples_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == EXAMPLE_EXTENSION)
            })
            .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
            .collect();
        on_disk.sort_unstable();
        let mut listed = AUTOMATION_EXAMPLES.to_vec();
        listed.sort_unstable();
        assert_eq!(listed, on_disk);
    }

    #[test]
    fn a_pattern_region_shows_its_examples_in_their_order() {
        for region in REGIONS {
            let Source::Examples(names) = region.source else {
                continue;
            };
            let generated = region.generate();
            let headings: Vec<&str> = generated
                .lines()
                .filter_map(|line| line.strip_prefix(PATTERN_HEADING))
                .collect();
            assert_eq!(headings, names, "{}", region.name);
        }
    }

    #[test]
    fn a_pattern_shows_the_name_description_needs_and_source_of_each_example() {
        let mut out = String::new();
        write_pattern(&mut out, NUDGE, NUDGE_SOURCE);
        write_pattern(&mut out, DESK, DESK_SOURCE);
        let expected = format!(
            "### {NUDGE}\n\nAsk for the tests when a turn ends.\n\n```rhai\n{NUDGE_SOURCE}\n```\n\n\
             ### {DESK}\n\nHand review requests to a workflow.\n\n\
             Needs `experimental.cross_session_messaging` and `experimental.workflows`.\n\n\
             ```rhai\n{}\n```\n\n",
            DESK_SOURCE.trim_end()
        );
        assert_eq!(out, expected);
    }
}
