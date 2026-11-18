//! Markdown 源文到 ratatui 行的确定性渲染（规格 §8.2）。
//!
//! 只实现提示词允许清单里的七种结构（§7.2 规则 3）；清单外的事件按字面文本
//! 落行，不做能力探测。折行交给渲染端的 `Wrap`。

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// 把一段 Markdown 渲染为行；块边界补空行。
pub fn to_lines(src: &str) -> Vec<Line<'static>> {
    let mut renderer = Renderer::default();
    for event in Parser::new(src) {
        renderer.event(event);
    }
    renderer.flush();
    renderer.lines
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    /// 当前逻辑行。
    spans: Vec<Span<'static>>,
    bold: bool,
    italic: bool,
    /// 每层列表：`Some(计数器)` 为有序，`None` 为无序。
    lists: Vec<Option<u64>>,
    /// 列表项首行前缀；续行缩进对齐。
    item_prefix: String,
    item_indent: String,
    /// 块开始前需要补一个空行。
    need_gap: bool,
}

impl Renderer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => self.text(&text),
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            Event::Rule => self.rule(),
            // 模型不该输出这些；出现时按字面落行，不做能力探测。
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::InlineMath(text) => self.text(&text),
            Event::FootnoteReference(text) => self.text(&text),
            Event::DisplayMath(text) => self.text(&text),
            Event::TaskListMarker(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block(),
            Tag::Heading { .. } => {
                self.block();
                self.bold = true;
            }
            Tag::CodeBlock(_) => {
                self.block();
                self.flush();
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.item_start();
            }
            Tag::Emphasis => self.italic = true,
            Tag::Strong => self.bold = true,
            // 清单外结构的降级：按普通段落处理。
            Tag::BlockQuote(_) | Tag::HtmlBlock | Tag::FootnoteDefinition(_) => self.block(),
            Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell => self.block(),
            Tag::Link { .. }
            | Tag::Image { .. }
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript => {}
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {}
            Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::CodeBlock
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell => self.flush(),
            TagEnd::Item => {
                self.flush();
                self.item_prefix.clear();
                self.item_indent.clear();
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strong => self.bold = false,
            TagEnd::Link
            | TagEnd::Image
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// 块开始：与前面的块之间补一个空行。
    fn block(&mut self) {
        self.need_gap = !self.lines.is_empty() || !self.spans.is_empty();
    }

    fn rule(&mut self) {
        let need_gap = std::mem::replace(&mut self.need_gap, false);
        if need_gap {
            self.push_gap();
        }
        self.push_line(Line::from("────────"));
    }

    fn item_start(&mut self) {
        // 嵌套列表开始前，父项的挂起内容先落行。
        self.flush();
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        let marker = match self.lists.last_mut() {
            Some(Some(counter)) => {
                let marker = format!("{counter}. ");
                *counter += 1;
                marker
            }
            _ => "• ".to_string(),
        };
        self.item_indent = indent.clone();
        self.item_prefix = format!("{indent}{marker}");
    }

    fn text(&mut self, text: &str) {
        if self.need_gap {
            self.need_gap = false;
            self.push_gap();
        }
        if self.spans.is_empty() {
            self.push_text(self.item_prefix.clone());
        }
        // 一个事件可以带多行内容（围栏代码块整块到达）；行内换行折行。
        let mut segments = text.split('\n');
        let Some(first) = segments.next() else {
            return;
        };
        let style = self.style();
        self.spans.push(Span::styled(first.to_string(), style));
        let rest: Vec<&str> = segments.collect();
        for (index, segment) in rest.iter().enumerate() {
            // 结尾的换行不属于内容：不折出空行。
            if segment.is_empty() && index + 1 == rest.len() {
                break;
            }
            let prefix = if self.spans.is_empty() {
                self.item_prefix.clone()
            } else {
                self.item_indent.clone()
            };
            self.flush_with_prefix(prefix);
            if !segment.is_empty() {
                let style = self.style();
                self.spans.push(Span::styled(segment.to_string(), style));
            }
        }
    }

    /// Soft/hard 折行：首行用项前缀，续行用项缩进。
    fn break_line(&mut self) {
        let prefix = if self.spans.is_empty() {
            self.item_prefix.clone()
        } else {
            self.item_indent.clone()
        };
        self.flush_with_prefix(prefix);
    }

    fn style(&self) -> Style {
        let mut style = Style::new();
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    }

    fn flush(&mut self) {
        // 块结束时没有内容就不落空行（空行由块间分隔负责）；项前缀已在行内。
        if self.spans.is_empty() {
            return;
        }
        let mut line = Line::default();
        line.spans.append(&mut self.spans);
        self.push_line(line);
    }

    fn flush_with_prefix(&mut self, prefix: String) {
        let mut line = Line::default();
        if !prefix.is_empty() {
            line.spans.push(Span::from(prefix));
        }
        line.spans.append(&mut self.spans);
        self.push_line(line);
    }

    fn push_text(&mut self, text: String) {
        if !text.is_empty() {
            self.spans.push(Span::from(text));
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn push_gap(&mut self) {
        self.lines.push(Line::from(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一行所有 Span 的拼接文本。
    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    fn render(src: &str) -> Vec<String> {
        to_lines(src).iter().map(plain).collect()
    }

    #[test]
    fn 标题加粗_正文分块() {
        let lines = render("# LS(1)\n\nplain text");
        assert_eq!(lines, vec!["LS(1)", "", "plain text"]);
        assert!(
            to_lines("# LS(1)")[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn 强调与行内代码() {
        let lines = render("**bold** and *it* and `code`");
        assert_eq!(lines, vec!["bold and it and code"]);
        let spans = &to_lines("**bold** and *it* and `code`")[0].spans;
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(spans[2].style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn 围栏代码块逐行保留() {
        let lines = render("```\nls -l\nfoo bar\n```\n");
        assert_eq!(lines, vec!["ls -l", "foo bar"]);
    }

    #[test]
    fn 列表前缀与嵌套() {
        let lines = render("- one\n- two\n\n1. first\n2. second\n\n- outer\n  - inner");
        assert_eq!(
            lines,
            vec![
                "• one",
                "• two",
                "",
                "1. first",
                "2. second",
                "",
                "• outer",
                "  • inner",
            ]
        );
    }

    #[test]
    fn 块间补一个空行() {
        let lines = render("a\n\n# H\n\ntext");
        assert_eq!(lines, vec!["a", "", "H", "", "text"]);
    }

    #[test]
    fn 清单外的链接按字面渲染() {
        let lines = render("see [foo](http://x)");
        assert_eq!(lines, vec!["see foo"]);
    }

    #[test]
    fn 退格叠加与_sgr_预转换标记照常渲染() {
        // 预转换产物（**bold** / *underline*）本身是合法 Markdown。
        let lines = render("**ls** - 列出\n\n*用途* - 提示");
        assert_eq!(lines, vec!["ls - 列出", "", "用途 - 提示"]);
    }
}
