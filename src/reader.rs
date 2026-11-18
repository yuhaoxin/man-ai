//! 进程内 Markdown 阅读器（规格 §8）。
//!
//! 阅读器接管终端（raw 模式 + 备用屏幕 + 鼠标），完成块按序整块出现；滚动、
//! 搜索、缩放重排在所有 TTY 页面行为一致；底部状态行只显示 `elapsed <t>`。
//! Ctrl-C 是按键：翻译期间第一次取消剩余翻译，翻译结束后等同 `q`。
//!
//! 按键与鼠标事件不经过 `crossterm::event`：它的 Unix 事件源基于 kqueue，
//! 而 macOS 内核对 `/dev/tty` 的 fd 一律 `kevent(EV_ADD)` 失败（errno 22），
//! 分页器模式的 stdin 是管道，没有别的输入 fd 可用。输入由本模块在
//! `/dev/tty` 上的阻塞读线程转发，只解析本阅读器用到的键与滚轮序列。

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use signal_hook::SigId;
use signal_hook::consts::SIGWINCH;
use signal_hook::low_level;

use crate::markdown;
use crate::output::Message;
use crate::signals::{self, CancelTrigger};

/// 首块到达前的占位行（§8.1）。
const PLACEHOLDER: &str = "Translating… (waiting for the first block)";

/// 事件循环节奏：按键响应按这个量级轮询。
const TICK: Duration = Duration::from_millis(20);
/// 状态行 elapsed 的刷新间隔。
const ELAPSED_FRAME: Duration = Duration::from_millis(100);
/// 滚轮一次滚动的行数。
const WHEEL: usize = 3;

/// 阅读器运行结果：退出码与恢复终端后待写的诊断。
pub struct ReaderExit {
    pub code: i32,
    pub diagnostics: Vec<String>,
}

/// 接管终端：raw 模式 + 备用屏幕 + 鼠标 + 终止信号处理器 + 输入线程。
///
/// 失败时把原因带回，调用方跳过翻译、预转换原文直写 stdout（§5.4）。
pub fn takeover() -> Result<TermGuard, String> {
    if env::var_os("TERM").is_none_or(|term| term.is_empty()) {
        return Err("TERM is not set".to_string());
    }
    let input = open_input().map_err(|error| error.to_string())?;
    if let Err(error) = crossterm::terminal::enable_raw_mode() {
        return Err(error.to_string());
    }
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(error.to_string());
    }
    match signals::install_termination() {
        Ok(signal_ids) => Ok(TermGuard {
            signal_ids,
            input_ids: input.ids.clone(),
            input,
        }),
        Err(error) => {
            restore();
            Err(error.to_string())
        }
    }
}

/// 终端持有权：丢弃时恢复终端、摘除信号处理器并停掉输入线程。
pub struct TermGuard {
    signal_ids: Vec<SigId>,
    input_ids: Vec<SigId>,
    input: Input,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        signals::uninstall_termination(std::mem::take(&mut self.signal_ids));
        for id in std::mem::take(&mut self.input_ids) {
            low_level::unregister(id);
        }
        restore();
    }
}

fn restore() {
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
}

/// `/dev/tty` 上的输入：阻塞读线程 + SIGWINCH 标志。
struct Input {
    /// 读线程转发的原始字节块。
    bytes: Receiver<Vec<u8>>,
    resize: Arc<AtomicBool>,
    /// SIGWINCH 处理器的注册号。
    ids: Vec<SigId>,
}

