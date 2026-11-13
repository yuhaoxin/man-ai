# 调研：终端 Markdown 增量渲染器选型（票 #27）

面向 man-ai v2：模型流式输出 Markdown，man-ai 在**全屏 ratatui TUI** 里边收边渲染，风格贴近 glow / glamour，宽度按终端动态。

## 结论

自写打印器：`pulldown-cmark 0.13` 事件流 + `unicode-width 0.2` 计算显示宽度，输出 ratatui 的 `Line`/`Span`。约 400–500 行（不含语法高亮），表格先做降级布局。

现成 crate 全部被排除，按原因排序：

1. **增量是硬指标**。`termimad`、`charmed-glamour`、`patine`、`markdown-to-ansi` 的公开 API 都是「整篇字符串进、整篇出」，要做流式必须绕开其核心结构。唯一按块渲染的现成实现是 `mdcat --stream`，代价是 305 个依赖、MSRV 1.95，且块间引用与表格状态丢失。
2. **视觉要贴近 glow**。`charmed-glamour` 的 `dark_style()` 是 glamour dark.json 的逐字段移植，最像，但中文不折行（实测宽度 40 渲染出 82 宽的行，原因是用空白分词），表格走 ASCII 管线，且不能流式——改造不比自己写便宜。
3. **输出目标是 ratatui TUI**。`mdcat` 的库层 `push_tty` 直接写 ANSI 字节，与 ratatui 的 `Buffer` 模型不接；`tui-markdown`、`ratskin` 是 ratatui 侧适配层，但都是整篇文档进、一次性产出 widget，没有增量入口。

## 候选对比

| 方案 | 版本 | 许可证 | 增量 | CJK 折行 | 输出目标 | 备注 |
|---|---|---|---|---|---|---|
| pulldown-cmark | 0.13.6 | MIT | 事件流，可对追加缓冲区重解析 | 自算 | 任意 | 7 个依赖，MSRV 1.71；对截断输入反复 `Parser::new_ext` 未观察到 panic |
| comrak | 0.59 | BSD-2 | 只产 AST/HTML | — | 无终端渲染 | 有宽度概念的层完全空缺 |
| termimad | 0.35.5 | MIT | 无（`print_text` 缓冲全文） | 支持 | ANSI 到 stdout | `MadSkin` 视觉与 glow 差距大：H1 居中下划线、引用符 `▐`、标题不加 `##` 前缀；宽度查询失败时回退 50 列 |
| mdcat / pulldown-cmark-mdcat | 2.20.1 | MPL-2.0 | `--stream` 按块独立解析 | 支持（UAX #14 + `display_width`） | ANSI 到 stdout | 305 个依赖；默认宽度封顶 80 |
| charmed-glamour | — | MIT | 无（`render(&str) -> String`） | **不支持**（按空白分词） | ANSI 字符串 | 主题最贴近 glamour |
| tui-markdown | 0.3.13 | MIT | 无 | 依赖下游 | ratatui `Text` | 整篇进整篇出 |
| ratskin | 0.3.1 | MIT | 无 | 继承 termimad | termimad → ratatui widget | 视觉同 termimad |
| patine / markdown-to-ansi | — | — | 无 | 支持 / 未确认 | ANSI 字符串 | `patine` 代码块永不折行 |
| anstyle / anstream | — | MIT/Apache | — | — | SGR 与「非 TTY 剥色」 | 不做 Markdown、不做宽度 |

## 宽度与 CJK

- glow（`glamour v2`）的定宽公式：`可用宽 = WordWrap - Indent - Margin*2`，`WordWrap` 取终端宽、封顶 120、查询失败时 80。中文按字断行。
- 折行必须自己按 `unicode-width` 逐字符累计，并对 CJK 允许在任何字符间断行；按空白分词会把整段中文当成一个词，产出 82 列的超宽行。
- 代码块不折行（glow 直接把代码交给 chroma），超宽由终端/分页器处理；本项目要折行，因为 TUI 里没有横向滚动之外的出路。

## glow 的视觉要素（`glamour/styles/dark.json`）

