//! Verifies that the Rust workspace (`Cargo.toml` + `crates/*/Cargo.toml`), `@apackdev/terminal`
//! (`node/package.json` and all of its platform `optionalDependencies`), and `@apackdev/terminal-view`
//! (`view/package.json` + `view/package-lock.json`) stay in strict version lockstep.

use std::fs;
use std::path::Path;

use serde_json::Value;

const SHIPPED_CRATES: &[&str] = &[
    "crates/apack-proto/Cargo.toml",
    "crates/apack-core/Cargo.toml",
    "crates/apack-sim/Cargo.toml",
    "crates/apack-vt-ghostty/Cargo.toml",
    "crates/apack-os/Cargo.toml",
    "crates/apack-wal/Cargo.toml",
    "crates/apack-broker/Cargo.toml",
];

const PLATFORM_PACKAGES: &[&str] =
    &["@apackdev/terminal-darwin-arm64", "@apackdev/terminal-linux-x64", "@apackdev/terminal-win32-x64"];

/// Verifies every release manifest under `root` declares the exact same version as root `Cargo.toml`.
pub fn check(root: &Path) -> Result<(), String> {
    let expected = cargo_package_version(&root.join("Cargo.toml"))?;
    let mut mismatches = Vec::new();

    for rel in SHIPPED_CRATES {
        let path = root.join(rel);
        let found = cargo_package_version(&path)?;
        if found != expected {
            mismatches.push(format!("{rel}: version is {found}, expected {expected}"));
        }
    }

    let node_pkg = read_json(&root.join("node/package.json"))?;
    check_json_version(&node_pkg, "node/package.json", &expected, &mut mismatches);
    match node_pkg.get("optionalDependencies").and_then(Value::as_object) {
        Some(deps) => {
            for &pkg in PLATFORM_PACKAGES {
                match deps.get(pkg).and_then(Value::as_str) {
                    Some(v) if v == expected => {}
                    Some(v) => mismatches.push(format!(
                        "node/package.json optionalDependencies[{pkg}]: version is {v}, expected {expected}"
                    )),
                    None => mismatches.push(format!("node/package.json optionalDependencies: missing {pkg}")),
                }
            }
            for key in deps.keys() {
                if !PLATFORM_PACKAGES.contains(&key.as_str()) {
                    mismatches
                        .push(format!("node/package.json optionalDependencies: unexpected platform package {key}"));
                }
            }
        }
        None => mismatches.push("node/package.json: missing optionalDependencies object".to_owned()),
    }

    let view_pkg = read_json(&root.join("view/package.json"))?;
    check_json_version(&view_pkg, "view/package.json", &expected, &mut mismatches);

    let view_lock_path = root.join("view/package-lock.json");
    if view_lock_path.exists() {
        let view_lock = read_json(&view_lock_path)?;
        check_json_version(&view_lock, "view/package-lock.json", &expected, &mut mismatches);
        match view_lock
            .get("packages")
            .and_then(|p| p.get(""))
            .and_then(|root_pkg| root_pkg.get("version"))
            .and_then(Value::as_str)
        {
            Some(v) if v == expected => {}
            Some(v) => mismatches
                .push(format!("view/package-lock.json packages[\"\"].version: version is {v}, expected {expected}")),
            None => mismatches.push("view/package-lock.json: missing packages[\"\"].version".to_owned()),
        }
    }

    if !mismatches.is_empty() {
        return Err(format!(
            "package version lockstep check failed (root Cargo.toml is {expected}):\n  - {}",
            mismatches.join("\n  - ")
        ));
    }
    println!("check-versions: all Rust crates, node/package.json, and view/package.json aligned at {expected}");
    Ok(())
}

fn cargo_package_version(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse_cargo_package_version(&text).ok_or_else(|| format!("missing [package] version in {}", path.display()))
}

