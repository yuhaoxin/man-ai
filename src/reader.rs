//! 进程内 Markdown 阅读器（规格 §8）。
//!
//! 阅读器接管终端（raw 模式 + 备用屏幕 + 鼠标），完成块按序整块出现；滚动、
//! 搜索、缩放重排在所有 TTY 页面行为一致；底部状态行只显示 `elapsed <t>`。
//! Ctrl-C 是按键：翻译期间第一次取消剩余翻译，翻译结束后等同 `q`。

use std::env;
use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use signal_hook::SigId;

use crate::markdown;
use crate::output::Message;
use crate::signals::{self, CancelTrigger};

/// 首块到达前的占位行（§8.1）。
const PLACEHOLDER: &str = "Translating… (waiting for the first block)";

/// 事件轮询间隔；elapsed 按这个节奏刷新。
const TICK: Duration = Duration::from_millis(100);

/// 滚轮一次滚动的行数。
const WHEEL: usize = 3;

/// 阅读器运行结果：退出码与恢复终端后待写的诊断。
pub struct ReaderExit {
    pub code: i32,
    pub diagnostics: Vec<String>,
}

/// 接管终端：raw 模式 + 备用屏幕 + 鼠标 + 终止信号处理器。
///
/// 失败时把原因带回，调用方跳过翻译、预转换原文直写 stdout（§5.4）。
pub fn takeover() -> Result<TermGuard, String> {
    if env::var_os("TERM").is_none_or(|term| term.is_empty()) {
        return Err("TERM is not set".to_string());
    }
    if let Err(error) = crossterm::terminal::enable_raw_mode() {
        return Err(error.to_string());
    }
    if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(error.to_string());
    }
    match signals::install_termination() {
        Ok(ids) => Ok(TermGuard { ids }),
        Err(error) => {
            restore();
            Err(error.to_string())
        }
    }
}

/// 终端持有权：丢弃时恢复终端并摘除终止信号处理器。
pub struct TermGuard {
    ids: Vec<SigId>,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        signals::uninstall_termination(std::mem::take(&mut self.ids));
        restore();
    }
}

fn restore() {
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
}

/// 翻译路径：阅读器接管终端后，翻译线程把内容消息发进来。
pub fn serve_translating(
    _guard: TermGuard,
    messages: Receiver<Message>,
    cancel: CancelTrigger,
) -> ReaderExit {
    let mut view = View::new(Instant::now());
    serve(&mut view, Some((messages, cancel)));
    finish(view)
}

/// 不翻译的 TTY 页面：预转换原文一次性进入阅读器，无页首行与尾行（§8.2）。
pub fn serve_static(_guard: TermGuard, original: String) -> ReaderExit {
    let mut view = View::new(Instant::now());
    view.translating = false;
    view.update(Message::Block { gap: 0, body: original });
    view.update(Message::Done);
    serve(&mut view, None);
    finish(view)
}

fn finish(view: View) -> ReaderExit {
    ReaderExit {
        code: 0,
        diagnostics: view.diagnostics,
    }
}

