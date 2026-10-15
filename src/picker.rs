//! The choice card, drawn under the prompt.
//!
//! It takes a few rows of the terminal you are already in. It does not take
//! the screen. Three channels, then a clarify field. A sure reading leaves
//! the extra channels dark and marks the pointer in leaf. A fork marks it in
//! hot. The command sits in ice under the readings. The card decrypts in
//! once. Moving the pointer sweeps that row. When you choose, the band is
//! erased and the cursor is back where the shell left it. Nothing here runs
//! the command.

use std::io::{self, Stdout, Write};
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::interpret::{self, Choice};
use crate::motion::Motion;
use crate::style::ink;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPIN: Duration = Duration::from_millis(80);
const BLINK: Duration = Duration::from_millis(530);
const CONFIRM: Duration = Duration::from_millis(190);

#[derive(Clone, Debug)]
struct View {
    request: String,
    choices: Vec<Choice>,
    selected: usize,
    clarify: String,
    cursor: usize,
    caret_on: bool,
    phase: Phase,
    frame: usize,
    notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Loading,
    Ready,
    Failed(String),
}

struct Stash {
    choices: Vec<Choice>,
    selected: usize,
}

struct Confirm {
    command: String,
    until: Instant,
}

struct Loaded {
    result: Result<Vec<Choice>, String>,
}

struct App {
    request: String,
    view: View,
    notes: Vec<String>,
    previous: Vec<Choice>,
    pending: Option<Receiver<Loaded>>,
    stash: Option<Stash>,
    confirm: Option<Confirm>,
    generation: u64,
    seen: u64,
    booted: bool,
    cue_row: Option<usize>,
    cue_field: bool,
    cue_fault: bool,
}

enum Close {
    Place(String),
    Cancel,
}

#[derive(Clone, Copy)]
struct Slots {
    card: Rect,
    list: [Option<Rect>; 3],
    preview: Option<Rect>,
    clarify: Option<Rect>,
    hints: Option<Rect>,
    promise: Option<Rect>,
    request: Option<Rect>,
    rule: Option<Rect>,
}

impl Slots {
    fn empty() -> Self {
        Self {
            card: Rect::default(),
            list: [None; 3],
            preview: None,
            clarify: None,
            hints: None,
            promise: None,
            request: None,
            rule: None,
        }
    }

    fn list_union(self) -> Option<Rect> {
        let rows: Vec<Rect> = self.list.into_iter().flatten().collect();
        let first = *rows.first()?;
        let last = *rows.last()?;
        Some(Rect::new(
            first.x,
            first.y,
            first.width,
            last.bottom() - first.y,
        ))
    }
}

struct Flags {
    request: bool,
    rule: bool,
    list_rows: u16,
    preview: bool,
    hints: bool,
    promise: bool,
}

/// Show the card and return the command to place, or `None` when the person cancels.
pub fn run(request: &str) -> Result<Option<String>, String> {
    let mut session =
        open().map_err(|error| format!("cmd needs a terminal to show the choices ({error})."))?;
    let outcome = session.drive(request);
    drop(session);
    outcome
}

struct Session {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Restored after the terminal, so the card is erased on the tty first.
    saved_stdout: SavedStdout,
}

/// stdout is a pipe when the shell captures the chosen command. The card has
/// to borrow the tty for that time, then give the pipe back.
struct SavedStdout {
    fd: libc::c_int,
}

impl SavedStdout {
    fn to_tty() -> io::Result<Self> {
        let _ = std::io::stdout().flush();
        let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if saved < 0 {
            return Err(io::Error::last_os_error());
        }
        let tty = std::fs::File::open("/dev/tty")?;
        let rc = unsafe { libc::dup2(tty.as_raw_fd(), libc::STDOUT_FILENO) };
        if rc < 0 {
            unsafe { libc::close(saved) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd: saved })
    }
}

impl Drop for SavedStdout {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.fd, libc::STDOUT_FILENO);
            libc::close(self.fd);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Keep the saved descriptor alive until the fields drop.
        let _kept = &self.saved_stdout;
        let top = self.terminal.get_frame().area().as_position();
        let backend = self.terminal.backend_mut();
        let _ = backend.set_cursor_position(top);
        let _ = backend.clear_region(ClearType::AfterCursor);
        let _ = backend.set_cursor_position(top);
        let _ = backend.show_cursor();
        let _ = Backend::flush(backend);
        let _ = disable_raw_mode();
    }
}

fn open() -> io::Result<Session> {
    attach_stdin()?;
    let saved_stdout = SavedStdout::to_tty()?;
    enable_raw_mode()?;
    let mut terminal = match Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(inline_rows()),
        },
    ) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            return Err(error);
        }
    };
    let _ = execute!(terminal.backend_mut(), cursor::Hide);
    Ok(Session {
        terminal,
        saved_stdout,
    })
}

