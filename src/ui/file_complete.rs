//! Shared attachment path completion and directory picker.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::mail::compose::expand_tilde;
use crate::ui::events::AppEvent;
use crate::ui::text_input::TextInput;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    text: String,
    cursor: usize,
    path_start: usize,
    start: usize,
    directory: PathBuf,
    needle: String,
}

impl Query {
    fn new(input: &TextInput, start: usize) -> Option<Self> {
        // Completion replaces the final component, including text after the
        // cursor, so editing a filename cannot duplicate its existing suffix.
        let raw = input.as_str().get(start..)?;
        let cursor = input.cursor().checked_sub(start)?;
        let split = raw.rfind('/').map_or(0, |i| i + 1);
        if cursor < split {
            return None;
        }
        let directory = match &raw[..split] {
            "" => PathBuf::from("."),
            prefix => expand_tilde(prefix),
        };
        Some(Self {
            text: input.as_str().into(),
            cursor: input.cursor(),
            path_start: start,
            start: start + split,
            directory,
            needle: raw[split..cursor].into(),
        })
    }

    fn matches(&self, input: &TextInput) -> bool {
        self.text == input.as_str() && self.cursor == input.cursor()
    }
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    directory: bool,
}

#[derive(Debug)]
struct Reply {
    generation: u64,
    result: Result<Vec<Entry>, String>,
}

#[derive(Debug)]
struct Worker {
    tx: Sender<Request>,
    rx: Receiver<Reply>,
}

#[derive(Debug)]
struct Request {
    generation: u64,
    query: Query,
    reload: bool,
}

#[derive(Debug, Default)]
pub struct FileComplete {
    query: Option<Query>,
    generation: u64,
    worker: Option<Worker>,
    items: Vec<Entry>,
    selected: usize,
    loading: bool,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyDispatch {
    Consumed,
    Submit,
    PassThrough,
}

impl FileComplete {
    pub fn clear(&mut self) {
        self.query = None;
        self.items.clear();
        self.selected = 0;
        self.error = None;
        self.loading = false;
    }

    pub fn refresh(&mut self, input: &TextInput, start: usize, wake: Option<&Sender<AppEvent>>) {
        let Some(query) = Query::new(input, start) else {
            self.clear();
            return;
        };
        if self.query.as_ref() != Some(&query) {
            let reload = self.query.is_none();
            self.generation = self.generation.wrapping_add(1);
            self.items.clear();
            self.selected = 0;
            self.error = None;
            self.loading = true;
            self.query = Some(query.clone());
            if self.worker.is_none() {
                match start_worker(wake.cloned()) {
                    Ok(worker) => self.worker = Some(worker),
                    Err(e) => {
                        self.error = Some(e.to_string());
                        self.loading = false;
                    }
                }
            }
            if let Some(worker) = &self.worker
                && worker
                    .tx
                    .send(Request {
                        generation: self.generation,
                        query,
                        reload,
                    })
                    .is_err()
            {
                self.error = Some("File picker worker stopped".into());
                self.loading = false;
            }
        }
        if let Some(worker) = &self.worker {
            while let Ok(reply) = worker.rx.try_recv() {
                if reply.generation != self.generation {
                    continue;
                }
                self.loading = false;
                match reply.result {
                    Ok(items) => self.items = items,
                    Err(error) => self.error = Some(error),
                }
            }
        }
    }

    pub fn handle_key(&mut self, input: &mut TextInput, k: KeyEvent) -> KeyDispatch {
        if !self.query.as_ref().is_some_and(|q| q.matches(input)) {
            return KeyDispatch::PassThrough;
        }
        let control = k.modifiers.contains(KeyModifiers::CONTROL);
        let delta = match k.code {
            KeyCode::Down => Some(1),
            KeyCode::Up | KeyCode::BackTab => Some(-1),
            KeyCode::Char('n') if control => Some(1),
            KeyCode::Char('p') if control => Some(-1),
            _ => None,
        };
        if let Some(delta) = delta {
            if !self.items.is_empty() {
                self.selected =
                    (self.selected as isize + delta).rem_euclid(self.items.len() as isize) as usize;
            }
            return KeyDispatch::Consumed;
        }
        if matches!(k.code, KeyCode::Tab | KeyCode::Enter) {
            let query = self.query.as_ref().expect("checked above");
            if &query.text[query.path_start..] == "~" {
                input.replace_range(
                    query.path_start..input.as_str().len(),
                    "~/",
                    query.path_start + 2,
                );
                self.clear();
                return KeyDispatch::Consumed;
            }
            let Some(entry) = self.items.get(self.selected) else {
                // Keep a fast Enter from submitting an unfinished fuzzy query
                // as a literal path and closing the command line with an error.
                return if k.code == KeyCode::Tab || self.loading {
                    KeyDispatch::Consumed
                } else {
                    KeyDispatch::PassThrough
                };
            };
            let query = self.query.as_ref().expect("checked above");
            let replacement = format!("{}{}", entry.name, if entry.directory { "/" } else { "" });
            let submit = k.code == KeyCode::Enter && !entry.directory;
            input.replace_range(
                query.start..input.as_str().len(),
                &replacement,
                query.start + replacement.len(),
            );
            self.clear();
            return if submit {
                KeyDispatch::Submit
            } else {
                KeyDispatch::Consumed
            };
        }
        KeyDispatch::PassThrough
    }

