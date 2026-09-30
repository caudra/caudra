//! The user docs under `site/docs/content`. They are embedded here rather than in `caudra-docs`, so editing a
//! page rebuilds this crate alone and not every crate that reads the docs.

use std::sync::LazyLock;

use caudra_agent::tools::native::skill::BuiltinSkill;
use caudra_docs::{Library, NAME, SKILL_DESCRIPTION};
use include_dir::{Dir, File, include_dir};

static CONTENT: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/site/docs/content");
static LIBRARY: LazyLock<Library> = LazyLock::new(|| Library::parse(markdown_files(&CONTENT)));

/// Parsed on first use, so a session that never opens the docs never pays for them.
pub fn library() -> &'static Library {
    &LIBRARY
}

/// The builtin `caudra-docs` skill: loading it returns the index, and `caudra-docs/<page>#<section>` or
/// `caudra-docs?<terms>` load a section or a search.
pub fn skill() -> BuiltinSkill {
    BuiltinSkill {
        name: NAME.to_owned(),
        description: SKILL_DESCRIPTION.to_owned(),
        resolve: Box::new(|| (library().index(), None)),
        pages: Some(Box::new(|address| {
            library().load(address).map_err(|error| error.to_string())
        })),
    }
}

fn markdown_files(dir: &'static Dir<'static>) -> Vec<(&'static str, &'static str)> {
    let mut files: Vec<(&'static str, &'static str)> = dir.files().filter_map(text_file).collect();
    for child in dir.dirs() {
        files.extend(markdown_files(child));
    }
    files
}

fn text_file(file: &'static File<'static>) -> Option<(&'static str, &'static str)> {
    Some((file.path().to_str()?, file.contents_utf8()?))
}

#[cfg(test)]
mod tests {
    use super::library;

    #[test]
    fn embedded_docs_hold_every_page() {
        let slugs: Vec<&str> = library().pages().iter().map(|page| page.slug).collect();
        assert!(
            slugs.contains(&"quick-start") && slugs.contains(&"lua-api"),
            "{slugs:?}"
        );
        assert!(library().load("/permissions#plan-mode").is_ok());
    }
}
