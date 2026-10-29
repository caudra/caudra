use std::io::Read;

use caudra_models::{MAX_BYTES, Snapshot, parse_bounded};

const COMPRESSED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bundled.json.zst"));

#[cfg(not(test))]
pub(super) fn snapshot() -> Snapshot {
    decode(COMPRESSED)
}

#[cfg(test)]
pub(super) fn snapshot() -> Snapshot {
    parse_bounded(include_bytes!("fixture.json")).expect("the frozen fixture is a valid snapshot")
}

fn decode(compressed: &[u8]) -> Snapshot {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .expect("bundled model snapshot compression was checked at build time");
    let mut bytes = Vec::new();
    decoder
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .expect("bundled model snapshot decompression was checked at build time");
    parse_bounded(&bytes).expect("bundled model snapshot was checked at build time")
}

#[cfg(test)]
mod tests {
    use super::{COMPRESSED, decode, snapshot};

    #[test]
    fn frozen_fixture_passes_validation() {
        let snapshot = snapshot();
        caudra_models::validate(&snapshot).unwrap();
        assert!(snapshot.hash_matches());
    }

    #[test]
    fn pinned_copy_round_trips() {
        assert!(decode(COMPRESSED).hash_matches());
    }
}
