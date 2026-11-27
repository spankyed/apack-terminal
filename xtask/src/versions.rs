//! Verifies that the Rust workspace (`Cargo.toml` + every `[workspace].members` crate excluding
//! `xtask`), `@apackdev/terminal` (`node/package.json` and all of its platform
//! `optionalDependencies`), and `@apackdev/terminal-view` (`view/package.json` +
//! `view/package-lock.json`) stay in strict version lockstep.

use std::fs;
use std::path::Path;

use serde_json::Value;

const PLATFORM_PACKAGES: &[&str] =
    &["@apackdev/terminal-darwin-arm64", "@apackdev/terminal-linux-x64", "@apackdev/terminal-win32-x64"];

/// Verifies every release manifest under `root` declares the exact same version as root `Cargo.toml`.
pub fn check(root: &Path) -> Result<(), String> {
    let root_toml_path = root.join("Cargo.toml");
    let root_toml =
        fs::read_to_string(&root_toml_path).map_err(|e| format!("reading {}: {e}", root_toml_path.display()))?;
    let expected = parse_cargo_package_version(&root_toml)
        .ok_or_else(|| format!("missing [package] version in {}", root_toml_path.display()))?;
    let members = parse_workspace_members(&root_toml)?;
    let crate_members: Vec<&str> = members.iter().map(String::as_str).filter(|m| *m != "." && *m != "xtask").collect();
    if crate_members.is_empty() {
        return Err(format!("no workspace crate members found in {}", root_toml_path.display()));
    }

    let mut mismatches = Vec::new();

    for member in crate_members {
        let rel = format!("{member}/Cargo.toml");
        let path = root.join(&rel);
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

    let view_lock = read_json(&root.join("view/package-lock.json"))?;
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

/// Extracts `members = [...]` string items from the `[workspace]` section of root `Cargo.toml`.
pub(crate) fn parse_workspace_members(toml_text: &str) -> Result<Vec<String>, String> {
    let mut in_workspace = false;
    let mut in_members = false;
    let mut members = Vec::new();

    for raw in toml_text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if !in_members && line.starts_with('[') && line.ends_with(']') {
            in_workspace = line == "[workspace]";
            continue;
        }
        if !in_workspace {
            continue;
        }
        let scan = if !in_members {
            let Some((key, rest)) = line.split_once('=') else {
                continue;
            };
            if key.trim() != "members" {
                continue;
            }
            let Some(after_open) = rest.trim().strip_prefix('[') else {
                return Err("workspace.members must be a TOML array".to_owned());
            };
            in_members = true;
            after_open
        } else {
            line
        };

        let (slice, closed) = match scan.split_once(']') {
            Some((before, _)) => (before, true),
            None => (scan, false),
        };
        for token in slice.split(',') {
            let t = token.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(unquoted) = t.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                && !unquoted.is_empty()
            {
                members.push(unquoted.to_owned());
            } else {
                return Err(format!("invalid workspace member entry: {t}"));
            }
        }
        if closed {
            return Ok(members);
        }
    }
    Err("missing [workspace] members array in Cargo.toml".to_owned())
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

    struct Fixture<'a> {
        members: &'a [&'a str],
        root_v: &'a str,
        crate_v: &'a str,
        node_v: &'a str,
        opt_v: &'a str,
        view_v: &'a str,
        lock_v: Option<&'a str>,
    }

    fn write_fixture(root: &Path, f: &Fixture<'_>) {
        let members_toml = f.members.iter().map(|m| format!("    \"{m}\",")).collect::<Vec<_>>().join("\n");
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\n{members_toml}\n]\n\n[package]\nname = \"apack-terminal\"\nversion = \"{}\"\n",
                f.root_v
            ),
        )
        .unwrap();
        for &m in f.members {
            if m == "." || m == "xtask" {
                continue;
            }
            let p = root.join(m).join("Cargo.toml");
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("[package]\nname = \"x\"\nversion = \"{}\"\n", f.crate_v)).unwrap();
        }
        fs::create_dir_all(root.join("node")).unwrap();
        fs::write(
            root.join("node/package.json"),
            format!(
                "{{\"name\":\"@apackdev/terminal\",\"version\":\"{}\",\"optionalDependencies\":{{\"@apackdev/terminal-darwin-arm64\":\"{}\",\"@apackdev/terminal-linux-x64\":\"{}\",\"@apackdev/terminal-win32-x64\":\"{}\"}}}}",
                f.node_v, f.opt_v, f.opt_v, f.opt_v
            ),
        )
        .unwrap();
        fs::create_dir_all(root.join("view")).unwrap();
        fs::write(
            root.join("view/package.json"),
            format!("{{\"name\":\"@apackdev/terminal-view\",\"version\":\"{}\"}}", f.view_v),
        )
        .unwrap();
        let lock_path = root.join("view/package-lock.json");
        if let Some(lv) = f.lock_v {
            fs::write(
                &lock_path,
                format!(
                    "{{\"name\":\"@apackdev/terminal-view\",\"version\":\"{lv}\",\"packages\":{{\"\":{{\"version\":\"{lv}\"}}}}}}"
                ),
            )
            .unwrap();
        } else {
            let _ = fs::remove_file(&lock_path);
        }
    }

    #[test]
    fn check_versions_discovers_workspace_members_and_rejects_every_drift_source() {
        let dir = tempdir().unwrap();
        let members = &[".", "xtask", "crates/apack-proto", "crates/apack-new-crate"];
        let base = Fixture {
            members,
            root_v: "0.2.0",
            crate_v: "0.2.0",
            node_v: "0.2.0",
            opt_v: "0.2.0",
            view_v: "0.2.0",
            lock_v: Some("0.2.0"),
        };
        write_fixture(dir.path(), &base);
        assert!(check(dir.path()).is_ok());

        // Dynamically discovered newly-added crate version drift
        write_fixture(dir.path(), &Fixture { crate_v: "0.1.9", ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("crates/apack-new-crate/Cargo.toml"));

        // node/package.json version drift
        write_fixture(dir.path(), &Fixture { node_v: "0.1.0", ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("node/package.json: version is 0.1.0"));

        // optionalDependencies drift
        write_fixture(dir.path(), &Fixture { opt_v: "0.1.0", ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("optionalDependencies[@apackdev/terminal-linux-x64]"));

        // view/package.json drift
        write_fixture(dir.path(), &Fixture { view_v: "0.1.0", ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("view/package.json: version is 0.1.0"));

        // view/package-lock.json drift AND missing view/package-lock.json must fail!
        write_fixture(dir.path(), &Fixture { lock_v: Some("0.1.0"), ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("view/package-lock.json"));

        write_fixture(dir.path(), &Fixture { lock_v: None, ..base });
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("view/package-lock.json"));

        // Inline array workspace.members and comments in Cargo.toml parse cleanly
        let inline = parse_workspace_members("[workspace]\nmembers = [\".\", \"crates/a\"] # inline\n").unwrap();
        assert_eq!(inline, vec![".", "crates/a"]);
        let parsed = parse_cargo_package_version(
            "[workspace]\nversion = \"9.9.9\"\n[package]\n# comment\nname = \"a\"\nversion = \"1.2.3\" # inline\n[dependencies]\nversion = \"0.0.1\"\n",
        );
        assert_eq!(parsed.as_deref(), Some("1.2.3"));
        assert_eq!(parse_cargo_package_version("[workspace]\nmembers = []\n"), None);
    }
}