fn attach_stdin() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if io::IsTerminal::is_terminal(&io::stdin()) {
        return Ok(());
    }
    let tty = std::fs::File::open("/dev/tty")?;
    let rc = unsafe { libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Session {
    fn drive(&mut self, request: &str) -> Result<Option<String>, String> {
        let effects = crate::style::effects_enabled();
        let color = crate::style::color_enabled();
        let mut app = App::start(request);
        let mut motion = Motion::new(effects);
        let mut last_spin = Instant::now();
        let mut last_caret = Instant::now();

        loop {
            if effects {
                if last_caret.elapsed() >= BLINK {
                    app.view.caret_on = !app.view.caret_on;
                    last_caret = Instant::now();
                }
            } else {
                app.view.caret_on = true;
            }
            if matches!(app.view.phase, Phase::Loading) && last_spin.elapsed() >= SPIN {
                app.view.frame = app.view.frame.wrapping_add(1);
                last_spin = Instant::now();
            }
            app.poll_load();
            if app
                .confirm
                .as_ref()
                .is_some_and(|confirm| Instant::now() >= confirm.until)
            {
                return Ok(app.confirm.take().map(|confirm| confirm.command));
            }

            let mut cue_row = app.cue_row.take();
            let cue_field = std::mem::take(&mut app.cue_field);
            let cue_fault = std::mem::take(&mut app.cue_fault);
            let generation = app.generation;
            let seen = app.seen;
            self.terminal
                .draw(|frame| {
                    let slots = render(frame, &app.view, color);
                    if !app.booted && slots.card.width > 0 {
                        motion.arrive(slots.card);
                        app.booted = true;
                    }
                    if generation != seen {
                        if cue_fault || matches!(app.view.phase, Phase::Failed(_)) {
                            motion.fault(slots.card);
                        } else if matches!(app.view.phase, Phase::Ready) {
                            if let Some(list) = slots.list_union() {
                                motion.decode(list);
                            }
                        }
                        app.seen = generation;
                    }
                    if let Some(index) = cue_row.take() {
                        let rows = slots.list.iter().flatten().count();
                        let start = window_start(app.view.selected, app.view.choices.len(), rows);
                        if let Some(slot) = index.checked_sub(start) {
                            if let Some(rect) = slots.list.get(slot).copied().flatten() {
                                motion.lock(rect);
                            }
                        }
                    }
                    if cue_field {
                        if let Some(rect) = slots.clarify {
                            motion.field(rect);
                        }
                    }
                    let area = frame.area();
                    motion.render(frame.buffer_mut(), area);
                })
                .map_err(|error| format!("The card could not be drawn ({error})."))?;

            if event::poll(Duration::from_millis(16))
                .map_err(|error| format!("The card could not be drawn ({error})."))?
            {
                match event::read()
                    .map_err(|error| format!("The card could not be drawn ({error})."))?
                {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if let Some(close) = app.on_key(key, effects) {
                            return Ok(match close {
                                Close::Cancel => None,
                                Close::Place(command) => Some(command),
                            });
                        }
                    }
                    Event::Paste(text) => app.paste(&text),
                    _ => {}
                }
            }
        }
    }
}

impl App {
    fn start(request: &str) -> Self {
        let request = request.to_string();
        let pending = Some(spawn(request.clone(), Vec::new(), Vec::new()));
        let shown = request.clone();
        Self {
            request,
            view: View {
                request: shown,
                choices: Vec::new(),
                selected: 0,
                clarify: String::new(),
                cursor: 0,
                caret_on: true,
                phase: Phase::Loading,
                frame: 0,
                notice: None,
            },
            notes: Vec::new(),
            previous: Vec::new(),
            pending,
            stash: None,
            confirm: None,
            generation: 0,
            seen: 0,
            booted: false,
            cue_row: None,
            cue_field: false,
            cue_fault: false,
        }
    }

