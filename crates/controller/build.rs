use std::{env, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let ui_dir = manifest_dir.join("../../frontend/controller");

    for path in [
        "../../frontend/pnpm-lock.yaml",
        "../../frontend/ui/src",
        "../../frontend/controller/index.html",
        "../../frontend/controller/package.json",
        "../../frontend/controller/tsconfig.app.json",
        "../../frontend/controller/vite.config.ts",
        "../../frontend/controller/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    assert!(
        ui_dir.join("dist/index.html").is_file(),
        "controller UI is not built; run `make frontend` first"
    );
}
