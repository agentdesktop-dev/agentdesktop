use std::{env, path::PathBuf};

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
    assert!(
        ui_dir.join("dist/index.html").is_file(),
        "controller UI is not built; run `make ui` first"
    );
}