fn open_input() -> io::Result<Input> {
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    // 读线程在 `TermGuard` 丢弃、通道销毁后随写端失败自行退出。
    thread::spawn(move || {
        let mut tty = tty;
        let mut buffer = [0u8; 1024];
        loop {
            match tty.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    if sender.send(buffer[..size].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let resize = Arc::new(AtomicBool::new(false));
    let flag = resize.clone();
    let id = unsafe { low_level::register(SIGWINCH, move || flag.store(true, Ordering::Relaxed))? };
    Ok(Input {
        bytes: receiver,
        resize,
        ids: vec![id],
    })
}

/// 阅读器认识的输入事件；解析见 [`Parser::feed`]。
#[derive(Debug, PartialEq, Eq)]
enum InputEvent {
    Char(char),
    /// Ctrl 与字母的组合；字母是小写。
    Ctrl(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    WheelUp,
    WheelDown,
}

/// 输入字节流到事件的增量解析；只认本阅读器用到的序列，其余丢弃。
#[derive(Default)]
struct Parser {
    pending: Vec<u8>,
    events: Vec<InputEvent>,
}

impl Parser {
    fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        let bytes = self.pending.clone();
        let mut cursor = 0;
        while cursor < bytes.len() {
            let Some(consumed) = self.parse_at(&bytes[cursor..]) else {
                break;
            };
            if consumed == 0 {
                // 序列不完整：等下一块字节。
                break;
            }
            cursor += consumed;
        }
        self.pending.drain(..cursor);
    }

    /// 从 `bytes[0]` 起解析一个事件；返回消耗的字节数，`Some(0)` 表示不完整。
    fn parse_at(&mut self, bytes: &[u8]) -> Option<usize> {
        match bytes[0] {
            0x1b => self.parse_escape(bytes),
            b'\r' | b'\n' => {
                self.events.push(InputEvent::Enter);
                Some(1)
            }
            0x7f => {
                self.events.push(InputEvent::Backspace);
                Some(1)
            }
            byte if byte < 0x20 => {
                // Ctrl+字母：按键化 Ctrl-C 与 Ctrl-Z 的来源。
                self.events
                    .push(InputEvent::Ctrl((byte - 1 + b'a') as char));
                Some(1)
            }
            byte if byte < 0x80 => {
                self.events.push(InputEvent::Char(byte as char));
                Some(1)
            }
            _ => self.parse_utf8(bytes),
        }
    }

    fn parse_escape(&mut self, bytes: &[u8]) -> Option<usize> {
        if bytes.len() < 2 {
            return Some(0);
        }
        if bytes[1] != b'[' {
            // 裸 Esc：搜索输入用它取消。
            self.events.push(InputEvent::Esc);
            return Some(1);
        }
        if bytes.len() < 3 {
            return Some(0);
        }
        // SGR 鼠标：ESC [ < 64|65 ; col ; row M 是滚轮。
        if bytes[2] == b'<' {
            let end = bytes[3..]
                .iter()
                .position(|byte| matches!(byte, b'M' | b'm'))?;
            if bytes.get(3) == Some(&b'6') && bytes.get(4) == Some(&b'4') {
                self.events.push(InputEvent::WheelUp);
            } else if bytes.get(3) == Some(&b'6') && bytes.get(4) == Some(&b'5') {
                self.events.push(InputEvent::WheelDown);
            }
            return Some(3 + end + 1);
        }
        // CSI：终结字节在 0x40..=0x7E；`5~`/`6~` 是 PgUp/PgDn。
        let end = bytes[2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))?;
        let sequence = &bytes[2..2 + end + 1];
        let event = match sequence {
            b"A" => InputEvent::Up,
            b"B" => InputEvent::Down,
            b"5~" => InputEvent::PageUp,
            b"6~" => InputEvent::PageDown,
            _ => return Some(2 + end + 1),
        };
        self.events.push(event);
        Some(2 + end + 1)
    }

    fn parse_utf8(&mut self, bytes: &[u8]) -> Option<usize> {
        let lead = bytes[0];
        let length = if lead >= 0xf0 {
            4
        } else if lead >= 0xe0 {
            3
        } else if lead >= 0xc0 {
            2
        } else {
            // 孤立的续字节：丢弃。
            return Some(1);
        };
        if bytes.len() < length {
            return Some(0);
        }
        match std::str::from_utf8(&bytes[..length]) {
            Ok(text) => {
                self.events
                    .push(InputEvent::Char(text.chars().next().unwrap()));
                Some(length)
            }
            Err(_) => Some(1),
        }
    }
}

/// 翻译路径：阅读器接管终端后，翻译线程把内容消息发进来。
pub fn serve_translating(
    guard: &mut TermGuard,
    messages: Receiver<Message>,
    cancel: CancelTrigger,
) -> ReaderExit {
    let mut view = View::new(Instant::now());
    serve(&mut view, Some((messages, cancel)), &mut guard.input);
    finish(view)
}

/// 不翻译的 TTY 页面：预转换原文一次性进入阅读器，无页首行与尾行（§8.2）。
pub fn serve_static(guard: &mut TermGuard, original: String) -> ReaderExit {
    let mut view = View::new(Instant::now());
    view.translating = false;
    view.update(Message::Block {
        gap: 0,
        body: original,
    });
    view.update(Message::Done);
    serve(&mut view, None, &mut guard.input);
    finish(view)
}

fn finish(view: View) -> ReaderExit {
    ReaderExit {
        code: 0,
        diagnostics: view.diagnostics,
    }
}

/// 事件循环：内容消息、按键与 SIGWINCH 交替处理，`q` 与翻译结束后的
/// Ctrl-C 退出。
fn serve(view: &mut View, channel: Option<(Receiver<Message>, CancelTrigger)>, input: &mut Input) {
    let Ok(mut terminal) = Terminal::new(CrosstermBackend::new(io::stdout())) else {
        return;
    };
    let mut parser = Parser::default();
    let mut last_frame = Instant::now();
    // 首帧无条件画：静态页面（不翻译路径）没有后续消息。
    let mut dirty = true;
    loop {
        if let Some((messages, _)) = &channel {
            while let Ok(message) = messages.try_recv() {
                view.update(message);
                dirty = true;
            }
        }
        while let Ok(chunk) = input.bytes.try_recv() {
            parser.feed(&chunk);
        }
        let mut quit = false;
        for event in parser.events.drain(..) {
            match view.input(event) {
                Decision::Continue => {}
                Decision::Cancel => {
                    view.translating = false;
                    if let Some((_, cancel)) = &channel {
                        cancel.trigger();
                    }
                }
                Decision::Quit => quit = true,
            }
        }
        if input.resize.swap(false, Ordering::Relaxed) {
            dirty = true;
        }
        if quit {
            return;
        }
        let live = !view.done;
        if dirty || (live && last_frame.elapsed() >= ELAPSED_FRAME) {
            let _ = terminal.draw(|frame| view.draw(frame));
            last_frame = Instant::now();
        }
        thread::sleep(TICK);
    }
}

enum Decision {
    Continue,
    /// 翻译期间的 Ctrl-C：取消剩余翻译，留在阅读器。
    Cancel,
    Quit,
}

/// 阅读器状态：内容行、视口、搜索与状态行口径。
struct View {
    lines: Vec<Line<'static>>,
    /// 占位行还在正文里。
    placeholder: bool,
    translating: bool,
    done: bool,
    start: Instant,
    done_at: Option<Instant>,
    follow: bool,
    offset: usize,
    /// 折行布局缓存：宽度和内容版本命中时复用。
    width: u16,
    version: u64,
    layout_version: u64,
    /// 每逻辑行之前已占的折行行数；末项是全文折行总行数。
    prefix: Vec<usize>,
    /// 上一帧的正文区高度。
    viewport: usize,
    search: Search,
    diagnostics: Vec<String>,
}

#[derive(Default)]
enum Search {
    #[default]
    Idle,
    /// 正在输入 `/` 查询。
    Typing(String),
    /// 已定位的查询；`n`/`N` 在当前全文的匹配集合内循环。
    Active { query: String, cursor: usize },
}

impl View {
    fn new(start: Instant) -> Self {
        Self {
            lines: Vec::new(),
            placeholder: false,
            translating: true,
            done: false,
            start,
            done_at: None,
            follow: true,
            offset: 0,
            width: 0,
            version: 0,
            layout_version: u64::MAX,
            prefix: Vec::new(),
            viewport: 0,
            search: Search::Idle,
            diagnostics: Vec::new(),
        }
    }

    fn update(&mut self, message: Message) {
        self.version += 1;
        match message {
            Message::Header(text) => {
                self.lines.push(Line::from(text));
                self.lines.push(Line::from(""));
                self.lines.push(Line::styled(
                    PLACEHOLDER,
                    Style::new().add_modifier(Modifier::DIM),
                ));
                self.placeholder = true;
            }
            Message::Block { gap, body } => {
                if self.placeholder {
                    self.placeholder = false;
                    self.lines.pop();
                }
                for _ in 0..gap {
                    self.lines.push(Line::from(""));
                }
                self.lines.extend(markdown::to_lines(&body));
            }
            Message::Diagnostic(line) => self.diagnostics.push(line),
            Message::Tail(text) => {
                self.lines.push(Line::from(""));
                self.lines.push(Line::from(text));
            }
            Message::Done => {
                self.done = true;
                self.done_at = Some(Instant::now());
                self.translating = false;
            }
        }
    }

    fn input(&mut self, event: InputEvent) -> Decision {
        if let Some(decision) = self.search_input(&event) {
            return decision;
        }
        match event {
            InputEvent::Char('q') => return Decision::Quit,
            InputEvent::Char('/') => {
                self.search = Search::Typing(String::new());
                return Decision::Continue;
            }
            InputEvent::Char('n') => self.jump_match(1),
            InputEvent::Char('N') => self.jump_match(-1),
            InputEvent::Char('G') => self.follow = true,
            // Ctrl-C：翻译期间取消，翻译结束后等同 q（§5.4）。
            InputEvent::Ctrl('c') => {
                return if self.translating {
                    Decision::Cancel
                } else {
                    Decision::Quit
                };
            }
            // Ctrl-Z 在 raw 模式下是普通按键，忽略（§5.4）。
            InputEvent::Ctrl('z') => return Decision::Continue,
            InputEvent::Up => return self.scroll(1, -1),
            InputEvent::Down => return self.scroll(1, 1),
            InputEvent::PageUp => return self.scroll(self.viewport, -1),
            InputEvent::PageDown => return self.scroll(self.viewport, 1),
            InputEvent::WheelUp => return self.scroll(WHEEL, -1),
            InputEvent::WheelDown => return self.scroll(WHEEL, 1),
            _ => {}
        }
        Decision::Continue
    }

    /// 滚动：向上即停跟随，触底恢复跟随（§8.2）。
    fn scroll(&mut self, amount: usize, direction: i32) -> Decision {
        if direction < 0 {
            self.follow = false;
            self.offset = self.offset.saturating_sub(amount.max(1));
        } else {
            self.offset = self.offset.saturating_add(amount.max(1));
            self.clamp();
            self.follow = self.at_bottom();
        }
        Decision::Continue
    }

    fn at_bottom(&self) -> bool {
        self.height() <= self.offset + self.viewport
    }

    fn clamp(&mut self) {
        let max = self.height().saturating_sub(self.viewport);
        self.offset = self.offset.min(max);
    }

    /// 搜索态的输入处理；搜索态之外返回 `None` 交给常规路径。
    fn search_input(&mut self, event: &InputEvent) -> Option<Decision> {
        let Search::Typing(query) = &mut self.search else {
            return None;
        };
        match event {
            InputEvent::Esc => self.search = Search::Idle,
            InputEvent::Enter => {
                let query = std::mem::take(query);
                self.search = Search::Active { query, cursor: 0 };
                self.jump_match(0);
            }
            InputEvent::Backspace => {
                query.pop();
            }
            InputEvent::Char(ch) => query.push(*ch),
            _ => {}
        }
        Some(Decision::Continue)
    }

    /// `direction > 0` 下一个匹配，`< 0` 上一个，`0` 从视口向下找首个。
    fn jump_match(&mut self, direction: i32) {
        let Search::Active { query, .. } = &self.search else {
            return;
        };
        let query = query.clone();
        let matches: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect();
                text.contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        if matches.is_empty() {
            return;
        }
        let len = matches.len();
        let cursor = match direction.signum() {
            0 => {
                let from = self.line_at_offset();
                matches.iter().position(|&m| m >= from).unwrap_or(0)
            }
            1 => {
                let Search::Active { cursor, .. } = &mut self.search else {
                    return;
                };
                (*cursor + 1) % len
            }
            _ => {
                let Search::Active { cursor, .. } = &mut self.search else {
                    return;
                };
                (*cursor + len - 1) % len
            }
        };
        if let Search::Active { cursor: slot, .. } = &mut self.search {
            *slot = cursor;
        }
        self.follow = false;
        self.offset = self.prefix_of(matches[cursor]);
    }

    /// 折行偏移所在的逻辑行下标。
    fn line_at_offset(&self) -> usize {
        let mut index = 0;
        for line in 0..self.lines.len() {
            if self.prefix[line + 1] > self.offset {
                index = line;
                break;
            }
            index = line;
        }
        index
    }

    /// 逻辑行 `index` 之前的折行行数。
    fn prefix_of(&self, index: usize) -> usize {
        self.prefix.get(index).copied().unwrap_or(0)
    }

    /// 全文折行总行数。
    fn height(&self) -> usize {
        self.prefix.get(self.lines.len()).copied().unwrap_or(0)
    }

    /// 折行布局缓存（§8.2）：宽度或内容变化时重算。
    fn ensure_layout(&mut self, width: u16) {
        if width == self.width && self.version == self.layout_version {
            return;
        }
        let mut prefix = Vec::with_capacity(self.lines.len() + 1);
        let mut total = 0;
        for line in &self.lines {
            prefix.push(total);
            total += Paragraph::new(vec![line.clone()])
                .wrap(Wrap { trim: false })
                .line_count(width);
        }
        prefix.push(total);
        self.prefix = prefix;
        self.width = width;
        self.layout_version = self.version;
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let [content, status] =
            Layout::vertical([Constraint::Fill(0), Constraint::Length(1)]).areas(frame.area());
        self.ensure_layout(content.width);
        self.viewport = content.height as usize;
        if self.follow {
            self.offset = self.height().saturating_sub(self.viewport);
        } else {
            self.clamp();
        }
        let paragraph = Paragraph::new(Text::from(self.lines.clone()))
            .wrap(Wrap { trim: false })
            .scroll((self.offset as u16, 0));
        frame.render_widget(paragraph, content);
        let status_line = match &self.search {
            Search::Idle => format!("elapsed {:.1}s", self.elapsed().as_secs_f32()),
            Search::Typing(query) => format!("/{query}"),
            Search::Active { query, .. } => format!("/{query}"),
        };
        frame.render_widget(Paragraph::new(status_line), status);
    }

    /// 整页流逝时间：接管起到最后一块处置完成（§8.3）。
    fn elapsed(&self) -> Duration {
        self.done_at.unwrap_or_else(Instant::now) - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// 解析结果：消化字节块后收到的事件。
    fn parsed(chunks: &[&[u8]]) -> Vec<InputEvent> {
        let mut parser = Parser::default();
        for chunk in chunks {
            parser.feed(chunk);
        }
        std::mem::take(&mut parser.events)
    }

    #[test]
    fn 解析方向键与翻页键() {
        assert_eq!(
            parsed(&[b"\x1b[A\x1b[B\x1b[5~\x1b[6~"]),
            vec![
                InputEvent::Up,
                InputEvent::Down,
                InputEvent::PageUp,
                InputEvent::PageDown,
            ]
        );
    }

    #[test]
    fn 解析_ctrl_字母_回车_退格_裸_esc() {
        assert_eq!(
            parsed(&[b"\x03\x1a\r\x7f\x1bX"]),
            vec![
                InputEvent::Ctrl('c'),
                InputEvent::Ctrl('z'),
                InputEvent::Enter,
                InputEvent::Backspace,
                InputEvent::Esc,
                InputEvent::Char('X'),
            ]
        );
    }

    #[test]
    fn 解析_sgr_滚轮并丢弃其它鼠标事件() {
        assert_eq!(
            parsed(&[b"\x1b[<64;10;2M\x1b[<65;10;2M\x1b[<0;10;2M"]),
            vec![InputEvent::WheelUp, InputEvent::WheelDown]
        );
    }

    #[test]
    fn 解析跨块序列与多字节字符() {
        assert_eq!(
            parsed(&[b"\x1b", b"[", b"A\xe5", b"\x96\x80"]),
            vec![InputEvent::Up, InputEvent::Char('\u{5580}')]
        );
    }

    #[test]
    fn 孤立续字节被丢弃() {
        assert_eq!(parsed(&[b"\x80\x41"]), vec![InputEvent::Char('A')]);
    }

    // ---------------------------------------------------------------- 视图

    /// 渲染到 TestBackend，返回每行的拼接文本。
    fn render(view: &mut View, cols: u16, rows: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| buffer[(col, row)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn translating_view() -> View {
        let mut view = View::new(Instant::now());
        view.update(Message::Header(
            "── man-ai: translation (zh-CN) ──".to_string(),
        ));
        view
    }

    #[test]
    fn 占位行在首块前出现并随首块消失() {
        let mut view = translating_view();
        let screen = render(&mut view, 60, 8);
        assert!(screen.iter().any(|line| line.contains(PLACEHOLDER)));

        view.update(Message::Block {
            gap: 0,
            body: "BLOCK-1\n".to_string(),
        });
        let screen = render(&mut view, 60, 8);
        assert!(screen.iter().all(|line| !line.contains(PLACEHOLDER)));
        assert!(screen.iter().any(|line| line.contains("BLOCK-1")));
    }

    #[test]
    fn 完成块按序整块出现() {
        let mut view = translating_view();
        view.update(Message::Block {
            gap: 0,
            body: "ALPHA\n".to_string(),
        });
        let first = render(&mut view, 60, 8);
        assert!(first.iter().any(|line| line.contains("ALPHA")));
        assert!(first.iter().all(|line| !line.contains("BETA")));

        view.update(Message::Block {
            gap: 1,
            body: "BETA\n".to_string(),
        });
        let second = render(&mut view, 60, 8);
        // ALPHA 在 BETA 之前：阅读顺序恒等块序。
        assert!(
            second.iter().position(|l| l.contains("ALPHA")).unwrap()
                < second.iter().position(|l| l.contains("BETA")).unwrap()
        );
    }

    #[test]
    fn 页首行在正文前_尾行在末尾() {
        let mut view = translating_view();
        view.update(Message::Block {
            gap: 0,
            body: "BODY\n".to_string(),
        });
        view.update(Message::Tail(
            "── man-ai: translated (zh-CN) ──".to_string(),
        ));
        view.update(Message::Done);
        let screen = render(&mut view, 60, 8);
        let header = screen
            .iter()
            .position(|l| l.contains("translation (zh-CN)"))
            .unwrap();
        let body = screen.iter().position(|l| l.contains("BODY")).unwrap();
        let tail = screen
            .iter()
            .position(|l| l.contains("translated (zh-CN)"))
            .unwrap();
        assert!(header < body && body < tail);
    }

    #[test]
    fn 状态行常驻底部只显示_elapsed() {
        let mut view = translating_view();
        view.update(Message::Block {
            gap: 0,
            body: "BODY\n".to_string(),
        });
        view.update(Message::Done);
        let screen = render(&mut view, 60, 8);
        assert!(screen[7].starts_with("elapsed "), "{}", screen[7]);
        assert!(screen[7].ends_with('s'), "{}", screen[7]);
        assert!(
            !screen
                .iter()
                .take(7)
                .any(|line| line.starts_with("elapsed"))
        );
    }

    #[test]
    fn 默认跟随底部_上滚停随_到底恢复() {
        let mut view = translating_view();
        let body = (1..=30).map(|n| format!("L{n:02}\n\n")).collect::<String>();
        view.update(Message::Block { gap: 0, body });
        view.update(Message::Done);
        let bottom = render(&mut view, 20, 8);
        // 跟随：视口贴住末行。
        assert!(bottom.iter().any(|line| line.contains("L29")));

        // PgUp 连按到顶：视口回到页首，跟随保持关闭。
        for _ in 0..10 {
            assert!(matches!(view.input(InputEvent::PageUp), Decision::Continue));
        }
        assert!(!view.follow);
        let top = render(&mut view, 20, 8);
        assert!(top.iter().any(|line| line.contains("L01")));

        // G 回到底部恢复跟随。
        assert!(matches!(
            view.input(InputEvent::Char('G')),
            Decision::Continue
        ));
        assert!(view.follow);
        let bottom = render(&mut view, 20, 8);
        assert!(bottom.iter().any(|line| line.contains("L29")));
    }

    #[test]
    fn 搜索跳转到匹配行_视口停在该行() {
        let mut view = translating_view();
        let body = (1..=30).map(|n| format!("L{n:02}\n\n")).collect::<String>();
        view.update(Message::Block { gap: 0, body });
        view.update(Message::Done);
        // 布局先于搜索建立。
        let _ = render(&mut view, 20, 8);
        // 输入 /L25 + 回车。
        for event in [
            InputEvent::Char('/'),
            InputEvent::Char('L'),
            InputEvent::Char('2'),
            InputEvent::Char('5'),
        ] {
            assert!(matches!(view.input(event), Decision::Continue));
        }
        assert!(matches!(view.input(InputEvent::Enter), Decision::Continue));
        assert!(!view.follow);
        let screen = render(&mut view, 20, 8);
        assert!(screen.iter().any(|line| line.contains("L25")));

        // n 循环到下一个匹配。
        assert!(matches!(
            view.input(InputEvent::Char('n')),
            Decision::Continue
        ));
        let screen = render(&mut view, 20, 8);
        assert!(
            screen.iter().any(|line| line.contains("L25")),
            "只有一个匹配时 n 留在原处"
        );
    }

    #[test]
    fn 缩放重排_窄宽度折行() {
        let mut view = translating_view();
        view.update(Message::Block {
            gap: 0,
            body: "aaa bbb ccc ddd\n".to_string(),
        });
        view.update(Message::Done);
        let wide = render(&mut view, 40, 6);
        assert!(wide.iter().any(|line| line.contains("aaa bbb ccc ddd")));
        let narrow = render(&mut view, 7, 8);
        assert!(narrow.iter().any(|line| line.contains("aaa bbb")));
        assert!(narrow.iter().any(|line| line.contains("ccc ddd")));
    }

    #[test]
    fn 翻译期间_ctrl_c_取消_翻译结束后等同_q() {
        let mut view = translating_view();
        view.update(Message::Block {
            gap: 0,
            body: "BODY\n".to_string(),
        });
        assert!(matches!(
            view.input(InputEvent::Ctrl('c')),
            Decision::Cancel
        ));
        // 取消后翻译标志清除：再按等同 q。
        view.update(Message::Done);
        assert!(matches!(view.input(InputEvent::Ctrl('c')), Decision::Quit));
    }

    #[test]
    fn 不翻译页面_ctrl_c_直接退出() {
        let mut view = View::new(Instant::now());
        view.translating = false;
        view.update(Message::Block {
            gap: 0,
            body: "原文\n".to_string(),
        });
        view.update(Message::Done);
        assert!(matches!(view.input(InputEvent::Ctrl('c')), Decision::Quit));
    }

    #[test]
    fn ctrl_z_被忽略() {
        let mut view = translating_view();
        assert!(matches!(
            view.input(InputEvent::Ctrl('z')),
            Decision::Continue
        ));
    }

    #[test]
    fn 占位行暗色显示() {
        let mut view = translating_view();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let col = (0..buffer.area.width)
            .find(|col| !buffer[(*col, 2)].symbol().is_empty())
            .unwrap();
        assert!(
            buffer[(col, 2)]
                .style()
                .add_modifier
                .contains(Modifier::DIM)
        );
    }
}
