// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2025 Sreehari Anil <sreehari7102008@gmail.com>

use std::process::Command;

fn main() {
    // Get git commit hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let pkg_version = env!("CARGO_PKG_VERSION");
    let full_version = format!("{}+{}", pkg_version, git_hash);

    println!("cargo:rustc-env=FULL_VERSION={}", full_version);
    println!("cargo:rustc-env=GIT_COMMIT={}", git_hash);

    // Rerun if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
