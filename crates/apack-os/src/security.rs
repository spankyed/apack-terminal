//! Named-pipe security descriptor policy (SDDL builder on all platforms, Win32 token lookup on
//! Windows).

/// Builds the owner-only SDDL string used for Windows named pipes:
///
/// - `O:{sid}`: current user is the explicit owner.
/// - `D:P`: protected DACL (inherits zero ACEs).
/// - `(A;;GA;;;{sid})`: full access for the current user.
/// - `(A;;GA;;;SY)`: full access for `SYSTEM`.
///
/// Everyone else (including `Everyone` and `Interactive Users`) is denied by absence.
pub fn windows_pipe_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
}

/// Builds a Windows `SecurityDescriptor` granting access solely to the current user and `SYSTEM`.
#[cfg(windows)]
pub fn owner_only_security_descriptor()
-> std::io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    crate::security_windows::owner_only_security_descriptor()
}

/// Returns the current process token's user SID string (`S-1-5-...`).
#[cfg(windows)]
pub(crate) fn current_user_sid() -> std::io::Result<widestring::U16CString> {
    crate::security_windows::current_user_sid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_pipe_sddl_is_protected_owner_and_system_only() {
        let sddl = windows_pipe_sddl("S-1-5-21-100-200-300-1001");
        assert_eq!(sddl, "O:S-1-5-21-100-200-300-1001D:P(A;;GA;;;S-1-5-21-100-200-300-1001)(A;;GA;;;SY)");
        assert!(sddl.contains("D:P("), "DACL must be protected against inheritance");
        assert!(!sddl.contains("WD"), "must never grant Everyone (WD)");
        assert!(!sddl.contains("IU"), "must never grant Interactive Users (IU)");
        assert!(!sddl.contains("AU"), "must never grant Authenticated Users (AU)");
    }
}
