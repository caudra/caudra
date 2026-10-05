//! The reference of every TOML config file on one page, as `caudra config example` prints it. The same text is
//! published as one `<stem>.example.toml` download per file.

use std::fmt::Write;

use caudra_config::files::{self, ConfigFile};

use crate::page_header;

pub const SLUG: &str = "reference-configs";
const TITLE: &str = "Reference configs";
const DESCRIPTION: &str =
    "Every TOML config file in full, with each key commented out and described.";
const INTRO: &str = "`caudra config example FILE` prints the reference of a TOML config file: every key the \
                     file accepts, with its description, type, and default, all commented out. This page shows \
                     the reference of each file. [Config files](/docs/configuration/#config-files) explains \
                     what each file is for.\n";
const EXAMPLE_SUFFIX: &str = ".example.toml";

/// The download under the site's docs path that holds the reference of `file`.
pub fn example_file_name(file: &ConfigFile) -> String {
    format!("{}{EXAMPLE_SUFFIX}", file.stem())
}

/// The section of this page that shows the reference of `file`.
pub fn link(file: &ConfigFile) -> String {
    format!("/docs/{SLUG}/#{}", file.name.replace('.', "-"))
}

pub fn generate() -> String {
    let mut out = page_header(TITLE, DESCRIPTION);
    out.push_str(INTRO);
    for (file, reference) in files::examples().filter_map(|file| Some((file, file.reference()?))) {
        let needs = file
            .feature
            .map(|feature| {
                format!(
                    " Caudra reads it only when `experimental.{}` is on.",
                    feature.key()
                )
            })
            .unwrap_or_default();
        write!(
            out,
            "\n## {name}\n\n\
             [`{name}`]({docs}) holds {holds}.{needs} Download this reference as \
             [{example}](/docs/{example}).\n\n\
             ```toml\n{reference}\n```\n",
            name = file.name,
            docs = file.docs,
            holds = file.holds,
            example = example_file_name(file),
            reference = reference.trim_end(),
        )
        .unwrap();
    }
    out
}
