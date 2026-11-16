//! The TUI's state and how keys and background results change it.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use serde_json::Value;

use crate::runner::{Msg, Runner, split_args};
use crate::sections::{Action, Fill, Mode, SECTIONS, Scope, Section, lookup, text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Table,
}

pub enum Load {
    Idle,
    Loading,
    Loaded(Vec<Value>),
    Failed(String),
}

pub struct SectionData {
    pub load: Load,
    pub table: TableState,
    /// The (org, workload) the rows were read for.
    loaded_for: Option<(Option<String>, Option<String>)>,
}

/// A command waiting for its inputs or confirmation.
pub struct Pending {
    pub label: String,
    tokens: &'static [&'static str],
    prompts: &'static [&'static str],
    confirm: Option<&'static str>,
    mode: Mode,
    reselect: bool,
    row: Option<Value>,
    key: &'static str,
    inputs: Vec<String>,
}

pub enum Modal {
    Input {
        pending: Pending,
        buffer: String,
    },
    Confirm {
        question: String,
        args: Vec<String>,
        mode: Mode,
        reselect: bool,
    },
    Output {
        id: u64,
        title: String,
        lines: Vec<String>,
        /// None while running; then the exit summary and whether it succeeded.
        done: Option<(String, bool)>,
        /// Lines scrolled up from the bottom; 0 follows the output.
        scroll: usize,
    },
    Palette {
        buffer: String,
        terminal: bool,
    },
    Help,
}

/// A command to run attached to the terminal, which the event loop does since it owns it.
pub struct Attached {
    pub args: Vec<String>,
    pub reselect: bool,
}

enum Read {
    Section(usize, (Option<String>, Option<String>)),
    ActiveOrg,
    SelectedWl,
}

pub struct App {
    pub runner: Runner,
    tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    pub org: Option<String>,
    pub wl: Option<String>,
    pub section: usize,
    pub focus: Focus,
    pub data: Vec<SectionData>,
    pub modal: Option<Modal>,
    pub status: Option<(String, bool)>,
    pub detail_scroll: u16,
    pub filter: String,
    pub filtering: bool,
    pub quit: bool,
    pub attached: Option<Attached>,
    /// Reads of which org and workload are selected are under way.
    pub reselecting: bool,
    /// The first read of the selection has not finished yet.
    starting: bool,
    next_id: u64,
    reads: HashMap<u64, Read>,
    /// Captured commands still running, and whether to re-read the selection after each.
    jobs: HashMap<u64, bool>,
}

impl App {
    pub fn new(runner: Runner, org: Option<String>, wl: Option<String>) -> Self {
        let (tx, rx) = channel();
        let mut app = App {
            runner,
            tx,
            rx,
            org: org.clone(),
            wl: wl.clone(),
            section: 0,
            focus: Focus::Table,
            data: SECTIONS
                .iter()
                .map(|_| SectionData {
                    load: Load::Idle,
                    table: TableState::default().with_selected(Some(0)),
                    loaded_for: None,
                })
                .collect(),
            modal: None,
            status: None,
            detail_scroll: 0,
            filter: String::new(),
            filtering: false,
            quit: false,
            attached: None,
            reselecting: false,
            starting: true,
            next_id: 0,
            reads: HashMap::new(),
            jobs: HashMap::new(),
        };
        match (org, wl) {
            (None, _) => app.reselect(),
            (Some(_), None) => app.read_selected_wl(),
            (Some(_), Some(_)) => {
                // Start where the work is.
                app.section = section_index("Deploy");
                app.ensure_loaded();
            }
        }
        app
    }

    pub fn current(&self) -> &'static Section {
        &SECTIONS[self.section]
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Global flags for the current selection, unless `args` sets them itself.
    fn scoped(&self, args: Vec<String>, with_org: bool) -> Vec<String> {
        let mut out = Vec::new();
        let has = |flag: &str| {
            args.iter()
                .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
        };
        if with_org
            && !has("--org")
            && let Some(org) = &self.org
        {
            out.extend(["--org".to_string(), org.clone()]);
        }
        if with_org
            && !has("--wl")
            && let Some(wl) = &self.wl
        {
            out.extend(["--wl".to_string(), wl.clone()]);
        }
        out.extend(args);
        out
    }