/// 事件循环：内容消息与按键交替处理，`q` / 翻译结束后的 Ctrl-C 退出。
fn serve(view: &mut View, channel: Option<(Receiver<Message>, CancelTrigger)>) {
    let Ok(mut terminal) = Terminal::new(CrosstermBackend::new(io::stdout())) else {
        return;
    };
    loop {
        let mut dirty = false;
        if let Some((messages, _)) = &channel {
            while let Ok(message) = messages.try_recv() {
                view.update(message);
                dirty = true;
            }
        }
        if dirty || !view.done {
            let _ = terminal.draw(|frame| view.draw(frame));
        }
        let decision = match event::poll(TICK) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => view.key(key),
                Ok(Event::Mouse(mouse)) => view.mouse(mouse.kind),
                Ok(_) => Decision::Continue,
                Err(_) => Decision::Quit,
            },
            Ok(false) => Decision::Continue,
            Err(_) => Decision::Quit,
        };
        match decision {
            Decision::Quit => return,
            Decision::Cancel => {
                view.translating = false;
                if let Some((_, cancel)) = &channel {
                    cancel.trigger();
                }
            }
            Decision::Continue => {}
        }
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
        eprintln!("[dbg] reader got message: {message:?}");
        self.version += 1;
        match message {
            Message::Header(text) => {
                self.lines.push(Line::from(text));
                self.lines.push(Line::from(""));
                self.lines
                    .push(Line::styled(PLACEHOLDER, Style::new().add_modifier(Modifier::DIM)));
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

    fn key(&mut self, key: KeyEvent) -> Decision {
        if let Some(decision) = self.search_key(key) {
            return decision;
        }
        let plain = key.modifiers.intersection(KeyModifiers::CONTROL).is_empty();
        if plain {
            match key.code {
                KeyCode::Char('q') => return Decision::Quit,
                KeyCode::Char('/') => {
                    self.search = Search::Typing(String::new());
                    return Decision::Continue;
                }
                KeyCode::Char('n') => self.jump_match(1),
                KeyCode::Char('N') => self.jump_match(-1),
                KeyCode::Char('G') | KeyCode::End => self.follow = true,
                _ => {}
            }
        }
        match key.code {
            // Ctrl-C：翻译期间取消，翻译结束后等同 q（§5.4）。
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.translating {
                    Decision::Cancel
                } else {
                    Decision::Quit
                }
            }
            // Ctrl-Z 在 raw 模式下是普通按键，忽略（§5.4）。
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Decision::Continue
            }
            KeyCode::Up => self.scroll(1, -1),
            KeyCode::Down => self.scroll(1, 1),
            KeyCode::PageUp => self.scroll(self.viewport, -1),
            KeyCode::PageDown | KeyCode::Char(' ') if plain => self.scroll(self.viewport, 1),
            _ => Decision::Continue,
        }
    }

    fn mouse(&mut self, kind: MouseEventKind) -> Decision {
        match kind {
            MouseEventKind::ScrollUp => self.scroll(WHEEL, -1),
            MouseEventKind::ScrollDown => self.scroll(WHEEL, 1),
            _ => Decision::Continue,
        }
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

    /// 搜索态的按键处理；搜索态之外返回 `None` 交给常规路径。
    fn search_key(&mut self, key: KeyEvent) -> Option<Decision> {
        let Search::Typing(query) = &mut self.search else {
            return None;
        };
        match key.code {
            KeyCode::Esc => self.search = Search::Idle,
            KeyCode::Enter => {
                let query = std::mem::take(query);
                self.search = Search::Active { query, cursor: 0 };
                self.jump_match(0);
            }
            KeyCode::Backspace => {
                query.pop();
            }
            KeyCode::Char(ch) => {
                if key.modifiers.intersection(KeyModifiers::CONTROL).is_empty() {
                    query.push(ch);
                }
            }
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
                let text: String = line.spans.iter().map(|span| span.content.to_string()).collect();
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
        let [content, status] = Layout::vertical([Constraint::Fill(0), Constraint::Length(1)])
            .areas(frame.area());
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
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;

    fn press(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty())
    }

    fn press_code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

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
        view.update(Message::Header("── man-ai: translation (zh-CN) ──".to_string()));
        view
    }

    #[test]
    fn 占位行在首块前出现并随首块消失() {
        let mut view = translating_view();
        let screen = render(&mut view, 60, 8);
        assert!(screen.iter().any(|line| line.contains(PLACEHOLDER)));

        view.update(Message::Block { gap: 0, body: "BLOCK-1\n".to_string() });
        let screen = render(&mut view, 60, 8);
        assert!(screen.iter().all(|line| !line.contains(PLACEHOLDER)));
        assert!(screen.iter().any(|line| line.contains("BLOCK-1")));
    }

    #[test]
    fn 完成块按序整块出现() {
        let mut view = translating_view();
        view.update(Message::Block { gap: 0, body: "ALPHA\n".to_string() });
        let first = render(&mut view, 60, 8);
        assert!(first.iter().any(|line| line.contains("ALPHA")));
        assert!(first.iter().all(|line| !line.contains("BETA")));

        view.update(Message::Block { gap: 1, body: "BETA\n".to_string() });
        let second = render(&mut view, 60, 8);
        // ALPHA 在 BETA 之前：阅读顺序恒等块序。
        assert!(second.iter().position(|l| l.contains("ALPHA")).unwrap()
            < second.iter().position(|l| l.contains("BETA")).unwrap());
    }

    #[test]
    fn 页首行在正文前_尾行在末尾() {
        let mut view = translating_view();
        view.update(Message::Block { gap: 0, body: "BODY\n".to_string() });
        view.update(Message::Tail("── man-ai: translated (zh-CN) ──".to_string()));
        view.update(Message::Done);
        let screen = render(&mut view, 60, 8);
        let header = screen.iter().position(|l| l.contains("translation (zh-CN)")).unwrap();
        let body = screen.iter().position(|l| l.contains("BODY")).unwrap();
        let tail = screen.iter().position(|l| l.contains("translated (zh-CN)")).unwrap();
        assert!(header < body && body < tail);
    }

    #[test]
    fn 状态行常驻底部只显示_elapsed() {
        let mut view = translating_view();
        view.update(Message::Block { gap: 0, body: "BODY\n".to_string() });
        view.update(Message::Done);
        let screen = render(&mut view, 60, 8);
        assert!(screen[7].starts_with("elapsed "), "{}", screen[7]);
        assert!(screen[7].ends_with('s'), "{}", screen[7]);
        assert!(!screen.iter().take(7).any(|line| line.starts_with("elapsed")));
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
            view.key(press_code(KeyCode::PageUp));
        }
        assert!(!view.follow);
        let top = render(&mut view, 20, 8);
        assert!(top.iter().any(|line| line.contains("L01")));

        // G 回到底部恢复跟随。
        assert!(matches!(view.key(press('G')), Decision::Continue));
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
        for key in ['/', 'L', '2', '5'] {
            view.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::empty()));
        }
        view.key(press_code(KeyCode::Enter));
        assert!(!view.follow);
        let screen = render(&mut view, 20, 8);
        assert!(screen.iter().any(|line| line.contains("L25")));

        // n 循环到下一个匹配。
        view.key(press('n'));
        let screen = render(&mut view, 20, 8);
        assert!(
            screen.iter().any(|line| line.contains("L25")),
            "只有一个匹配时 n 留在原处"
        );
    }

    #[test]
    fn 缩放重排_窄宽度折行() {
        let mut view = translating_view();
        view.update(Message::Block { gap: 0, body: "aaa bbb ccc ddd\n".to_string() });
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
        view.update(Message::Block { gap: 0, body: "甲\n".to_string() });
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(view.key(ctrl_c), Decision::Cancel));
        // 取消后翻译标志清除：再按等同 q。
        view.update(Message::Done);
        assert!(matches!(view.key(ctrl_c), Decision::Quit));
    }

    #[test]
    fn 不翻译页面_ctrl_c_直接退出() {
        let mut view = View::new(Instant::now());
        view.translating = false;
        view.update(Message::Block { gap: 0, body: "原文\n".to_string() });
        view.update(Message::Done);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(view.key(ctrl_c), Decision::Quit));
    }

    #[test]
    fn ctrl_z_被忽略() {
        let mut view = translating_view();
        let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(matches!(view.key(ctrl_z), Decision::Continue));
    }

    #[test]
    fn 占位行暗色显示() {
        let mut view = translating_view();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();

        terminal.draw(|frame| view.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.width)
            .find(|col| !buffer[(*col, 2)].symbol().is_empty())
            .unwrap();
        assert!(buffer[(row, 2)].style().add_modifier.contains(Modifier::DIM));
    }
}