    fn poll_load(&mut self) {
        let arrived = match self.pending.as_ref() {
            Some(receiver) => match receiver.try_recv() {
                Ok(loaded) => Some(loaded),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Loaded {
                    result: Err("The request stopped.".into()),
                }),
            },
            None => None,
        };
        if let Some(loaded) = arrived {
            self.apply(loaded);
        }
    }

    fn apply(&mut self, loaded: Loaded) {
        self.pending = None;
        match loaded.result {
            Ok(choices) => {
                self.stash = None;
                self.previous = choices.clone();
                self.view.choices = choices;
                self.view.selected = 0;
                self.view.phase = Phase::Ready;
                self.view.notice = None;
                self.generation = self.generation.wrapping_add(1);
            }
            Err(message) => {
                let message = one_line(&message, 180);
                if let Some(stash) = self.stash.take() {
                    self.view.choices = stash.choices;
                    self.view.selected = stash.selected.min(self.view.choices.len());
                    self.view.phase = Phase::Ready;
                    self.view.notice = Some(message);
                    self.notes.pop();
                    self.cue_fault = true;
                    self.generation = self.generation.wrapping_add(1);
                } else {
                    self.view.choices.clear();
                    self.view.selected = 0;
                    self.view.phase = Phase::Failed(message);
                    self.cue_fault = true;
                    self.generation = self.generation.wrapping_add(1);
                }
            }
        }
    }

    fn in_field(&self) -> bool {
        !matches!(self.view.phase, Phase::Loading) && self.view.selected >= self.view.choices.len()
    }

    fn on_key(&mut self, key: KeyEvent, effects: bool) -> Option<Close> {
        if self.confirm.is_some() {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return Some(Close::Cancel);
        }
        if matches!(self.view.phase, Phase::Loading) {
            return match key.code {
                KeyCode::Esc => self.on_esc(),
                _ => None,
            };
        }
        if self.in_field() && ctrl {
            match key.code {
                KeyCode::Char('u') => {
                    self.view.clarify.clear();
                    self.view.cursor = 0;
                    self.view.notice = None;
                }
                KeyCode::Char('w') => {
                    delete_word(&mut self.view.clarify, &mut self.view.cursor);
                    self.view.notice = None;
                }
                KeyCode::Char('a') => self.view.cursor = 0,
                KeyCode::Char('e') => self.view.cursor = self.view.clarify.chars().count(),
                KeyCode::Char('d') => delete_forward(&mut self.view.clarify, self.view.cursor),
                _ => {}
            }
            return None;
        }
        if ctrl && matches!(key.code, KeyCode::Char('d')) {
            return Some(Close::Cancel);
        }
        match key.code {
            KeyCode::Esc => self.on_esc(),
            KeyCode::Enter => self.on_enter(effects),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') if !self.in_field() && !alt => self.move_by(-1),
            KeyCode::Char('j') if !self.in_field() && !alt => self.move_by(1),
            KeyCode::Char('z') if !self.in_field() && !alt => self.focus_field(),
            KeyCode::Char(digit @ '1'..='3') if !self.in_field() && !alt => self.jump(digit),
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Left if self.in_field() => {
                self.view.cursor = self.view.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right if self.in_field() => {
                let len = self.view.clarify.chars().count();
                self.view.cursor = (self.view.cursor + 1).min(len);
                None
            }
            KeyCode::Home if self.in_field() => {
                self.view.cursor = 0;
                None
            }
            KeyCode::End if self.in_field() => {
                self.view.cursor = self.view.clarify.chars().count();
                None
            }
            KeyCode::Backspace if self.in_field() => {
                backspace(&mut self.view.clarify, &mut self.view.cursor);
                self.view.notice = None;
                None
            }
            KeyCode::Delete if self.in_field() => {
                delete_forward(&mut self.view.clarify, self.view.cursor);
                self.view.notice = None;
                None
            }
            KeyCode::Char(ch) if self.in_field() && !alt => {
                insert_char(&mut self.view.clarify, &mut self.view.cursor, ch);
                self.view.notice = None;
                None
            }
            _ => None,
        }
    }

    fn on_esc(&mut self) -> Option<Close> {
        if matches!(self.view.phase, Phase::Loading) {
            if let Some(stash) = self.stash.take() {
                self.pending = None;
                self.notes.pop();
                self.view.choices = stash.choices;
                self.view.selected = stash.selected.min(self.view.choices.len());
                self.view.phase = Phase::Ready;
                self.cue_row = Some(
                    self.view
                        .selected
                        .min(self.view.choices.len().saturating_sub(1)),
                );
                return None;
            }
            return Some(Close::Cancel);
        }
        if self.view.selected >= self.view.choices.len() && !self.view.choices.is_empty() {
            self.view.selected = self.view.choices.len() - 1;
            self.cue_row = Some(self.view.selected);
            return None;
        }
        Some(Close::Cancel)
    }

    fn on_enter(&mut self, effects: bool) -> Option<Close> {
        if self.in_field() {
            if !self.view.clarify.trim().is_empty() {
                self.refine();
            } else if matches!(self.view.phase, Phase::Failed(_)) {
                self.retry();
            }
            return None;
        }
        let command = self.view.choices.get(self.view.selected)?.command.clone();
        if !effects {
            return Some(Close::Place(command));
        }
        self.cue_row = Some(self.view.selected);
        self.confirm = Some(Confirm {
            command,
            until: Instant::now() + CONFIRM,
        });
        None
    }

    fn move_by(&mut self, delta: i32) -> Option<Close> {
        let last = self.view.choices.len();
        let current = self.view.selected.min(last);
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(last)
        };
        self.aim(next);
        None
    }

    fn cycle(&mut self, delta: i32) -> Option<Close> {
        let count = self.view.choices.len() + 1;
        if count == 0 {
            return None;
        }
        let current = self.view.selected.min(count - 1);
        let next = if delta > 0 {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.aim(next);
        None
    }

    fn jump(&mut self, digit: char) -> Option<Close> {
        let index = (digit as usize) - ('1' as usize);
        if index < self.view.choices.len() {
            self.aim(index);
        }
        None
    }

    fn focus_field(&mut self) -> Option<Close> {
        self.aim(self.view.choices.len());
        None
    }

    fn aim(&mut self, next: usize) {
        let last = self.view.choices.len();
        let next = next.min(last);
        if next == self.view.selected.min(last) {
            return;
        }
        self.view.notice = None;
        self.view.selected = next;
        if next == last {
            self.cue_field = true;
        } else {
            self.cue_row = Some(next);
        }
    }

    fn refine(&mut self) {
        let note = self.view.clarify.trim().to_string();
        if note.is_empty() || self.pending.is_some() {
            return;
        }
        if !self.view.choices.is_empty() {
            self.stash = Some(Stash {
                choices: self.view.choices.clone(),
                selected: self.view.selected,
            });
        }
        self.notes.push(note);
        self.view.phase = Phase::Loading;
        self.view.notice = None;
        self.pending = Some(spawn(
            self.request.clone(),
            self.notes.clone(),
            self.previous.clone(),
        ));
    }

    fn retry(&mut self) {
        if self.pending.is_some() {
            return;
        }
        self.view.phase = Phase::Loading;
        self.view.notice = None;
        self.pending = Some(spawn(
            self.request.clone(),
            self.notes.clone(),
            self.previous.clone(),
        ));
    }

    fn paste(&mut self, text: &str) {
        if !self.in_field() {
            return;
        }
        for ch in text.chars() {
            insert_char(&mut self.view.clarify, &mut self.view.cursor, ch);
        }
        self.view.notice = None;
    }
}

