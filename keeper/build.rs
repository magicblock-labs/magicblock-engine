//! Builds the v42 calculator SBF program for runtime tests.

use std::{env, io, path::PathBuf, process::Command};

const PROGRAM_DIR: &str = "programs/v42-calculator-program";
const PROGRAM: &str = "programs/v42-calculator-program/Cargo.toml";
const SO: &str = "v42_calculator_program.so";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The embedded program is only compiled by `keeper::testkit`.
    if env::var_os("CARGO_FEATURE_TESTKIT").is_none() {
        return Ok(());
    }

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR is not set"))?,
    );
    let workspace = manifest_dir.parent().ok_or_else(|| {
        io::Error::other(format!(
            "CARGO_MANIFEST_DIR has no parent: {}",
            manifest_dir.display()
        ))
    })?;
    let manifest = workspace.join(PROGRAM);
    let artifact = workspace.join("target/deploy").join(SO);
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(PROGRAM_DIR).display()
    );

    let output = Command::new("cargo")
        .arg("build-sbf")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--arch")
        .arg("v3")
        .current_dir(workspace)
        // `cargo clippy` exports these wrappers pointing at `clippy-driver`; left in
        // place they hijack the SBF toolchain's rustc, which can't resolve the
        // `sbpf*-solana` target. Strip them so `build-sbf` uses its own toolchain.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .map_err(|e| io::Error::other(format!("failed to run `cargo build-sbf`: {e}")))?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "`cargo build-sbf` failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    if !artifact.is_file() {
        Err(io::Error::other(format!(
            "missing SBF artifact: {}",
            artifact.display()
        )))?;
    }
    println!(
        "cargo:rustc-env=V42_CALCULATOR_PROGRAM_SO={}",
        artifact.display()
    );
    Ok(())
}
