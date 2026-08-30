use std::env;
use std::fmt::Write;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

const WORKER_ENV: &str = "WORKCELL_BUNDLED_MONTY_WORKER";
const WORKER_VERSION: &str = "0.0.21";

fn main() {
    println!("cargo:rerun-if-env-changed={WORKER_ENV}");
    println!("cargo:rustc-check-cfg=cfg(workcell_bundled_monty_worker)");

    let Some(worker) = env::var_os(WORKER_ENV) else {
        return;
    };
    let worker = Path::new(&worker);
    println!("cargo:rerun-if-changed={}", worker.display());

    let metadata = fs::symlink_metadata(worker).unwrap_or_else(|error| {
        panic!(
            "{WORKER_ENV} does not identify a readable worker at {}: {error}",
            worker.display()
        )
    });
    assert!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "{WORKER_ENV} must identify a regular file, not a symlink: {}",
        worker.display()
    );
    let bytes = fs::read(worker).unwrap_or_else(|error| {
        panic!(
            "read the worker configured by {WORKER_ENV} at {}: {error}",
            worker.display()
        )
    });
    let target = env::var("TARGET").expect("Cargo sets TARGET for build scripts");
    validate_binary_format(&bytes, &target);

    let file_name = if target.contains("windows") {
        "monty.exe"
    } else {
        "monty"
    };
    let output = Path::new(&env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"))
        .join("bundled-monty-worker");
    fs::write(&output, &bytes).unwrap_or_else(|error| {
        panic!(
            "stage bundled Monty worker at {}: {error}",
            output.display()
        )
    });

    println!("cargo:rustc-cfg=workcell_bundled_monty_worker");
    println!(
        "cargo:rustc-env=WORKCELL_MONTY_WORKER_SHA256={}",
        sha256_bytes(&bytes)
    );
    println!("cargo:rustc-env=WORKCELL_MONTY_WORKER_TARGET={target}");
    println!("cargo:rustc-env=WORKCELL_MONTY_WORKER_FILE_NAME={file_name}");
    println!("cargo:rustc-env=WORKCELL_MONTY_WORKER_VERSION={WORKER_VERSION}");
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn validate_binary_format(bytes: &[u8], target: &str) {
    let architecture = target.split('-').next().unwrap_or_default();
    let valid = if target.contains("windows") {
        matches!(
            (architecture, pe_machine(bytes)),
            ("x86_64", Some([0x64, 0x86]))
        )
    } else if target.contains("apple") {
        bytes.get(..4) == Some(&[0xcf, 0xfa, 0xed, 0xfe])
            && matches!(
                (architecture, bytes.get(4..8)),
                ("x86_64", Some([0x07, 0x00, 0x00, 0x01]))
                    | ("aarch64", Some([0x0c, 0x00, 0x00, 0x01]))
            )
    } else if target.contains("linux") {
        bytes.starts_with(b"\x7fELF\x02\x01")
            && matches!(
                (architecture, bytes.get(18..20)),
                ("x86_64", Some([0x3e, 0x00])) | ("aarch64", Some([0xb7, 0x00]))
            )
    } else {
        false
    };
    assert!(
        valid,
        "{WORKER_ENV} is not a supported executable for target {target}"
    );
}

fn pe_machine(bytes: &[u8]) -> Option<[u8; 2]> {
    if !bytes.starts_with(b"MZ") {
        return None;
    }
    let header_offset = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if bytes.get(header_offset..header_offset.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    bytes
        .get(header_offset.checked_add(4)?..header_offset.checked_add(6)?)?
        .try_into()
        .ok()
}
