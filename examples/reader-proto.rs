//! PROTOTYPE for wayfinder ticket #40 — not production code, never merged to main.
//!
//! Judge the streaming Markdown reader's display behaviour with a scripted stream:
//! no network, no cache, no config. Run one command:
//!
//! ```sh
//! cargo run --example reader-proto             # interactive
//! cargo run --example reader-proto -- --selftest
//! ```
//!
//! Premises already decided elsewhere: only the first incomplete block of the
//! ordered prefix streams (later blocks appear whole); a failed live block is
//! replaced whole by its pre-converted original; the reader owns the terminal
//! before translation starts.

use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tui_markdown::{from_str_with_options, Options};

const FIRST_BLOCK_DELAY: Duration = Duration::from_millis(1500);
const BLOCK_GAP: Duration = Duration::from_millis(350);
const CHUNK_INTERVAL: Duration = Duration::from_millis(65);
const CHARS_PER_CHUNK: usize = 2;

struct Spec {
    full: &'static str,
    fallback: &'static str,
}

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            full: "── man-ai: translation (Chinese) ──",
            fallback: "── man-ai: translation (Chinese) ──",
        },
        Spec {
            full: "## 名称\n\ntar — GNU 归档工具",
            fallback: "## 名称\n\ntar — GNU 归档工具",
        },
        Spec {
            full: "## 概要\n\n**tar** [*选项*] [*文件*]…",
            fallback: "## 概要\n\ntar [选项] [文件]…",
        },
        Spec {
            full: "## 描述\n\n**tar** 把多个文件或目录存进一个归档文件（*archive*），也可以从归档中取出文件。归档文件通常是 tar 格式，也可以配合 `-z`、`-j` 或 `--zstd` 在读写时调用压缩程序。\n\nGNU **tar** 的选项可以分成三类：操作选项决定要做什么，如 `--create`、`--extract` 或 `--list`；文件选择选项决定处理哪些文件；其它选项调整输出、压缩与校验行为。\n\n归档文件内部保存文件名、权限、属主、时间戳与文件内容。解包时 **tar** 按归档记录的路径还原文件，因此归档里的绝对路径与 `..` 会被剥除，避免写到归档目录之外。",
            fallback: "## 描述\n\ntar 把多个文件或目录存进一个归档文件，也可以从归档中取出文件。归档文件通常是 tar 格式，可以配合压缩程序使用。",
        },
        Spec {
            full: "```sh\n$ tar -cf archive.tar one.txt two.txt\n$ tar -tvf archive.tar\n-rw-r--r-- user/group 0 2024-01-01 00:00 one.txt\n```",
            fallback: "```\ntar -cf archive.tar one.txt two.txt\ntar -tvf archive.tar\n```",
        },
        Spec {
            full: "## 选项\n\n- **-c, --create**：创建新的归档文件。\n- **-x, --extract**：从归档中取出文件，保存到当前目录。\n- **-t, --list**：列出归档内容，不改动文件系统。\n- **-f, --file=*ARCHIVE***：指定归档文件名，通常紧跟操作选项。\n- **--exclude=*PATTERN***：跳过匹配给定模式的文件名。",
            fallback: "## 选项\n\n-c, --create  创建新的归档文件。\n-x, --extract 从归档中取出文件。\n-t, --list    列出归档内容。\n-f, --file    指定归档文件名。",
        },
        Spec {
            full: "── man-ai: translated (Chinese) ──",
            fallback: "── man-ai: translated (Chinese) ──",
        },
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveMode {
    Styled,
    HideUnclosed,
    Plain,
}

impl LiveMode {
    fn label(self) -> &'static str {
        match self {
            Self::Styled => "styled",
            Self::HideUnclosed => "hide-unclosed",
            Self::Plain => "plain",
        }
    }
}

enum Status {
    Normal,
    Input(String),
}

struct App {
    specs: Vec<Spec>,
    chunk_lists: Vec<Vec<String>>,
    chunk_pos: Vec<usize>,
    done: Vec<Option<String>>,
    live: Option<usize>,
    live_text: String,
    next_chunk_at: Instant,
    next_block_at: Instant,
    started: Instant,
    ended: Option<Instant>,
    output_tokens: u64,
    cached_input: u64,
    uncached_input: u64,
    mode: LiveMode,
    follow: bool,
    offset: usize,
    max_offset: usize,
    viewport: usize,
    status_variant: u8,
    placeholder: bool,
    failure: Option<&'static str>,
    status: Status,
    matches: Vec<usize>,
    match_index: usize,
    flat: Vec<String>,
    content_width: u16,
}

