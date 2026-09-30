use crate::files;
use crate::sandbox::{
    MAX_NETWORK_RULES, MAX_SANDBOX_FILE_BYTES, MAX_SANDBOX_NAME_BYTES, MAX_SANDBOX_RECORD_BYTES,
    MAX_SANDBOX_RECORDS, MAX_TRANSFER_EXCLUDES, NetworkPolicy, SANDBOX_VERSION, SandboxProfile,
    SandboxProvider, TransferPolicy,
};

use super::{Document, Header, RECORDS_USAGE, Table, global_location, preamble};

const SANDBOX: &str = "sandbox";
pub const PROVIDER: &str = "sandbox.providers.local";
pub const NETWORK: &str = "sandbox.networks.default";
pub const TRANSFER: &str = "sandbox.transfers.default";
pub const PROFILE: &str = "sandbox.profiles.dev";
const BYTES_PER_KIB: usize = 1024;
const PROTECTION: &str = "It has to be a regular file with one link, owned by you, that group \
     and others have no access to, so `chmod 600` suits it. The directory that holds it has to be \
     yours, and each directory above that has to be yours or root's. None of them may let group or \
     others write, except a sticky directory that root owns, such as /tmp. No part of the path may \
     be a symlink.";
const SAVES: &str = "`/sandbox` saves this file and keeps your comments. Saving creates no VM.";
const SANDBOX_ABOUT: &str = "Keep this header, because the file needs it even without records.";
const NETWORK_ABOUT: &str = "Each [sandbox.networks.NAME] table is one network policy, which \
     profiles can share.";
const TRANSFER_ABOUT: &str = "Each [sandbox.transfers.NAME] table is one file transfer policy, \
     which profiles can share. Protected paths stay protected even with `exclude = []`.";
const PROFILE_ABOUT: &str = "Each [sandbox.profiles.NAME] table is one launch profile, which \
     `caudra --sandbox NAME` starts. The file is refused while a record the profile names is \
     missing. The provider checks the template, the resources, and the lease only at launch.";

/// Every `sandboxes.toml` key, on one record of each kind. The profile names
/// the other three, so the live form loads as a whole.
pub fn document() -> Document {
    let file = &files::SANDBOXES;
    Document {
        preamble: preamble(
            file,
            [
                RECORDS_USAGE.to_owned(),
                format!("{} {PROTECTION}", global_location(file)),
                limits(),
                SAVES.to_owned(),
            ],
        ),
        version: SANDBOX_VERSION,
        tables: vec![
            Table::new(Header::Fixed(SANDBOX.into()), Vec::new()).about(SANDBOX_ABOUT),
            Table::of(Header::Record(PROVIDER.into()), SandboxProvider::FIELDS)
                .about(provider_about()),
            Table::of(Header::Record(NETWORK.into()), NetworkPolicy::FIELDS).about(NETWORK_ABOUT),
            Table::of(Header::Record(TRANSFER.into()), TransferPolicy::FIELDS)
                .about(TRANSFER_ABOUT),
            Table::of(Header::Record(PROFILE.into()), SandboxProfile::FIELDS).about(PROFILE_ABOUT),
        ],
    }
}

fn limits() -> String {
    format!(
        "The file holds at most {MAX_SANDBOX_RECORDS} records and {} KiB, and one record at most \
         {} KiB. A network lists at most {MAX_NETWORK_RULES} domains and CIDRs together, and a \
         transfer at most {MAX_TRANSFER_EXCLUDES} globs.",
        MAX_SANDBOX_FILE_BYTES / BYTES_PER_KIB,
        MAX_SANDBOX_RECORD_BYTES / BYTES_PER_KIB
    )
}

fn provider_about() -> String {
    format!(
        "Each [sandbox.providers.NAME] table is one sandbox provider. Every kind of record has \
         names of its own: 1 to {MAX_SANDBOX_NAME_BYTES} ASCII letters, digits, \".\", \"-\", \
         and \"_\", starting with a letter or digit."
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde::Serialize;

    use super::{NETWORK, PROFILE, PROVIDER, TRANSFER, document};
    use crate::example::Render;
    use crate::sandbox::SandboxDraft;

    fn import(render: Render) -> SandboxDraft {
        SandboxDraft::import(&document().render(render)).unwrap()
    }

    fn names(path: &str) -> BTreeSet<String> {
        let document = document();
        let table = document.table(path).unwrap();
        table
            .entries
            .iter()
            .map(|entry| entry.name.into())
            .collect()
    }

    fn keys<'a, T: Serialize + 'a>(records: impl IntoIterator<Item = &'a T>) -> BTreeSet<String> {
        let record = records.into_iter().next().unwrap();
        let value = serde_json::to_value(record).unwrap();
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn the_reference_saves_no_record() {
        assert_eq!(import(Render::Reference), SandboxDraft::default());
    }

    #[test]
    fn every_stated_default_is_the_built_in_one() {
        assert_eq!(
            import(Render::Live { defaults: true }),
            import(Render::Live { defaults: false })
        );
    }

    #[test]
    fn every_key_of_each_record_is_described() {
        let draft = import(Render::Live { defaults: false });
        assert_eq!(names(PROVIDER), keys(draft.providers.values()));
        assert_eq!(names(NETWORK), keys(draft.networks.values()));
        assert_eq!(names(TRANSFER), keys(draft.transfers.values()));
        assert_eq!(names(PROFILE), keys(draft.profiles.values()));
    }
}
