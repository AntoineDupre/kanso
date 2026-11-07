//! Guards for the SBOM inputs: the frontend package metadata, the vendored
//! marked build and the crate version must stay in sync with each other.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(path: &str) -> Value {
    let full = root().join(path);
    let text = fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("read {}: {error}", full.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", full.display()))
}

#[test]
fn frontend_package_matches_the_crate_version() {
    let package = read_json("web/package.json");

    assert_eq!(package["name"], "kanso-web");
    assert_eq!(package["private"].as_bool(), Some(true));
    assert_eq!(package["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn frontend_lock_pins_the_vendored_marked_build() {
    let package = read_json("web/package.json");
    let version = package["dependencies"]["marked"]
        .as_str()
        .expect("web/package.json declares marked");

    let lock = read_json("web/package-lock.json");
    assert_eq!(lock["lockfileVersion"], 3);
    assert_eq!(lock["packages"][""]["version"], env!("CARGO_PKG_VERSION"));

    let marked = &lock["packages"]["node_modules/marked"];
    assert_eq!(marked["version"], version);
    assert_eq!(marked["license"], "MIT");
    let integrity = marked["integrity"]
        .as_str()
        .expect("the lockfile records marked's tarball integrity");
    assert!(integrity.starts_with("sha512-"), "{integrity}");

    let vendor_path = root().join("web/vendor/marked.min.js");
    let vendor = fs::read_to_string(&vendor_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", vendor_path.display()));
    let banner = vendor
        .lines()
        .take(5)
        .find(|line| line.contains("marked v"))
        .expect("the vendored marked build carries its version banner");
    assert!(
        banner.contains(&format!("marked v{version} ")),
        "web/vendor/marked.min.js is not marked {version}: {banner}"
    );
}
