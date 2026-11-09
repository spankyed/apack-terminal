//! Who the broker's pipe is for, as the string that decides it.
//!
//! The descriptor itself can only be built and accepted on Windows, but the access it describes is text —
//! and that text is the whole of the decision. A case here is what stops the broadest ACE in Windows from
//! being one edit away: the default descriptor a pipe gets grants Everyone read, which is what this
//! replaced, and re-admitting any of those well-known accounts would restore it silently.

use apack_terminal::security::access_for;

/// A SID shaped like a real one, belonging to nobody.
const USER: &str = "S-1-5-21-3623811015-3361044348-30300820-1013";

#[test]
fn the_pipe_is_the_starting_user_s_and_nobody_broader() {
    let sddl = access_for(USER);

    assert!(sddl.starts_with(&format!("O:{USER}")), "the user owns it: {sddl}");
    assert!(sddl.contains(&format!("(A;;GA;;;{USER})")), "and may do everything with it: {sddl}");
    assert!(sddl.contains("D:P("), "the DACL is protected, so nothing is inherited into it: {sddl}");

    // The accounts that make a pipe readable by someone else. `WD` is Everyone — the default descriptor's
    // own grant and the whole reason this exists — `AN` the anonymous logon, `BU` the local Users group,
    // `IU` everyone logged in interactively, and `WD`'s cousin `AU` every authenticated account
    for broader in ["WD", "AN", "BU", "IU", "AU"] {
        assert!(!sddl.contains(&format!(";{broader})")), "{broader} may not appear in the pipe's access: {sddl}");
    }
}

/// Exactly two accounts are granted anything: the user, and `SYSTEM`, which could take ownership whatever
/// this said. A third ACE is someone widening the pipe.
#[test]
fn exactly_two_accounts_are_granted_anything() {
    let sddl = access_for(USER);

    assert_eq!(sddl.matches("(A;;").count(), 2, "two allow aces and no more: {sddl}");
    assert!(sddl.contains("(A;;GA;;;SY)"), "SYSTEM is the second: {sddl}");
}
