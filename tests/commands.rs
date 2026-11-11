//! The bookkeeping of a session's commands, driven as the shell's reports arrive.
//!
//! A row here is a plain number, so a case can say where the cursor was at each moment and read back which
//! command got which row. What the terminal adds — the mark that follows a row — is `tests/command_rows.rs`.

use std::time::{Duration, SystemTime};

use apack_terminal::commands::{COMMANDS_KEPT, Commands};

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// Each command as `(id, command, exit status, prompt row, output row, end row, running)`.
type Summary = (u64, Option<String>, Option<i32>, Option<u32>, Option<u32>, Option<u32>, bool);

fn summary(commands: &Commands<u32>) -> Vec<Summary> {
    commands
        .iter()
        .map(|one| (one.id, one.command.clone(), one.exit_status, one.prompt, one.output, one.end, one.running()))
        .collect()
}

#[test]
fn a_command_is_recorded_from_its_prompt_to_its_end_with_the_row_each_was_at() {
    let mut commands = Commands::<u32>::default();
    commands.prompt_started(at(1));
    commands.place(|_| Some(10));
    commands.output_started(Some("make".to_owned()), Some("/src".to_owned()), at(2));
    commands.place(|_| Some(11));
    assert_eq!(summary(&commands), vec![(1, Some("make".to_owned()), None, Some(10), Some(11), None, true)]);

    commands.ended(Some(2), at(5));
    commands.place(|_| Some(40));
    assert_eq!(summary(&commands), vec![(1, Some("make".to_owned()), Some(2), Some(10), Some(11), Some(40), false)]);
    let made = commands.get(1).expect("the command is kept");
    assert_eq!((made.cwd.as_deref(), made.started, made.finished), (Some("/src"), Some(at(2)), Some(at(5))));
}

/// A shell that reports an end and then draws its prompt has ended one command, and begun waiting for the
/// next.
#[test]
fn the_prompt_after_an_end_is_the_next_command_waiting() {
    let mut commands = Commands::<u32>::default();
    commands.prompt_started(at(1));
    commands.output_started(None, None, at(2));
    commands.ended(Some(0), at(3));
    commands.prompt_started(at(3));
    commands.place(|_| Some(7));
    let all = summary(&commands);
    assert_eq!(all.len(), 2);
    assert_eq!((all[0].2, all[0].6), (Some(0), false), "the first ended once, with its status");
    assert_eq!((all[1].0, all[1].3, all[1].6), (2, Some(7), false), "the second is waiting at its prompt");
}

/// A prompt with no end reported still ends what was running — with no status, which is not a zero.
#[test]
fn a_prompt_ends_a_command_whose_end_was_never_reported() {
    let mut commands = Commands::<u32>::default();
    commands.output_started(Some("sleep 1".to_owned()), None, at(1));
    commands.prompt_started(at(2));
    let all = summary(&commands);
    assert_eq!((all[0].2, all[0].6), (None, false));
    assert_eq!(commands.get(1).and_then(|one| one.finished), Some(at(2)));
}

/// An empty line entered, or the prompt redrawn: nothing ran, so it is the same command still waiting,
/// now at the row the prompt moved to.
#[test]
fn a_prompt_drawn_again_with_nothing_run_is_the_same_command_moved() {
    let mut commands = Commands::<u32>::default();
    commands.prompt_started(at(1));
    commands.place(|_| Some(3));
    commands.prompt_started(at(2));
    commands.place(|_| Some(4));
    assert_eq!(summary(&commands), vec![(1, None, None, Some(4), None, None, false)]);
}

#[test]
fn an_end_with_nothing_running_is_not_a_command() {
    let mut commands = Commands::<u32>::default();
    commands.ended(Some(1), at(1));
    assert_eq!(summary(&commands), vec![]);
    assert_eq!(commands.generation(), 0, "nothing happened, so nothing watching is told to look");
}

/// The generation is what a view watches: it moves for every change and for nothing else.
#[test]
fn the_generation_moves_with_every_change() {
    let mut commands = Commands::<u32>::default();
    let mut seen = vec![commands.generation()];
    commands.prompt_started(at(1));
    seen.push(commands.generation());
    commands.output_started(None, None, at(1));
    seen.push(commands.generation());
    commands.ended(None, at(1));
    seen.push(commands.generation());
    commands.ended(None, at(1));
    seen.push(commands.generation());
    assert_eq!(seen, vec![0, 1, 2, 3, 3]);
}

/// Each moment gets a row of its own, in the order the moments happened, and a row that cannot be marked
/// is absent rather than borrowed from another.
#[test]
fn moments_are_given_rows_in_the_order_they_happened() {
    let mut commands = Commands::<u32>::default();
    commands.prompt_started(at(1));
    commands.output_started(None, None, at(1));
    commands.ended(None, at(1));
    assert!(commands.moments_waiting());
    let mut rows = [Some(1), None, Some(3)].into_iter();
    commands.place(|_| rows.next().flatten());
    assert!(!commands.moments_waiting());
    assert_eq!(summary(&commands), vec![(1, None, None, Some(1), None, Some(3), false)]);
}

#[test]
fn only_the_newest_commands_are_kept_and_an_id_is_never_reused() {
    let mut commands = Commands::<u32>::default();
    for _ in 0..COMMANDS_KEPT + 5 {
        commands.output_started(None, None, at(1));
        commands.ended(Some(0), at(1));
    }
    let ids: Vec<u64> = commands.iter().map(|one| one.id).collect();
    assert_eq!(ids.len(), COMMANDS_KEPT);
    assert_eq!((ids[0], *ids.last().expect("commands are kept")), (6, (COMMANDS_KEPT + 5) as u64));
    assert!(commands.get(1).is_none());
}
