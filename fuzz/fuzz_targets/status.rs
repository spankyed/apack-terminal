//! Reports and endings in any order, against what a session's status records must always be true of.
//!
//! Each four bytes are one step: a report on one of a few hundred records, a clear, or the program's
//! command ending. The core holds a report to the specification before the store sees it, so what is
//! fuzzed here is the keeping: the cap, what an ending leaves, and what a client is told.
#![no_main]

use std::time::SystemTime;

use apack_terminal::status::{CAP, Kind, Records, Report, State};
use libfuzzer_sys::fuzz_target;

const STATES: [State; 5] = [State::Idle, State::Working, State::Blocked, State::Done, State::Error];
const KINDS: [Option<Kind>; 4] = [None, Some(Kind::Permission), Some(Kind::Question), Some(Kind::Auth)];

/// One of about three hundred records: the program itself, a few with records beneath them, and enough
/// others to pass the cap.
fn id(pick: u16) -> String {
    match pick % 320 {
        0 => String::new(),
        pick @ 1..=8 => format!("a/{}", "b/".repeat(usize::from(pick) - 1)).trim_end_matches('/').to_owned(),
        pick => format!("r{pick}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut records = Records::default();
    let now = SystemTime::now();
    for step in data.chunks_exact(4) {
        let pick = u16::from_le_bytes([step[1], step[2]]);
        match step[0] % 8 {
            0 => records.ended(),
            1 => records.apply(
                Report { state: None, kind: None, progress: None, id: id(pick), app: None, title: None, message: None },
                now,
            ),
            op => records.apply(
                Report {
                    state: Some(STATES[usize::from(step[3]) % STATES.len()]),
                    kind: KINDS[usize::from(step[3] >> 3) % KINDS.len()],
                    progress: (step[3] & 1 == 1).then_some(step[3] % 101),
                    id: id(pick),
                    app: (op % 2 == 0).then(|| "app".to_owned()),
                    title: None,
                    message: None,
                },
                now,
            ),
        }
        if step[0] % 8 == 0 {
            assert!(records.told().iter().all(|record| matches!(record.state, State::Done | State::Error)));
        }
        let told = records.told();
        assert!(told.len() <= CAP);
        assert!(told.windows(2).all(|pair| pair[0].id < pair[1].id), "told by id, each once");
        // Every change took a count of its own
        let mut counts: Vec<u32> = told.iter().map(|record| record.changed).collect();
        counts.sort_unstable();
        counts.dedup();
        assert_eq!(counts.len(), told.len());
        records.confirm(None);
    }
});
