//! Generated regions of hand-written pages. Each region sits between
//! `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->`, and
//! everything outside the markers stays as written.

use std::fmt::{self, Display, Formatter, Write};

use caudra_config::example::{Document, mcp, permissions, sandboxes, workcell};
use caudra_config::files::{self, ConfigFile};

use crate::gen_config::{example_file_name, write_entries};

const MARKER_OPEN: &str = "<!-- caudra-docgen:";
const MARKER_CLOSE: &str = "<!-- /caudra-docgen:";
const MARKER_END: &str = " -->";
/// The path of the keys that come before any table header.
const TOP_LEVEL: &str = "";

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
    file: &'static ConfigFile,
    tables: &'static [RegionTable],
    /// Ends with the ways to get the whole reference. One region per page
    /// does.
    footer: bool,
}

pub const REGIONS: &[Region] = &[
    Region {
        page: "mcp",
        name: "mcp-server-fields",
        file: &files::MCP,
        tables: &[RegionTable {
            heading: None,
            paths: &[mcp::STDIO_SERVER, mcp::HTTP_SERVER],
        }],
        footer: false,
    },
    Region {
        page: "mcp",
        name: "mcp-top-level",
        file: &files::MCP,
        tables: &[RegionTable {
            heading: None,
            paths: &[TOP_LEVEL],
        }],
        footer: true,
    },
    Region {
        page: "mcp",
        name: "mcp-oauth",
        file: &files::MCP,
        tables: &[RegionTable {
            heading: None,
            paths: &[mcp::OAUTH_CLIENT],
        }],
        footer: false,
    },
    Region {
        page: "remote-workspaces",
        name: "workcell-profile-fields",
        file: &files::WORKCELL,
        tables: &[RegionTable {
            heading: None,
            paths: &[workcell::PROFILE],
        }],
        footer: true,
    },
    Region {
        page: "sandboxes",
        name: "sandbox-records",
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
    Region {
        page: "permissions",
        name: "permissions-keys",
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
];

impl Region {
    fn generate(&self) -> String {
        let document = reference(self.file);
        let mut out = String::new();
        for (index, table) in self.tables.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            if let Some(heading) = table.heading {
                writeln!(out, "#### {heading}\n").unwrap();
            }
            out.push_str(&keys_table(&document, table.paths));
        }
        if self.footer {
            writeln!(out, "\n{}", footer(self.file)).unwrap();
        }
        out
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
         [{example}](/docs/{example}) holds the same text.",
        command = file.example_command(),
        name = file.name,
        example = example_file_name(file),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use caudra_config::files::{self, ConfigFile};
    use test_case::test_case;

    use super::{MarkerError, REGIONS, Region, reference};
    use crate::page_path;

    const OPEN: &str = "<!-- caudra-docgen:sample -->";
    const CLOSE: &str = "<!-- /caudra-docgen:sample -->";
    const BEFORE: &str = "Hand-written text before.\n\n";
    const AFTER: &str = "\n\nHand-written text after.\n";
    const STALE: &str = "| stale | table |";
    const SAMPLE: Region = Region {
        page: "sample",
        name: "sample",
        file: &files::MCP,
        tables: &[],
        footer: true,
    };

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
        assert_eq!(SAMPLE.splice(&parts.concat()), Err(expected));
    }

    #[test]
    fn every_region_has_one_pair_of_markers_in_its_page() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for region in REGIONS {
            let page = fs::read_to_string(root.join(page_path(region.page))).unwrap();
            if let Err(error) = region.splice(&page) {
                panic!("{}: {error}", region.page);
            }
        }
    }

    #[test]
    fn the_regions_of_a_file_list_every_key_of_its_reference() {
        let mut generated: BTreeMap<&str, (&ConfigFile, String)> = BTreeMap::new();
        for region in REGIONS {
            generated
                .entry(region.file.name)
                .or_insert_with(|| (region.file, String::new()))
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
}
