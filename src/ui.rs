//! The interactive view: sessions on the left, a live picture of the
//! selected pane on the right, one line of keys at the bottom.
//!
//! Keys and clicks both become an `Act`, so a binding can never work from
//! the keyboard and not the mouse or the other way round. Every clickable
//! thing records its full rectangle (row AND column), so one hint can't
//! claim clicks meant for a list row that shares its column.

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::claude::{self, TailCache};
use crate::config::Config;
use crate::live::{self, Kind, Pane, Session, State, Window};
use crate::naming;
use crate::newsess;
use crate::restore;
use crate::snapshot::{self, Saved, Snapshot};
use crate::tmux::{exact, Tmux};
use crate::util::{self, ago, fit, fit_left, pad, pad_left, tilde, width};

type Term = Terminal<CrosstermBackend<Stdout>>;

const ACCENT: Color = Color::Rgb(122, 162, 247);
const BUSY: Color = Color::Rgb(158, 206, 106);
const IDLE: Color = Color::Rgb(125, 133, 172);
const WARN: Color = Color::Rgb(224, 175, 104);
const PROG: Color = Color::Rgb(187, 154, 247);
const DIM: Color = Color::Rgb(96, 103, 135);
const BAD: Color = Color::Rgb(247, 118, 142);
const SEL_BG: Color = Color::Rgb(38, 42, 60);
const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const REFRESH: Duration = Duration::from_millis(1500);
const PREVIEW_EVERY: Duration = Duration::from_millis(400);

pub struct Opts {
    pub popup: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Session(usize),
    Window(usize, usize),
}

#[derive(Clone, Debug)]
enum Target {
    Session(String),
    Window {
        session: String,
        id: String,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Open,
    New,
    Rename,
    Kill,
    Save,
    Snapshots,
    Filter,
    Help,
    Mouse,
    Preview,
    Quit,
    /// a list row was clicked
    Row(usize),
    /// an item in an overlay list (menu entry, picker row, snapshot)
    Item(usize),
    /// an item in an overlay's second column (snapshot sessions)
    Item2(usize),
    /// confirm / accept in an overlay
    Yes,
    /// close an overlay
    Back,
    /// a click that lands on an overlay but on nothing in it
    Nop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PickKind {
    ClaudeFolder,
    ShellFolder,
    Host,
}

struct SnapView {
    files: Vec<PathBuf>,
    sel: usize,
    loaded: Option<(usize, Snapshot)>,
    checks: Vec<bool>,
    right: bool,
    rsel: usize,
}

enum Mode {
    Normal,
    Filter,
    Help,
    Confirm(Target),
    Rename(Target, String),
    NewMenu(usize),
    Pick {
        kind: PickKind,
        input: String,
        items: Vec<String>,
        sel: usize,
    },
    Snapshots(SnapView),
    Progress {
        title: String,
        lines: Arc<Mutex<Vec<String>>>,
        done: Arc<AtomicBool>,
    },
}

const MENU: &[(&str, &str)] = &[
    ("c", "Claude in a folder"),
    ("r", "Resume a past Claude (mn)"),
    ("s", "Shell in a folder"),
    ("h", "SSH to a host"),
];

pub struct App {
    tmux: Tmux,
    cfg: Config,
    cdir: PathBuf,
    popup: bool,
    inside: bool,
    state: State,
    rows: Vec<Row>,
    sel: usize,
    scroll: usize,
    sel_key: Option<(String, Option<String>)>,
    filter: String,
    mode: Mode,
    preview_on: bool,
    preview: Option<(String, Text<'static>)>,
    mouse: bool,
    spinner: usize,
    last_refresh: Instant,
    last_preview: Instant,
    flash: Option<(String, Instant, bool)>,
    hits: Vec<(Rect, Act)>,
    tails: TailCache,
    transcripts: HashMap<String, Option<PathBuf>>,
    branches: HashMap<String, Option<String>>,
    saved_at: Option<i64>,
    jump: (String, Instant),
    list_h: usize,
    quit: bool,
    exit: Option<String>,
}

fn setup(mouse: bool) -> Result<Term> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn teardown(term: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
}

pub fn run(tmux: Tmux, cfg: Config, opts: Opts) -> Result<()> {
    let inside = std::env::var_os("TMUX").is_some() || opts.popup;
    let current = tmux.current_session();
    let mut app = App {
        preview_on: cfg.preview,
        mouse: cfg.mouse && std::env::var_os("TM_NO_MOUSE").is_none(),
        tmux,
        cfg,
        cdir: util::claude_dir(),
        popup: opts.popup,
        inside,
        state: State::default(),
        rows: vec![],
        sel: 0,
        scroll: 0,
        sel_key: None,
        filter: String::new(),
        mode: Mode::Normal,
        preview: None,
        spinner: 0,
        last_refresh: Instant::now(),
        last_preview: Instant::now() - PREVIEW_EVERY,
        flash: None,
        hits: vec![],
        tails: TailCache::default(),
        transcripts: HashMap::new(),
        branches: HashMap::new(),
        saved_at: None,
        jump: (String::new(), Instant::now()),
        list_h: 10,
        quit: false,
        exit: None,
    };
    app.refresh();
    if let Some(cur) = current {
        if let Some(i) = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Session(si) if app.state.sessions[*si].name == cur))
        {
            app.sel = i;
            app.remember();
        }
    }

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        );
        prev_hook(info);
    }));

    let mut term = setup(app.mouse)?;
    let res = app.event_loop(&mut term);
    teardown(&mut term);
    res?;
    if let Some(target) = app.exit.take() {
        attach_outside(&app.tmux, &target)?;
    }
    Ok(())
}

/// Leave tm and become `tmux attach`, so the terminal belongs to tmux.
pub fn attach_outside(tmux: &Tmux, target: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut c = tmux.command();
    c.stdin(std::process::Stdio::inherit());
    c.args(["attach-session", "-t", target]);
    Err(c.exec().into())
}