/// Extracts `version = "..."` from the `[package]` section of a `Cargo.toml`.
pub(crate) fn parse_cargo_package_version(toml_text: &str) -> Option<String> {
    let mut in_package = false;
    for raw in toml_text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((key, val)) = line.split_once('=')
            && key.trim() == "version"
        {
            let trimmed = val.trim();
            if let Some(unquoted) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                && !unquoted.is_empty()
            {
                return Some(unquoted.to_owned());
            }
        }
    }
    None
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn check_json_version(json: &Value, label: &str, expected: &str, mismatches: &mut Vec<String>) {
    match json.get("version").and_then(Value::as_str) {
        Some(v) if v == expected => {}
        Some(v) => mismatches.push(format!("{label}: version is {v}, expected {expected}")),
        None => mismatches.push(format!("{label}: missing string version field")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_fixture(root: &Path, root_v: &str, crate_v: &str, node_v: &str, opt_v: &str, view_v: &str, lock_v: &str) {
        fs::write(root.join("Cargo.toml"), format!("[package]\nname = \"apack-terminal\"\nversion = \"{root_v}\"\n"))
            .unwrap();
        for rel in SHIPPED_CRATES {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("[package]\nname = \"x\"\nversion = \"{crate_v}\"\n")).unwrap();
        }
        fs::create_dir_all(root.join("node")).unwrap();
        fs::write(
            root.join("node/package.json"),
            format!(
                "{{\"name\":\"@apackdev/terminal\",\"version\":\"{node_v}\",\"optionalDependencies\":{{\"@apackdev/terminal-darwin-arm64\":\"{opt_v}\",\"@apackdev/terminal-linux-x64\":\"{opt_v}\",\"@apackdev/terminal-win32-x64\":\"{opt_v}\"}}}}"
            ),
        )
        .unwrap();
        fs::create_dir_all(root.join("view")).unwrap();
        fs::write(
            root.join("view/package.json"),
            format!("{{\"name\":\"@apackdev/terminal-view\",\"version\":\"{view_v}\"}}"),
        )
        .unwrap();
        fs::write(
            root.join("view/package-lock.json"),
            format!("{{\"name\":\"@apackdev/terminal-view\",\"version\":\"{lock_v}\",\"packages\":{{\"\":{{\"version\":\"{lock_v}\"}}}}}}"),
        )
        .unwrap();
    }

    #[test]
    fn check_versions_accepts_lockstep_and_rejects_every_drift_source() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), "0.2.0", "0.2.0", "0.2.0", "0.2.0", "0.2.0", "0.2.0");
        assert!(check(dir.path()).is_ok());

        // Crate version drift
        write_fixture(dir.path(), "0.2.0", "0.1.9", "0.2.0", "0.2.0", "0.2.0", "0.2.0");
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("crates/apack-proto/Cargo.toml"));

        // node/package.json version drift
        write_fixture(dir.path(), "0.2.0", "0.2.0", "0.1.0", "0.2.0", "0.2.0", "0.2.0");
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("node/package.json: version is 0.1.0"));

        // optionalDependencies drift
        write_fixture(dir.path(), "0.2.0", "0.2.0", "0.2.0", "0.1.0", "0.2.0", "0.2.0");
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("optionalDependencies[@apackdev/terminal-linux-x64]"));

        // view/package.json drift
        write_fixture(dir.path(), "0.2.0", "0.2.0", "0.2.0", "0.2.0", "0.1.0", "0.2.0");
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("view/package.json: version is 0.1.0"));

        // view/package-lock.json drift
        write_fixture(dir.path(), "0.2.0", "0.2.0", "0.2.0", "0.2.0", "0.2.0", "0.1.0");
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("view/package-lock.json"));

        // Comments / non-package sections in Cargo.toml do not confuse parser
        let parsed = parse_cargo_package_version(
            "[workspace]\nversion = \"9.9.9\"\n[package]\n# comment\nname = \"a\"\nversion = \"1.2.3\" # inline\n[dependencies]\nversion = \"0.0.1\"\n",
        );
        assert_eq!(parsed.as_deref(), Some("1.2.3"));
        assert_eq!(parse_cargo_package_version("[workspace]\nmembers = []\n"), None);
    }
}
