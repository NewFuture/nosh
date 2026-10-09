use std::path::Path;
use std::process::Command;

use serde_json::Value;

#[test]
fn production_dependencies_follow_the_architecture() {
    let boundaries: &[(&str, &[&str])] = &[
        ("nosh-engine", &[]),
        ("nosh-platform", &[]),
        ("nosh-permissions", &[]),
        ("nosh-hub", &["nosh-platform"]),
        ("nosh-llm", &["nosh-engine"]),
        ("nosh-shell", &["nosh-platform"]),
        (
            "nosh-core",
            &[
                "nosh-engine",
                "nosh-permissions",
                "nosh-platform",
                "nosh-shell",
            ],
        ),
        (
            "nosh-cli",
            &[
                "nosh-core",
                "nosh-engine",
                "nosh-hub",
                "nosh-llm",
                "nosh-permissions",
                "nosh-platform",
                "nosh-shell",
            ],
        ),
    ];
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--locked",
            "--offline",
        ])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
        .output()
        .expect("read workspace Cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let members = metadata["workspace_members"].as_array().unwrap();
    let mut checked = 0;
    for package in metadata["packages"].as_array().unwrap() {
        if !members.contains(&package["id"]) {
            continue;
        }
        let name = package["name"].as_str().unwrap();
        if name == "nosh-tests" {
            continue;
        }
        let (_, allowed) = boundaries
            .iter()
            .find(|(member, _)| *member == name)
            .unwrap_or_else(|| panic!("declare the dependency boundary for {name}"));
        checked += 1;
        for dependency in package["dependencies"].as_array().unwrap() {
            if dependency["kind"] == "dev" {
                continue;
            }
            let target = dependency["name"].as_str().unwrap();
            assert!(
                !target.starts_with("nosh-") || allowed.contains(&target),
                "{name} must not depend on {target} in production; allowed: {allowed:?}"
            );
        }
    }
    assert_eq!(checked, boundaries.len(), "stale architecture boundaries");
}