impl App {
    fn new() -> Self {
        let specs = specs();
        let chunk_lists: Vec<Vec<String>> = specs
            .iter()
            .map(|spec| {
                let chars: Vec<char> = spec.full.chars().collect();
                chars
                    .chunks(CHARS_PER_CHUNK)
                    .map(|chunk| chunk.iter().collect())
                    .collect()
            })
            .collect();
        let n = specs.len();
        let now = Instant::now();
        Self {
            specs,
            chunk_lists,
            chunk_pos: vec![0; n],
            done: (0..n).map(|_| None).collect(),
            live: None,
            live_text: String::new(),
            next_chunk_at: now,
            next_block_at: now + FIRST_BLOCK_DELAY,
            started: now,
            ended: None,
            output_tokens: 0,
            cached_input: 1312,
            uncached_input: 486,
            mode: LiveMode::Styled,
            follow: true,
            offset: 0,
            max_offset: 0,
            viewport: 1,
            status_variant: 0,
            placeholder: true,
            failure: None,
            status: Status::Normal,
            matches: Vec::new(),
            match_index: 0,
            flat: Vec::new(),
            content_width: 80,
        }
    }

    fn restart(&mut self) {
        let mode = self.mode;
        let status_variant = self.status_variant;
        let placeholder = self.placeholder;
        *self = Self::new();
        self.mode = mode;
        self.status_variant = status_variant;
        self.placeholder = placeholder;
    }

    fn tick(&mut self, now: Instant) {
        if let Some(index) = self.live {
            while now >= self.next_chunk_at && self.chunk_pos[index] < self.chunk_lists[index].len()
            {
                let chunk_pos = self.chunk_pos[index];
                self.live_text
                    .push_str(&self.chunk_lists[index][chunk_pos].clone());
                self.chunk_pos[index] += 1;
                self.output_tokens += 1;
                self.next_chunk_at += CHUNK_INTERVAL;
            }
            if self.chunk_pos[index] == self.chunk_lists[index].len() {
                self.finish_live(index);
                self.next_block_at = now + BLOCK_GAP;
            }
            return;
        }
        let next_index = self.done.iter().take_while(|slot| slot.is_some()).count();
        if next_index < self.specs.len() && now >= self.next_block_at {
            self.live = Some(next_index);
            self.next_chunk_at = now;
        }
    }

    fn finish_live(&mut self, index: usize) {
        self.done[index] = Some(std::mem::take(&mut self.live_text));
        self.live = None;
        if self.done.iter().all(Option::is_some) {
            self.ended = Some(Instant::now());
        }
    }

    fn fail_live(&mut self) {
        let Some(index) = self.live else { return };
        let fallback = self.specs[index].fallback.to_string();
        self.done[index] = Some(fallback);
        self.live = None;
        self.failure = Some("backend error");
        self.live_text.clear();
        self.next_block_at = Instant::now() + BLOCK_GAP;
        if self.done.iter().all(Option::is_some) {
            self.ended = Some(Instant::now());
        }
    }

    fn elapsed(&self) -> f64 {
        self.ended
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
            .as_secs_f64()
    }

