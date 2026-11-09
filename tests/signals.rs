//! What a signal is aimed at, as arithmetic rather than as an effect.
//!
//! **`kill(0, …)` signals every process in the caller's own group**, and `kill(-0, …)` is the same call
//! written the other way — so a session whose process id is unknown, which records as 0
//! (`child.process_id().unwrap_or_default()`), would turn one stop request into the broker killing itself
//! and every session it owns. These cases are the refusal that cannot be tested through the socket: a
//! session with a real pid would have to be signalled to see where the signal went.
#![cfg(unix)]

use apack_terminal::protocol::Scope;
use apack_terminal::session::signal_target;

#[test]
fn a_real_pid_is_itself_and_its_group_is_the_negative_of_it() {
    assert_eq!(signal_target(4242, Scope::Leader).expect("a leader"), 4242);
    assert_eq!(signal_target(4242, Scope::Group).expect("a group"), -4242);
    assert_eq!(signal_target(4242, Scope::Pid { pid: 4243 }).expect("a pid"), 4243);
}

#[test]
fn a_session_whose_process_id_is_unknown_is_refused_rather_than_aimed_at_everything() {
    for scope in [Scope::Leader, Scope::Group] {
        let refused = signal_target(0, scope).expect_err("0 is not a pid to signal");
        assert_eq!(refused.code, apack_terminal::protocol::ErrorCode::Pty, "{}", refused.message);
    }
    // And the pid scope, where the number came from a caller rather than from a spawn
    signal_target(4242, Scope::Pid { pid: 0 }).expect_err("0 is not a pid to signal");
}

#[test]
fn init_is_refused_too_because_its_group_is_every_process_the_user_may_signal() {
    signal_target(1, Scope::Group).expect_err("-1 is every process");
    signal_target(1, Scope::Leader).expect_err("1 is init");
    signal_target(4242, Scope::Pid { pid: 1 }).expect_err("1 is init");
}
