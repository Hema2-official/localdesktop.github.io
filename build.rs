use std::{env, fs};

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

fn main() {
    let lib_path = "./assets/libs/arm64-v8a";
    println!("cargo::rustc-link-search={}", lib_path);

    let package = package_name();
    println!("cargo::rustc-env=LOCALDESKTOP_PACKAGE={package}");

    // Only the official app reports to the maintainers' Sentry project.
    println!("cargo::rustc-check-cfg=cfg(official_package)");
    if package == "app.polarbear" {
        println!("cargo::rustc-cfg=official_package");
    }
}