    fn tokens_per_second(&self) -> f64 {
        let elapsed = self.elapsed();
        if elapsed <= 0.0 {
            0.0
        } else {
            self.output_tokens as f64 / elapsed
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        self.follow = false;
        self.offset = (self.offset as isize + delta).max(0) as usize;
        if self.offset >= self.max_offset {
            self.offset = self.max_offset;
            self.follow = true;
        }
    }

    fn page_text(&self, width: u16) -> Text<'static> {
        let options = Options::default().table_width(width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, spec) in self.specs.iter().enumerate() {
            let piece: Text<'static> = if let Some(done) = &self.done[i] {
                own(from_str_with_options(done, &options))
            } else if self.live == Some(i) {
                match self.mode {
                    LiveMode::Styled => own(from_str_with_options(&self.live_text, &options)),
                    LiveMode::HideUnclosed => {
                        own(from_str_with_options(&hide_unclosed(&self.live_text), &options))
                    }
                    LiveMode::Plain => Text::raw(self.live_text.clone()),
                }
            } else {
                continue;
            };
            if !lines.is_empty() && !spec.full.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.extend(piece.lines);
        }
        if lines.is_empty() && self.placeholder {
            lines.push(Line::styled(
                "Translating… (waiting for the first block)",
                Style::new().fg(Color::DarkGray),
            ));
        }
        Text::from(lines)
    }

    fn refresh_flat(&mut self, text: &Text<'static>) {
        self.flat = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
    }

    fn commit_search(&mut self) {
        let Status::Input(query) = &self.status else {
            return;
        };
        let query = query.trim().to_string();
        if query.is_empty() {
            self.status = Status::Normal;
            return;
        }
        self.matches = self
            .flat
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.match_index = 0;
        self.status = Status::Normal;
        self.jump_to_match();
    }

    fn jump_to_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let line_index = self.matches[self.match_index];
        let prefix: Vec<Line<'static>> = self.flat[..line_index]
            .iter()
            .map(|line| Line::raw(line.clone()))
            .collect();
        let height = Paragraph::new(Text::from(prefix)).line_count(self.content_width);
        self.follow = false;
        self.offset = height.min(self.max_offset);
    }

    fn next_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        if forward {
            self.match_index = (self.match_index + 1) % self.matches.len();
        } else {
            self.match_index = (self.match_index + self.matches.len() - 1) % self.matches.len();
        }
        self.jump_to_match();
    }
}

fn own(text: Text<'_>) -> Text<'static> {
    Text::from(
        text.lines
            .into_iter()
            .map(|line| {
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|span| Span::styled(span.content.into_owned(), span.style))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    )
}

// Display-only heuristic for the hide-unclosed mode. Odd marker counts mean the
// last occurrence is an unmatched opener; removing it hides the dangling
// marker. Unclosed fenced code blocks are not handled.
fn hide_unclosed(text: &str) -> String {
    let mut out = text.to_string();
    for marker in ["**", "`", "*"] {
        if out.matches(marker).count() % 2 == 1
            && let Some(pos) = out.rfind(marker)
        {
            out.replace_range(pos..pos + marker.len(), "");
        }
    }
    out
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let status_height = if app.status_variant == 2 { 2 } else { 1 };
    let [content, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(status_height),
    ])
    .areas(area);

    let text = app.page_text(content.width);
    let total = Paragraph::new(text.clone()).line_count(content.width);
    app.viewport = content.height as usize;
    app.max_offset = total.saturating_sub(app.viewport);
    if app.follow {
        app.offset = app.max_offset;
    }
    app.offset = app.offset.min(app.max_offset);
    app.refresh_flat(&text);
    app.content_width = content.width;

    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.offset as u16, 0)),
        content,
    );
    frame.render_widget(Paragraph::new(status_lines(app)), status_area);
}