    // ---- selection ------------------------------------------------------------------------

    /// Re-reads the active org (from `ikit org list`) and then its selected workload.
    pub fn reselect(&mut self) {
        self.reselecting = true;
        let id = self.id();
        self.reads.insert(id, Read::ActiveOrg);
        self.runner
            .read_json(id, vec!["org".into(), "list".into()], self.tx.clone());
    }

    fn read_selected_wl(&mut self) {
        self.reselecting = true;
        let id = self.id();
        self.reads.insert(id, Read::SelectedWl);
        let mut args = vec!["wl".to_string(), "show".to_string()];
        if let Some(org) = &self.org {
            args.splice(0..0, ["--org".to_string(), org.clone()]);
        }
        self.runner.read_json(id, args, self.tx.clone());
    }

    fn selection_changed(&mut self) {
        for d in &mut self.data {
            d.loaded_for = None;
            d.load = Load::Idle;
        }
        self.ensure_loaded();
    }

    // ---- loading --------------------------------------------------------------------------

    fn scope_missing(&self, s: &Section) -> Option<&'static str> {
        match s.scope {
            Scope::Home => None,
            Scope::Org if self.org.is_none() => {
                Some("No org selected: create one or select one in Orgs (press o).")
            }
            Scope::Workload if self.org.is_none() => {
                Some("No org selected: create one or select one in Orgs (press o).")
            }
            Scope::Workload if self.wl.is_none() => {
                Some("No workload selected: create one or select one in Workloads (press w).")
            }
            _ => None,
        }
    }

    /// Lists the current section unless its rows are fresh for the current selection.
    pub fn ensure_loaded(&mut self) {
        let idx = self.section;
        let sel = (self.org.clone(), self.wl.clone());
        let s = self.current();
        if self.scope_missing(s).is_some() || self.reselecting {
            return;
        }
        let d = &self.data[idx];
        if d.loaded_for.as_ref() == Some(&sel) || matches!(d.load, Load::Loading) {
            return;
        }
        self.load(idx);
    }

    pub fn refresh(&mut self) {
        let idx = self.section;
        self.data[idx].loaded_for = None;
        if self.scope_missing(self.current()).is_none() {
            self.load(idx);
        }
    }

    fn load(&mut self, idx: usize) {
        let s = &SECTIONS[idx];
        let sel = (self.org.clone(), self.wl.clone());
        let args = s.list.iter().map(|a| a.to_string()).collect();
        let args = self.scoped(args, s.scope != Scope::Home);
        let id = self.id();
        self.reads.insert(id, Read::Section(idx, sel.clone()));
        let d = &mut self.data[idx];
        d.load = Load::Loading;
        d.loaded_for = Some(sel);
        self.runner.read_json(id, args, self.tx.clone());
    }

    /// Applies results from background jobs.
    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Loaded(id, result) => self.on_loaded(id, result),
            Msg::Line(id, line) => {
                if let Some(Modal::Output {
                    id: mid,
                    lines,
                    scroll,
                    ..
                }) = &mut self.modal
                    && *mid == id
                {
                    lines.push(line);
                    if *scroll > 0 {
                        *scroll += 1;
                    }
                }
            }
            Msg::Done(id, status) => {
                let reselect = self.jobs.remove(&id).unwrap_or(false);
                let (summary, ok) = match status {
                    Ok(s) if s.success() => ("done".to_string(), true),
                    Ok(s) => match s.code() {
                        Some(c) => (format!("exited with status {c}"), false),
                        None => ("stopped by a signal".to_string(), false),
                    },
                    Err(e) => (e, false),
                };
                if let Some(Modal::Output {
                    id: mid,
                    done,
                    lines,
                    ..
                }) = &mut self.modal
                    && *mid == id
                {
                    if ok && reselect {
                        // A selection change says all it has to say in one line.
                        let last = lines.iter().rev().find(|l| !l.trim().is_empty()).cloned();
                        self.modal = None;
                        self.set_info(last.unwrap_or_else(|| "done".into()));
                    } else {
                        *done = Some((summary, ok));
                    }
                }
                self.after_change(ok && reselect);
            }
        }
    }

    fn on_loaded(&mut self, id: u64, result: Result<Value, String>) {
        let Some(read) = self.reads.remove(&id) else {
            return;
        };
        match read {
            Read::Section(idx, sel) => {
                let d = &mut self.data[idx];
                if d.loaded_for.as_ref() != Some(&sel) {
                    return; // stale
                }
                d.load = match result {
                    Ok(v) => {
                        let rows = (SECTIONS[idx].rows)(v);
                        let len = rows.len();
                        let sel = d.table.selected().unwrap_or(0);
                        d.table.select(Some(sel.min(len.saturating_sub(1))));
                        Load::Loaded(rows)
                    }
                    Err(e) => Load::Failed(e),
                };
            }
            Read::ActiveOrg => {
                match result {
                    Ok(Value::Array(orgs)) => {
                        self.org = orgs
                            .iter()
                            .find(|o| o.get("active").and_then(Value::as_bool) == Some(true))
                            .and_then(|o| o.get("name").and_then(Value::as_str))
                            .map(str::to_string);
                    }
                    Ok(_) => {}
                    Err(e) => self.set_error(e),
                }
                if self.org.is_some() {
                    self.read_selected_wl();
                } else {
                    self.wl = None;
                    self.reselecting = false;
                    self.section = section_index("Orgs");
                    self.selection_changed();
                }
            }
            Read::SelectedWl => {
                self.wl = result
                    .ok()
                    .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string));
                self.reselecting = false;
                if std::mem::take(&mut self.starting) && self.wl.is_some() {
                    // Start where the work is.
                    self.section = section_index("Deploy");
                }
                self.selection_changed();
            }
        }
    }

    /// After a command that may have changed state: re-read what is shown.
    fn after_change(&mut self, reselect: bool) {
        if reselect {
            self.reselect();
        } else {
            for d in &mut self.data {
                d.loaded_for = None;
            }
            self.ensure_loaded();
        }
    }

    pub fn after_attached(&mut self, reselect: bool) {
        self.after_change(reselect);
    }

    // ---- rows -----------------------------------------------------------------------------

    /// The current section's rows after the filter, with their index in the full list.
    pub fn visible_rows(&self) -> Vec<&Value> {
        let Load::Loaded(rows) = &self.data[self.section].load else {
            return vec![];
        };
        let needle = self.filter.to_lowercase();
        rows.iter()
            .filter(|r| {
                needle.is_empty()
                    || self
                        .current()
                        .columns
                        .iter()
                        .any(|(_, p)| text(lookup(r, p)).to_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn selected_row(&self) -> Option<&Value> {
        let i = self.data[self.section].table.selected()?;
        self.visible_rows().get(i).copied()
    }

    fn move_row(&mut self, delta: isize) {
        let len = self.visible_rows().len();
        let t = &mut self.data[self.section].table;
        if len == 0 {
            t.select(Some(0));
            return;
        }
        let cur = t.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        t.select(Some(next));
        self.detail_scroll = 0;
    }

    fn set_section(&mut self, idx: usize) {
        if idx != self.section {
            self.section = idx;
            self.filter.clear();
            self.filtering = false;
            self.detail_scroll = 0;
        }
        self.ensure_loaded();
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), true));
    }

    fn set_info(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false));
    }

    // ---- actions --------------------------------------------------------------------------

    fn start_action(&mut self, a: &'static Action) {
        let s = self.current();
        if let Some(missing) = self.scope_missing(s) {
            self.set_error(missing);
            return;
        }
        let row = self.selected_row().cloned();
        if a.needs_selection() && row.is_none() {
            self.set_error(format!("{}: select a row first", a.label));
            return;
        }
        let pending = Pending {
            label: format!("{} › {}", s.title, a.label),
            tokens: a.args,
            prompts: a.inputs,
            confirm: a.confirm,
            mode: a.mode,
            reselect: a.reselect,
            row,
            key: s.key,
            inputs: Vec::new(),
        };
        self.advance(pending);
    }

    /// Moves a pending command on: next input, then confirmation, then running it.
    fn advance(&mut self, pending: Pending) {
        if pending.inputs.len() < pending.prompts.len() {
            self.modal = Some(Modal::Input {
                pending,
                buffer: String::new(),
            });
            return;
        }
        let fill = Fill {
            row: pending.row.as_ref(),
            key: pending.key,
            inputs: &pending.inputs,
            org: self.org.as_deref(),
            wl: self.wl.as_deref(),
        };
        let args = match fill.args(pending.tokens) {
            Ok(a) => a,
            Err(e) => {
                self.modal = None;
                self.set_error(format!("{}: {e}", pending.label));
                return;
            }
        };
        let args = self.scoped(args, true);
        if let Some(q) = pending.confirm {
            let question = fill.text(q).unwrap_or_else(|_| q.to_string());
            self.modal = Some(Modal::Confirm {
                question,
                args,
                mode: pending.mode,
                reselect: pending.reselect,
            });
            return;
        }
        self.execute(args, pending.mode, pending.reselect);
    }

    fn execute(&mut self, args: Vec<String>, mode: Mode, reselect: bool) {
        match mode {
            Mode::Terminal => {
                self.modal = None;
                self.attached = Some(Attached { args, reselect });
            }
            Mode::Capture => {
                let id = self.id();
                let title = self.runner.display(&args);
                self.runner.run_captured(id, args, self.tx.clone());
                self.jobs.insert(id, reselect);
                self.modal = Some(Modal::Output {
                    id,
                    title,
                    lines: Vec::new(),
                    done: None,
                    scroll: 0,
                });
            }
        }
    }

    fn run_palette(&mut self, line: &str, terminal: bool) {
        let line = line.trim();
        let line = line.strip_prefix("ikit ").unwrap_or(line);
        match split_args(line) {
            Ok(args) if args.is_empty() => self.modal = None,
            Ok(args) => {
                // `org use`/`wl use` typed by hand change the selection too.
                let reselect = args
                    .iter()
                    .any(|a| a == "use" || a == "create" || a == "rm");
                let args = self.scoped(args, true);
                self.execute(
                    args,
                    if terminal {
                        Mode::Terminal
                    } else {
                        Mode::Capture
                    },
                    reselect,
                );
            }
            Err(e) => {
                self.modal = None;
                self.set_error(format!("command: {e}"));
            }
        }
    }

    // ---- keys -----------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.modal.is_some() {
                self.modal = None;
            } else {
                self.quit = true;
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.on_modal_key(modal, key);
            return;
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                KeyCode::Up => self.move_row(-1),
                KeyCode::Down => self.move_row(1),
                _ => {}
            }
            self.data[self.section].table.select(Some(0));
            return;
        }
        self.status = None;
        match key.code {
            KeyCode::Char('r') if ctrl => self.refresh(),
            KeyCode::F(5) => self.refresh(),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char(':') => {
                self.modal = Some(Modal::Palette {
                    buffer: String::new(),
                    terminal: false,
                })
            }
            KeyCode::Char('!') => {
                self.modal = Some(Modal::Palette {
                    buffer: String::new(),
                    terminal: true,
                })
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Char('o') => {
                self.set_section(section_index("Orgs"));
                self.focus = Focus::Table;
            }
            KeyCode::Char('w') => {
                self.set_section(section_index("Workloads"));
                self.focus = Focus::Table;
            }
            KeyCode::Esc => self.filter.clear(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Table,
                    Focus::Table => Focus::Sidebar,
                }
            }
            KeyCode::Left => self.focus = Focus::Sidebar,
            KeyCode::Right => self.focus = Focus::Table,
            KeyCode::Up | KeyCode::Char('k') => self.nav(-1),
            KeyCode::Down | KeyCode::Char('j') => self.nav(1),
            KeyCode::PageUp => self.move_row(-10),
            KeyCode::PageDown => self.move_row(10),
            KeyCode::Home => self.move_row(isize::MIN / 2),
            KeyCode::End => self.move_row(isize::MAX / 2),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Enter if self.focus == Focus::Sidebar => self.focus = Focus::Table,
            KeyCode::Enter => self.action_for('\n'),
            KeyCode::Char(c) => self.action_for(c),
            _ => {}
        }
    }

    fn nav(&mut self, delta: isize) {
        match self.focus {
            Focus::Sidebar => {
                let n = SECTIONS.len() as isize;
                let next = (self.section as isize + delta).rem_euclid(n) as usize;
                self.set_section(next);
            }
            Focus::Table => self.move_row(delta),
        }
    }

    fn action_for(&mut self, c: char) {
        if let Some(a) = self.current().actions.iter().find(|a| a.key == c) {
            self.start_action(a);
        } else if c == '\n' {
            // No default action: the details are already on screen.
        } else if let Some(d) = c.to_digit(10)
            && (1..=SECTIONS.len().min(9) as u32).contains(&d)
        {
            self.set_section(d as usize - 1);
        }
    }

    fn on_modal_key(&mut self, modal: Modal, key: KeyEvent) {
        match modal {
            Modal::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter
                ) {
                    self.modal = Some(Modal::Help);
                }
            }
            Modal::Input {
                mut pending,
                mut buffer,
            } => match key.code {
                KeyCode::Esc => self.set_info(format!("{}: cancelled", pending.label)),
                KeyCode::Enter => {
                    pending.inputs.push(std::mem::take(&mut buffer));
                    self.advance(pending);
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.modal = Some(Modal::Input { pending, buffer });
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    self.modal = Some(Modal::Input { pending, buffer });
                }
                _ => self.modal = Some(Modal::Input { pending, buffer }),
            },
            Modal::Confirm {
                question,
                args,
                mode,
                reselect,
            } => match key.code {
                KeyCode::Char('y' | 'Y') => self.execute(args, mode, reselect),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.set_info("cancelled"),
                _ => {
                    self.modal = Some(Modal::Confirm {
                        question,
                        args,
                        mode,
                        reselect,
                    })
                }
            },
            Modal::Palette {
                mut buffer,
                terminal,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.run_palette(&buffer, terminal),
                KeyCode::Tab => {
                    self.modal = Some(Modal::Palette {
                        buffer,
                        terminal: !terminal,
                    })
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    self.modal = Some(Modal::Palette { buffer, terminal });
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    self.modal = Some(Modal::Palette { buffer, terminal });
                }
                _ => self.modal = Some(Modal::Palette { buffer, terminal }),
            },
            Modal::Output {
                id,
                title,
                lines,
                done,
                mut scroll,
            } => {
                let max = lines.len();
                match key.code {
                    // Esc hides a running command too; it carries on and the view refreshes
                    // when it ends.
                    KeyCode::Esc => return,
                    KeyCode::Char('q') | KeyCode::Enter if done.is_some() => return,
                    KeyCode::Up | KeyCode::Char('k') => scroll = (scroll + 1).min(max),
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageUp => scroll = (scroll + 15).min(max),
                    KeyCode::PageDown => scroll = scroll.saturating_sub(15),
                    KeyCode::Home => scroll = max,
                    KeyCode::End => scroll = 0,
                    _ => {}
                }
                self.modal = Some(Modal::Output {
                    id,
                    title,
                    lines,
                    done,
                    scroll,
                });
            }
        }
    }

    /// The prompt shown in an input dialog.
    pub fn input_prompt(pending: &Pending) -> (&'static str, usize, usize) {
        (
            pending.prompts[pending.inputs.len()],
            pending.inputs.len() + 1,
            pending.prompts.len(),
        )
    }
}

pub fn section_index(title: &str) -> usize {
    SECTIONS
        .iter()
        .position(|s| s.title == title)
        .expect("known section")
}