fn spawn(request: String, notes: Vec<String>, previous: Vec<Choice>) -> Receiver<Loaded> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = interpret::ask(&request, &notes, &previous);
        let _ = tx.send(Loaded { result });
    });
    rx
}

fn render(frame: &mut Frame, view: &View, color: bool) -> Slots {
    let area = frame.area();
    let buf = frame.buffer_mut();
    if area.width < 16 || area.height < 4 {
        write_at(
            buf,
            area.x + 1,
            area.y,
            area.right(),
            "make the terminal taller",
            inked(color, ink::GHOST, ink::VOID, false, false),
        );
        return Slots::empty();
    }
    let (card, flags) = plan(area);
    let slots = place_slots(card, &flags);
    paint_card(buf, card, color);
    if let Some(rect) = slots.request {
        let bg = card_bg(card, rect.y, color);
        write_at(
            buf,
            rect.x + 1,
            rect.y,
            rect.right().saturating_sub(1),
            &fit(
                &view.request,
                width_between(rect.x + 1, rect.right().saturating_sub(1)),
            ),
            inked(color, ink::TEXT, bg, false, false),
        );
    }
    if let Some(rect) = slots.rule {
        let bg = card_bg(card, rect.y, color);
        let style = inked(color, ink::GHOST, bg, false, false);
        for x in rect.x + 1..rect.right().saturating_sub(1) {
            put(buf, x, rect.y, "─", style);
        }
    }
    paint_list(buf, card, &slots, view, color);
    if let Some(rect) = slots.preview {
        paint_preview(buf, card, rect, view, color);
    }
    if let Some(rect) = slots.clarify {
        paint_field(buf, rect, view, color);
    }
    if let Some(rect) = slots.hints {
        paint_hints(buf, card, rect, view, color);
    }
    if let Some(rect) = slots.promise {
        let bg = card_bg(card, rect.y, color);
        write_at(
            buf,
            rect.x + 1,
            rect.y,
            rect.right().saturating_sub(1),
            &fit(
                "nothing runs until you press enter on the prompt",
                width_between(rect.x + 1, rect.right().saturating_sub(1)),
            ),
            inked(color, ink::GHOST, bg, false, false),
        );
    }
    slots
}