fn status_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    if let Status::Input(query) = &app.status {
        return vec![Line::from(vec![
            Span::styled("/", dim),
            Span::styled(query.clone(), Style::new()),
            Span::styled("▏", Style::new().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled("  Enter search · Esc cancel", dim),
        ])];
    }

    let mut metrics = format!(
        "elapsed {:.1}s · out {} tok · {:.1} tok/s · in {} cached + {} uncached",
        app.elapsed(),
        app.output_tokens,
        app.tokens_per_second(),
        app.cached_input,
        app.uncached_input
    );
    if !app.matches.is_empty() {
        metrics.push_str(&format!(
            " · match {}/{}",
            app.match_index + 1,
            app.matches.len()
        ));
    }
    if let Some(reason) = app.failure {
        metrics.push_str(&format!(" · fallback: {reason}"));
    }
    let live = format!(
        "q quit · ↑↓ PgUp/PgDn space scroll · wheel · / search n/N · 1/2/3 live:{} · a follow:{} · e empty:{} · s status:{} · f fail · r replay",
        app.mode.label(),
        if app.follow { "on" } else { "off" },
        if app.placeholder { "on" } else { "off" },
        app.status_variant,
    );

    match app.status_variant {
        0 => vec![Line::styled(metrics, dim)],
        1 => vec![Line::from(vec![
            Span::styled(metrics, dim),
            Span::styled(" · ", dim),
            Span::styled(live, dim),
        ])],
        _ => vec![
            Line::styled(metrics, dim),
            Line::styled(live, Style::new().fg(Color::DarkGray)),
        ],
    }
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if let Status::Input(_) = &app.status {
        match code {
            KeyCode::Esc => app.status = Status::Normal,
            KeyCode::Enter => app.commit_search(),
            KeyCode::Backspace => {
                if let Status::Input(query) = &mut app.status {
                    query.pop();
                }
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Status::Input(query) = &mut app.status {
                    query.push(c);
                }
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                app.status = Status::Normal;
            }
            _ => {}
        }
        return false;
    }

    match code {
        KeyCode::Char('q') => return true,
        KeyCode::Up | KeyCode::Char('k') => app.scroll_by(-1),
        KeyCode::Down | KeyCode::Char('j') => app.scroll_by(1),
        KeyCode::PageUp => app.scroll_by(-(app.viewport.saturating_sub(1) as isize)),
        KeyCode::PageDown | KeyCode::Char(' ') => {
            app.scroll_by(app.viewport.saturating_sub(1) as isize)
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.follow = false;
            app.offset = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            app.follow = true;
            app.offset = app.max_offset;
        }
        KeyCode::Char('/') => app.status = Status::Input(String::new()),
        KeyCode::Char('n') => app.next_match(true),
        KeyCode::Char('N') => app.next_match(false),
        KeyCode::Char('1') => app.mode = LiveMode::Styled,
        KeyCode::Char('2') => app.mode = LiveMode::HideUnclosed,
        KeyCode::Char('3') => app.mode = LiveMode::Plain,
        KeyCode::Char('a') => app.follow = !app.follow,
        KeyCode::Char('e') => app.placeholder = !app.placeholder,
        KeyCode::Char('s') => app.status_variant = (app.status_variant + 1) % 3,
        KeyCode::Char('f') => app.fail_live(),
        KeyCode::Char('r') => app.restart(),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return true,
        _ => {}
    }
    false
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    let mut app = App::new();
    loop {
        app.tick(Instant::now());
        terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(25))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(&mut app, key.code, key.modifiers)
        {
            return Ok(());
        }
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_by(-3),
                    MouseEventKind::ScrollDown => app.scroll_by(3),
                    _ => {}
                },
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(&mut app, key.code, key.modifiers) {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }
}

fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    let mut row = String::new();
    for x in 0..buffer.area.width {
        if let Some(cell) = buffer.cell((x, y)) {
            row.push_str(cell.symbol());
        }
    }
    row
}

fn selftest() {
    let mut app = App::new();
    app.next_block_at = Instant::now();
    for (i, spec) in app.specs.iter().enumerate() {
        app.done[i] = Some(spec.full.to_string());
    }
    app.ended = Some(Instant::now());

    for width in [40, 80, 120] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height).map(|y| buffer_row(buffer, y)).collect();
        let screen = rows.join("\n");
        assert!(screen.contains("elapsed"), "status line missing at width {width}");
        assert!(screen.contains('归'), "content missing at width {width}");
        if width >= 80 {
            assert!(screen.contains("tok/s"), "metrics missing at width {width}");
        }
    }

    // Live block mid-emphasis: styled keeps the unmatched marker literal,
    // hide-unclosed removes it, plain shows the source without styling.
    let mut app = App::new();
    app.done[0] = Some(app.specs[0].full.to_string());
    app.done[1] = Some(app.specs[1].full.to_string());
    app.live = Some(2);
    app.live_text = "## 概要\n\nGNU **tar 是一个".to_string();
    for mode in [LiveMode::Styled, LiveMode::HideUnclosed, LiveMode::Plain] {
        app.mode = mode;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| buffer_row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        match mode {
            LiveMode::Styled | LiveMode::Plain => assert!(screen.contains("**tar")),
            LiveMode::HideUnclosed => assert!(!screen.contains("**tar")),
        }
    }

    // Search jumps into the page.
    let mut app = App::new();
    for (i, spec) in app.specs.iter().enumerate() {
        app.done[i] = Some(spec.full.to_string());
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    app.status = Status::Input("选项".to_string());
    app.commit_search();
    assert!(!app.matches.is_empty(), "search found no match");
    assert!(app.max_offset > 0, "page is not scrollable");
    assert!(!app.follow, "search should leave follow mode");

    // Empty screen placeholder.
    let mut app = App::new();
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = (0..buffer.area.height)
        .map(|y| buffer_row(buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Translating"));

    println!("selftest ok");
}

fn main() {
    if std::env::args().any(|arg| arg == "--selftest") {
        selftest();
        return;
    }
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture).ok();
    let result = run(&mut terminal);
    execute!(std::io::stdout(), DisableMouseCapture).ok();
    ratatui::restore();
    if let Err(error) = result {
        eprintln!("reader-proto: {error}");
        std::process::exit(1);
    }
}
