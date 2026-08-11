use std::{env, path::PathBuf, process::Command};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let ui_dir = manifest_dir.join("../../ui");

    for path in [
        "../../ui/index.html",
        "../../ui/package.json",
        "../../ui/pnpm-lock.yaml",
        "../../ui/tsconfig.app.json",
        "../../ui/vite.config.ts",
        "../../ui/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let status = Command::new("pnpm")
        .args(["frontend:build"])
        .current_dir(&ui_dir)
        .status()
        .expect("pnpm is required to build the embedded controller UI");
    assert!(
        status.success(),
        "failed to build the embedded controller UI"
    );
}
