# netinspect: Rust crate and API research (verified 2026-09-29)

**How this was checked.** Every version, release date and MSRV below comes from the crates.io API (`/api/v1/crates/<name>` and `/<version>`), fetched on 2026-09-29. APIs were read from the published crate tarballs, downloaded from `https://crates.io/api/v1/crates/<name>/<version>/download` into `scratchpad/tmp-crates/src-dl/`. Some claims were also checked against docs.rs and GitHub (`gh api`).

Rust is **not installed** here, so none of the code below has been compiled. Snippets are either quoted from upstream docs/examples or built from signatures read in the source.

**UNVERIFIED** marks anything I could not confirm from a primary source during this task. **Inferred from source** means I read the code but did not run it.

---

## 1. Toolchain

| Item | Value | Source |
|---|---|---|
| Current stable Rust | **1.98.1**, released 2026-09-03 (1.98.0 was 2026-08-20) | `https://static.rust-lang.org/dist/channel-rust-stable.toml` (`date = "2026-09-03"`, `pkg.rust version = "1.98.1 (48a229cea 2026-09-01)"`); `gh api repos/rust-lang/rust/releases/latest` returned tag `1.98.1`; https://blog.rust-lang.org/releases/latest/ redirects to `/2026/09/03/Rust-1.98.1/` |
| Minimum Rust for edition 2024 | **1.85.0**, released 2025-02-20 | https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/, which says: *"We are excited to announce that the Rust 2024 Edition is now stable!"*; the edition guide (https://doc.rust-lang.org/edition-guide/rust-2024/index.html) lists "Release version: 1.85.0" |
| Next stable | 1.99.0, expected around 2026-10-01 on the 6-week cadence (**UNVERIFIED**, this is a projection) | — |

**Effective MSRV of the proposed dependency set: 1.90.** This is derived from crates.io `rust_version` metadata and has not been compile-tested.
- ratatui 0.30.2 and its sub-crates need 1.88.0.
- image 0.25.10, globset 0.4.20, time 0.3.55 (transitive) and instability 0.3.14 (transitive) also need 1.88.
- The floor rises to **1.90** because ratatui-image 11.1.0 requires `icy_sixel ^0.5.0`, which resolves to 0.5.1 and requires `quantette ^0.6.0`. Every quantette version from 0.4.0 onward declares `rust_version = 1.90`. ratatui-image itself only declares 1.86.0, so its declared MSRV is optimistic.
- Recommendation: set `edition = "2024"` and `rust-version = "1.90"`, and pin the toolchain with `rust-toolchain.toml` → `channel = "1.98.1"`. Edition 2024 packages use Cargo resolver "3", which is MSRV-aware.

---

## 2. Ratatui

### 2.1 Versions and crate split

| Crate | Latest | Released | MSRV | Role |
|---|---|---|---|---|
| `ratatui` | **0.30.2** | 2026-06-19 | 1.88.0 (ed. 2024) | Facade crate apps should use |
| `ratatui-core` | 0.1.2 | 2026-06-19 | 1.88.0 | Traits (`Widget`, `StatefulWidget`, `Backend`), `Terminal`, `Frame`, buffer, layout, style, text, symbols, `TestBackend` |
| `ratatui-widgets` | 0.3.2 | 2026-06-19 | 1.88.0 | Built-in widgets |
| `ratatui-crossterm` | 0.1.2 | 2026-06-19 | 1.88.0 | `CrosstermBackend` |
| `ratatui-macros` | 0.7.2 | 2026-06-19 | – | Macros |

Also published: `ratatui-termion`, `ratatui-termwiz` and `ratatui-termina`.

The split happened in 0.30.0 (2025-12-26). The crate docs say: *"Starting with Ratatui 0.30.0, the project was reorganized into a modular workspace … Most applications should continue using this main `ratatui` crate, which re-exports everything for convenience."*

The GitHub `main` branch already has a **v0.31.0** section in `BREAKING-CHANGES.md` ("`Backend` adds cursor save and restore methods"), so 0.31 is being developed but is not on crates.io.
Sources: https://github.com/ratatui/ratatui/blob/main/BREAKING-CHANGES.md, https://github.com/ratatui/ratatui/blob/main/CHANGELOG.md

**Facade re-exports** (from `ratatui-0.30.2/src/lib.rs`):
- `pub use ratatui_core::terminal::{CompletedFrame, Frame, Terminal, TerminalOptions, Viewport};`
- `pub use ratatui_core::{buffer, layout};` and `pub use ratatui_core::{style, symbols, text};`
- `pub mod widgets` re-exports `ratatui_core::widgets::{StatefulWidget, Widget}` and all ratatui-widgets widgets: `Axis, Chart, Dataset, GraphType, LegendPosition`, `Cell, HighlightSpacing, Row, Table, TableState`, `Scrollbar…`, `Paragraph, Wrap`, `List…`, `Sparkline…`, `Tabs`, `Block…`, `canvas`, `calendar`. It also re-exports the unstable `WidgetRef` and `StatefulWidgetRef`.
- `pub use ratatui_widgets::border;`
- `pub mod backend { pub use ratatui_core::backend::{Backend, ClearType, TestBackend, WindowSize}; pub use ratatui_crossterm::{CrosstermBackend, FromCrossterm, IntoCrossterm}; … }`
- `pub use ratatui_crossterm::crossterm;`, which re-exports the crossterm crate ratatui was built with.
- `pub use crate::init::{DefaultTerminal, init, init_with_options, restore, run, try_init, try_init_with_options, try_restore};`, plus `macros` (ratatui-macros) and `palette` (feature-gated).

**Features.** `default = ["all-widgets", "crossterm", "layout-cache", "macros", "underline-color"]`. Other features: `crossterm_0_28`, `crossterm_0_29`, `termion`, `termwiz`, `termina`, `serde`, `palette`, `scrolling-regions`, `portable-atomic` and `unstable-*`.

BREAKING-CHANGES warns: *"Disabling `default-features` will now disable layout cache, which can have a negative impact on performance."*

### 2.2 Which crossterm, and how the version is selected

- ratatui has features `crossterm_0_28 = ["crossterm", "ratatui-crossterm/crossterm_0_28"]` and `crossterm_0_29 = [...]`.
- ratatui-crossterm 0.1.2 has `default = ["crossterm_0_29", "underline-color"]`. It depends on `crossterm_0_28 = { package = "crossterm", version = "0.28" }` and `crossterm_0_29 = { package = "crossterm", version = "0.29" }`, both optional.
- The re-export uses `cfg_if`, and the crate docs state: *"The highest enabled feature flag of the available `crossterm_0_xx` features … takes precedence."* The re-export is `pub use crossterm_0_29 as crossterm;`.
- ratatui 0.30.2 depends on `ratatui-crossterm` **without** `default-features = false`. That means `crossterm_0_29` is always on when the `crossterm` feature is on. **Inferred from the manifests:** turning on `crossterm_0_28` would compile both 0.28 and 0.29, and 0.29 would still win.
- **In practice ratatui 0.30.2 uses crossterm 0.29.0.**

### 2.3 MSRV

1.88.0. This was raised in 0.30.1; 0.30.0 required 1.86.0.

### 2.4 Chart, Dataset, Axis (ratatui-widgets 0.3.2 `src/chart.rs`)

```rust
// Chart
pub fn new(datasets: Vec<Dataset<'a>>) -> Self
pub fn block(mut self, block: Block<'a>) -> Self
pub fn style<S: Into<Style>>(mut self, style: S) -> Self
pub fn x_axis(mut self, axis: Axis<'a>) -> Self
pub fn y_axis(mut self, axis: Axis<'a>) -> Self
pub const fn hidden_legend_constraints(mut self, constraints: (Constraint, Constraint)) -> Self
pub const fn legend_position(mut self, position: Option<LegendPosition>) -> Self

// Dataset<'a> { name: Option<Line<'a>>, data: &'a [(f64, f64)], marker, graph_type, style, fill_to_y }
pub fn name<S>(mut self, name: S) -> Self where S: Into<Line<'a>>
pub const fn data(mut self, data: &'a [(f64, f64)]) -> Self
pub const fn marker(mut self, marker: symbols::Marker) -> Self
pub const fn graph_type(mut self, graph_type: GraphType) -> Self
pub fn style<S: Into<Style>>(mut self, style: S) -> Self
pub const fn fill_to_y(mut self, fill_to_y: f64) -> Self        // used with GraphType::Area

pub enum GraphType { #[default] Scatter, Line, Bar, Area }

// ratatui-core symbols::Marker (#[non_exhaustive])
// Dot (default), Block, Bar, Braille, HalfBlock, Quadrant, Sextant, Octant, Custom(char)

// Axis<'a> { title, bounds: [f64; 2], labels: Vec<Line<'a>>, style, labels_alignment }
pub fn title<T>(mut self, title: T) -> Self where T: Into<Line<'a>>
pub const fn bounds(mut self, bounds: [f64; 2]) -> Self
pub fn labels<Labels>(mut self, labels: Labels) -> Self
    where Labels: IntoIterator, Labels::Item: Into<Line<'a>>
pub fn style<S: Into<Style>>(mut self, style: S) -> Self
pub const fn labels_alignment(mut self, alignment: Alignment) -> Self
```

Upstream doc example, quoted:

```rust
Dataset::default()
    .name("data2")
    .marker(symbols::Marker::Braille)
    .graph_type(GraphType::Line)
    .style(Style::default().magenta())
    .data(&[(4.0, 5.0), (5.0, 8.0), (7.66, 13.5)]),
```

Things to know about Chart:
- The `Axis::labels` docs say *"you need to give at least two labels for them to be rendered. Also, giving more than 3 labels is currently broken and the middle labels won't be in the correct position, see issue 334."* Keep to 2–3 labels.
- `GraphType::Area` together with `Dataset::fill_to_y` was added in the 0.30 line. It gives the filled-area look of Android Studio's chart.
- Render cost: `Chart` paints through `Canvas` and runs `for data in dataset.data.windows(2)` on every frame. The cost is O(points), so **downsample to about 2 points per column**, because Braille gives 2×4 dots per cell.

### 2.5 Table, Row, Cell, TableState: does rendering touch every row?

Key API:

```rust
pub fn new<R, C>(rows: R, widths: C) -> Self
where R: IntoIterator, R::Item: Into<Row<'a>>, C: IntoIterator, C::Item: Into<Constraint>
.header(Row) .footer(Row) .widths(..) .column_spacing(u16) .block(Block) .style(..)
.row_highlight_style(..) .column_highlight_style(..) .cell_highlight_style(..)
.highlight_symbol(impl Into<Text>) .highlight_spacing(HighlightSpacing) .flex(Flex)
// highlight_style() is deprecated: "use `row_highlight_style()` instead"

Row::new(cells) .height(u16) .top_margin(u16) .bottom_margin(u16) .style(..)
Cell::new(content) / Cell::from(..) .style(..) .column_span(u16)   // column_span is new in 0.30

TableState::new() .with_offset(usize) .with_selected(impl Into<Option<usize>>)
  .offset() .offset_mut() .selected() .select(Option<usize>) .select_next() .select_previous()
  .select_first() .select_last() .scroll_down_by(u16) .scroll_up_by(u16)
  (+ column/cell variants)
```

Reading `impl StatefulWidget for &Table` in ratatui-widgets 0.3.2 `src/table.rs` shows what is proportional to the total number of rows (N) and what is not.

**Per-frame work that grows with N:**
- **Construction:** `Table::new` and `rows()` do `rows.into_iter().map(Into::into).collect()`. Every frame therefore allocates N `Row`s, each with a `Vec<Cell>`, each cell holding `Text`. **For 50,000 rows this is the dominant cost.**
- **`column_count()`:** runs `self.rows.iter().chain(self.footer.iter()).chain(self.header.iter()).map(|r| r.cells.len()).max()` on every render. This is O(N), although it is cheap.

**Work that does not grow with N:**
- There is **no content measurement**. Column widths come only from the `widths` constraints via `Layout::horizontal(widths).flex(..).spacing(..).split(..)`, and only the header/footer and visible rows are ever rendered.
- `visible_rows()` starts at `state.offset` using `self.rows.iter().skip(start)`, which is O(1) on a slice. It adds row heights until the area is full. If the selection lies below the window, it walks forward from the end to the selection, which is O(distance).
- `render_rows()` uses `.iter().enumerate().skip(start_index).take(end_index - start_index)`, so only visible rows are drawn.

**Conclusion:** pass `Table` only the visible slice. Keep your own `offset` and `selected` as `usize` indices into a filtered index vector, and render with a fresh `TableState::default().with_selected(sel - offset)`. `Row<'a>`/`Cell<'a>` can borrow `&'a str` from your store, so the visible rows cost no extra copies. Draw a separate `Scrollbar` whose `ScrollbarState` uses the total count.

`Paragraph` has the same problem for large bodies (from `src/paragraph.rs`):
- `scroll()` takes `(Vertical, Horizontal)` as `u16`, so it **cannot scroll past 65,535 lines**.
- With `Wrap`, it re-wraps every line from the top until the scroll offset on every frame.
- So body viewers (JSON tree, hex, diff) must also be virtualized.

### 2.6 Terminal and Frame (ratatui-core 0.1.2)

```rust
impl<B: Backend> Terminal<B> {
    pub fn new(backend: B) -> Result<Self, B::Error>
    pub fn with_options(mut backend: B, options: TerminalOptions) -> Result<Self, B::Error>
    pub fn draw<F>(&mut self, render_callback: F) -> Result<CompletedFrame<'_>, B::Error>
        where F: FnOnce(&mut Frame)
    pub fn try_draw<F, E>(&mut self, render_callback: F) -> Result<CompletedFrame<'_>, B::Error> …
    pub const fn backend(&self) -> &B        pub const fn backend_mut(&mut self) -> &mut B
}
impl Frame<'_> {
    pub const fn area(&self) -> Rect
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect)
    pub fn render_stateful_widget<W>(&mut self, widget: W, area: Rect, state: &mut W::State)
        where W: StatefulWidget
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P)
}
```

Since 0.30, `Backend` has an associated `Error` type. `CrosstermBackend` uses `io::Error`, and `TestBackend` uses `core::convert::Infallible`. Helpers from 0.30 used in upstream examples include `Rect::layout::<N>(self, &Layout) -> [Rect; N]` and crossterm's `Event::as_key_press_event()`.

### 2.7 `ratatui::init()`, `restore()` and `run()` (`ratatui-0.30.2/src/init.rs`)

- `pub type DefaultTerminal = Terminal<CrosstermBackend<Stdout>>;`
- `pub fn run<F, R>(f: F) -> R where F: FnOnce(&mut DefaultTerminal) -> R` is new in 0.30.0. It calls `init()`, then `f`, then `restore()`.
- `init()` and `try_init()` do three things: `set_panic_hook(); enable_raw_mode()?; execute!(stdout(), EnterAlternateScreen)?;`, then build `CrosstermBackend::new(stdout())`.
- `restore()` and `try_restore()` do `disable_raw_mode()?; execute!(stdout(), LeaveAlternateScreen)?;`. `restore()` prints errors to stderr and ignores them.
- The installed panic hook calls `restore()` and then the previous hook. The docs say: *"Call the initialization functions after installing any other panic hooks."*
- **init/restore do not enable mouse capture or bracketed paste.** You enable them yourself, as upstream `examples/apps/mouse-drawing` does:

  ```rust
  execute!(std::io::stdout(), EnableMouseCapture)?;
  … 
  execute!(std::io::stdout(), DisableMouseCapture)?;
  ```

  (https://github.com/ratatui/ratatui/blob/main/examples/apps/mouse-drawing/src/main.rs)

  Your own teardown and panic path must also disable them.

The upstream async pattern, from `examples/apps/async-github/src/main.rs` on main (it uses `tokio-stream`):

```rust
let period = Duration::from_secs_f32(1.0 / Self::FRAMES_PER_SECOND);
let mut interval = tokio::time::interval(period);
let mut events = EventStream::new();
while !self.should_quit {
    tokio::select! {
        _ = interval.tick() => { terminal.draw(|frame| self.render(frame))?; },
        Some(Ok(event)) = events.next() => self.handle_event(&event),
    }
}
```

The example's Cargo.toml has `crossterm = { workspace = true, features = ["event-stream"] }`, `tokio = { …, features = ["macros", "rt-multi-thread"] }` and `tokio-stream`.

### 2.8 TestBackend and snapshot tests with insta

`TestBackend` (ratatui-core `src/backend/test.rs`):
- API: `new(width, height)`, `with_lines(..)`, `buffer()`, `resize()`, `assert_buffer(..)`, `assert_buffer_lines(..)`, `assert_cursor_position(..)`, and `impl fmt::Display` (one quoted line per row).
- Its docs recommend unit-testing widgets against `Buffer` directly and using `TestBackend` for whole-UI integration tests.

Recommended pattern, quoted verbatim from the Ratatui website recipe (https://ratatui.rs/recipes/testing/snapshots/, source `ratatui/ratatui-website` `src/content/docs/recipes/testing/snapshots.md`):

```rust
#[cfg(test)]
mod tests {
    use super::App;
    use insta::assert_snapshot;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn test_render_app() {
        let app = App::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(&app, frame.area()))
            .unwrap();
        assert_snapshot!(terminal.backend());
    }
}
```

- The recipe notes: *"Asserting with color is not supported as of now."*
- Workaround, inferred from source: `Buffer`'s `Debug` impl prints a `styles: [ x, y, fg, bg, underline, modifier ]` list whenever the style changes. So `insta::assert_debug_snapshot!(terminal.backend().buffer())` does capture colors and modifiers.
- Review changes with `cargo insta review`. cargo-insta 1.48.0 is on crates.io.

### 2.9 Built-in scrollbar

Yes. `ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState}`:
- `Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(Some("↑")).end_symbol(Some("↓"))`
- `ScrollbarState::new(content_length: usize).position(usize).viewport_content_length(usize)`
- Rendered as a stateful widget: `frame.render_stateful_widget(scrollbar, area.inner(Margin{vertical:1,horizontal:0}), &mut state)`.

This is the upstream doc example. The 0.30.2 changelog adds a fix that keeps a large thumb inside the track.

---

## 3. crossterm

- **Latest: 0.29.0**, released 2025-04-05, MSRV 1.63.0, edition 2021. crates.io `max_version` is 0.29.0 and the GitHub releases are `0.29` and `0.28`.
- `master` has an **unreleased** section. It raises MSRV to 1.85, moves to edition 2024, removes `IsTty`, and fixes NO_COLOR color commands emitting a bare `CSI m` (see §Design, NO_COLOR).
  Source: https://github.com/crossterm-rs/crossterm/blob/master/CHANGELOG.md
- **Features (0.29.0):**
  - `default = ["bracketed-paste", "events", "windows", "derive-more"]`
  - `event-stream = ["dep:futures-core", "events"]`
  - `osc52 = ["dep:base64"]` (base64 0.22)
  - `serde`
  - `use-dev-tty` (unix; reads events from `/dev/tty`, which helps if stdin is piped)
- **`EventStream`**: *"A stream of `Result<Event>` … not available by default. You have to use the `event-stream` feature flag"*. It implements `futures_core::Stream` and is made with `EventStream::new()`. The official example is `examples/event-stream-tokio.rs` (run with `cargo run --features="event-stream" --example event-stream-tokio`). It uses `reader.next().fuse()` with `select!`.
- **Mouse**: `EnableMouseCapture`/`DisableMouseCapture`.
  - On unix, `EnableMouseCapture` writes `CSI ?1000h ?1002h ?1003h ?1015h ?1006h`. Mode 1003 is "any-event tracking" and reports **every motion**, so expect event floods.
  - On Windows it goes through the console API.
  - Events: `Event::Mouse(MouseEvent { kind, column, row, modifiers })`. `MouseEventKind` is `Down(MouseButton) | Up(..) | Drag(..) | Moved | ScrollDown | ScrollUp | ScrollLeft | ScrollRight`.
- **Bracketed paste**: feature `bracketed-paste` (on by default). `EnableBracketedPaste` writes `CSI ?2004h` and pastes arrive as `Event::Paste(String)`. On the legacy Windows API it returns `Unsupported`: *"Bracketed paste not implemented in the legacy Windows API."*
- **Also in 0.29:** `Event::is_key_press()`, `Event::as_key_press_event()` and `KeyEvent::is_press()` from the "is_*/as_* methods" change. There is also `crossterm::style::available_color_count()` (COLORTERM/TERM heuristics) and `force_color_output(bool)`.
- **OSC 52: yes.** The module is `crossterm::clipboard` behind `#[cfg(feature = "osc52")]`.
  - Types: `CopyToClipboard<T: AsRef<[u8]>> { content, destination: ClipboardSelection }` with `CopyToClipboard::to_clipboard_from(content)` and `::to_primary_from(content)`, plus `ClipboardType::{Clipboard, Primary, Other(char)}`.
  - It was **introduced in 0.29.0** (CHANGELOG "Version 0.29 → Added: Copy to clipboard using OSC52 (#974)").
  - It is **copy only**; there is no read or paste.
  - The encoding is `osc!("52;{destination};{base64}")`.
  - On Windows, `execute_winapi` returns `Unsupported ("Copying is not implemented for the Windows API.")`. The ANSI path is used whenever VT sequences are supported, as in Windows Terminal.
  - Its docs include a terminal matrix (xterm, Alacritty, WezTerm, Konsole, Kitty, foot, tmux 3.5a). The tmux entry requires `set-clipboard external`, which acts as OSC 52 pass-through.
  - Usage from the docs: `execute!(std::io::stdout(), CopyToClipboard::to_clipboard_from("foo"));`

  Sources: https://docs.rs/crossterm/0.29.0/crossterm/clipboard/struct.CopyToClipboard.html, crate `src/clipboard.rs`.

---

## 4. ratatui-image

- **Latest stable: 11.1.0.** The crate was updated 2026-09-17; the changelog dates the release 2026-09-16. MSRV 1.86.0, but the effective floor is 1.90 through quantette (see §1). The prerelease **12.0.0-rc.0** adds a probe for Kitty shared-memory transmission (`t=s`, `Capability::KittySharedMemory`).
- **Depends on `ratatui = "^0.30.1"` with `default-features = false`.** It shares ratatui with 0.30.2, and its `crossterm` feature maps to `ratatui/crossterm`.
- **Default features include the C library chafa.** `default = ["image-defaults", "crossterm", "chafa-dyn"]`.
  - `chafa-dyn` uses pkg-config at **build time**. `build.rs` panics with *"Failed to find chafa via pkg-config. Install libchafa-dev … Needs version >= 1.8.0."*
  - It also needs libchafa at **runtime**.
  - The docs say: *"If you absolutely don't want to deal with libchafa, then you should use `--no-default-features --features image-defaults,crossterm`."*
  - **For a single self-contained binary use `default-features = false, features = ["crossterm"]`** (add `"tokio"` if you want the tokio channel type).
- **Protocols:** `ProtocolType::{Halfblocks, Sixel, Kitty, Iterm2}`. Halfblocks is the fallback; without chafa it uses the built-in "primitive" half-block renderer.
- **Detection API** (`picker.rs`):
  - `Picker::from_query_stdio() -> Result<Picker>`: *"This writes and reads from stdio momentarily. WARNING: this method should be called after entering alternate screen but before reading terminal events."*
  - `Picker::from_query_stdio_with_options(QueryStdioOptions { timeout, text_sizing_protocol, terminal_background_color_osc, blacklist_protocols, kitty_compression })`. The **default timeout is 2000 ms**.
  - `Picker::halfblocks()`.
  - `Picker::from_fontsize()` is *deprecated since 9.0.0*.
  - Accessors: `protocol_type()`, `set_protocol_type()`, `font_size()`, `tmux_detected()` (new in 11.1.0), `set_background_color()`, `capabilities()`, `new_protocol(img, size, Resize)`, `new_resize_protocol(img)`.
- **tmux: supported.**
  - It detects tmux from `TERM` starting with `tmux` or `TERM_PROGRAM == "tmux"`.
  - **As a side effect it runs `tmux set -p allow-passthrough on`.**
  - It wraps every protocol and query in DCS passthrough (`"\x1bPtmux;"`, ESC doubled, `"\x1b\\"`).
  - It guesses the outer iTerm2 or WezTerm from `ITERM_SESSION_ID` and `WEZTERM_EXECUTABLE`; Kitty is detected by an in-band query.
  - The tmux version needed for `allow-passthrough` is 3.3 or later (**UNVERIFIED**; from memory).
- **Widgets:**
  - **`Image::new(&Protocol)`** is stateless and fixed-size. You build the protocol once with `picker.new_protocol(dyn_img, size, Resize::Fit(None))`, and rendering is then cheap. `allow_clipping(bool)` is available.
  - **`StatefulImage::default().resize(Resize::Crop(None))`** is rendered with `render_stateful_widget(.., &mut StatefulProtocol)` and resizes to the area at render time.
  - The docs warn: *"Do not use it without `thread::ThreadProtocol` in a reactive UI. Rendering the widget will block the UI thread."*
  - `thread::ThreadProtocol::new(tx, Some(proto))` + `ResizeRequest::resize_encode()` + `update_resized_protocol(..)`. `examples/tokio.rs` uses a tokio `unbounded_channel`.
  - There is also a `sliced` module.
- **Dependencies (11.1.0):**
  - `image ^0.25.6` with `default-features = false, features = ["png"]`. The `image-defaults` feature turns on `image/default`, which includes AVIF encoding via ravif/rav1e plus rayon.
  - `icy_sixel ^0.5.0`, described on crates.io as *"A 100% Rust SIXEL encoder and decoder library"*. It resolves to 0.5.1 → `quantette ^0.6` (pure Rust, MSRV 1.90).
  - `base64-simd 0.8`, `flate2 ^1.0` (Kitty compression), `rand ^0.8.5` (resolves to 0.8.8), `self_cell`, `thiserror 1.0.59` (the v1 line).
  - unix: `rustix ^0.38.4`, a second rustix alongside crossterm's 1.x. Windows: `windows 0.58`.
  - Optional: `tokio` (sync) and `serde`.
- **Compatibility matrix** (README): Kitty, Ghostty, WezTerm (iTerm2 protocol), iTerm2, foot, xterm (`-ti 340`), Rio and mlterm work. Alacritty, Konsole, Contour and Warp do not. Known issue #57: *"Sixel image rendered on the last line of terminal causes a scroll"*.

---

## 5. Text input

| Crate | Latest (date) | Depends on | Works with ratatui 0.30.2? |
|---|---|---|---|
| `tui-textarea` | **0.7.0** (2024-10-22; repo `rhysd/tui-textarea` last push 2024-12-01) | `ratatui 0.29.0`, `crossterm 0.28` | **No.** Different `Widget` trait crate and different crossterm `KeyEvent` type |
| **`ratatui-textarea`** | **0.9.2** (2026-06-12; MSRV 1.86; repo `ratatui/ratatui-textarea`, push 2026-07-26) | `ratatui-core ^0.1.1`, `ratatui-widgets ^0.3.1`, `ratatui-crossterm ^0.1.1` (uses `ratatui_crossterm::crossterm`, so crossterm 0.29) | **Yes.** README: *"This project is a Ratatui fork of tui-textarea and maintained independently."* Features: `crossterm` (default), `search` (regex), `serde`, `termion`, `termwiz` |
| `tui-textarea-2` | 0.13.2 (2026-08-23; MSRV 1.88; fork `srothgan/tui-textarea`) | `ratatui-core ^0.1.0`, `ratatui-widgets ^0.3.0`, `crossterm 0.29` | Yes (small community fork) |
| **`tui-input`** | **0.15.5** (2026-09-26) | `ratatui ^0.30.2` (features `crossterm`, default features on), crossterm 0.29 | **Yes.** Single-line: `Input`, `backend::crossterm::EventHandler::handle_event(&Event)`, `value()`, `visual_cursor()`, `visual_scroll(width)` |
| `edtui` | 0.11.7 (2026-08-16) | `ratatui-core 0.1`, `ratatui-widgets ^0.3.0`, `crossterm 0.29` | Yes. Vim/Emacs-style editor, but its default features pull `arboard` and `syntect = "5"` **with defaults, which means Oniguruma (C)**. Use `default-features = false` if you adopt it |

Recommendation:
- **tui-input** for the one-line filter, jq and search bars.
- **ratatui-textarea** for multi-line editing, such as the rules editor or composing a request body. Usage is `TextArea::default()`, `textarea.input(key)` (anything convertible `Into<Input>`, including crossterm `Event`/`KeyEvent`), `f.render_widget(&textarea, rect)` and `textarea.lines()`.

---

## 6. jaq (jq filters)

| Crate | Latest | Released | MSRV |
|---|---|---|---|
| `jaq-core` | **3.1.1** | 2026-08-28 | 1.69 |
| `jaq-std` | **3.0.3** | 2026-08-28 | 1.70 |
| `jaq-json` | **2.0.3** | 2026-08-28 | 1.70 |
| `jaq-all` (high-level "instant food" API) | 0.3.0 | 2026-08-05 | 1.70 |
| `jaq` (CLI) | 3.1.1 | 2026-08-05 | 1.70 |

The old 1.x split crates `jaq-interpret` 1.5.0, `jaq-parse` 1.0.3 and `jaq-syn` 1.6.0 date from 2024 and are superseded. Repo: https://github.com/01mf02/jaq (pushed 2026-09-28).

**Minimal compile and run**, quoted verbatim from the `jaq-core` 3.1.1 crate docs (https://docs.rs/jaq-core/3.1.1/jaq_core/):

```rust
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_core::load::{Arena, File, Loader};
use jaq_json::{read, Val};

let input = r#"["Hello", "world"]"#;
let input = read::parse_single(&input.as_bytes()).unwrap();
let program = File { code: ".[]", path: () };

// named filters, such as `keys`, `map`, ...
let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());

let loader = Loader::new(defs);
let arena = Arena::default();

// parse the filter
let modules = loader.load(&arena, program).unwrap();

// compile the filter
let filter = jaq_core::Compiler::default()
    .with_funs(funs)
    .compile(modules)
    .unwrap();

// context for filter execution
let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
// iterator over the output values
let mut out = filter.id.run((ctx, input)).map(unwrap_valr);

assert_eq!(out.next(), Some(Ok(Val::from("Hello".to_owned()))));;
assert_eq!(out.next(), Some(Ok(Val::from("world".to_owned()))));;
assert_eq!(out.next(), None);;
```

**Value type.** jaq uses its own `jaq_json::Val`, not `serde_json::Value`:
- Variants: `Null | Bool | Num(Num) | BStr | TStr | Arr(Rc<Vec<Val>>) | Obj(Rc<IndexMap<Val,Val>>)`.
- `Num = Int(isize) | BigInt | Float(f64) | Dec(Rc<String>)`, so big integers and decimal literals keep full precision, which serde_json does not do by default. Object key order is preserved.
- Input comes from `jaq_json::read::{parse_single, parse_many, read_many}`.
- Feature `serde` gives **only** `impl Deserialize for Val`, so `Val::deserialize(serde_json::Value)` works but there is no `Serialize`.
- Output uses `Display` (JSON text) or `jaq_json::write::write(w, &Pp { indent, sort_keys, styles, sep_space }, level, &v)`.
- Feature `sync` switches `Rc` to `Arc`; the crate has a `Send + Sync` test gated on `sync`.

**Error types** (jaq-core 3.1.1 source):
- **Load (lex/parse):** `jaq_core::load::Errors<S, P, E = load::Error<S>> = Vec<(File<S, P>, E)>`, where `load::Error<S> = Io(Vec<(S, String)>) | Lex(Vec<lex::Error<S>>) | Parse(Vec<parse::Error<S>>)`.
- **Compile:** `jaq_core::compile::Errors<S, P> = load::Errors<S, P, Vec<compile::Error<S>>>`, where `compile::Error<S> = (S, Undefined)` and `Undefined = Mod | Var | Label | Filter(Arity)` (non_exhaustive).
- **Positions:** `jaq_core::load::span(whole: &str, part: &str) -> Range<usize>` maps an error slice to a byte range, which is what you need to underline errors in the input bar.
- **Runtime:** the iterator yields `ValX<'a, V> = Result<V, Exn<'a, V>>`.
  - `unwrap_valr` turns it into `ValR<V> = Result<V, jaq_core::Error<V>>`, and `Error<V>` implements `Display`.
  - **Warning:** `unwrap_valr` *"will exit the current process if the value exception results from a call to the filter `halt`"* (`std::process::exit`).
  - In the TUI, call `exn.get_err()` and `exn.get_halt()` yourself instead.
- **JSON parse:** `jaq_json::read::Error` ("byte offset N: …"), which implements `std::error::Error`.
- `jaq-all 0.3.0` provides `compile_with(code, defs, funs, vars) -> Result<Filter, Vec<FileReports>>` and `load::FileReportsDisp` for pretty ANSI error reports.

**Features:**
- jaq-std: `default = ["std", "format", "log", "math", "regex", "time"]`. `time` uses jiff 0.2, `regex` uses regex-bites, and `format` uses base64 0.22, aho-corasick and urlencoding.
- jaq-json: `default = ["std"]`, plus `serde` and `sync`.

---

## 7. Syntax highlighting

- **syntect 5.3.0**, released 2025-09-27, no MSRV declared. `default = ["default-onig"]`, which links the **Oniguruma C library**.
  - The pure-Rust set is `default-fancy` (`regex-fancy` → `fancy-regex ^0.16.2`).
  - A leaner set works: `default-features = false, features = ["parsing", "default-syntaxes", "default-themes", "regex-fancy"]`. `default-syntaxes` needs only `parsing` and `dump-load`, not `yaml-load`.
  - Engine precedence: `#[cfg(feature = "regex-onig")]` wins over fancy (`all(feature = "regex-fancy", not(feature = "regex-onig"))`). **Any dependency that enables syntect's default features turns Oniguruma on.** Examples are edtui, syntect-tui, and two-face with its defaults.
  - The README says fancy-regex is *"about half the speed of the default Oniguruma engine"* and *"absurdly slow in debug mode"*.
- **Bundled syntaxes and themes.** I grepped `assets/default_newlines.packdump` and found JSON (`source.json`), XML (`text.xml`), HTML (`text.html.basic`), JavaScript, CSS, YAML, Markdown and Diff. TOML is **not** included; `two-face` adds it. Themes from `ThemeSet::load_defaults()` are `base16-ocean.dark/.light`, `base16-eighties.dark`, `base16-mocha.dark`, `InspiredGitHub` and `Solarized (dark)/(light)`.
- **API**, verbatim from `syntect::easy` docs:

  ```rust
  let ps = SyntaxSet::load_defaults_newlines();
  let ts = ThemeSet::load_defaults();
  let syntax = ps.find_syntax_by_extension("rs").unwrap();
  let mut h = HighlightLines::new(syntax, &ts.themes["base16-ocean.dark"]);
  for line in LinesWithEndings::from(s) {
      let ranges: Vec<(Style, &str)> = h.highlight_line(line, &ps).unwrap();
      …
  }
  ```

  - `highlight_line` returns `Result<Vec<(Style, &'b str)>, syntect::Error>`.
  - `Style { foreground: Color{r,g,b,a}, background, font_style: FontStyle(BOLD|UNDERLINE|ITALIC) }`.
  - `HighlightLines::from_state(..)` resumes from cached parse and highlight states, which enables virtualized highlighting.
  - Finders: `find_syntax_by_name`, `…_by_extension`, `…_by_token`, `…_by_first_line`, `find_syntax_plain_text`.
- **Ratatui adapter:** `syntect-tui` 3.0.6 (2025-05-09) depends on **`ratatui 0.29.0`**, and its repo `chanq-io/syntect-tui` has had no push since 2025-05-09. Its `deep-defaults` default feature enables `syntect/default-onig`. **It is not usable with ratatui 0.30.** A crates.io search turned up no maintained replacement. Write the ~25-line mapping yourself: `Color::Rgb(fg.r, fg.g, fg.b)` plus `Modifier::{BOLD, ITALIC, UNDERLINED}`, wrapped in `Span::styled`.
- `two-face` 0.5.2+bat-0.26.1 (2026-08-07, MSRV 1.79) adds bat's syntaxes (about 960 KB). Its default is `syntect-onig`, so use `features = ["syntect-fancy"]` with `default-features = false` if you want it.
- **Opinion:** for **JSON**, a hand-written tokenizer is simpler and better.
  - You need a JSON parser anyway for the foldable tree and the jq bridge.
  - A single O(n) pass produces the spans, fold ranges and search hits together.
  - It is fast on multi-MB bodies, and it avoids fancy-regex's debug-mode slowness.
  - Use syntect (fancy) only for XML, HTML, JS and CSS. Highlight only the visible window and checkpoint parse state every N lines.

---

## 8. Compression

| Crate | Latest | Released | MSRV | Notes |
|---|---|---|---|---|
| `flate2` | **1.1.10** | 2026-08-28 | 1.67.0 | `default = ["rust_backend", "runtime_detection"]`; `rust_backend` is miniz_oxide 0.9 (*"only uses safe Rust"*). The `zlib-rs` feature uses zlib-rs 0.6.8 (MSRV 1.75), which the docs say *"is the fastest, at the cost of some unsafe Rust code"* and *"typically outperforms all the C implementations"*. Precedence when several are on: zlib-ng > zlib-rs > miniz_oxide. The C backends (`zlib`, `zlib-ng`, `cloudflare_zlib`) are optional |
| `brotli` | 9.0.0 | 2026-09-02 | 1.59.0 | Encoder and decoder; depends on `brotli-decompressor ~6.0` |
| `brotli-decompressor` | **6.0.1** | 2026-09-24 | – | Decode-only, pure Rust, `forbid(unsafe)` since 5.0.2. API: `Decompressor::new(reader, buf_size)` implements `Read`; `BrotliDecompress(r, w, buf)`. The README says 6.0 commits were *"effectively authored by Claude"* and that *"Libraries that do not wish to depend on AI-authored code should remain on 5.x"* (5.0.3 is the alternative) |
| `zstd` | 0.14.0 | 2026-09-04 | 1.64 | C libzstd via `zstd-safe 8.0.0` → `zstd-sys 2.1.0+zstd.1.5.7`. `build.rs` compiles the **vendored C sources with `cc`** by default; pkg-config is used only with the `pkg-config` feature or `ZSTD_SYS_USE_PKG_CONFIG`. This needs a working C compiler for every target (MSVC on Windows; a cross C toolchain or zig for cross-builds) |
| `ruzstd` | **0.9.0** | 2026-07-26 | 1.87 | Pure Rust. README: *"fully operational implementation of the decompression portion"*, about 1.4–3.5× slower than C. `StreamingDecoder::new(&mut src)` implements `Read`; `new_with_max_window_size(..)` lets you cap memory. Includes a "Fastest"-level encoder, handy for test fixtures |

**Recommendation:** decode with `flate2 { default-features = false, features = ["zlib-rs"] }` (`MultiGzDecoder` for gzip; `ZlibDecoder` with a fallback to `DeflateDecoder` because HTTP `deflate` is ambiguous), `brotli-decompressor` 6.0.1, and `ruzstd` 0.9.0. The whole decode stack is then pure Rust, and cross-compiling for macOS, Linux (including musl) and Windows needs no C toolchain.

Always put a `Read::take(limit)` on decoded output to guard against decompression bombs. Use `brotli` 9.0.0 only as a dev-dependency to generate fixtures. `crc32fast` is not needed directly: flate2 uses zlib-rs's CRC when `zlib-rs` is on.

---

## 9. Everything else (latest, released, MSRV, notes)

| Crate | Latest | Released | MSRV | Notes |
|---|---|---|---|---|
| tokio | 1.53.1 | 2026-07-20 | 1.71 | Features: `rt-multi-thread, macros, sync, time, signal, net, io-util, fs` (or `full`) |
| tokio-stream | 0.1.19 | 2026-07-22 | 1.71 | `StreamExt::next()` for `EventStream`, as in ratatui's async example. `futures-util` 0.3.34 is an alternative |
| clap | 4.6.7 | 2026-09-14 | 1.85 (ed. 2024) | `features = ["derive", "env", "wrap_help"]`. The idiom `#[derive(Parser)] #[command(version, about, long_about = None)]` + `#[arg(short, long)]` comes from `examples/demo.rs` |
| serde | 1.0.229 | 2026-07-18 | 1.56 | `derive` |
| serde_json | 1.0.151 | 2026-07-20 | 1.71 | `preserve_order = ["indexmap", "std"]` keeps key order. Also available: `raw_value`, `float_roundtrip`, `arbitrary_precision`. Avoid `arbitrary_precision` because it is global through feature unification; use jaq-json's `Num::Dec` for faithful numbers |
| toml | 1.1.6+spec-1.1.0 | 2026-09-10 | 1.85 | Defaults `std, serde, parse, display`; optional `preserve_order`. It no longer uses toml_edit internally (it uses toml_parser/toml_writer) |
| toml_edit | 0.25.15+spec-1.1.0 | 2026-09-11 | 1.85 | *"parse and modify toml documents, while preserving comments, spaces and relative order"*. `DocumentMut` supports `doc["a"]["b"] = value(..)`. `serde` is optional (`de`/`ser` modules). **Use it for edits made from the TUI** |
| similar | **3.2.0** | 2026-08-17 | 1.85 (ed. 2024) | New major version and Apache-2.0 licence. Features: `text` (default), `inline`, `unicode`, `bytes`. 3.0 changes: owned inputs are accepted, and `old_slices`/`new_slices` were removed in favour of `old_slice()` etc. Added Histogram/Hunt algorithms, `TextDiffConfig::whitespace_mode`, `TextMerge`. API: `TextDiff::from_lines(a, b).iter_all_changes()` with `ChangeTag` |
| insta | 1.48.0 | 2026-06-11 | 1.66 | Features: `colors` (default), `filters` (`Settings::add_filter(regex, repl)`), `redactions`, `json`, `yaml`, `toml`, `csv`, `ron`, `glob`. Uses `similar 2.x` internally, so dev builds get a second copy |
| notify | 8.2.0 | 2025-08-03 | 1.77 | 9.0.0-rc.5 is a prerelease. `default = ["macos_fsevent"]`; `crossbeam-channel` is optional (off). "Known problems" docs: editor save behaviour varies, NFS/WSL need `PollWatcher`, and watching a parent is needed to see deletion |
| notify-debouncer-full | 0.7.0 | 2026-01-23 | 1.85 | Requires `notify ^8.2.0`; 0.8.0-rc.2 is a prerelease. Stitches rename events together; `new_debouncer(timeout, None, handler)` then `debouncer.watch(path, RecursiveMode::…)` |
| notify-debouncer-mini | 0.7.0 | 2025-08-03 | 1.77 | Simpler, paths only |
| arboard | 3.6.1 | 2025-08-23 | 1.71 | Features: `image-data` (default), `wayland-data-control`. Details below the table |
| directories | 6.0.0 | 2025-01-12 | – | `ProjectDirs::from("com","Foo Corp","Bar App").config_dir()` gives Linux `~/.config/barapp`, Windows `…\AppData\Roaming\Foo Corp\Bar App\config`, macOS `~/Library/Application Support/com.Foo-Corp.Bar-App` |
| dirs | 7.0.0 | 2026-09-05 | – | Base dirs only. 7.0 breaking change: `preference_dir` on Windows is now `RoamingAppData` |
| etcetera | 0.11.0 | 2025-10-28 | 1.87 | Lets you choose a strategy: `choose_app_strategy` (XDG on macOS, i.e. `~/.config/app`) or `choose_native_strategy` (Apple dirs) |
| regex | 1.13.1 | 2026-07-15 | 1.65 | — |
| globset | 0.4.20 | 2026-08-04 | 1.88 (ed. 2024) | — |
| proptest | 1.11.0 | 2026-03-24 | 1.85 | — |
| anyhow | 1.0.104 | 2026-07-18 | 1.68 | — |
| thiserror | 2.0.21 | 2026-09-23 | 1.77 | ratatui-image still pulls thiserror 1.0.69 |
| tracing | 0.1.44 | 2025-12-18 | 1.65 | — |
| tracing-subscriber | 0.3.23 | 2026-03-13 | 1.65 | Defaults `smallvec, fmt, ansi, tracing-log, std`; add `env-filter`; call `.with_ansi(false)` when writing to a file |
| tracing-appender | 0.2.5 | 2026-04-17 | 1.63 | `rolling::never(dir, file)`, `rolling::daily(..)`, `non_blocking(w) -> (NonBlocking, WorkerGuard)`. Depends on `time` |
| bytes | 1.12.1 | 2026-07-08 | 1.57 | — |
| base64 | 0.23.1 | 2026-08-04 | 1.71 | 0.23 adds SIMD engines behind the default `simd-unsafe` feature. crossterm (osc52) and jaq-std use 0.22, so expect two copies (harmless) |
| tempfile | 3.27.0 | 2026-03-11 | 1.63 | Atomic save: `NamedTempFile::new_in(dir)` then `persist(path)` |
| memmap2 | 0.9.11 | 2026-06-22 | 1.65 | Large HAR and body files |
| jiff | 0.2.37 | 2026-09-12 | 1.70 | **Recommended for local time.** Defaults include `tz-system` and `tzdb-bundle-platform` (bundles the tzdb on Windows only). API: `Zoned::now()`, `Timestamp::from_millisecond(ms)?.to_zoned(TimeZone::system())`, `.strftime("%H:%M:%S%.3f")`. jaq-std already uses jiff |
| chrono | 0.4.45 | 2026-06-04 | 1.62 | Alternative |
| time | 0.3.55 | 2026-08-01 | 1.88 (ed. 2024) | Already in the tree via ratatui-widgets' calendar feature and tracing-appender |
| unicode-width | 0.2.2 | 2025-10-06 | 1.66 | ratatui requires `>=0.2.0` |
| image | 0.25.10 | 2026-03-10 | 1.88 | Details below the table |
| quick-xml | 0.42.0 | 2026-08-22 | 1.86 (ed. 2024) | `Reader::from_str`, `read_event()`, `Writer::new_with_indent(w, b' ', 2)`, `write_event(..)`. Reader config: `check_end_names`, `allow_unmatched_ends`, `trim_text_start`/`trim_text_end`. **HTML is not XML** (void elements, raw `<script>`). For HTML consider `html5gum` 0.8.4 or `html5ever` 0.40.1 (MSRV 1.85); **their APIs were not reviewed (UNVERIFIED suitability)** |
| percent-encoding | 2.3.2 | 2025-08-21 | 1.51 | — |
| url | 2.5.8 | 2026-01-05 | 1.63 | — |
| shlex | **2.0.1** | 2026-05-17 | 1.46 | 2.0 removed the deprecated `quote`/`join`. Use `try_quote`/`try_join`, which return `QuoteError` on NUL bytes (RUSTSEC-2024-0006 background). Quoting is POSIX only |
| shell-escape | 0.1.5 | 2020-06-19 | – | Stale since 2020; has a `windows` escaping module. Prefer shlex plus hand-written PowerShell/cmd quoting |
| crc32fast | 1.5.2 | 2026-09-12 | 1.63 | Not needed directly |
| anstyle-query | 1.1.5 | 2025-11-13 | 1.66 | `no_color()`, `clicolor()`, `clicolor_force()`, `term_supports_color()`, `term_supports_ansi_color()`, `truecolor()`, `is_ci()`. Probably already in the tree via clap → anstream (anstream lists it as an optional dependency) |
| supports-color | 3.0.2 | 2024-11-26 | 1.70 | `on(Stream) -> Option<ColorLevel{has_basic, has_256, has_16m}>`; honours `NO_COLOR` and `FORCE_COLOR`; depends on `is_ci`. An alternative to anstyle-query |
| serde_with | 3.24.0 | 2026-09-26 | 1.88 | Not needed |
| har | 0.9.0 | 2026-03-22 | 1.88 | Optional HAR types (serde, serde_json, serde_with). Hand-written structs are fine too |

**arboard details.**
- **Linux default is X11** through x11rb 0.13, which is pure Rust.
- The README says: *"To support Wayland correctly, arboard users should enable the `wayland-data-control` feature."* That feature pulls in wl-clipboard-rs 0.9.4. Its wayland-backend runs in pure-Rust mode unless `client_system` is enabled.
- The **selection ownership** caveat: the README advises keeping the `Clipboard` alive for the whole process in a TUI, or using `SetExtLinux::wait()`.
- Over SSH with no DISPLAY, arboard fails; use OSC 52 there.
- Use `default-features = false` to drop the image dependencies.

**image details.**
- `default = ["rayon", "default-formats"]`, where `default-formats` includes `avif` (the encoder, ravif). Decoding AVIF needs `avif-native`, which uses the **dav1d C library**.
- For previews use `default-features = false, features = ["png","jpeg","gif","webp","bmp","ico"]`.
- `Limits` is `#[non_exhaustive]` with fields `max_image_width`, `max_image_height` and `max_alloc` (default 512 MiB). Apply it with `ImageReader::limits(..)` and `with_guessed_format()`.

---

## 10. Compatibility check: proposed manifest

```toml
[package]
name = "netinspect"
edition = "2024"
rust-version = "1.90"        # ratatui needs 1.88; transitive quantette (ratatui-image → icy_sixel) needs 1.90

[dependencies]
# ---- TUI (all on ratatui 0.30 / ratatui-core 0.1 / crossterm 0.29) ----
ratatui          = "0.30.2"                                   # crossterm 0.29 via ratatui-crossterm 0.1.2
crossterm        = { version = "0.29.0", features = ["event-stream", "osc52"] }  # same crate instance as ratatui::crossterm
ratatui-image    = { version = "11.1.0", default-features = false, features = ["crossterm", "tokio"] } # NO chafa
image            = { version = "0.25.10", default-features = false, features = ["png", "jpeg", "gif", "webp", "bmp", "ico"] }
ratatui-textarea = { version = "0.9.2", features = ["search"] }  # multi-line editor (ratatui-core 0.1 / widgets 0.3)
tui-input        = "0.15.5"                                      # single-line inputs (ratatui ^0.30.2)
tokio-stream     = "0.1.19"                                      # StreamExt for crossterm::event::EventStream

# ---- runtime / CLI / config ----
tokio      = { version = "1.53.1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net", "io-util", "fs"] }
clap       = { version = "4.6.7", features = ["derive", "env", "wrap_help"] }
serde      = { version = "1.0.229", features = ["derive"] }
serde_json = { version = "1.0.151", features = ["preserve_order"] }
toml       = "1.1.6"
toml_edit  = "0.25.15"
directories = "6.0.0"
notify      = "8.2.0"
notify-debouncer-full = "0.7.0"
tempfile    = "3.27.0"
memmap2     = "0.9.11"

# ---- body processing ----
jaq-core  = "3.1.1"
jaq-std   = "3.0.3"
jaq-json  = "2.0.3"
syntect   = { version = "5.3.0", default-features = false, features = ["parsing", "default-syntaxes", "default-themes", "regex-fancy"] }
quick-xml = "0.42.0"
similar   = { version = "3.2.0", features = ["inline", "unicode"] }
flate2    = { version = "1.1.10", default-features = false, features = ["zlib-rs"] }
brotli-decompressor = "6.0.1"
ruzstd    = "0.9.0"
base64    = "0.23.1"
percent-encoding = "2.3.2"
url       = "2.5.8"
shlex     = "2.0.1"
bytes     = "1.12.1"
unicode-width = "0.2.2"
regex     = "1.13.1"
globset   = "0.4.20"
jiff      = "0.2.37"

# ---- clipboard / terminal capabilities ----
arboard       = { version = "3.6.1", default-features = false, features = ["wayland-data-control"] }
anstyle-query = "1.1.5"

# ---- errors / logging ----
anyhow    = "1.0.104"
thiserror = "2.0.21"
tracing   = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
tracing-appender   = "0.2.5"

[dev-dependencies]
insta    = { version = "1.48.0", features = ["filters", "json", "redactions"] }
proptest = "1.11.0"
brotli   = "9.0.0"     # pure-Rust encoder to generate br fixtures (ruzstd can encode zstd fixtures)

# Optional: usable dev builds with fancy-regex (the syntect README warns debug mode is "absurdly slow")
[profile.dev.package.fancy-regex]
opt-level = 3
[profile.dev.package.syntect]
opt-level = 3
```

**Why these pieces should fit together.** This reasoning comes from reading the published manifests; nothing was compiled.
- **One ratatui stack.**
  - ratatui 0.30.2 needs ratatui-core ^0.1.2, ratatui-widgets ^0.3.2 and ratatui-crossterm ^0.1.2.
  - ratatui-image needs `ratatui ^0.30.1`.
  - ratatui-textarea needs `ratatui-core ^0.1.1`, `ratatui-widgets ^0.3.1` and `ratatui-crossterm ^0.1.1`.
  - tui-input needs `ratatui ^0.30.2`.
  - All of these resolve to single copies (0.30.2 / 0.1.2 / 0.3.2 / 0.1.2), so `Widget`, `Frame`, `Rect` and `Style` are the same types everywhere.
- **One crossterm.** ratatui-crossterm defaults to `crossterm_0_29`. Our direct `crossterm = "0.29.0"` unifies with it, so `event-stream` and `osc52` apply to `ratatui::crossterm` too, and ratatui-textarea's `ratatui_crossterm::crossterm` is the same crate. Keep all crossterm imports on a single path; both `crossterm::…` and `ratatui::crossterm::…` resolve to the same crate.
- **No C libraries in the graph:**
  - ratatui-image without chafa (icy_sixel is pure Rust)
  - syntect without onig
  - flate2 on zlib-rs
  - brotli-decompressor
  - ruzstd instead of zstd-sys
  - arboard with x11rb and pure-Rust wayland
  - static builds with musl should therefore be feasible (**UNVERIFIED**, not built here)
- **Expected duplicate versions (harmless):**
  - base64 0.22 (crossterm osc52, jaq-std) + 0.23
  - thiserror 1 (ratatui-image) + 2
  - rustix 0.38 (ratatui-image) + 1.x (crossterm)
  - rand 0.8.8 (ratatui-image)
  - similar 2 (insta, dev only) + 3
  - windows 0.58 + several windows-sys versions
- **Default-feature traps to avoid:**
  - `ratatui-image` default `chafa-dyn` breaks the build without libchafa.
  - `syntect` default `onig` needs C, and any crate enabling syntect defaults flips the engine.
  - `edtui`, `syntect-tui` and `two-face` defaults all turn onig on.
  - `image` defaults pull the AVIF encoder (rav1e) and rayon.
  - `arboard` default pulls image dependencies.
  - `zstd` compiles C.
- **Rejected:** tui-textarea 0.7.0 (ratatui 0.29, crossterm 0.28) and syntect-tui 3.0.6 (ratatui 0.29).
- **Supply-chain notes** (informational advisories, verified in rustsec/advisory-db):
  - `bincode` (via syntect's dump loading): RUSTSEC-2025-0141, "unmaintained". Allow-list it in cargo-deny.
  - `yaml-rust`: RUSTSEC-2024-0320. It stays out of the graph because the feature set above does not enable `yaml-load`.
  - `rand`: RUSTSEC-2026-0097 (unsound only with a custom logger calling `rand::rng`). Patched in ≥0.8.6, and the resolver picks 0.8.8.

---

## Design implications for netinspect

1. **Table virtualization is required.** Table allocates every `Row` passed in and runs an O(N) `column_count()` pass. It never measures content, so the fix is not to pass it everything.
   - Keep `Vec<u32>` filtered and sorted indices, updated incrementally on insert and never re-sorted per frame.
   - Build `Row`s only for `offset..offset+viewport_h`, borrowing `&str` from storage.
   - Render with `TableState::default().with_selected(sel - offset)`.
   - Draw a `Scrollbar` with `ScrollbarState::new(total).position(offset)`.
   - The same rule applies to body views: `Paragraph::scroll` is `u16` (at most 65,535 lines) and re-wraps from the top, so feed it only the visible lines.
   - Chart: downsample each series to about 2×width points, use `Marker::Braille` with `GraphType::Line` or `Area`, and keep to 3 axis labels or fewer.

2. **Event loop.** Use tokio plus `crossterm::event::EventStream` in `tokio::select!` with a frame `interval` (the upstream async example uses a 60 FPS tick). Redraw only when dirty.
   - `EnableMouseCapture` turns on any-motion tracking (?1003h). Coalesce `MouseEventKind::Moved` events, or skip them, to protect frame time over SSH.
   - Handle `Event::Paste` for bracketed paste into input fields.

3. **Terminal lifecycle.**
   - Call `ratatui::init()` for raw mode, the alternate screen and the panic hook.
   - Then `execute!(stdout, EnableMouseCapture, EnableBracketedPaste)`.
   - On exit **and in a panic hook installed before `init()`**, disable both before `ratatui::restore()`. ratatui's own hook only undoes raw mode and the alternate screen.
   - Run `ratatui_image::picker::Picker::from_query_stdio_with_options(..)` **after** entering the alternate screen and **before** creating `EventStream`, because both read stdin. Use a shorter timeout than the 2 s default.
   - Fall back to `Picker::halfblocks()`.
   - Offer an `images = "auto|kitty|sixel|iterm2|halfblocks|off"` setting. Document that ratatui-image runs `tmux set -p allow-passthrough on` inside tmux.

4. **NO_COLOR and color depth** (inferred from source, not runtime-tested).
   - When `NO_COLOR` is set, crossterm 0.29's color commands write an empty SGR (`CSI m` / `CSI ;m`), which resets **all** attributes.
   - ratatui-crossterm queues modifiers *before* `SetColors`, so REVERSED or BOLD highlights would be wiped.
   - The unreleased crossterm changelog confirms the empty-SGR bug.
   - Mitigation: detect `NO_COLOR` yourself (`anstyle_query::no_color()`), switch to a monochrome theme that uses only modifiers, and call `crossterm::style::force_color_output(true)` so crossterm emits real `39`/`49` resets.
   - syntect themes are 24-bit RGB. If `!anstyle_query::truecolor()` (for example tmux without Tc, or macOS Terminal), quantize to the xterm-256 palette.

5. **Clipboard.** Use a `clipboard = "auto|osc52|native|off"` setting.
   - `auto` picks OSC 52 (`crossterm::clipboard::CopyToClipboard::to_clipboard_from(..)`, copy only) when `SSH_TTY`/`SSH_CONNECTION` is set or there is no DISPLAY/WAYLAND_DISPLAY; otherwise arboard.
   - Keep one long-lived `arboard::Clipboard` because of X11/Wayland selection ownership.
   - Inside tmux, OSC 52 needs `set-clipboard on|external`, which crossterm's docs mention for external. Show a hint on failure.
   - OSC 52 size limits in particular terminals are **UNVERIFIED**.

6. **JSON and jq.**
   - Parse bodies once with `jaq_json::read::parse_single(bytes)`. `Val` keeps key order, big integers and decimal literals.
   - Build the fold tree and highlight spans with a hand-written tokenizer over the raw bytes.
   - Use `serde_json` (`preserve_order`) for config, HAR and NDJSON.
   - Run jq filters on a worker thread with a deadline and an output cap. A filter can loop forever and there is no cancellation.
   - `jaq_json::Val` is `Rc`-based unless the `sync` feature is on, so compile and run on the same worker. Whether `Filter` is `Send` is **UNVERIFIED**.
   - **Never call `unwrap_valr`.** On `halt` it calls `process::exit` and leaves the terminal in raw mode. Match `Exn::get_err()` and `get_halt()` instead.
   - Underline filter errors using `jaq_core::load::span(code, err_slice)`.

7. **Highlighting.** Use the hand-written JSON highlighter, and syntect (fancy-regex) for XML, HTML, JS and CSS.
   - Highlight only the visible range, caching `ParseState`/`HighlightState` checkpoints and resuming with `HighlightLines::from_state`.
   - Do the work off the UI thread for large bodies.
   - For XML pretty-printing, use quick-xml `Reader` → `Writer::new_with_indent`.
   - For HTML use a tolerant tokenizer (hand-written, or html5gum, which is unreviewed). Do not treat HTML as XML.

8. **Decoding.** Use flate2 (zlib-rs), brotli-decompressor and ruzstd.
   - Handle gzip with `MultiGzDecoder`.
   - For `deflate`, try `ZlibDecoder` and fall back to `DeflateDecoder`.
   - Cap output with `.take(limit)` and cap the zstd window with `new_with_max_window_size`.
   - Limit image decoding with `image::Limits` and do it in `spawn_blocking`.
   - Show "AVIF not supported" rather than pulling in dav1d (C).

9. **Config and rules.**
   - Config lives at `directories::ProjectDirs::from(..).config_dir()`. etcetera is the alternative if you want `~/.config` on macOS.
   - Deserialize with `toml`. Apply in-TUI edits with `toml_edit::DocumentMut` so comments survive.
   - Save atomically with `tempfile::NamedTempFile::new_in(dir)` then `persist`.
   - Watch the **parent directory** (NonRecursive) with notify-debouncer-full, because editors save by rename. Filter events by file name, keep the last good rules if parsing fails, and ignore your own writes by comparing content hashes.

10. **Logging and headless mode.**
    - Log through `tracing-appender::rolling::never(state_dir, "netinspect.log")` + `non_blocking`, keeping the `WorkerGuard` alive, with `.with_ansi(false)`.
    - Nothing may be written to stdout while the TUI is active.
    - The NDJSON mode owns stdout; logs go to the file or to stderr.
    - Consider crossterm's `use-dev-tty` (unix) if you support piped stdin together with the TUI.

11. **Testing.**
    - `insta::assert_snapshot!(terminal.backend())` on a fixed-size `TestBackend` captures text layout.
    - `assert_debug_snapshot!(terminal.backend().buffer())` also captures styles.
    - Use insta `filters` for timestamps and durations, and `Picker::halfblocks()` in tests, because the Kitty, Sixel and iTerm2 protocols embed escape sequences in cells.
    - Use `proptest` for decoders, the JSON tokenizer and the virtualization maths.

12. **Generating cURL commands.** Use `shlex::try_quote` for the POSIX form, and write separate PowerShell and cmd quoting (shlex is POSIX only).

### Risks

- **Ratatui 0.31 is being developed on main** (BREAKING-CHANGES has a v0.31.0 section), and **crossterm 0.30 is unreleased on master** (MSRV 1.85, edition 2024, NO_COLOR fix). ratatui, ratatui-image, ratatui-textarea and tui-input must all upgrade together, so pin the minor versions and commit `Cargo.lock`.
- **MSRV creep.** A transitive crate (quantette) already pushes the floor to 1.90. Set `rust-version = "1.90"` so the MSRV-aware resolver warns, and CI-test on both the pinned toolchain and the MSRV.
- **Prereleases pending:** ratatui-image 12.0.0-rc.0, notify 9.0.0-rc.5 and notify-debouncer-full 0.8.0-rc.2. Expect API churn.
- **Feature-unification traps.** Adding any crate that enables syntect's defaults, ratatui-image's defaults (chafa), `image`'s defaults, or `zstd` puts C or big builds back into the single-binary target. Guard against this with a `cargo tree -e features` check in CI.
- **Performance targets are unmeasured.** The ≥30 fps / 50,000-row target is achievable only with virtualization (§2.5), but it has **not** been benchmarked here (**UNVERIFIED**). fancy-regex is about half the speed of onig and very slow in debug builds.
- **Terminal variance.** Graphics protocols (Alacritty, Konsole, Warp are not supported), OSC 52 support, and tmux settings (`allow-passthrough`, `set-clipboard`) differ per user. Always degrade to halfblocks, and to a copy-to-file fallback.
- **brotli-decompressor 6.x** is described upstream as AI-authored. If that is a concern, pin `5.0.3`. The API used here (`Decompressor::new(reader, buf)`) is the same in 5.x per its README, though that equivalence is **UNVERIFIED**.
- **Nothing here was compiled.** Signatures come from the published sources, but the snippets and the proposed manifest must be validated with `cargo check` on the target platforms before implementation starts.

### Key sources
- crates.io API: `https://crates.io/api/v1/crates/<name>` and `/<name>/<version>`, plus tarballs from `/download`.
- Rust: https://static.rust-lang.org/dist/channel-rust-stable.toml, https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/
- Ratatui: https://docs.rs/ratatui/0.30.2/ratatui/, https://github.com/ratatui/ratatui (BREAKING-CHANGES.md, CHANGELOG.md, `examples/apps/async-github`, `examples/apps/mouse-drawing`), https://ratatui.rs/recipes/testing/snapshots/
- crossterm: https://docs.rs/crossterm/0.29.0/crossterm/, https://github.com/crossterm-rs/crossterm/blob/master/CHANGELOG.md, `examples/event-stream-tokio.rs`
- ratatui-image: https://docs.rs/ratatui-image/11.1.0/ratatui_image/, https://github.com/ratatui/ratatui-image (README compatibility matrix, `build.rs`, `src/picker.rs`, `examples/tokio.rs`)
- Text input: https://github.com/ratatui/ratatui-textarea, https://crates.io/crates/tui-input, https://crates.io/crates/tui-textarea, https://crates.io/crates/edtui, https://crates.io/crates/tui-textarea-2
- jaq: https://docs.rs/jaq-core/3.1.1/jaq_core/, https://docs.rs/jaq-all/0.3.0/jaq_all/, https://github.com/01mf02/jaq
- syntect: https://docs.rs/syntect/5.3.0/syntect/easy/struct.HighlightLines.html, https://github.com/trishume/syntect (Readme, "Pure Rust fancy-regex mode"); syntect-tui https://github.com/chanq-io/syntect-tui
- Compression: https://docs.rs/flate2/1.1.10/flate2/, https://github.com/dropbox/rust-brotli-decompressor, https://github.com/gyscos/zstd-rs (zstd-sys `build.rs`), https://github.com/KillingSpark/zstd-rs
- Others: https://github.com/1Password/arboard (README, Linux), https://docs.rs/notify/8.2.0/notify/, https://docs.rs/notify-debouncer-full/0.7.0/, https://codeberg.org/dirs/directories-rs, https://github.com/lunacookies/etcetera, https://github.com/comex/rust-shlex/blob/master/CHANGELOG.md, https://github.com/mitsuhiko/similar/blob/main/CHANGELOG.md, https://docs.rs/insta/1.48.0/insta/, https://docs.rs/image/0.25.10/image/
- RustSec: https://rustsec.org/advisories/RUSTSEC-2025-0141.html, https://rustsec.org/advisories/RUSTSEC-2024-0320.html, https://rustsec.org/advisories/RUSTSEC-2026-0097.html
