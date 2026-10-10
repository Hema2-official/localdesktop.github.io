use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// The Android package this build is for: `LOCALDESKTOP_PACKAGE` if set, otherwise the
/// `package:` entry of `manifest.yaml`.
///
/// The Linux rootfs lives in the package's private data directory, so a build under another
/// package name (e.g. `app.polarbear.dev`) gets its own installation next to the official app.
fn package_name() -> String {
    println!("cargo::rerun-if-env-changed=LOCALDESKTOP_PACKAGE");
    println!("cargo::rerun-if-changed=manifest.yaml");

    if let Ok(package) = env::var("LOCALDESKTOP_PACKAGE") {
        return package;
    }
    fs::read_to_string("manifest.yaml")
        .expect("Failed to read manifest.yaml")
        .lines()
        .find_map(|line| line.trim().strip_prefix("package:"))
        .map(|package| package.trim().to_string())
        .expect("manifest.yaml has no `package:` entry")
}

/// What `scripts/guest/build-xr.sh` builds headsets' VR support from, for setup to build it
/// where no prebuilt bundle installs (src/android/proot/xr.rs): `$OUT_DIR/xr_recipe.rs`, an
/// expression listing `(path, contents)` by the files' paths in the repository, sorted.
fn embed_xr_recipe() {
    let mut paths = vec!["scripts/guest/build-xr.sh".to_string()];
    println!("cargo::rerun-if-changed=scripts/guest/build-xr.sh");
    for dir in ["patches/mesa", "patches/monado"] {
        println!("cargo::rerun-if-changed={dir}");
        files_under(Path::new(dir), &mut paths);
    }
    paths.sort();
    let mut code = String::from("&[\n");
    for path in &paths {
        let absolute = fs::canonicalize(path).expect("Failed to find a file of the XR recipe");
        code.push_str(&format!("    ({path:?}, include_bytes!({absolute:?})),\n"));
    }
    code.push_str("]\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is unset")).join("xr_recipe.rs");
    fs::write(out, code).expect("Failed to write the XR recipe");
}

fn files_under(dir: &Path, paths: &mut Vec<String>) {
    let entries = fs::read_dir(dir).expect("Failed to list a directory of the XR recipe");
    for entry in entries {
        let path = entry
            .expect("Failed to list a directory of the XR recipe")
            .path();
        if path.is_dir() {
            files_under(&path, paths);
        } else {
            paths.push(path.to_str().expect("Non-UTF-8 path").replace('\\', "/"));
        }
    }
}

fn main() {
    let lib_path = "./assets/libs/arm64-v8a";
    println!("cargo::rustc-link-search={}", lib_path);

    embed_xr_recipe();

    let package = package_name();
    println!("cargo::rustc-env=LOCALDESKTOP_PACKAGE={package}");

    // Only the official app reports to the maintainers' Sentry project.
    println!("cargo::rustc-check-cfg=cfg(official_package)");
    if package == "app.polarbear" {
        println!("cargo::rustc-cfg=official_package");
    }
}