    pub fn draw(&self, f: &mut Frame, anchor: Rect, bounds: Rect) {
        if self.query.is_none() || bounds.width < 4 || bounds.height < 3 {
            return;
        }
        let below = bounds.bottom().saturating_sub(anchor.bottom());
        let above = anchor.y.saturating_sub(bounds.y);
        let down = below >= above || below >= 10;
        let available = if down { below } else { above };
        let height = ((self.items.len().clamp(1, 8) + 2) as u16).min(available);
        if height < 3 {
            return;
        }
        let width = bounds.width.min(76);
        let area = Rect::new(
            anchor
                .x
                .min(bounds.right().saturating_sub(width))
                .max(bounds.x),
            if down {
                anchor.bottom()
            } else {
                anchor.y - height
            },
            width,
            height,
        );
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Files · Tab complete · Enter choose ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let first = self
            .selected
            .saturating_sub(inner.height.saturating_sub(1) as usize);
        let lines: Vec<Line<'static>> = if self.loading {
            vec![Line::from(" Loading…")]
        } else if let Some(error) = &self.error {
            vec![Line::from(error.clone()).style(Style::default().fg(Color::Red))]
        } else if self.items.is_empty() {
            vec![Line::from(" No matching files")]
        } else {
            self.items
                .iter()
                .enumerate()
                .skip(first)
                .take(inner.height as usize)
                .map(|(i, entry)| {
                    let name: String = entry
                        .name
                        .chars()
                        .map(|c| if c.is_control() { '�' } else { c })
                        .collect();
                    let line = Line::from(format!(
                        " {}{}",
                        name,
                        if entry.directory { "/" } else { "" }
                    ));
                    if i == self.selected {
                        line.style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::REVERSED),
                        )
                    } else {
                        line
                    }
                })
                .collect()
        };
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn start_worker(wake: Option<Sender<AppEvent>>) -> std::io::Result<Worker> {
    let (tx, requests) = mpsc::channel::<Request>();
    let (results, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("epost-files".into())
        .spawn(move || {
            let mut cache: Option<(PathBuf, Vec<Entry>)> = None;
            while let Ok(mut request) = requests.recv() {
                // Typing during a directory read only queues the latest query.
                while let Ok(newer) = requests.try_recv() {
                    if request.reload {
                        cache = None;
                    }
                    request = newer;
                }
                let Request {
                    generation,
                    query,
                    reload,
                } = request;
                let result = (|| {
                    if reload
                        || cache
                            .as_ref()
                            .is_none_or(|(path, _)| path != &query.directory)
                    {
                        cache = Some((query.directory.clone(), read_directory(&query.directory)?));
                    }
                    Ok(rank(
                        &cache.as_ref().expect("populated above").1,
                        &query.needle,
                    ))
                })();
                if results.send(Reply { generation, result }).is_err() {
                    break;
                }
                if let Some(tx) = &wake {
                    let _ = tx.send(AppEvent::Wake);
                }
            }
        })?;
    Ok(Worker { tx, rx })
}

fn read_directory(path: &std::path::Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Follow symlinks for navigation, but omit sockets and other special files.
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() || metadata.is_dir() {
            entries.push(Entry {
                name,
                directory: metadata.is_dir(),
            });
        }
    }
    entries.push(Entry {
        name: "..".into(),
        directory: true,
    });
    Ok(entries)
}

