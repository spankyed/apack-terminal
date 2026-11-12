//! What is queued for a session's program: how much may wait, why more is refused — said so that a
//! session whose pty went wrong is not taken for one that is over — and when the pty's writer is let go of.

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use apack_terminal::writer::{INPUT_BYTES, Input, Refused};

/// How long a wait for another thread goes on with nothing arriving. **Shorter than the runner's deadline for a
/// case** (`.config/nextest.toml`), so a wait that runs out fails saying what it was waiting for, where
/// one as long as the deadline is ended by the runner saying nothing.
const PATIENCE: Duration = Duration::from_secs(15);

/// A pty that takes nothing.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pty whose program is not reading: a write to it waits until the case has let go of the other end.
struct Stalled(Receiver<()>);

impl Write for Stalled {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pty's writer that takes everything, and says when it is let go of.
struct LetGo(Sender<()>);

impl Write for LetGo {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LetGo {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Waits for an input's thread to have ended. The thread holds the input for as long as it runs, so the
/// case's being the one holder left is the thread gone.
fn written_out(input: &Arc<Input>) {
    let waiting_since = Instant::now();
    while Arc::strong_count(input) > 1 {
        assert!(waiting_since.elapsed() < PATIENCE, "an input that was closed went on being written");
        std::thread::yield_now();
    }
}

/// **A pty that refused a write is not a session that ended.** Both take no more, and whoever sends is
/// told which: a view told "no such session" forgets the session, and this one is still there.
#[test]
fn input_refused_by_the_pty_is_told_apart_from_input_for_a_session_that_ended() {
    let broken = Input::start(Box::new(Refusing));
    // The first is queued, and written by the input's own thread; what is sent once that write has
    // failed is refused, however long the thread takes to get to it
    let waiting_since = Instant::now();
    let refused = loop {
        if let Err(refused) = broken.send(b"typed".to_vec()) {
            break refused;
        }
        assert!(waiting_since.elapsed() < PATIENCE, "a pty that takes nothing went on being written to");
        std::thread::yield_now();
    };
    assert_eq!(refused, Refused::Broken);

    let ended = Input::start(Box::new(std::io::sink()));
    ended.close();
    assert_eq!(ended.send(b"typed".to_vec()), Err(Refused::Closed));
}

/// **What may wait for a program that is not reading is `INPUT_BYTES`, to the byte.** Exactly that much is
/// taken, what the thread has begun writing counted with what it has not; a byte more is refused, and
/// said to be refused for that. Once the program reads, there is room again.
#[test]
fn input_is_taken_up_to_the_most_that_may_wait_and_refused_a_byte_past_it() {
    let (reading, stalled) = channel();
    let input = Input::start(Box::new(Stalled(stalled)));
    assert_eq!(input.send(vec![b'p'; INPUT_BYTES / 2]), Ok(()));
    assert_eq!(input.send(vec![b'p'; INPUT_BYTES / 2]), Ok(()), "as much as may wait was refused");
    assert_eq!(input.send(b"x".to_vec()), Err(Refused::Full));

    drop(reading);
    let waiting_since = Instant::now();
    while input.send(b"x".to_vec()) == Err(Refused::Full) {
        assert!(waiting_since.elapsed() < PATIENCE, "a program that read everything left no room for more");
        std::thread::yield_now();
    }
}

/// **Closing a session's input does not let go of its pty.** Letting go of a pty's writer types an end
/// of input into it, which a shell with a line half typed takes as the line being entered; so the writer
/// is put down when the input is closed, and let go of when the program is said to be gone.
#[test]
fn the_ptys_writer_is_kept_when_input_is_closed_and_let_go_of_once_the_program_is_gone() {
    let (let_go, gone) = channel();
    let input = Input::start(Box::new(LetGo(let_go)));
    input.close();
    written_out(&input);
    assert!(gone.try_recv().is_err(), "closing the input let go of the pty, with the program still there to read it");

    input.release();
    gone.recv_timeout(PATIENCE).expect("the pty's writer is let go of once the program is gone");
}

/// The same when the program is gone before the thread writing to it has stopped: the thread still has
/// the writer then, and lets go of it itself when it does stop.
#[test]
fn a_program_gone_before_its_input_is_closed_has_its_pty_let_go_of_at_the_close() {
    let (let_go, gone) = channel();
    let input = Input::start(Box::new(LetGo(let_go)));
    input.release();
    assert!(gone.try_recv().is_err(), "the pty was let go of from under the thread that writes to it");

    input.close();
    gone.recv_timeout(PATIENCE).expect("the pty's writer is let go of when the thread that held it stops");
}
