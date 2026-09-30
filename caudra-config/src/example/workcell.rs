use crate::files;
use crate::workcell::{
    MAX_ENDPOINT_BYTES, MAX_EXPECTED_ID_BYTES, MAX_PROFILE_FILE_BYTES, MAX_PROFILE_NAME_BYTES,
    PROFILE_FIELDS, WORKCELL_PROFILE_VERSION,
};

use super::{Document, Header, RECORDS_USAGE, Table, global_location, preamble};

const PROFILES: &str = "workcell.profiles";
pub const PROFILE: &str = "workcell.profiles.dev";
const BYTES_PER_KIB: u64 = 1024;
const PROFILES_ABOUT: &str = "Keep this header, because the file needs it even without profiles.";

/// Every `workcell.toml` key, on an example profile.
pub fn document() -> Document {
    let file = &files::WORKCELL;
    Document {
        preamble: preamble(
            file,
            [
                RECORDS_USAGE.to_owned(),
                format!("{} {}", global_location(file), protection()),
            ],
        ),
        version: WORKCELL_PROFILE_VERSION,
        tables: vec![
            Table::new(Header::Fixed(PROFILES.into()), Vec::new()).about(PROFILES_ABOUT),
            Table::of(Header::Record(PROFILE.into()), PROFILE_FIELDS).about(profile_about()),
        ],
    }
}

fn protection() -> String {
    format!(
        "It has to be a regular file, not a symlink, that you own and that group and others \
         cannot write, so `chmod 600` suits it. It holds at most {} KiB.",
        MAX_PROFILE_FILE_BYTES / BYTES_PER_KIB
    )
}

fn profile_about() -> String {
    format!(
        "Each [workcell.profiles.NAME] table is one profile, which `caudra --workcell-profile \
         NAME` connects to. NAME is 1 to {MAX_PROFILE_NAME_BYTES} ASCII letters, digits, \".\", \
         \"-\", and \"_\", and starts with a letter or digit. `endpoint` is at most \
         {MAX_ENDPOINT_BYTES} bytes, and each expected ID at most {MAX_EXPECTED_ID_BYTES}."
    )
}