fn rank(entries: &[Entry], needle: &str) -> Vec<Entry> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        needle,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut scored: Vec<_> = entries
        .iter()
        .filter(|e| !e.name.starts_with('.') || needle.starts_with('.'))
        .filter_map(|entry| {
            pattern
                .score(
                    Utf32String::from(entry.name.as_str()).slice(..),
                    &mut matcher,
                )
                .map(|score| (entry, score))
        })
        .collect();
    scored.sort_by(|(a, sa), (b, sb)| {
        // A fully typed filename must win even when a longer directory gets
        // the same fuzzy score, or Enter could open the wrong path.
        (b.name == needle)
            .cmp(&(a.name == needle))
            .then_with(|| sb.cmp(sa))
            .then_with(|| b.directory.cmp(&a.directory))
            .then_with(|| a.name.cmp(&b.name))
    });
    scored.into_iter().map(|(entry, _)| entry.clone()).collect()
}

pub fn command_start(input: &TextInput) -> Option<usize> {
    let trimmed = input.as_str().trim_start();
    let rest = trimmed.strip_prefix("attach")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(input.as_str().len() - rest.trim_start().len())
}

pub fn poll(app: &mut crate::ui::app::App) {
    use crate::ui::app::Mode;
    use crate::ui::compose::ComposeField;

    let wake = app.event_tx.clone();
    if app.mode == Mode::Command
        && app.active_compose().is_some()
        && let Some(start) = command_start(&app.cmdline)
    {
        app.file_completion
            .refresh(&app.cmdline, start, wake.as_ref());
    } else {
        app.file_completion.clear();
    }
    let normal = app.mode == Mode::Normal;
    if let Some(c) = app.active_compose_mut() {
        if normal
            && c.focused == ComposeField::Attach
            && c.confirm_close.is_none()
            && c.from_picker.is_none()
            && c.editor.is_none()
            && let Some(input) = &c.attach.adding
        {
            c.attach.completion.refresh(input, 0, wake.as_ref());
        } else {
            c.attach.completion.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ComposeWrap, Config as AppConfig};
    use crate::mail::compose::Draft;
    use crate::ui::app::{App, Mode, Screen};
    use crate::ui::compose::{ComposeField, ComposeScreen};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn settle(state: &mut FileComplete, input: &TextInput, start: usize) {
        state.refresh(input, start, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while state.loading {
            std::thread::sleep(std::time::Duration::from_millis(1));
            state.refresh(input, start, None);
            assert!(
                std::time::Instant::now() < deadline,
                "file picker timed out"
            );
        }
    }

    fn names(items: &[Entry]) -> Vec<&str> {
        items.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn directory_filters_special_files_and_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("résumé final.pdf"), "file").unwrap();
        std::fs::create_dir(dir.path().join("Documents")).unwrap();
        std::os::unix::fs::symlink("Documents", dir.path().join("linked")).unwrap();
        std::os::unix::fs::symlink("missing", dir.path().join("broken")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("socket")).unwrap();
        let entries = read_directory(dir.path()).unwrap();
        assert!(names(&entries).contains(&"résumé final.pdf"));
        assert!(entries.iter().any(|e| e.name == "linked" && e.directory));
        assert!(!names(&entries).contains(&"socket"));
        assert!(!names(&entries).contains(&"broken"));
    }

    #[test]
    fn fuzzy_matching_ranks_exact_names_and_hides_dotfiles() {
        let entries: Vec<_> = [
            "report.pdf",
            "revised-project.pdf",
            ".report.pdf",
            "other.txt",
        ]
        .into_iter()
        .map(|name| Entry {
            name: name.into(),
            directory: false,
        })
        .collect();
        assert_eq!(
            names(&rank(&entries, "rpt")),
            ["report.pdf", "revised-project.pdf"]
        );
        assert_eq!(names(&rank(&entries, "report.pdf")), ["report.pdf"]);
        assert_eq!(names(&rank(&entries, ".rpt")), [".report.pdf"]);
        assert!(rank(&entries, "RPT").is_empty());
    }

    #[test]
    fn exact_file_precedes_a_directory_with_the_same_prefix() {
        let entries = [
            Entry {
                name: "a".into(),
                directory: false,
            },
            Entry {
                name: "a-folder".into(),
                directory: true,
            },
        ];
        assert_eq!(names(&rank(&entries, "a")), ["a", "a-folder"]);
    }

    #[test]
    fn worker_wakes_ui_and_reports_unreadable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let input = TextInput::from_string(format!("{}/missing/", dir.path().display()));
        let (tx, rx) = mpsc::channel();
        let mut state = FileComplete::default();
        state.refresh(&input, 0, Some(&tx));
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(),
            AppEvent::Wake
        ));
        state.refresh(&input, 0, Some(&tx));
        assert!(!state.loading);
        assert!(state.error.as_ref().unwrap().contains("missing"));
        assert!(state.items.is_empty());
    }

    #[test]
    fn navigation_scrolls_to_candidates_beyond_first_page() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            std::fs::write(dir.path().join(format!("report-{i:02}.pdf")), "file").unwrap();
        }
        let mut input = TextInput::from_string(format!("{}/", dir.path().display()));
        let mut state = FileComplete::default();
        settle(&mut state, &input, 0);
        let previous = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(
            state.handle_key(&mut input, previous),
            KeyDispatch::Consumed
        );
        assert_eq!(state.selected, 11);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| state.draw(f, Rect::new(0, 23, 80, 1), f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("report-11.pdf"));
        assert!(!text.contains("report-00.pdf"));
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Tab)),
            KeyDispatch::Consumed
        );
        assert!(input.as_str().ends_with("/report-11.pdf"));
    }

    #[test]
    fn complete_unicode_filename_with_spaces_and_existing_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("résumé final.pdf"), "file").unwrap();
        let mut input =
            TextInput::from_string(format!("attach {}/ré-old.pdf", dir.path().display()));
        input.set_cursor_char(
            input
                .as_str()
                .find("-old")
                .map(|i| input.as_str()[..i].chars().count())
                .unwrap(),
        );
        let start = command_start(&input).unwrap();
        let mut state = FileComplete::default();
        settle(&mut state, &input, start);
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Tab)),
            KeyDispatch::Consumed
        );
        assert_eq!(
            input.as_str(),
            format!("attach {}/résumé final.pdf", dir.path().display())
        );
        assert_eq!(input.cursor(), input.as_str().len());
    }

    #[test]
    fn tab_completes_directory_and_enter_only_submits_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Documents")).unwrap();
        std::fs::write(dir.path().join("Documents/report.pdf"), "file").unwrap();
        let mut input = TextInput::from_string(format!("{}/Dcm", dir.path().display()));
        let mut state = FileComplete::default();
        settle(&mut state, &input, 0);
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Enter)),
            KeyDispatch::Consumed
        );
        assert!(input.as_str().ends_with("/Documents/"));
        settle(&mut state, &input, 0);
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Enter)),
            KeyDispatch::Submit
        );
        assert!(input.as_str().ends_with("/Documents/report.pdf"));
    }

    #[test]
    fn completion_does_not_accept_stale_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.pdf"), "file").unwrap();
        let mut input = TextInput::from_string(format!("{}/", dir.path().display()));
        let mut state = FileComplete::default();
        settle(&mut state, &input, 0);
        input.insert_str("missing");
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Enter)),
            KeyDispatch::PassThrough
        );
        assert!(input.as_str().ends_with("/missing"));
    }

    #[test]
    fn enter_keeps_the_query_open_while_results_are_loading() {
        let mut input = TextInput::from_string("attach /tmp/rpt");
        let mut state = FileComplete {
            query: Query::new(&input, 7),
            loading: true,
            ..Default::default()
        };
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Enter)),
            KeyDispatch::Consumed
        );
        assert_eq!(input.as_str(), "attach /tmp/rpt");
    }

    #[test]
    fn late_worker_reply_cannot_replace_newer_query() {
        let input = TextInput::from_string("new");
        let (tx, _requests) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let mut state = FileComplete {
            query: Query::new(&input, 0),
            generation: 2,
            loading: true,
            worker: Some(Worker { tx, rx }),
            ..Default::default()
        };
        results
            .send(Reply {
                generation: 1,
                result: Ok(vec![Entry {
                    name: "old".into(),
                    directory: false,
                }]),
            })
            .unwrap();
        state.refresh(&input, 0, None);
        assert!(state.items.is_empty());
        assert!(state.loading);
        results
            .send(Reply {
                generation: 2,
                result: Ok(vec![Entry {
                    name: "new".into(),
                    directory: false,
                }]),
            })
            .unwrap();
        state.refresh(&input, 0, None);
        assert_eq!(names(&state.items), ["new"]);
        assert!(!state.loading);
    }

    #[test]
    fn reopening_picker_refreshes_directory_contents() {
        let dir = tempfile::tempdir().unwrap();
        let input = TextInput::from_string(format!("{}/", dir.path().display()));
        let mut state = FileComplete::default();
        settle(&mut state, &input, 0);
        assert!(state.items.is_empty());
        state.clear();
        std::fs::write(dir.path().join("new.pdf"), "file").unwrap();
        settle(&mut state, &input, 0);
        assert_eq!(names(&state.items), ["new.pdf"]);
    }

    #[test]
    fn bare_tilde_can_be_completed_into_home_directory() {
        let mut input = TextInput::from_string("~");
        let mut state = FileComplete::default();
        state.refresh(&input, 0, None);
        assert_eq!(
            state.handle_key(&mut input, key(KeyCode::Tab)),
            KeyDispatch::Consumed
        );
        assert_eq!(input.as_str(), "~/");
    }

    fn compose_app() -> (App, AppConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig::default();
        let mut app = App::new(&cfg, dir.path().join("cache.sqlite"), None, None);
        let screen = ComposeScreen::from_draft(
            Draft::new_blank("test", "a@example.org"),
            ComposeWrap::default(),
        )
        .unwrap();
        app.screens.push(Screen::Compose(Box::new(screen)));
        app.active = 1;
        (app, cfg, dir)
    }

    #[test]
    fn command_and_inline_picker_attach_the_same_file() {
        for command in [false, true] {
            let (mut app, cfg, dir) = compose_app();
            let path = dir.path().join("annual report.pdf");
            std::fs::write(&path, "file").unwrap();
            let raw = format!("{}/anrpt", dir.path().display());
            if command {
                app.mode = Mode::Command;
                app.cmdline = TextInput::from_string(format!("attach {raw}"));
                let start = command_start(&app.cmdline).unwrap();
                settle(&mut app.file_completion, &app.cmdline, start);
            } else {
                crate::ui::cmdline::dispatch("attach", &mut app, &cfg);
                let c = app.active_compose_mut().unwrap();
                assert_eq!(c.focused, ComposeField::Attach);
                c.attach.adding = Some(TextInput::from_string(raw));
                settle(
                    &mut c.attach.completion,
                    c.attach.adding.as_ref().unwrap(),
                    0,
                );
            }
            crate::ui::keys::handle(&mut app, &cfg, key(KeyCode::Enter));
            assert_eq!(app.active_compose().unwrap().attachments, [path]);
            assert_eq!(app.mode, Mode::Normal);
        }
    }

    #[test]
    fn paste_and_clipboard_prefix_do_not_accept_a_file() {
        let (mut app, cfg, dir) = compose_app();
        std::fs::write(dir.path().join("report.pdf"), "file").unwrap();
        crate::ui::cmdline::dispatch("attach", &mut app, &cfg);
        let c = app.active_compose_mut().unwrap();
        c.attach.adding = Some(TextInput::from_string(format!("{}/", dir.path().display())));
        settle(
            &mut c.attach.completion,
            c.attach.adding.as_ref().unwrap(),
            0,
        );
        crate::ui::keys::handle(
            &mut app,
            &cfg,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        crate::ui::keys::handle(&mut app, &cfg, key(KeyCode::Enter));
        assert!(app.active_compose().unwrap().attachments.is_empty());
        crate::ui::paste::insert(
            &mut app,
            &cfg,
            "report.pdf",
            crate::ui::paste::Placement::Cursor,
        );
        poll(&mut app);
        let c = app.active_compose().unwrap();
        assert!(
            c.attach
                .adding
                .as_ref()
                .unwrap()
                .as_str()
                .ends_with("/report.pdf")
        );
        assert!(c.attachments.is_empty());
    }

    #[test]
    fn escape_cancels_without_attaching_and_popup_fits_small_terminals() {
        let (mut app, cfg, dir) = compose_app();
        std::fs::write(dir.path().join("file.pdf"), "file").unwrap();
        crate::ui::cmdline::dispatch("attach", &mut app, &cfg);
        let c = app.active_compose_mut().unwrap();
        c.attach.adding = Some(TextInput::from_string(format!("{}/", dir.path().display())));
        settle(
            &mut c.attach.completion,
            c.attach.adding.as_ref().unwrap(),
            0,
        );
        for (width, height) in [(1, 1), (8, 3), (32, 12), (80, 24)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| {
                    c.attach
                        .completion
                        .draw(f, Rect::new(0, height - 1, width, 1), f.area())
                })
                .unwrap();
        }
        crate::ui::keys::handle(&mut app, &cfg, key(KeyCode::Esc));
        let c = app.active_compose().unwrap();
        assert!(c.attachments.is_empty());
        assert!(c.attach.adding.is_none());
        assert!(c.attach.completion.query.is_none());
    }

    #[test]
    fn command_detection_only_handles_attachment_arguments() {
        assert_eq!(
            command_start(&TextInput::from_string("  attach   ~/file")),
            Some(11)
        );
        assert_eq!(command_start(&TextInput::from_string("attach ")), Some(7));
        assert_eq!(
            command_start(&TextInput::from_string("attachment file")),
            None
        );
        assert_eq!(command_start(&TextInput::from_string("detach 1")), None);
    }
}
