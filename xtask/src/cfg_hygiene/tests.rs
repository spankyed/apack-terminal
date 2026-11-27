use super::*;

#[test]
fn allows_thin_windows_wiring_cross_platform_any_and_test_only_cfg() {
    let src = r#"
#[cfg(windows)]
mod win_impl;
#[cfg(windows)]
use crate::win_impl::helper;

pub struct State {
    #[cfg(windows)]
    pub handle: Arc<dyn Fn() + Send + Sync>,
    #[cfg(windows)]
    count: usize,
}

pub enum Pty {
    #[cfg(windows)]
    Windows { bridge: Vec<u8>, eof: bool },
}

#[cfg(any(unix, windows))]
pub fn compiled_on_both(a: usize, b: usize) -> usize {
    a + b
}

pub fn run(state: &mut State, pty: &Pty) -> Option<usize> {
    #[cfg(windows)]
    let _x: Result<usize, String> = helper(state, move || state.count);

    #[cfg(windows)]
    {
        let mut flag = false;
        *(&mut flag) = true;
        if let Pty::Windows { bridge, .. } = pty
            && let Some(first) = bridge.first()
        {
            let _ = format!("{first}");
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (state, pty);
        None
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    fn test_only_helper(a: usize, b: usize) -> bool {
        if a > b { a + b == 10 } else { false }
    }
}

#[cfg(all(test, windows))]
fn another_test_helper(x: bool) -> bool {
    !x
}
"#;
    let res = scan_source("crates/demo/src/lib.rs", src);
    assert!(res.cfg_attrs_seen >= 8);
    assert!(res.violations.is_empty(), "unexpected violations: {:?}", res.violations);
}

#[test]
fn rejects_windows_functions_methods_impls_inline_mods_and_inner_cfg() {
    let fn_src = "#[cfg(windows)]\npub(crate) fn win_only() -> bool { true }\n";
    let res = scan_source("crates/demo/src/a.rs", fn_src);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("Windows-only function `win_only`"));

    let method_src =
        "impl Foo {\n    #[cfg(all(not(test), windows))]\n    fn hidden_method(&self) -> bool { true }\n}\n";
    let res = scan_source("crates/demo/src/method.rs", method_src);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("Windows-only function `hidden_method`"));

    let not_unix_fn = "#[cfg(not(unix))]\nasync fn fallback() {}\n";
    let res = scan_source("crates/demo/src/b.rs", not_unix_fn);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("Windows-only function `fallback`"));

    let impl_src = "#[cfg(windows)]\nimpl Foo { fn bar(&self) {} }\n";
    let res = scan_source("crates/demo/src/c.rs", impl_src);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("`impl` block"));

    let inline_mod = "#[cfg(target_os = \"windows\")]\nmod inner { fn x() {} }\n";
    let res = scan_source("crates/demo/src/d.rs", inline_mod);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("inline Windows-only module `mod inner"));

    let inner_attr = "#![cfg(windows)]\npub fn f() {}\n";
    let res = scan_source("crates/demo/src/e.rs", inner_attr);
    assert_eq!(res.violations.len(), 1);
    assert!(res.violations[0].reason.contains("inner attribute"));
}

#[test]
fn rejects_mutable_logic_inside_windows_blocks_and_match_guards() {
    let cases = [
        ("fn f(x: bool) {\n    #[cfg(windows)]\n    if x { do_it(); }\n}\n", "conditional `if` branch"),
        (
            "fn f(x: Option<u32>) {\n    #[cfg(not(unix))]\n    match x { Some(_) => {}, None => {} }\n}\n",
            "`match` dispatch",
        ),
        ("fn f(a: u32, b: u32) -> bool {\n    #[cfg(windows)]\n    { a == b }\n}\n", "comparison operator"),
        (
            "fn f(a: u32, b: u32) -> u32 {\n    #[cfg(windows)]\n    let n = a + b;\n    n\n}\n",
            "arithmetic/bitwise operator",
        ),
        ("fn f(a: bool, b: bool) -> bool {\n    #[cfg(windows)]\n    { a || b }\n}\n", "boolean logic operator"),
        (
            "fn f(flag: bool) -> bool {\n    #[cfg(all(windows, not(feature = \"x\")))]\n    { !flag }\n}\n",
            "boolean negation",
        ),
        ("fn f(a: u32, b: u32) -> bool {\n    #[cfg(windows)]\n    { a < b }\n}\n", "comparison operator `<`"),
        (
            "fn f(x: Option<u32>) {\n    match x {\n        #[cfg(windows)]\n        Some(n) if n > 0 => drop(n),\n        _ => {}\n    }\n}\n",
            "conditional `if` branch",
        ),
    ];

    for (src, expected) in cases {
        let res = scan_source("crates/demo/src/mixed.rs", src);
        assert_eq!(res.violations.len(), 1, "expected 1 violation for:\n{src}");
        assert!(res.violations[0].reason.contains(expected), "expected `{expected}` in `{}`", res.violations[0].reason);
    }
}

#[test]
fn check_fails_on_synthetic_violation_and_passes_when_allowlisted() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("crates/demo/src")).unwrap();

    std::fs::write(tmp.path().join("src/main.rs"), "#[cfg(unix)]\nfn main() {}\n").unwrap();
    std::fs::write(tmp.path().join("crates/demo/src/win.rs"), "#![cfg(windows)]\npub fn f(x: bool) -> bool { !x }\n")
        .unwrap();
    std::fs::write(tmp.path().join(".cargo/windows-only"), "crates/demo/src/win.rs\n").unwrap();

    check(tmp.path()).expect("allowlisted windows-only file must pass");

    std::fs::write(
        tmp.path().join("crates/demo/src/mixed.rs"),
        "#[cfg(windows)]\npub fn hidden_win_logic(x: usize) -> usize { x + 1 }\n",
    )
    .unwrap();
    let err = check(tmp.path()).unwrap_err();
    assert!(err.contains("crates/demo/src/mixed.rs:1"), "got: {err}");
    assert!(err.contains("hidden_win_logic"), "got: {err}");
}
