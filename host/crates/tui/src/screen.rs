//! What a frame writes to the terminal (ARCHITECTURE.md §5.8).

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// crossterm's backend, with two changes for frames that follow each other quickly.
///
/// A frame that changes cells goes out as one synchronized update (mode 2026): a terminal that
/// knows the mode shows the frame whole, never half-drawn, and the others ignore the two
/// sequences. And a frame that changes nothing writes nothing: crossterm's backend resets the
/// colors and hides the cursor again on every frame (25 bytes), which would wake the terminal
/// at the frame rate while the picture stands still.
pub(crate) struct Screen<W: Write> {
    inner: CrosstermBackend<W>,
    /// In a synchronized update, which `flush` ends.
    updating: bool,
    /// What the terminal was last told about the cursor (`None`: not known).
    cursor_shown: Option<bool>,
    cursor_at: Option<Position>,
}

impl<W: Write> Screen<W> {
    pub(crate) fn new(writer: W) -> Self {
        Screen { inner: CrosstermBackend::new(writer), updating: false, cursor_shown: None, cursor_at: None }
    }

    fn begin(&mut self) -> io::Result<()> {
        if !self.updating {
            queue!(self.inner, BeginSynchronizedUpdate)?;
            self.updating = true;
        }
        Ok(())
    }
}

impl<W: Write> Backend for Screen<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.begin()?;
        // printing moves the cursor
        self.cursor_at = None;
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.cursor_at = None;
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        if self.cursor_shown != Some(false) {
            queue!(self.inner, Hide)?;
            self.cursor_shown = Some(false);
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if self.cursor_shown != Some(true) {
            queue!(self.inner, Show)?;
            self.cursor_shown = Some(true);
        }
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        if self.cursor_at != Some(position) {
            queue!(self.inner, MoveTo(position.x, position.y))?;
            self.cursor_at = Some(position);
        }
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    /// Clearing (ratatui does it after a resize, before it draws the frame) opens the update
    /// too, so the empty screen is never shown; the frame's `flush` ends it.
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.begin()?;
        self.cursor_at = None;
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.updating {
            queue!(self.inner, EndSynchronizedUpdate)?;
            self.updating = false;
        }
        Backend::flush(&mut self.inner)
    }
}

// Without a console that takes escape codes, crossterm on Windows calls the console instead of
// writing bytes, so the bytes are checked on the other systems.
#[cfg(all(test, not(windows)))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::layout::Rect;
    use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

    use super::*;

    const BEGIN: &str = "\x1b[?2026h";
    const END: &str = "\x1b[?2026l";
    const HIDE: &str = "\x1b[?25l";
    const SHOW: &str = "\x1b[?25h";

    /// The bytes written, readable while the terminal owns the writer.
    #[derive(Clone, Default)]
    struct Written(Rc<RefCell<Vec<u8>>>);

    impl Write for Written {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Written {
        /// What was written since the last call.
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.borrow_mut())).expect("utf-8")
        }
    }

    fn terminal() -> (Terminal<Screen<Written>>, Written) {
        let out = Written::default();
        // a fixed viewport, so the size of the real terminal is never asked for
        let options = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 20, 3)) };
        (Terminal::with_options(Screen::new(out.clone()), options).expect("terminal"), out)
    }

    fn text(f: &mut Frame, s: &str) {
        f.buffer_mut().set_string(0, 0, s, ratatui::style::Style::default());
    }

    #[test]
    fn a_frame_is_one_synchronized_update() {
        let (mut term, out) = terminal();
        term.draw(|f| text(f, "hello")).unwrap();
        let first = out.take();
        assert!(first.starts_with(BEGIN) && first.ends_with(END), "{first:?}");
        assert!(first.contains("hello") && first.contains(HIDE), "{first:?}");
        assert_eq!((first.matches(BEGIN).count(), first.matches(END).count()), (1, 1), "{first:?}");

        term.draw(|f| text(f, "hellp")).unwrap();
        let second = out.take();
        assert!(second.starts_with(BEGIN) && second.ends_with(END) && second.contains('p'), "{second:?}");
        assert!(!second.contains(HIDE), "the cursor is hidden already: {second:?}");
    }

    #[test]
    fn a_frame_that_changes_nothing_writes_nothing() {
        let (mut term, out) = terminal();
        term.draw(|f| text(f, "hello")).unwrap();
        out.take();
        for _ in 0..3 {
            term.draw(|f| text(f, "hello")).unwrap();
            assert_eq!(out.take(), "");
        }
    }

    #[test]
    fn the_cursor_is_placed_again_only_when_it_moved_or_cells_changed() {
        let (mut term, out) = terminal();
        let frame = |f: &mut Frame, s: &str, x: u16| {
            text(f, s);
            f.set_cursor_position((x, 0));
        };
        term.draw(|f| frame(f, "ab", 2)).unwrap();
        let first = out.take();
        assert!(first.ends_with(&format!("{SHOW}\x1b[1;3H{END}")), "{first:?}");

        // nothing changed: nothing written
        term.draw(|f| frame(f, "ab", 2)).unwrap();
        assert_eq!(out.take(), "");

        // only the cursor moved: no update to synchronize
        term.draw(|f| frame(f, "ab", 1)).unwrap();
        assert_eq!(out.take(), "\x1b[1;2H");

        // cells changed: printing moved the cursor, so it is put back
        term.draw(|f| frame(f, "xb", 1)).unwrap();
        let changed = out.take();
        assert!(changed.starts_with(BEGIN) && changed.ends_with(&format!("\x1b[1;2H{END}")), "{changed:?}");
        assert!(!changed.contains(SHOW), "the cursor is shown already: {changed:?}");

        // back to no cursor
        term.draw(|f| text(f, "xb")).unwrap();
        assert_eq!(out.take(), HIDE);
    }

    #[test]
    fn clearing_is_part_of_the_next_frame() {
        let (mut term, out) = terminal();
        term.draw(|f| text(f, "hello")).unwrap();
        out.take();
        term.backend_mut().clear().unwrap();
        let cleared = out.take();
        assert!(cleared.starts_with(BEGIN) && !cleared.contains(END), "{cleared:?}");
        Backend::flush(term.backend_mut()).unwrap();
        assert_eq!(out.take(), END);
    }
}