impl App {
    fn event_loop(&mut self, term: &mut Term) -> Result<()> {
        loop {
            if self.quit {
                return Ok(());
            }
            if self.last_refresh.elapsed() >= REFRESH {
                self.refresh();
            }
            if self.preview_on && self.last_preview.elapsed() >= PREVIEW_EVERY {
                self.update_preview();
            }
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k, term)?,
                    Event::Mouse(m) => self.on_mouse(m, term)?,
                    _ => {}
                }
            }
            self.spinner = self.spinner.wrapping_add(1);
        }
    }

    // ---------------------------------------------------------------- data

    fn refresh(&mut self) {
        let mut st = live::collect(&self.tmux, &self.cdir, Some(&self.state));
        if self.cfg.auto_name && st.running {
            for (old, new) in naming::apply(&self.tmux, &st) {
                if let Some(s) = st.sessions.iter_mut().find(|s| s.name == old) {
                    s.name = new.clone();
                    s.auto = new;
                }
            }
        }
        self.state = st;
        self.saved_at = snapshot::list().last().and_then(|p| {
            p.metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });
        self.branches.clear();
        self.rebuild();
        self.last_refresh = Instant::now();
    }

    fn rebuild(&mut self) {
        let f = self.filter.to_lowercase();
        self.rows.clear();
        for (si, s) in self.state.sessions.iter().enumerate() {
            if !f.is_empty() && !session_matches(s, &f) {
                continue;
            }
            self.rows.push(Row::Session(si));
            if s.windows.len() > 1 {
                for wi in 0..s.windows.len() {
                    self.rows.push(Row::Window(si, wi));
                }
            }
        }
        if let Some(key) = self.sel_key.clone() {
            if let Some(i) = (0..self.rows.len()).find(|i| self.row_key(self.rows[*i]) == key) {
                self.sel = i;
            } else if let Some(i) =
                (0..self.rows.len()).find(|i| self.row_key(self.rows[*i]).0 == key.0)
            {
                self.sel = i;
            }
        }
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    fn row_key(&self, r: Row) -> (String, Option<String>) {
        match r {
            Row::Session(si) => (self.state.sessions[si].id.clone(), None),
            Row::Window(si, wi) => {
                let s = &self.state.sessions[si];
                (s.id.clone(), Some(s.windows[wi].id.clone()))
            }
        }
    }

    fn remember(&mut self) {
        self.sel_key = self.rows.get(self.sel).map(|r| self.row_key(*r));
        self.last_preview = Instant::now() - PREVIEW_EVERY;
    }

    fn row_parts(&self, r: Row) -> (&Session, Option<&Window>, Option<&Pane>) {
        match r {
            Row::Session(si) => {
                let s = &self.state.sessions[si];
                (s, s.active_window(), s.primary())
            }
            Row::Window(si, wi) => {
                let s = &self.state.sessions[si];
                let w = &s.windows[wi];
                (s, Some(w), w.active_pane())
            }
        }
    }

    fn selected(&self) -> Option<(&Session, Option<&Window>, Option<&Pane>)> {
        self.rows.get(self.sel).map(|r| self.row_parts(*r))
    }

    fn target(&self) -> Option<Target> {
        match self.rows.get(self.sel)? {
            Row::Session(si) => Some(Target::Session(self.state.sessions[*si].name.clone())),
            Row::Window(si, wi) => {
                let s = &self.state.sessions[*si];
                let w = &s.windows[*wi];
                Some(Target::Window {
                    session: s.name.clone(),
                    id: w.id.clone(),
                    name: w.name.clone(),
                })
            }
        }
    }

    fn update_preview(&mut self) {
        self.last_preview = Instant::now();
        let Some(id) = self
            .selected()
            .and_then(|(_, _, p)| p)
            .map(|p| p.id.clone())
        else {
            self.preview = None;
            return;
        };
        match self.tmux.capture(&id, None) {
            Ok(b) => {
                let t = b.into_text().unwrap_or_default();
                self.preview = Some((id, t));
            }
            Err(_) => self.preview = None,
        }
    }

    fn branch(&mut self, cwd: &str) -> Option<String> {
        if let Some(b) = self.branches.get(cwd) {
            return b.clone();
        }
        let b = crate::git::branch(std::path::Path::new(cwd));
        self.branches.insert(cwd.to_string(), b.clone());
        b
    }

    fn model_of(&mut self, p: &Pane) -> Option<String> {
        let sid = p.claude.as_ref()?.session_id.clone()?;
        let path = self
            .transcripts
            .entry(sid.clone())
            .or_insert_with(|| claude::find_transcript(&self.cdir, &sid, Some(&p.cwd)))
            .clone()?;
        self.tails.get(&path).model
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now(), false));
    }

    fn fail(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now(), true));
    }

    // ------------------------------------------------------------- actions

    fn on_key(&mut self, k: KeyEvent, term: &mut Term) -> Result<()> {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(self.mode, Mode::Normal) {
                self.quit = true;
            } else {
                self.mode = Mode::Normal;
            }
            return Ok(());
        }
        match &mut self.mode {
            Mode::Normal => self.key_normal(k, term),
            Mode::Filter => {
                match k.code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Up => self.move_sel(-1),
                    KeyCode::Down => self.move_sel(1),
                    KeyCode::Char(c) => self.filter.push(c),
                    _ => {}
                }
                self.rebuild();
                Ok(())
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                Ok(())
            }
            Mode::Confirm(_) => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Enter => self.overlay(Act::Yes, term)?,
                    _ => self.mode = Mode::Normal,
                }
                Ok(())
            }
            Mode::Rename(_, input) => {
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => self.overlay(Act::Yes, term)?,
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() => input.push(c),
                    _ => {}
                }
                Ok(())
            }
            Mode::NewMenu(sel) => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                    KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(MENU.len() - 1),
                    KeyCode::Enter => {
                        let i = *sel;
                        self.overlay(Act::Item(i), term)?
                    }
                    KeyCode::Char(c) => {
                        if let Some(i) = MENU.iter().position(|(key, _)| key.starts_with(c)) {
                            self.overlay(Act::Item(i), term)?;
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            Mode::Pick {
                input, items, sel, ..
            } => {
                let n = filtered(items, input).len();
                match k.code {
                    KeyCode::Esc => self.mode = Mode::NewMenu(0),
                    KeyCode::Up => *sel = sel.saturating_sub(1),
                    KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
                    KeyCode::Enter => self.overlay(Act::Yes, term)?,
                    KeyCode::Backspace => {
                        input.pop();
                        *sel = 0;
                    }
                    KeyCode::Char(c) if !c.is_control() => {
                        input.push(c);
                        *sel = 0;
                    }
                    _ => {}
                }
                Ok(())
            }
            Mode::Snapshots(v) => {
                let nright = v
                    .loaded
                    .as_ref()
                    .map(|(_, s)| s.sessions.len())
                    .unwrap_or(0);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                    KeyCode::Tab
                    | KeyCode::Right
                    | KeyCode::Left
                    | KeyCode::Char('l')
                    | KeyCode::Char('h') => v.right = !v.right && nright > 0,
                    KeyCode::Up | KeyCode::Char('k') => {
                        if v.right {
                            v.rsel = v.rsel.saturating_sub(1)
                        } else {
                            v.sel = v.sel.saturating_sub(1)
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if v.right {
                            v.rsel = (v.rsel + 1).min(nright.saturating_sub(1))
                        } else {
                            v.sel = (v.sel + 1).min(v.files.len().saturating_sub(1))
                        }
                    }
                    KeyCode::Char(' ') => {
                        if v.right {
                            let i = v.rsel;
                            self.overlay(Act::Item2(i), term)?;
                        }
                    }
                    KeyCode::Char('a') => {
                        if let Some((_, snap)) = &v.loaded {
                            let runnable: Vec<bool> = snap
                                .sessions
                                .iter()
                                .map(|s| restore_skip(s, &self.state).is_none())
                                .collect();
                            let all = v.checks.iter().zip(&runnable).all(|(c, r)| *c || !r);
                            v.checks = runnable.iter().map(|r| *r && !all).collect();
                        }
                    }
                    KeyCode::Enter => self.overlay(Act::Yes, term)?,
                    _ => {}
                }
                if let Mode::Snapshots(v) = &mut self.mode {
                    load_snap(v);
                }
                Ok(())
            }
            Mode::Progress { done, .. } => {
                if done.load(Ordering::SeqCst) || k.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    self.refresh();
                }
                Ok(())
            }
        }
    }

    fn key_normal(&mut self, k: KeyEvent, term: &mut Term) -> Result<()> {
        let act = match k.code {
            KeyCode::Char('q') => Some(Act::Quit),
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    Some(Act::Quit)
                } else {
                    self.filter.clear();
                    self.rebuild();
                    None
                }
            }
            KeyCode::Up | KeyCode::Char('k') => Some(Act::Up),
            KeyCode::Down | KeyCode::Char('j') => Some(Act::Down),
            KeyCode::PageUp => Some(Act::PageUp),
            KeyCode::PageDown => Some(Act::PageDown),
            KeyCode::Home | KeyCode::Char('g') => Some(Act::Top),
            KeyCode::End | KeyCode::Char('G') => Some(Act::Bottom),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Some(Act::Open),
            KeyCode::Char('n') => Some(Act::New),
            KeyCode::Char('r') => Some(Act::Rename),
            KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => Some(Act::Kill),
            KeyCode::Char('s') => Some(Act::Save),
            KeyCode::Char('S') => Some(Act::Snapshots),
            KeyCode::Char('/') => Some(Act::Filter),
            KeyCode::Char('?') => Some(Act::Help),
            KeyCode::Char('M') => Some(Act::Mouse),
            KeyCode::Char('p') => Some(Act::Preview),
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.jump_digit(c);
                None
            }
            _ => None,
        };
        if let Some(a) = act {
            self.act(a, term)?;
        }
        Ok(())
    }

    /// Typing a session number selects it; digits typed within a moment of
    /// each other build one number (1 then 3 = 13).
    fn jump_digit(&mut self, c: char) {
        if self.jump.1.elapsed() > Duration::from_millis(800) {
            self.jump.0.clear();
        }
        self.jump.0.push(c);
        self.jump.1 = Instant::now();
        let Ok(n) = self.jump.0.parse::<u32>() else {
            return;
        };
        if let Some(i) = self.rows.iter().position(
            |r| matches!(r, Row::Session(si) if self.state.sessions[*si].number() == Some(n)),
        ) {
            self.sel = i;
            self.remember();
        }
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.sel = (self.sel as isize + d).clamp(0, max) as usize;
        self.remember();
    }

    fn act(&mut self, a: Act, term: &mut Term) -> Result<()> {
        if !matches!(self.mode, Mode::Normal | Mode::Filter) {
            return self.overlay(a, term);
        }
        let page = self.list_h.max(1) as isize;
        match a {
            Act::Up => self.move_sel(-1),
            Act::Down => self.move_sel(1),
            Act::PageUp => self.move_sel(-page),
            Act::PageDown => self.move_sel(page),
            Act::Top => self.move_sel(-(self.rows.len() as isize)),
            Act::Bottom => self.move_sel(self.rows.len() as isize),
            Act::Row(i) => {
                if i == self.sel {
                    self.open();
                } else if i < self.rows.len() {
                    self.sel = i;
                    self.remember();
                }
            }
            Act::Open => self.open(),
            Act::New => self.mode = Mode::NewMenu(0),
            Act::Rename => {
                if let Some(t) = self.target() {
                    let cur = match &t {
                        Target::Session(n) => n.clone(),
                        Target::Window { name, .. } => name.clone(),
                    };
                    self.mode = Mode::Rename(t, cur);
                }
            }
            Act::Kill => {
                if let Some(t) = self.target() {
                    self.mode = Mode::Confirm(t);
                }
            }
            Act::Save => self.save_now(),
            Act::Snapshots => {
                let mut files = snapshot::list();
                files.reverse();
                let mut v = SnapView {
                    files,
                    sel: 0,
                    loaded: None,
                    checks: vec![],
                    right: false,
                    rsel: 0,
                };
                load_snap(&mut v);
                self.mode = Mode::Snapshots(v);
            }
            Act::Filter => self.mode = Mode::Filter,
            Act::Help => self.mode = Mode::Help,
            Act::Mouse => {
                self.mouse = !self.mouse;
                if self.mouse {
                    execute!(term.backend_mut(), EnableMouseCapture)?;
                    self.flash("mouse on");
                } else {
                    execute!(term.backend_mut(), DisableMouseCapture)?;
                    self.flash(
                        "mouse off: the terminal can select text again (M turns it back on)",
                    );
                }
            }
            Act::Preview => self.preview_on = !self.preview_on,
            Act::Quit => self.quit = true,
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self) {
        let Some(t) = self.target() else { return };
        let spec = match &t {
            Target::Session(n) => exact(n),
            Target::Window { id, .. } => id.clone(),
        };
        self.go(spec);
    }

    fn go(&mut self, spec: String) {
        if self.inside {
            match self.tmux.run(["switch-client", "-t", &spec]) {
                Ok(_) => self.quit = true,
                Err(e) => self.fail(e.to_string()),
            }
        } else {
            self.exit = Some(spec);
            self.quit = true;
        }
    }

    fn save_now(&mut self) {
        match snapshot::save(&self.tmux, &self.state, &self.cfg, &mut self.tails) {
            Ok(Saved::Written(p)) => {
                self.flash(format!("saved snapshot {}", snapshot::stamp_of(&p)));
                self.saved_at = Some(util::now());
            }
            Ok(Saved::Unchanged(_)) => self.flash("nothing changed since the last snapshot"),
            Ok(Saved::Empty) => self.fail("no sessions to save"),
            Ok(Saved::Skipped(why)) => self.fail(format!("not saved: {why}")),
            Err(e) => self.fail(format!("save failed: {e}")),
        }
    }

    /// Acts inside an overlay (keys already mapped, or clicks).
    fn overlay(&mut self, a: Act, term: &mut Term) -> Result<()> {
        if a == Act::Back {
            self.mode = match self.mode {
                Mode::Pick { .. } => Mode::NewMenu(0),
                _ => Mode::Normal,
            };
            return Ok(());
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Help => {}
            Mode::Confirm(t) => {
                if matches!(a, Act::Yes) {
                    let r = match &t {
                        Target::Session(n) => self.tmux.run(["kill-session", "-t", &exact(n)]),
                        Target::Window { id, .. } => self.tmux.run(["kill-window", "-t", id]),
                    };
                    match r {
                        Ok(_) => self.flash("killed"),
                        Err(e) => self.fail(e.to_string()),
                    }
                    self.refresh();
                } else {
                    self.mode = Mode::Confirm(t);
                }
            }
            Mode::Rename(t, input) => {
                if matches!(a, Act::Yes) {
                    let new = input.trim().to_string();
                    if new.is_empty() || new.contains(['.', ':']) {
                        self.fail("a name can't be empty or hold '.' or ':'");
                        self.mode = Mode::Rename(t, input);
                        return Ok(());
                    }
                    let r = match &t {
                        Target::Session(old) => {
                            let r = self.tmux.run(["rename-session", "-t", &exact(old), &new]);
                            if r.is_ok() {
                                naming::release(&self.tmux, &new);
                            }
                            r
                        }
                        Target::Window { id, .. } => {
                            self.tmux.run(["rename-window", "-t", id, &new])
                        }
                    };
                    if let Err(e) = r {
                        self.fail(e.to_string());
                    }
                    self.refresh();
                } else {
                    self.mode = Mode::Rename(t, input);
                }
            }
            Mode::NewMenu(sel) => match a {
                Act::Item(0) => self.start_pick(PickKind::ClaudeFolder),
                Act::Item(1) => self.resume_with_mn(term)?,
                Act::Item(2) => self.start_pick(PickKind::ShellFolder),
                Act::Item(3) => self.start_pick(PickKind::Host),
                _ => self.mode = Mode::NewMenu(sel),
            },
            Mode::Pick {
                kind,
                input,
                items,
                mut sel,
            } => {
                if let Act::Item(i) = a {
                    if i == sel {
                        self.finish_pick(kind, &input, &items, sel);
                        return Ok(());
                    }
                    sel = i;
                } else if a == Act::Yes {
                    self.finish_pick(kind, &input, &items, sel);
                    return Ok(());
                }
                self.mode = Mode::Pick {
                    kind,
                    input,
                    items,
                    sel,
                };
            }
            Mode::Snapshots(mut v) => {
                match a {
                    Act::Item(i) => {
                        v.sel = i;
                        v.right = false;
                    }
                    Act::Item2(i) => {
                        v.right = true;
                        v.rsel = i;
                        if let Some((_, snap)) = &v.loaded {
                            if let Some(s) = snap.sessions.get(i) {
                                if restore_skip(s, &self.state).is_none() {
                                    if let Some(c) = v.checks.get_mut(i) {
                                        *c = !*c;
                                    }
                                }
                            }
                        }
                    }
                    Act::Yes => {
                        if let Some((_, snap)) = &v.loaded {
                            let picked: Vec<String> = snap
                                .sessions
                                .iter()
                                .zip(&v.checks)
                                .filter(|(_, c)| **c)
                                .map(|(s, _)| s.name.clone())
                                .collect();
                            let snap = snap.clone();
                            self.start_restore(snap, picked);
                            return Ok(());
                        }
                    }
                    _ => {}
                }
                load_snap(&mut v);
                self.mode = Mode::Snapshots(v);
            }
            Mode::Progress { title, lines, done } => {
                if done.load(Ordering::SeqCst) {
                    self.refresh();
                } else {
                    self.mode = Mode::Progress { title, lines, done };
                }
            }
            Mode::Normal | Mode::Filter => {}
        }
        Ok(())
    }

    fn start_pick(&mut self, kind: PickKind) {
        let items = match kind {
            PickKind::Host => newsess::ssh_hosts(),
            _ => newsess::recent_folders(&self.state, 60)
                .into_iter()
                .map(|p| tilde(&p))
                .collect(),
        };
        self.mode = Mode::Pick {
            kind,
            input: String::new(),
            items,
            sel: 0,
        };
    }

    fn finish_pick(&mut self, kind: PickKind, input: &str, items: &[String], sel: usize) {
        let shown = filtered(items, input);
        let typed = input.trim();
        let choice = match kind {
            PickKind::Host => {
                if !typed.is_empty() && shown.is_empty() {
                    Some(typed.to_string())
                } else {
                    shown.get(sel).map(|s| s.to_string())
                }
            }
            _ => {
                let typed_dir = (!typed.is_empty()).then(|| util::expand(typed));
                let use_typed = typed_dir
                    .as_ref()
                    .map(|d| d.is_dir() && (shown.is_empty() || typed.starts_with(['/', '~'])))
                    .unwrap_or(false);
                if use_typed {
                    typed_dir.map(|d| d.to_string_lossy().into_owned())
                } else {
                    shown
                        .get(sel)
                        .map(|s| util::expand(s).to_string_lossy().into_owned())
                }
            }
        };
        let Some(choice) = choice else {
            self.fail("nothing picked");
            self.mode = Mode::Pick {
                kind,
                input: input.to_string(),
                items: items.to_vec(),
                sel,
            };
            return;
        };
        let home = util::home().to_string_lossy().into_owned();
        let (hint, cwd, typed_cmd) = match kind {
            PickKind::ClaudeFolder => {
                let mut argv = vec!["claude".to_string()];
                argv.extend(self.cfg.claude_args.iter().cloned());
                (
                    util::basename(&choice).to_string(),
                    choice.clone(),
                    Some(argv),
                )
            }
            PickKind::ShellFolder => (util::basename(&choice).to_string(), choice.clone(), None),
            PickKind::Host => (
                choice.clone(),
                home,
                Some(vec!["ssh".to_string(), choice.clone()]),
            ),
        };
        self.create_and_go(&hint, &cwd, typed_cmd);
    }

    fn create_and_go(&mut self, hint: &str, cwd: &str, typed: Option<Vec<String>>) {
        let name = newsess::new_name(&self.state, hint);
        match newsess::create(&self.tmux, &name, cwd, typed, true) {
            Ok(n) => self.go(exact(&n)),
            Err(e) => self.fail(format!("couldn't start it: {e}")),
        }
        self.refresh();
    }

    fn resume_with_mn(&mut self, term: &mut Term) -> Result<()> {
        let mouse = self.mouse;
        let picked = suspend(term, mouse, newsess::pick_with_mn)?;
        match picked {
            Err(e) => self.fail(e.to_string()),
            Ok(None) => self.flash("nothing picked in mn"),
            Ok(Some(p)) => {
                self.refresh();
                if let Some((Some(sess), _)) = self.state.where_is(&p.session_id) {
                    self.flash(format!("already open in {sess}"));
                    self.go(exact(&sess));
                    return Ok(());
                }
                let mut argv = vec![
                    "claude".to_string(),
                    "--resume".into(),
                    p.session_id.clone(),
                ];
                if !p.model.is_empty() {
                    argv.push("--model".into());
                    argv.push(p.model.clone());
                }
                argv.extend(newsess::mn_perm_flags(&p.perms));
                let cwd = if std::path::Path::new(&p.cwd).is_dir() {
                    p.cwd.clone()
                } else {
                    util::home().to_string_lossy().into_owned()
                };
                let hint = if p.title.is_empty() {
                    util::basename(&cwd).to_string()
                } else {
                    p.title.clone()
                };
                self.create_and_go(&hint, &cwd, Some(argv));
            }
        }
        Ok(())
    }

    fn start_restore(&mut self, snap: Snapshot, only: Vec<String>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (l2, d2) = (lines.clone(), done.clone());
        let tmux = self.tmux.clone();
        let cfg = self.cfg.clone();
        let restore_all = only.is_empty();
        let opts = restore::Options {
            only,
            ..Default::default()
        };
        std::thread::spawn(move || {
            let mut log = |s: String| {
                if let Ok(mut v) = l2.lock() {
                    v.push(s);
                }
            };
            if restore_all {
                log("nothing ticked: restoring every session that isn't running".into());
            }
            match restore::restore(&tmux, &snap, &cfg, &opts, &mut log) {
                Ok(r) => log(format!(
                    "done: {} made, {} skipped, {} failed. Any key closes this.",
                    r.made.len(),
                    r.skipped.len(),
                    r.failed.len()
                )),
                Err(e) => log(format!("restore failed: {e}")),
            }
            d2.store(true, Ordering::SeqCst);
        });
        self.mode = Mode::Progress {
            title: "Restoring".into(),
            lines,
            done,
        };
    }

    fn on_mouse(&mut self, m: MouseEvent, term: &mut Term) -> Result<()> {
        let at = |r: &Rect| {
            m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, a)) = self.hits.iter().rev().find(|(r, _)| at(r)).cloned() {
                    self.act(a, term)?;
                } else if !matches!(self.mode, Mode::Normal | Mode::Filter) {
                    // a click outside an overlay closes it
                    self.overlay(Act::Back, term)?;
                }
            }
            MouseEventKind::ScrollUp => match &mut self.mode {
                Mode::Normal | Mode::Filter => self.move_sel(-1),
                _ => {}
            },
            MouseEventKind::ScrollDown => match &mut self.mode {
                Mode::Normal | Mode::Filter => self.move_sel(1),
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    // ---------------------------------------------------------------- draw

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        let area = f.area();
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, head);
        let body = Rect {
            y: body.y + 1,
            height: body.height.saturating_sub(1),
            ..body
        };
        if !self.state.running {
            let why = self
                .state
                .error
                .clone()
                .filter(|e| !e.contains("no server running") && !e.contains("No such file"))
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            let msg = Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("  no tmux server is running{why}"),
                    Style::default().fg(WARN),
                )),
                Line::from(Span::styled(
                    "  n starts a session; tm restore brings back the last snapshot",
                    Style::default().fg(DIM),
                )),
            ]);
            f.render_widget(msg, body);
        } else if self.preview_on && body.width >= 90 {
            let lw = (body.width as u32 * 56 / 100) as u16;
            let [list, gap, prev] = Layout::horizontal([
                Constraint::Length(lw),
                Constraint::Length(2),
                Constraint::Min(10),
            ])
            .areas(body);
            self.draw_list(f, list);
            let rule: Vec<Line> = (0..gap.height)
                .map(|_| Line::from(Span::styled("│", Style::default().fg(DIM))))
                .collect();
            f.render_widget(Paragraph::new(rule), gap);
            self.draw_preview(f, prev);
        } else {
            self.draw_list(f, body);
        }
        self.draw_footer(f, foot);

        match &self.mode {
            Mode::Help => self.draw_help(f, area),
            Mode::Confirm(t) => {
                let t = t.clone();
                self.draw_confirm(f, area, &t)
            }
            Mode::Rename(t, input) => {
                let (t, input) = (t.clone(), input.clone());
                self.draw_rename(f, area, &t, &input)
            }
            Mode::NewMenu(sel) => {
                let sel = *sel;
                self.draw_menu(f, area, sel)
            }
            Mode::Pick {
                kind,
                input,
                items,
                sel,
            } => {
                let (kind, input, items, sel) = (*kind, input.clone(), items.clone(), *sel);
                self.draw_pick(f, area, kind, &input, &items, sel)
            }
            Mode::Snapshots(_) => self.draw_snapshots(f, area),
            Mode::Progress { title, lines, done } => {
                let lines = lines.lock().map(|l| l.clone()).unwrap_or_default();
                let (title, done) = (title.clone(), done.load(Ordering::SeqCst));
                self.draw_progress(f, area, &title, &lines, done)
            }
            Mode::Normal | Mode::Filter => {}
        }
    }

    fn draw_header(&mut self, f: &mut Frame, area: Rect) {
        let n = self.state.sessions.len();
        let c = self.state.claude_count();
        let b = self.state.busy_count();
        let mut left = vec![
            Span::styled(
                " tm",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   {n} session{}  ·  {c} claude",
                    if n == 1 { "" } else { "s" }
                ),
                Style::default().fg(DIM),
            ),
        ];
        if b > 0 {
            left.push(Span::styled(
                format!("  ·  {b} busy"),
                Style::default().fg(BUSY),
            ));
        }
        let right = match &self.flash {
            Some((m, t, err)) if t.elapsed() < Duration::from_secs(5) => Span::styled(
                format!("{m} "),
                Style::default().fg(if *err { BAD } else { WARN }),
            ),
            _ => match self.saved_at {
                Some(t) => Span::styled(
                    format!("saved {} ago ", ago(util::now() - t)),
                    Style::default().fg(DIM),
                ),
                None => Span::styled("no snapshot yet ", Style::default().fg(WARN)),
            },
        };
        let used: usize = left.iter().map(|s| width(&s.content)).sum();
        let rw = width(&right.content);
        let gap = (area.width as usize).saturating_sub(used + rw);
        left.push(Span::raw(" ".repeat(gap)));
        left.push(right);
        f.render_widget(Paragraph::new(Line::from(left)), area);
    }

    fn glyph(&self, p: &Pane) -> (String, Color) {
        if p.dead {
            return ("✕".into(), BAD);
        }
        match p.kind {
            Kind::Claude => {
                let st = p.claude.as_ref().map(|c| c.status.as_str()).unwrap_or("");
                let spin = SPIN[self.spinner % SPIN.len()].to_string();
                match st {
                    "busy" => (spin, BUSY),
                    "shell" => (spin, WARN),
                    "idle" | "" => ("○".into(), IDLE),
                    _ => ("●".into(), WARN),
                }
            }
            Kind::Program => ("▪".into(), PROG),
            Kind::Shell => ("·".into(), DIM),
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let h = area.height as usize;
        self.list_h = h;
        if self.rows.is_empty() {
            let msg = if self.filter.is_empty() {
                "  no sessions"
            } else {
                "  nothing matches the filter"
            };
            f.render_widget(
                Paragraph::new(Span::styled(msg, Style::default().fg(DIM))),
                area,
            );
            return;
        }
        if self.sel < self.scroll {
            self.scroll = self.sel;
        }
        if h > 0 && self.sel >= self.scroll + h {
            self.scroll = self.sel + 1 - h;
        }
        let w = area.width as usize;
        let name_w = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Session(si) => Some(width(&self.state.sessions[*si].short_name())),
                _ => None,
            })
            .max()
            .unwrap_or(1)
            .clamp(1, 18);
        // columns, dropped right to left as the list narrows
        // the folder column is only as wide as its widest entry needs
        let folders: Vec<String> = self
            .rows
            .iter()
            .filter_map(|r| self.row_parts(*r).2.map(|p| p.cwd.clone()))
            .collect();
        let mut fo_need = 2usize;
        for cwd in folders {
            let br = self
                .branch(&cwd)
                .map(|b| width(&fit(&b, 12)) + 1)
                .unwrap_or(0);
            fo_need = fo_need.max(width(&tilde(&cwd)) + br + 1);
        }
        let (mut st_w, mut fo_w, mut ac_w, mut cpu_w, mut ram_w) =
            (6usize, fo_need.min(24), 5usize, 5usize, 6usize);
        let base = 4 + name_w + 2;
        let mut step = 0;
        let label_w = loop {
            let used = base + st_w + fo_w + ac_w + cpu_w + ram_w;
            let lw = w.saturating_sub(used);
            if lw >= 20 || step >= 6 {
                break lw;
            }
            match step {
                0 => fo_w = fo_w.min(16),
                1 => ram_w = 0,
                2 => cpu_w = 0,
                3 => fo_w = 0,
                4 => st_w = 0,
                _ => ac_w = 0,
            }
            step += 1;
        };
        let now = util::now();
        let rows: Vec<Row> = self
            .rows
            .iter()
            .skip(self.scroll)
            .take(h)
            .copied()
            .collect();
        for (i, r) in rows.into_iter().enumerate() {
            let idx = self.scroll + i;
            let selected = idx == self.sel;
            let y = area.y + i as u16;
            let rect = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            let (s, win, p) = self.row_parts(r);
            let (s, win, p) = (s.clone(), win.cloned(), p.cloned());
            let mut spans: Vec<Span> = Vec::new();
            spans.push(if selected {
                Span::styled(
                    "❯ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            });
            let (g, gc) = p
                .as_ref()
                .map(|p| self.glyph(p))
                .unwrap_or((" ".into(), DIM));
            spans.push(Span::styled(format!("{g} "), Style::default().fg(gc)));
            let is_window = matches!(r, Row::Window(..));
            if is_window {
                spans.push(Span::raw(" ".repeat(name_w)));
            } else {
                let mut st = Style::default().fg(ACCENT);
                if s.attached > 0 {
                    st = st.add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(pad(&fit(&s.short_name(), name_w), name_w), st));
            }
            spans.push(Span::raw("  "));

            let label = p.as_ref().map(|p| p.label.clone()).unwrap_or_default();
            let (label_style, kind) = match p.as_ref().map(|p| &p.kind) {
                Some(Kind::Claude) => (Style::default(), Some(Kind::Claude)),
                Some(Kind::Program) => (Style::default().fg(PROG), Some(Kind::Program)),
                _ => (Style::default().fg(DIM), Some(Kind::Shell)),
            };
            let mut extra = String::new();
            let shown_label = if is_window {
                let wi = win.as_ref().map(|w| w.index).unwrap_or(0);
                format!("└ {wi}  {label}")
            } else {
                if s.windows.len() > 1 {
                    extra = format!("  +{} windows", s.windows.len() - 1);
                } else if let Some(w) = &win {
                    if w.panes.len() > 1 {
                        extra = format!("  +{} panes", w.panes.len() - 1);
                    }
                }
                label
            };
            let lw = label_w.saturating_sub(1);
            let main = fit(&shown_label, lw);
            let rest = lw.saturating_sub(width(&main));
            let extra = fit(&extra, rest);
            spans.push(Span::styled(main.clone(), label_style));
            spans.push(Span::styled(extra.clone(), Style::default().fg(DIM)));
            spans.push(Span::raw(
                " ".repeat(label_w.saturating_sub(width(&main) + width(&extra))),
            ));

            if st_w > 0 {
                let (word, c) = match (&kind, p.as_ref().and_then(|p| p.claude.as_ref())) {
                    (Some(Kind::Claude), Some(c)) => {
                        let col = match c.status.as_str() {
                            "busy" => BUSY,
                            "idle" | "" => IDLE,
                            _ => WARN,
                        };
                        (
                            if c.status.is_empty() {
                                "claude".to_string()
                            } else {
                                c.status.clone()
                            },
                            col,
                        )
                    }
                    _ => (String::new(), DIM),
                };
                spans.push(Span::styled(
                    pad(&fit(&word, st_w - 1), st_w),
                    Style::default().fg(c),
                ));
            }
            if fo_w > 0 {
                let cwd = p.as_ref().map(|p| p.cwd.clone()).unwrap_or_default();
                let br = self.branch(&cwd);
                let br = br.map(|b| fit(&b, 12)).unwrap_or_default();
                let room = fo_w.saturating_sub(1 + if br.is_empty() { 0 } else { width(&br) + 1 });
                let folder = fit_left(&tilde(&cwd), room);
                let used = width(&folder) + if br.is_empty() { 0 } else { 1 + width(&br) };
                spans.push(Span::styled(folder, Style::default().fg(DIM)));
                if !br.is_empty() {
                    spans.push(Span::styled(format!(" {br}"), Style::default().fg(IDLE)));
                }
                spans.push(Span::raw(" ".repeat(fo_w.saturating_sub(used))));
            }
            if ac_w > 0 {
                let act = win.as_ref().map(|w| w.activity).unwrap_or(s.activity);
                spans.push(Span::styled(
                    pad_left(&ago(now - act), ac_w - 1) + " ",
                    Style::default().fg(DIM),
                ));
            }
            if !is_window {
                if cpu_w > 0 {
                    let c = s
                        .cpu
                        .map(|c| format!("{c:.0}%"))
                        .unwrap_or_else(|| "·".into());
                    let col = if s.cpu.unwrap_or(0.0) >= 50.0 {
                        WARN
                    } else {
                        DIM
                    };
                    spans.push(Span::styled(
                        pad_left(&c, cpu_w - 1) + " ",
                        Style::default().fg(col),
                    ));
                }
                if ram_w > 0 {
                    spans.push(Span::styled(
                        pad_left(&util::bytes(s.rss), ram_w - 1),
                        Style::default().fg(DIM),
                    ));
                }
            }
            let mut para = Paragraph::new(Line::from(spans));
            if selected {
                para = para.style(Style::default().bg(SEL_BG));
            }
            f.render_widget(para, rect);
            self.hits.push((rect, Act::Row(idx)));
        }
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let Some((s, w, p)) = self.selected() else {
            return;
        };
        let (s, w, p) = (s.clone(), w.cloned(), p.cloned());
        let Some(p) = p else { return };
        let now = util::now();
        let mut meta: Vec<Line> = Vec::new();
        let mut l1 = vec![Span::styled(
            s.name.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        if let Some(w) = &w {
            l1.push(Span::styled(
                format!("   window {} {}", w.index, w.name),
                Style::default().fg(DIM),
            ));
            if w.panes.len() > 1 {
                l1.push(Span::styled(
                    format!("  ·  pane {} of {}", p.index, w.panes.len()),
                    Style::default().fg(DIM),
                ));
            }
        }
        if s.attached > 0 {
            l1.push(Span::styled("  ·  attached", Style::default().fg(BUSY)));
        }
        meta.push(Line::from(l1));

        let l2 = match &p.kind {
            Kind::Claude => {
                let c = p.claude.clone();
                let status = c.as_ref().map(|c| c.status.clone()).unwrap_or_default();
                let sid = c
                    .as_ref()
                    .and_then(|c| c.session_id.clone())
                    .map(|s| s.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "id unknown".into());
                let model = self.model_of(&p).unwrap_or_default();
                let col = match status.as_str() {
                    "busy" => BUSY,
                    "idle" | "" => IDLE,
                    _ => WARN,
                };
                Line::from(vec![
                    Span::styled("claude ", Style::default()),
                    Span::styled(
                        if status.is_empty() {
                            "running".to_string()
                        } else {
                            status
                        },
                        Style::default().fg(col),
                    ),
                    Span::styled(format!("   {model}   {sid}"), Style::default().fg(DIM)),
                ])
            }
            Kind::Program => Line::from(Span::styled(
                fit(&p.fg_argv.join(" "), area.width as usize),
                Style::default().fg(PROG),
            )),
            Kind::Shell => Line::from(Span::styled(p.label.clone(), Style::default().fg(DIM))),
        };
        meta.push(l2);
        let br = self.branch(&p.cwd);
        let mut l3 = vec![Span::styled(tilde(&p.cwd), Style::default())];
        if let Some(b) = br {
            l3.push(Span::styled(format!("  {b}"), Style::default().fg(IDLE)));
        }
        let act = w.as_ref().map(|w| w.activity).unwrap_or(s.activity);
        l3.push(Span::styled(
            format!(
                "   up {}  ·  active {} ago",
                ago(now - s.created),
                ago(now - act)
            ),
            Style::default().fg(DIM),
        ));
        meta.push(Line::from(l3));
        let cpu = s
            .cpu
            .map(|c| format!("{c:.0}%"))
            .unwrap_or_else(|| "·".into());
        meta.push(Line::from(Span::styled(
            format!("cpu {cpu}   ram {}", util::bytes(s.rss)),
            Style::default().fg(DIM),
        )));
        meta.push(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(DIM),
        )));
        let mh = meta.len() as u16;
        f.render_widget(
            Paragraph::new(meta),
            Rect {
                height: mh.min(area.height),
                ..area
            },
        );
        if area.height <= mh {
            return;
        }
        let rest = Rect {
            y: area.y + mh,
            height: area.height - mh,
            ..area
        };
        if let Some((id, text)) = &self.preview {
            if *id == p.id {
                let mut lines = text.lines.clone();
                while lines
                    .last()
                    .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
                    .unwrap_or(false)
                {
                    lines.pop();
                }
                let keep = rest.height as usize;
                let start = lines.len().saturating_sub(keep);
                let shown: Vec<Line> = lines.into_iter().skip(start).collect();
                f.render_widget(Paragraph::new(Text::from(shown)), rest);
            }
        }
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        if matches!(self.mode, Mode::Filter) {
            let line = Line::from(vec![
                Span::styled(" / ", Style::default().fg(ACCENT)),
                Span::raw(format!("{}▏", self.filter)),
                Span::styled("    enter keep  ·  esc clear", Style::default().fg(DIM)),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        let hints: &[(&str, &str, Act)] = &[
            ("enter", "open", Act::Open),
            ("n", "new", Act::New),
            ("r", "rename", Act::Rename),
            ("x", "kill", Act::Kill),
            ("s", "save", Act::Save),
            ("S", "snapshots", Act::Snapshots),
            ("/", "filter", Act::Filter),
            ("p", "preview", Act::Preview),
            ("?", "help", Act::Help),
            ("q", if self.popup { "close" } else { "quit" }, Act::Quit),
        ];
        let mut spans = vec![Span::raw(" ")];
        let mut x = area.x + 1;
        for (k, label, a) in hints {
            let wk = width(k) as u16;
            let wl = width(label) as u16;
            let total = wk + 1 + wl + 3;
            if x + total > area.x + area.width {
                break;
            }
            spans.push(Span::styled(k.to_string(), Style::default().fg(ACCENT)));
            spans.push(Span::styled(
                format!(" {label}   "),
                Style::default().fg(DIM),
            ));
            self.hits.push((
                Rect {
                    x,
                    y: area.y,
                    width: wk + 1 + wl,
                    height: 1,
                },
                *a,
            ));
            x += total;
        }
        if !self.filter.is_empty() {
            spans.push(Span::styled(
                format!("[/{}]", self.filter),
                Style::default().fg(WARN),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn panel(&mut self, f: &mut Frame, area: Rect, w: u16, h: u16, title: &str) -> Rect {
        let r = centered(area, w, h);
        f.render_widget(Clear, r);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(DIM))
            .title(Span::styled(
                format!(" {title} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(r);
        f.render_widget(block, r);
        // swallow clicks inside the panel so they don't fall through to the list
        self.hits.push((r, Act::Nop));
        inner
    }

    fn draw_help(&mut self, f: &mut Frame, area: Rect) {
        let rows: &[(&str, &str)] = &[
            ("enter  click", "switch to the session or window"),
            ("j k  wheel", "move  (g G top/bottom, PgUp PgDn)"),
            ("1 2 3", "type a session number to jump to it"),
            (
                "n",
                "new: Claude in a folder, a past Claude (mn), shell, ssh",
            ),
            ("r", "rename (tm stops auto-naming a session you name)"),
            ("x", "kill the session or window (asks first)"),
            ("s", "save a snapshot now"),
            ("S", "snapshots: pick one, tick sessions, enter restores"),
            ("/", "filter by name, title or folder"),
            ("p", "show or hide the live preview"),
            ("M", "mouse on/off (off lets the terminal select text)"),
            ("q  esc", "quit"),
        ];
        let inner = self.panel(f, area, 78, rows.len() as u16 + 4, "keys");
        let mut lines: Vec<Line> = rows
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!(" {}", pad(k, 14)), Style::default().fg(ACCENT)),
                    Span::raw(d.to_string()),
                ])
            })
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            " Saved every 15 min by tm autosave, restored at boot.  Any key closes this.",
            Style::default().fg(DIM),
        )));
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_confirm(&mut self, f: &mut Frame, area: Rect, t: &Target) {
        let (what, busy, claudes) = match t {
            Target::Session(n) => {
                let s = self.state.session(n);
                let busy = s
                    .map(|s| {
                        s.panes().any(|p| {
                            p.claude
                                .as_ref()
                                .map(|c| c.status == "busy")
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                let claudes = s
                    .map(|s| s.panes().filter(|p| p.kind == Kind::Claude).count())
                    .unwrap_or(0);
                (format!("session {n}"), busy, claudes)
            }
            Target::Window { session, name, id } => {
                let w = self
                    .state
                    .session(session)
                    .and_then(|s| s.windows.iter().find(|w| &w.id == id));
                let busy = w
                    .map(|w| {
                        w.panes.iter().any(|p| {
                            p.claude
                                .as_ref()
                                .map(|c| c.status == "busy")
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                let claudes = w
                    .map(|w| w.panes.iter().filter(|p| p.kind == Kind::Claude).count())
                    .unwrap_or(0);
                (format!("window {name} of {session}"), busy, claudes)
            }
        };
        let inner = self.panel(f, area, 72, 8, "kill");
        let mut lines = vec![Line::from(vec![
            Span::raw(" Kill "),
            Span::styled(what, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("?"),
        ])];
        if busy {
            lines.push(Line::from(Span::styled(
                " Claude is working in it right now.",
                Style::default().fg(BAD),
            )));
        }
        if claudes > 0 {
            lines.push(Line::from(Span::styled(
                " The conversation stays on disk: mn or a snapshot can resume it.",
                Style::default().fg(DIM),
            )));
        }
        lines.push(Line::raw(""));
        let y = inner.y + lines.len() as u16;
        lines.push(Line::from(vec![
            Span::styled(" [y] kill ", Style::default().fg(BAD)),
            Span::raw("   "),
            Span::styled(" [n] keep ", Style::default().fg(DIM)),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
        self.hits.push((
            Rect {
                x: inner.x + 1,
                y,
                width: 9,
                height: 1,
            },
            Act::Yes,
        ));
        self.hits.push((
            Rect {
                x: inner.x + 14,
                y,
                width: 9,
                height: 1,
            },
            Act::Back,
        ));
    }

    fn draw_rename(&mut self, f: &mut Frame, area: Rect, t: &Target, input: &str) {
        let what = match t {
            Target::Session(n) => format!("session {n}"),
            Target::Window { name, .. } => format!("window {name}"),
        };
        let inner = self.panel(f, area, 60, 6, "rename");
        let lines = vec![
            Line::from(Span::styled(
                format!(" Rename {what}"),
                Style::default().fg(DIM),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(ACCENT)),
                Span::raw(format!("{input}▏")),
            ]),
            Line::from(Span::styled(
                " enter rename  ·  esc cancel",
                Style::default().fg(DIM),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_menu(&mut self, f: &mut Frame, area: Rect, sel: usize) {
        let inner = self.panel(f, area, 46, MENU.len() as u16 + 4, "new session");
        let mut lines = vec![Line::raw("")];
        for (i, (k, label)) in MENU.iter().enumerate() {
            let mark = if i == sel { "❯ " } else { "  " };
            let mut line = Line::from(vec![
                Span::styled(format!(" {mark}"), Style::default().fg(ACCENT)),
                Span::styled(format!("{k}  "), Style::default().fg(ACCENT)),
                Span::raw(label.to_string()),
            ]);
            if i == sel {
                line = line.style(Style::default().bg(SEL_BG));
            }
            lines.push(line);
            self.hits.push((
                Rect {
                    x: inner.x,
                    y: inner.y + 1 + i as u16,
                    width: inner.width,
                    height: 1,
                },
                Act::Item(i),
            ));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_pick(
        &mut self,
        f: &mut Frame,
        area: Rect,
        kind: PickKind,
        input: &str,
        items: &[String],
        sel: usize,
    ) {
        let title = match kind {
            PickKind::ClaudeFolder => "claude in a folder",
            PickKind::ShellFolder => "shell in a folder",
            PickKind::Host => "ssh to a host",
        };
        let h = (area.height.saturating_sub(4)).min(24);
        let inner = self.panel(f, area, 70, h, title);
        let shown = filtered(items, input);
        let hint = match kind {
            PickKind::Host => "type to filter, or a host name",
            _ => "type to filter, or a path (~/... or /...)",
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(ACCENT)),
                Span::raw(format!("{input}▏")),
                Span::styled(format!("   {hint}"), Style::default().fg(DIM)),
            ]),
            Line::raw(""),
        ];
        let room = inner.height.saturating_sub(3) as usize;
        let start = sel.saturating_sub(room.saturating_sub(1));
        for (i, item) in shown.iter().enumerate().skip(start).take(room) {
            let mark = if i == sel { "❯ " } else { "  " };
            let mut line = Line::from(vec![
                Span::styled(format!(" {mark}"), Style::default().fg(ACCENT)),
                Span::raw(fit(item, inner.width as usize - 4)),
            ]);
            if i == sel {
                line = line.style(Style::default().bg(SEL_BG));
            }
            self.hits.push((
                Rect {
                    x: inner.x,
                    y: inner.y + lines.len() as u16,
                    width: inner.width,
                    height: 1,
                },
                Act::Item(i),
            ));
            lines.push(line);
        }
        if shown.is_empty() {
            let msg = match kind {
                PickKind::Host if items.is_empty() => " no hosts in ~/.ssh/config: type one",
                _ => " no match: enter uses what you typed",
            };
            lines.push(Line::from(Span::styled(msg, Style::default().fg(DIM))));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_snapshots(&mut self, f: &mut Frame, area: Rect) {
        // full screen under the header: a centred panel narrower than the
        // terminal leaves the list showing through either side of it
        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        f.render_widget(Clear, body);
        let inner = self.panel(f, body, body.width, body.height, "snapshots");
        let Mode::Snapshots(v) = &self.mode else {
            return;
        };
        let lw = (inner.width / 3).max(26);
        let left = Rect { width: lw, ..inner };
        let right = Rect {
            x: inner.x + lw + 2,
            width: inner.width.saturating_sub(lw + 2),
            ..inner
        };
        let mut hits = Vec::new();
        let mut lines = vec![Line::from(Span::styled(
            format!(" {} saved", v.files.len()),
            Style::default().fg(DIM),
        ))];
        let room = left.height.saturating_sub(2) as usize;
        let start = v.sel.saturating_sub(room.saturating_sub(1));
        for (i, p) in v.files.iter().enumerate().skip(start).take(room) {
            let st = snapshot::stamp_of(p);
            let ts = stamp_to_pretty(&st);
            let mark = if i == v.sel { "❯ " } else { "  " };
            let mut line = Line::from(vec![
                Span::styled(format!(" {mark}"), Style::default().fg(ACCENT)),
                Span::raw(ts),
            ]);
            if i == v.sel {
                line = line.style(Style::default().bg(if v.right { Color::Reset } else { SEL_BG }));
            }
            hits.push((
                Rect {
                    x: left.x,
                    y: left.y + lines.len() as u16,
                    width: left.width,
                    height: 1,
                },
                Act::Item(i),
            ));
            lines.push(line);
        }
        if v.files.is_empty() {
            lines.push(Line::from(Span::styled(
                "   none yet: s saves one",
                Style::default().fg(DIM),
            )));
        }
        f.render_widget(Paragraph::new(lines), left);

        let mut rl: Vec<Line> = Vec::new();
        if let Some((_, snap)) = &v.loaded {
            rl.push(Line::from(Span::styled(
                format!(
                    " {} sessions  ·  {} claude  ·  from {}",
                    snap.sessions.len(),
                    snap.claude_count(),
                    if snap.host.is_empty() {
                        "?"
                    } else {
                        &snap.host
                    }
                ),
                Style::default().fg(DIM),
            )));
            let room = right.height.saturating_sub(3) as usize;
            let start = v.rsel.saturating_sub(room.saturating_sub(1));
            for (i, s) in snap.sessions.iter().enumerate().skip(start).take(room) {
                let skip = restore_skip(s, &self.state);
                let checked = v.checks.get(i).copied().unwrap_or(false);
                let mark = if v.right && i == v.rsel { "❯" } else { " " };
                let box_ = if skip.is_some() {
                    "   "
                } else if checked {
                    "[x]"
                } else {
                    "[ ]"
                };
                let nw = 26usize;
                let desc_w = (right.width as usize).saturating_sub(nw + 9);
                let mut spans = vec![
                    Span::styled(format!("{mark} "), Style::default().fg(ACCENT)),
                    Span::styled(
                        format!("{box_} "),
                        Style::default().fg(if checked { BUSY } else { DIM }),
                    ),
                    Span::styled(
                        pad(&fit(&s.name, nw), nw + 1),
                        Style::default().fg(if skip.is_some() { DIM } else { ACCENT }),
                    ),
                ];
                match &skip {
                    Some(why) => {
                        spans.push(Span::styled(fit(why, desc_w), Style::default().fg(DIM)))
                    }
                    None => spans.push(Span::raw(fit(&s.describe(), desc_w))),
                }
                let mut line = Line::from(spans);
                if v.right && i == v.rsel {
                    line = line.style(Style::default().bg(SEL_BG));
                }
                hits.push((
                    Rect {
                        x: right.x,
                        y: right.y + rl.len() as u16,
                        width: right.width,
                        height: 1,
                    },
                    Act::Item2(i),
                ));
                rl.push(line);
            }
            let y = right.y + right.height.saturating_sub(1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" tab", Style::default().fg(ACCENT)),
                    Span::styled(" side  ", Style::default().fg(DIM)),
                    Span::styled("space", Style::default().fg(ACCENT)),
                    Span::styled(" tick  ", Style::default().fg(DIM)),
                    Span::styled("a", Style::default().fg(ACCENT)),
                    Span::styled(" all  ", Style::default().fg(DIM)),
                    Span::styled("enter", Style::default().fg(ACCENT)),
                    Span::styled(" restore (no ticks = all)  ", Style::default().fg(DIM)),
                    Span::styled("esc", Style::default().fg(ACCENT)),
                    Span::styled(" back", Style::default().fg(DIM)),
                ])),
                Rect {
                    y,
                    height: 1,
                    ..right
                },
            );
        }
        f.render_widget(
            Paragraph::new(rl),
            Rect {
                height: right.height.saturating_sub(1),
                ..right
            },
        );
        self.hits.extend(hits);
    }

    fn draw_progress(
        &mut self,
        f: &mut Frame,
        area: Rect,
        title: &str,
        lines: &[String],
        done: bool,
    ) {
        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        f.render_widget(Clear, body);
        let inner = self.panel(f, body, body.width, body.height, title);
        let room = inner.height as usize;
        let start = lines.len().saturating_sub(room);
        let mut out: Vec<Line> = lines
            .iter()
            .skip(start)
            .map(|l| {
                let col = if l.contains("FAILED") || l.contains("could not") {
                    BAD
                } else if l.trim_start().starts_with("skip") {
                    DIM
                } else {
                    Color::Reset
                };
                Line::from(Span::styled(format!(" {l}"), Style::default().fg(col)))
            })
            .collect();
        if !done {
            out.push(Line::from(Span::styled(
                format!(" {} working", SPIN[self.spinner % SPIN.len()]),
                Style::default().fg(BUSY),
            )));
        }
        f.render_widget(Paragraph::new(out), inner);
    }
}

fn session_matches(s: &Session, f: &str) -> bool {
    s.name.to_lowercase().contains(f)
        || s.panes()
            .any(|p| p.label.to_lowercase().contains(f) || p.cwd.to_lowercase().contains(f))
}

fn filtered<'a>(items: &'a [String], input: &str) -> Vec<&'a String> {
    let q = input.trim().to_lowercase();
    if q.is_empty() {
        return items.iter().collect();
    }
    items
        .iter()
        .filter(|i| i.to_lowercase().contains(&q))
        .collect()
}

fn restore_skip(s: &snapshot::SSession, state: &State) -> Option<String> {
    restore::skip_reason(s, state)
}

fn load_snap(v: &mut SnapView) {
    if v.loaded.as_ref().map(|(i, _)| *i) == Some(v.sel) {
        return;
    }
    v.loaded = v
        .files
        .get(v.sel)
        .and_then(|p| snapshot::load(p).ok())
        .map(|s| (v.sel, s));
    let n = v
        .loaded
        .as_ref()
        .map(|(_, s)| s.sessions.len())
        .unwrap_or(0);
    v.checks = vec![false; n];
    v.rsel = 0;
}

fn stamp_to_pretty(stamp: &str) -> String {
    // 20261110-174500 -> 11-10 17:45  (local, as written)
    if stamp.len() >= 15 {
        format!(
            "{}-{} {}:{}:{}",
            &stamp[4..6],
            &stamp[6..8],
            &stamp[9..11],
            &stamp[11..13],
            &stamp[13..15]
        )
    } else {
        stamp.to_string()
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn suspend<T>(term: &mut Term, mouse: bool, f: impl FnOnce() -> T) -> Result<T> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )?;
    let r = f();
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        execute!(term.backend_mut(), EnableMouseCapture)?;
    }
    term.clear()?;
    Ok(r)
}