fn paint_list(buf: &mut Buffer, card: Rect, slots: &Slots, view: &View, color: bool) {
    let rows: Vec<Rect> = slots.list.into_iter().flatten().collect();
    match &view.phase {
        Phase::Loading => {
            for rect in &rows {
                fill(
                    buf,
                    *rect,
                    inked(color, ink::TEXT, card_bg(card, rect.y, color), false, false),
                );
            }
            if let Some(rect) = rows.first() {
                let bg = card_bg(card, rect.y, color);
                let x = write_at(
                    buf,
                    rect.x + 1,
                    rect.y,
                    rect.right(),
                    FRAMES[view.frame % FRAMES.len()],
                    inked(color, ink::ICE, bg, true, false),
                );
                write_at(
                    buf,
                    x + 1,
                    rect.y,
                    rect.right().saturating_sub(1),
                    "reading",
                    inked(color, ink::GHOST, bg, false, false),
                );
            }
        }
        Phase::Failed(message) => {
            for rect in &rows {
                fill(
                    buf,
                    *rect,
                    inked(color, ink::HOT, card_bg(card, rect.y, color), false, false),
                );
            }
            let width = rows
                .first()
                .map(|rect| width_between(rect.x + 1, rect.right().saturating_sub(1)))
                .unwrap_or(20);
            let lines = wrap(message, width);
            for (rect, line) in rows.iter().zip(lines) {
                let bg = card_bg(card, rect.y, color);
                write_at(
                    buf,
                    rect.x + 1,
                    rect.y,
                    rect.right().saturating_sub(1),
                    &line,
                    inked(color, ink::HOT, bg, false, false),
                );
            }
        }
        Phase::Ready => {
            let start = window_start(view.selected, view.choices.len(), rows.len());
            let sure = view.choices.len() == 1;
            for (slot, rect) in rows.iter().enumerate() {
                let choice = view.choices.get(start + slot);
                let on = choice.is_some() && view.selected == start + slot;
                let bg = if on && color {
                    ink::SELECT
                } else {
                    card_bg(card, rect.y, color)
                };
                fill(buf, *rect, inked(color, ink::TEXT, bg, false, on && !color));
                let Some(choice) = choice else {
                    continue;
                };
                let marker_fg = if sure { ink::LEAF } else { ink::HOT };
                if on {
                    put(
                        buf,
                        rect.x + 1,
                        rect.y,
                        "▶",
                        inked(color, marker_fg, bg, true, !color),
                    );
                }
                let title_x = rect.x + 3;
                let right = rect.right().saturating_sub(1);
                let budget = width_between(title_x, right);
                let min_detail = 16.min(budget / 3);
                let title_max = budget.saturating_sub(min_detail + 2);
                let title = fit(&choice.title, title_max);
                let title_w = title.width();
                let detail_max = budget.saturating_sub(title_w + 2);
                let detail = fit(&choice.detail, detail_max);
                let title_fg = ink::TEXT;
                write_at(
                    buf,
                    title_x,
                    rect.y,
                    right,
                    &title,
                    inked(color, title_fg, bg, on, on && !color),
                );
                if !detail.is_empty() && detail_max > 0 {
                    let detail_fg = if on { ink::ICE } else { ink::GHOST };
                    let detail_x = right.saturating_sub(detail.width() as u16);
                    if detail_x > title_x + title_w as u16 {
                        write_at(
                            buf,
                            detail_x,
                            rect.y,
                            right,
                            &detail,
                            inked(color, detail_fg, bg, false, on && !color),
                        );
                    }
                }
            }
        }
    }
}

fn paint_preview(buf: &mut Buffer, card: Rect, rect: Rect, view: &View, color: bool) {
    let bg = card_bg(card, rect.y, color);
    fill(buf, rect, inked(color, ink::TEXT, bg, false, false));
    let (text, fg, bold) = if let Some(notice) = &view.notice {
        if matches!(view.phase, Phase::Ready) {
            (notice.clone(), ink::AMBER, false)
        } else {
            return;
        }
    } else if matches!(view.phase, Phase::Ready) && view.selected >= view.choices.len() {
        let words = if view.clarify.trim().is_empty() {
            "say what the words left out"
        } else {
            "asks again with this"
        };
        (words.to_string(), ink::GHOST, false)
    } else if matches!(view.phase, Phase::Ready) {
        match view.choices.get(view.selected) {
            Some(choice) => (choice.command.clone(), ink::ICE, true),
            None => return,
        }
    } else {
        return;
    };
    let x = rect.x + 3;
    let right = rect.right().saturating_sub(1);
    write_at(
        buf,
        x,
        rect.y,
        right,
        &fit(&text, width_between(x, right)),
        inked(color, fg, bg, bold, false),
    );
}

fn paint_field(buf: &mut Buffer, rect: Rect, view: &View, color: bool) {
    let focused = !matches!(view.phase, Phase::Loading) && view.selected >= view.choices.len();
    let bg = if !color {
        Color::Reset
    } else if focused {
        ink::FIELD_ON
    } else {
        ink::FIELD
    };
    fill(buf, rect, inked(color, ink::TEXT, bg, false, false));
    let right = rect.right().saturating_sub(1);
    let title_x = rect.x + 3;
    if focused {
        put(
            buf,
            rect.x + 1,
            rect.y,
            "❯",
            inked(color, ink::AMBER, bg, true, false),
        );
        let room = width_between(title_x, right);
        let (shown, column) = field_window(&view.clarify, view.cursor, room);
        write_at(
            buf,
            title_x,
            rect.y,
            right,
            &shown,
            inked(color, ink::TEXT, bg, true, false),
        );
        if view.caret_on {
            let caret_x = title_x + column as u16;
            if caret_x < right {
                put(
                    buf,
                    caret_x,
                    rect.y,
                    "█",
                    inked(color, ink::AMBER, bg, true, false),
                );
            }
        }
    } else if view.clarify.is_empty() {
        write_at(
            buf,
            title_x,
            rect.y,
            right,
            "clarify",
            inked(color, ink::GHOST, bg, false, false),
        );
    } else {
        let room = width_between(title_x, right);
        let (shown, _) = field_window(&view.clarify, view.cursor, room);
        write_at(
            buf,
            title_x,
            rect.y,
            right,
            &shown,
            inked(color, ink::TEXT, bg, false, false),
        );
    }
}