| 视觉 | 配置 | 渲染 |
|---|---|---|
| 文档 | `block_prefix "\n"`、`margin 2`、`color 252` | 左右各缩 2 列 |
| H1 | `prefix " "`、`suffix " "`、`color 228`、`bg 63`、`bold` | 色块标题 |
| H2–H6 | `prefix "## "` … `"###### "`、`color 39 bold`（H6 改 35 非粗体） | 字面前缀 + 颜色 |
| 引用 | `indent 1`、`indent_token "│ "` | 左竖线 |
| 列表 | `item "• "`、`enumeration ". "`、`level_indent 2` | 项目符号，续行缩进 |
| 行内代码 | `prefix/suffix " "`、`color 203`、`bg 236` | 空格包裹的底色块 |
| 代码块 | `margin 2`、`color 244`、chroma 主题 | **无左边框**，只有 2 列缩进 + 底色 |
| 表格 | `lipgloss.NormalBorder()` 且外框全关 | 行内 `│`、表头分隔 `─`、交叉 `┼`，无外框 |
| 链接 | 文本 `color 35 bold`、URL `color 30 underline` | OSC 8，纯文本回退是「文本 + URL」 |
| 分隔线 | `"\n--------\n"`、`color 240` | 固定 8 个连字符 |

`termimad` 与 `charmed-glamour` 都有现成主题可抄常量，前者视觉不符、后者 CJK 不行，因此抄 `charmed-glamour::dark_style()` 的常量进自己的 printer 最省事，不必引它的 59 个依赖。

## 流式提交算法

1. 每个 delta 追加到 `buf`，不立即渲染。
2. 行扫描器维护状态：是否在 ``` / ~~~ fence 内、是否在 HTML 块内、当前表格是否结束、列表是否延续（下一行仍以 `-`/`*`/数字开头或缩进续行）。
3. 扫到「空行 + 下一行不属于上一个块」处记为新 `safe_end`。
4. 只对 `buf[..safe_end]` 跑 `Parser::new_ext(...).into_offset_iter()`，用事件 `range.start >= committed` 过滤后再渲染，`committed = safe_end`。
5. 尾部未闭合块留在缓冲区，直到 fence/表格/列表真正闭合才提交，避免「先画半张表再补边框」。

已知代价：重解析是 O(n²)，man 页规模可接受；引用式链接 `[x][r]` 的定义若出现在后面，已提交段落会显示字面括号——提示词要求模型用行内链接即可规避。

## 规模估计（手写 Rust，事件驱动）

| 模块 | 覆盖 | 行数 |
|---|---|---|
| 行内样式机 | strong/emph/strikethrough/inline code/link/image alt/脚注引用 | 80–120 |
| 段落与折行 | `unicode-width` 逐字符折行、续行缩进、悬挂缩进 | 80–120 |
| 标题 | H1 色块、H2–H6 前缀 | 30–40 |
| 列表 | 有序/无序/任务、嵌套、续行对齐 | 60–80 |
| 引用 | `│ ` 前缀、嵌套 | 20 |
| 代码块 | fence/缩进、折行、底色 | 30–40 |
| 表格 | 列宽（含 CJK）、`│/─/┼`、过窄退化 | 80–120 |
| 分隔线 / HTML 直通 / 空块 | — | 20–30 |
| 流式提交器 | 安全块边界扫描 + offset 过滤 | 60–80 |
| 合计 | 不含语法高亮 | **400–500**；砍表格约 300 |

## 未确认

- `charmed-glamour`、`anstream 1.0` 的实际 MSRV（未声明）。
- `patine`、`markdown-to-ansi` 的 CJK 折行只看了源码，未跑。
- `mdcat` 的中文折行只看了源码，未实跑（305 依赖构建成本高）。
- glow TUI 对超宽代码行是横滚还是截断，未核实。

## 与早先假设的两处更正

- 代码块**没有**左边框：glow 的 dark 主题只有 2 列缩进（有左边框的是 mdcat）。视觉设计按此对齐。
- 输出目标是 ratatui 全屏 TUI，不是「写 ANSI 到分页器 stdin」：printer 产出 `Line`/`Span`，颜色用 ratatui `Style`，不拼 escape 字符串。
