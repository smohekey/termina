// CREDIT: This one is shared between crossterm and termwiz but is mostly termwiz.
// Termwiz: <https://github.com/wezterm/wezterm/blame/a87358516004a652ad840bc1661bdf65ffc89b43/termwiz/src/terminal/windows.rs#L810-L853>
// Crossterm: <https://github.com/crossterm-rs/crossterm/blob/36d95b26a26e64b0f8c12edfe11f410a6d56a812/src/event/source/windows.rs>
// Also see the necessary methods on the handle from the terminal module and the credit comment
// there.
use std::{
    io,
    os::windows::prelude::*,
    ptr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::System::Threading;

use crate::{event::Event, parse::Parser, terminal::InputHandle, windows::InputReaderMode};

use super::{EventSource, PollTimeout};

#[derive(Debug)]
pub struct WindowsEventSource {
    input: InputHandle,
    parser: Parser,
    waker: Arc<EventHandle>,
    /// How long a lone Escape waits for the rest of a key, in milliseconds. Shared with the
    /// terminal, which sets it.
    escape_wait: Arc<AtomicU32>,
    /// When the lone Escape now pending started waiting.
    escape_since: Option<Instant>,
}

impl WindowsEventSource {
    pub(crate) fn new(
        input: InputHandle,
        mode: InputReaderMode,
        escape_wait: Arc<AtomicU32>,
    ) -> io::Result<Self> {
        Ok(Self {
            input,
            parser: Parser::with_mode(mode),
            waker: Arc::new(EventHandle::new()?),
            escape_wait,
            escape_since: None,
        })
    }

    fn escape_wait(&self) -> Duration {
        Duration::from_millis(self.escape_wait.load(Ordering::Relaxed).into())
    }
}

impl EventSource for WindowsEventSource {
    fn waker(&self) -> WindowsWaker {
        WindowsWaker {
            handle: self.waker.clone(),
        }
    }

    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
        use Threading::{WaitForMultipleObjects, INFINITE};

        let timeout = PollTimeout::new(timeout);

        loop {
            if let Some(event) = self.parser.pop() {
                return Ok(Some(event));
            }

            // A lone Escape waits for the rest of a key until its deadline, then it's the Escape key.
            // That holds even if more input is waiting by now: it wasn't read until after the
            // deadline, so it can't be told apart from input that came too late.
            let escape_deadline = self.escape_since.map(|since| since + self.escape_wait());
            if escape_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.parser.parse(&[], false);
                self.escape_since = None;
                continue;
            }
            if !self.input.has_pending_input_events()? {
                let mut handles = [self.input.as_raw_handle(), self.waker.as_raw_handle()];
                let escape_left = escape_deadline
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()));
                let wait = match (timeout.leftover(), escape_left) {
                    (Some(leftover), Some(escape_left)) => Some(leftover.min(escape_left)),
                    (leftover, escape_left) => leftover.or(escape_left),
                };
                // Round up, so a wait shorter than a millisecond doesn't return at once.
                let wait = wait
                    .map(|wait| {
                        ((wait.as_micros() + 999) / 1000)
                            .try_into()
                            .unwrap_or(INFINITE - 1)
                    })
                    .unwrap_or(INFINITE);
                let result = unsafe {
                    WaitForMultipleObjects(handles.len() as u32, handles.as_mut_ptr(), 0, wait)
                };

                if result == WAIT_OBJECT_0 {
                    // The input handle is signaled: there is input ready to be read. Fall through
                    // to `read_console_input` below.
                } else if result == WAIT_OBJECT_0 + 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Poll operation was woken up",
                    ));
                } else if result == WAIT_FAILED {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "failed to poll input handles: {}",
                            io::Error::last_os_error()
                        ),
                    ));
                } else {
                    // `WAIT_TIMEOUT` (or an abandoned handle): no event arrived within the timeout.
                    if escape_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        // Nothing followed the Escape in time; the check above reports it.
                        continue;
                    }
                    return Ok(None);
                }
            }

            let hold_escape = !self.escape_wait().is_zero();
            let records = self.input.read_console_input()?;

            let took_input = self.parser.decode_input_records(records, hold_escape);
            self.escape_since = match (self.parser.escape_pending(), took_input) {
                (false, _) => None,
                // An Escape pending after new input is a new one, with a wait of its own.
                (true, true) => Some(Instant::now()),
                // The same Escape, still waiting.
                (true, false) => self.escape_since.or_else(|| Some(Instant::now())),
            };

            // Decoding the records may have produced an event (a key press, a resize, a parsed VT
            // sequence). Return it before honoring a zero timeout, otherwise a non-blocking poll
            // would discard input it just read.
            if let Some(event) = self.parser.pop() {
                return Ok(Some(event));
            }

            if timeout.leftover().is_some_and(|t| t.is_zero()) {
                break;
            }
        }

        Ok(None)
    }
}

#[derive(Debug)]
struct EventHandle {
    handle: OwnedHandle,
}

impl EventHandle {
    fn new() -> io::Result<Self> {
        let handle = unsafe { Threading::CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            Ok(Self { handle })
        }
    }
}

impl AsRawHandle for EventHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

/// A handle that can unblock a pending [`EventReader::poll`](crate::EventReader::poll) call
/// from another thread.
///
/// Cloning this type is cheap. All clones wake the same underlying reader.
#[derive(Debug, Clone)]
pub struct WindowsWaker {
    handle: Arc<EventHandle>,
}

impl WindowsWaker {
    /// Unblocks a pending [`EventReader::poll`](crate::EventReader::poll) call.
    pub fn wake(&self) -> io::Result<()> {
        if unsafe { Threading::SetEvent(self.handle.as_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