fn paint_hints(buf: &mut Buffer, card: Rect, rect: Rect, view: &View, color: bool) {
    let bg = card_bg(card, rect.y, color);
    fill(buf, rect, inked(color, ink::TEXT, bg, false, false));
    let items: &[(&str, &str)] = match &view.phase {
        Phase::Loading => &[("esc", "cancel")],
        Phase::Failed(_) if view.clarify.trim().is_empty() => {
            &[("enter", "try again"), ("esc", "close")]
        }
        Phase::Failed(_) => &[("enter", "ask again"), ("esc", "close")],
        Phase::Ready if view.selected >= view.choices.len() => {
            &[("enter", "ask again"), ("esc", "back")]
        }
        Phase::Ready => &[
            ("↑↓", "move"),
            ("enter", "place"),
            ("z", "clarify"),
            ("esc", "cancel"),
        ],
    };
    let mut x = rect.x + 1;
    let max = rect.right().saturating_sub(1);
    for (index, (key, what)) in items.iter().enumerate() {
        if x >= max {
            break;
        }
        if index > 0 {
            x = write_at(
                buf,
                x,
                rect.y,
                max,
                "  ",
                inked(color, ink::GHOST, bg, false, false),
            );
        }
        let key_style = if color {
            Style::default()
                .fg(ink::TEXT)
                .bg(ink::KEYCAP)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        x = write_at(buf, x, rect.y, max, &format!(" {key} "), key_style);
        x = write_at(
            buf,
            x,
            rect.y,
            max,
            &format!(" {what}"),
            inked(color, ink::GHOST, bg, false, false),
        );
    }
}

fn full_flags() -> Flags {
    Flags {
        request: true,
        rule: true,
        list_rows: 3,
        preview: true,
        hints: true,
        promise: true,
    }
}

/// Rows the inline viewport reserves. Matches the card with every row showing.
fn inline_rows() -> u16 {
    card_height(&full_flags())
}

fn plan(area: Rect) -> (Rect, Flags) {
    let mut flags = full_flags();
    while card_height(&flags) > area.height && shrink(&mut flags) {}
    let height = card_height(&flags).min(area.height).max(2);
    (Rect::new(area.x, area.y, area.width.max(1), height), flags)
}

fn card_height(flags: &Flags) -> u16 {
    2 + u16::from(flags.request)
        + u16::from(flags.rule)
        + flags.list_rows
        + u16::from(flags.preview)
        + 1
        + u16::from(flags.hints)
        + u16::from(flags.promise)
}

fn shrink(flags: &mut Flags) -> bool {
    if flags.promise {
        flags.promise = false;
        return true;
    }
    if flags.hints {
        flags.hints = false;
        return true;
    }
    if flags.preview {
        flags.preview = false;
        return true;
    }
    if flags.rule {
        flags.rule = false;
        return true;
    }
    if flags.list_rows > 1 {
        flags.list_rows -= 1;
        return true;
    }
    if flags.request {
        flags.request = false;
        return true;
    }
    false
}

fn place_slots(card: Rect, flags: &Flags) -> Slots {
    let mut slots = Slots::empty();
    slots.card = card;
    if card.width < 2 || card.height < 2 {
        return slots;
    }
    let inner = Rect::new(
        card.x + 1,
        card.y + 1,
        card.width.saturating_sub(2),
        card.height.saturating_sub(2),
    );
    let mut y = inner.y;
    let mut take = |yes: bool| -> Option<Rect> {
        if !yes || y >= inner.bottom() {
            return None;
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
        y += 1;
        Some(rect)
    };
    slots.request = take(flags.request);
    slots.rule = take(flags.rule);
    for slot in slots.list.iter_mut().take(flags.list_rows as usize) {
        *slot = take(true);
    }
    slots.preview = take(flags.preview);
    slots.clarify = take(true);
    slots.hints = take(flags.hints);
    slots.promise = take(flags.promise);
    slots
}

fn paint_card(buf: &mut Buffer, card: Rect, color: bool) {
    if card.width < 2 || card.height < 2 {
        return;
    }
    for y in card.y..card.bottom() {
        let bg = card_bg(card, y, color);
        for x in card.x..card.right() {
            put(buf, x, y, " ", inked(color, ink::TEXT, bg, false, false));
        }
    }
    let left = card.left();
    let right = card.right() - 1;
    let top = card.top();
    let bottom = card.bottom() - 1;
    for x in left + 1..right {
        let near = x <= left + 2 || x + 2 >= right;
        let glyph = if near { "━" } else { "─" };
        let fg = if near { ink::ICE } else { ink::GHOST };
        put(
            buf,
            x,
            top,
            glyph,
            inked(color, fg, card_bg(card, top, color), near, false),
        );
        put(
            buf,
            x,
            bottom,
            glyph,
            inked(color, fg, card_bg(card, bottom, color), near, false),
        );
    }
    for y in top + 1..bottom {
        let near = y == top + 1 || y + 1 == bottom;
        let glyph = if near { "┃" } else { "│" };
        let fg = if near { ink::ICE } else { ink::GHOST };
        let style = inked(color, fg, card_bg(card, y, color), near, false);
        put(buf, left, y, glyph, style);
        put(buf, right, y, glyph, style);
    }
    let corner = inked(color, ink::ICE, card_bg(card, top, color), true, false);
    put(buf, left, top, "┏", corner);
    put(buf, right, top, "┓", corner);
    let foot = inked(color, ink::ICE, card_bg(card, bottom, color), true, false);
    put(buf, left, bottom, "┗", foot);
    put(buf, right, bottom, "┛", foot);
    if card.width > 20 {
        let bg = card_bg(card, top, color);
        let mut x = left + 2;
        x = write_at(
            buf,
            x,
            top,
            right,
            "╸",
            inked(color, ink::ICE, bg, true, false),
        );
        x = write_at(
            buf,
            x,
            top,
            right,
            " AI Command ",
            inked(color, ink::HOT, bg, true, false),
        );
        write_at(
            buf,
            x,
            top,
            right,
            "╺",
            inked(color, ink::ICE, bg, true, false),
        );
    }
}

fn window_start(selected: usize, len: usize, rows: usize) -> usize {
    if rows == 0 || len <= rows {
        return 0;
    }
    let focus = selected.min(len.saturating_sub(1));
    focus.saturating_sub(rows - 1).min(len - rows)
}

fn field_window(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if width == 0 {
        return (String::new(), 0);
    }
    let room = width.saturating_sub(1);
    if chars.len() <= room {
        return (chars.into_iter().collect(), cursor);
    }
    let start = cursor.saturating_sub(room);
    let end = (start + room).min(chars.len());
    let shown: String = chars[start..end].iter().collect();
    (shown, cursor - start)
}

fn insert_char(text: &mut String, cursor: &mut usize, ch: char) {
    if ch.is_control() || text.chars().count() >= 400 {
        return;
    }
    let at = text
        .char_indices()
        .nth(*cursor)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.insert(at, ch);
    *cursor += 1;
}

fn backspace(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = text
        .char_indices()
        .nth(*cursor - 1)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let end = text
        .char_indices()
        .nth(*cursor)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.replace_range(start..end, "");
    *cursor -= 1;
}

fn delete_forward(text: &mut String, cursor: usize) {
    let Some(start) = text.char_indices().nth(cursor).map(|(index, _)| index) else {
        return;
    };
    let end = text
        .char_indices()
        .nth(cursor + 1)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.replace_range(start..end, "");
}

fn delete_word(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = (*cursor).min(chars.len());
    while index > 0 && chars[index - 1].1.is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].1.is_whitespace() {
        index -= 1;
    }
    let start = chars.get(index).map(|(byte, _)| *byte).unwrap_or(0);
    let end = chars
        .get(*cursor)
        .map(|(byte, _)| *byte)
        .unwrap_or(text.len());
    text.replace_range(start..end, "");
    *cursor = index;
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line = fit(word, width);
            continue;
        }
        let next = format!("{line} {word}");
        if next.width() <= width {
            line = next;
        } else {
            lines.push(std::mem::take(&mut line));
            line = fit(word, width);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > 3 {
        lines.truncate(3);
        if let Some(last) = lines.last_mut() {
            *last = fit(last, width.saturating_sub(1));
            last.push('…');
        }
    }
    lines
}

fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let next = format!("{out}{ch}");
        if next.width() + 1 > width {
            break;
        }
        out.push(ch);
    }
    if out.width() < width {
        out.push('…');
    }
    out
}

fn width_between(left: u16, right: u16) -> usize {
    right.saturating_sub(left) as usize
}

fn card_bg(card: Rect, y: u16, color: bool) -> Color {
    if !color {
        Color::Reset
    } else if (y - card.y).is_multiple_of(2) {
        ink::VOID
    } else {
        ink::SCAN
    }
}

fn inked(color: bool, fg: Color, bg: Color, bold: bool, reverse: bool) -> Style {
    let mut style = Style::default();
    if color {
        style = style.fg(fg).bg(bg);
    }
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn fill(buf: &mut Buffer, rect: Rect, style: Style) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            put(buf, x, y, " ", style);
        }
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

fn write_at(buf: &mut Buffer, x: u16, y: u16, max: u16, text: &str, style: Style) -> u16 {
    let mut at = x;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(1) as u16;
        if width == 0 {
            continue;
        }
        if at.saturating_add(width) > max {
            break;
        }
        put(buf, at, y, &ch.to_string(), style);
        at += width;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn choice(title: &str, detail: &str, command: &str) -> Choice {
        Choice {
            title: title.into(),
            detail: detail.into(),
            command: command.into(),
            confidence: 0.5,
        }
    }

    fn fork(selected: usize) -> View {
        View {
            request: "git command to merge dev into test".into(),
            choices: vec![
                choice(
                    "merge into test",
                    "brings dev's commits into test and keeps dev",
                    "git checkout test && git merge dev",
                ),
                choice(
                    "merge and delete dev",
                    "merges into test, then deletes the dev branch",
                    "git checkout test && git merge dev && git branch -d dev",
                ),
            ],
            selected,
            clarify: String::new(),
            cursor: 0,
            caret_on: true,
            phase: Phase::Ready,
            frame: 0,
            notice: None,
        }
    }

    fn painted(view: &View) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, view, true);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    fn marker_color(buf: &Buffer) -> Color {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).map(|cell| cell.symbol()) == Some("▶") {
                    return buf.cell((x, y)).unwrap().fg;
                }
            }
        }
        panic!("no pointer");
    }

    #[test]
    fn the_card_puts_the_title_on_the_left_and_the_detail_on_the_right() {
        let buf = painted(&fork(0));
        let lines = lines(&buf);
        let text = lines.join("\n");
        let row = lines
            .iter()
            .find(|line| line.contains("merge into test") && line.contains("keeps dev"))
            .expect(&text);
        let title_at = row.find("merge into test").unwrap();
        let detail_at = row.find("keeps dev").unwrap();
        assert!(title_at < detail_at, "{row}");
        assert!(row.contains('▶'), "{row}");
        let other = lines
            .iter()
            .find(|line| line.contains("deletes the dev branch"))
            .expect(&text);
        assert!(!other.contains('▶'), "{other}");
        let command = lines
            .iter()
            .position(|line| {
                line.contains("git checkout test && git merge dev") && !line.contains("branch -d")
            })
            .expect(&text);
        let clarify = lines
            .iter()
            .position(|line| line.contains("clarify"))
            .expect(&text);
        let title = lines
            .iter()
            .position(|line| line.contains("keeps dev"))
            .unwrap();
        assert!(title < command && command < clarify, "{text}");
        assert!(
            text.contains("nothing runs until you press enter on the prompt"),
            "{text}"
        );
        assert!(text.contains("AI Command"), "{text}");
        assert!(
            !text.contains("branch -d"),
            "the other command stays off the card until it is chosen:\n{text}"
        );
    }

    #[test]
    fn the_card_is_a_short_band_across_the_terminal() {
        let buf = painted(&fork(0));
        let lines = lines(&buf);
        let text = lines.join("\n");
        let band = card_height(&full_flags()) as usize;
        assert!(
            lines[0].contains("AI Command"),
            "the wordmark sits on the first row under the prompt:\n{text}"
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┏");
        assert_eq!(
            buf.cell((buf.area.width - 1, 0)).unwrap().symbol(),
            "┓",
            "the band uses the full width"
        );
        let promise = lines
            .iter()
            .position(|line| line.contains("nothing runs until you press enter on the prompt"))
            .expect(&text);
        assert!(promise < band, "{text}");
        for y in band as u16..buf.area.height {
            for x in 0..buf.area.width {
                let cell = buf.cell((x, y)).unwrap();
                assert_eq!(cell.symbol(), " ", "row {y} was drawn");
                assert_eq!(cell.bg, Color::Reset, "row {y} was filled");
            }
        }
    }

    #[test]
    fn choosing_the_second_reading_shows_its_command() {
        let buf = painted(&fork(1));
        let text = lines(&buf).join("\n");
        assert!(text.contains("branch -d dev"), "{text}");
        let row = lines(&buf)
            .into_iter()
            .find(|line| line.contains("deletes the dev branch"))
            .unwrap();
        assert!(row.contains('▶'), "{row}");
    }

    #[test]
    fn one_reading_uses_a_leaf_pointer_and_a_fork_uses_hot() {
        let fork_buf = painted(&fork(0));
        assert_eq!(marker_color(&fork_buf), ink::HOT);
        let mut alone = fork(0);
        alone.choices.pop();
        let alone_buf = painted(&alone);
        assert_eq!(marker_color(&alone_buf), ink::LEAF);
        let text = lines(&alone_buf).join("\n");
        assert!(text.contains("clarify"), "{text}");
        assert!(!text.contains("merge and delete dev"), "{text}");
    }

    #[test]
    fn loading_reads_and_a_failure_says_what_happened() {
        let mut loading = fork(0);
        loading.phase = Phase::Loading;
        loading.choices[0].title = "SHOULD-NOT-SHOW".into();
        let text = lines(&painted(&loading)).join("\n");
        assert!(text.contains("reading"), "{text}");
        assert!(!text.contains("SHOULD-NOT-SHOW"), "{text}");

        let mut failed = fork(0);
        failed.phase = Phase::Failed("No Grok login found. Run grok login.".into());
        failed.choices.clear();
        failed.selected = 0;
        let text = lines(&painted(&failed)).join("\n");
        assert!(text.contains("No Grok login found"), "{text}");
        assert!(
            text.contains('❯'),
            "the clarify field stays and takes the cursor:\n{text}"
        );
        assert!(text.contains("try again"), "{text}");
        assert!(!text.contains("merge into test"), "{text}");
    }
}
