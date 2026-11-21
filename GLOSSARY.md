# 术语表

本仓库的领域术语。规格、ADR、issue 标题与实现命名以此为准，不要使用表里「不用」列的说法。

## 分页器模式（pager mode）

stdin 不是 TTY 时 `man-ai` 的运行模式：`man` 把渲染后的页面写入 stdin，`man-ai` 翻译后在阅读器或 stdout 上显示。页面内容只在 stdin，argv 里没有页名。

## 薄壳模式（shell mode）

stdin 是 TTY 时 `man-ai` 的运行模式：设置 `MANPAGER` 后 `exec` 平台自带的 `man`。薄壳不是 `man` 的替代实现。

## 薄壳（thin shell）

`man-ai` 二进制在薄壳模式下的角色：只处理自有参数、`MANPAGER` 与少数入口判定，页面检索和渲染全部交给平台 `man`。不用「包装器」「前端」指代它。

## 阅读器（reader）

`man-ai` 在 TTY 页面上接管终端显示的进程内 Markdown 阅读器：滚动、搜索、缩放重排与译文直播显示由它承担，页面不经过外置分页器。不要与 `man` 启动的 `man-ai` 混称「分页器」。

## 块（block）

渲染文本中由连续非空行组成的翻译单元。空行是唯一块边界，块不可再分，一次请求一个块。

## 直播块（live block）

按序前缀中第一个尚未完成的块：唯一把译文增量实时写进阅读器的块，半截内容是临时显示态，失败或完成后由整块内容替换。后续块静默累积，轮到后整块出现。不用「打字机效果」指代它。

## 页首行 / 尾行（header line / tail line）

阅读器正文前的第一行与全部块之后的最后一行，标明译文语言与整页翻译结果。

## 原因（reason）

翻译失败的用户可见分类，固定五项：`not configured`、`model unreachable`、`timeout`、`backend error`、`interrupted`。

## 预转换（pre-conversion）

切块后对块内渲染字节做的确定性转换：SGR 粗体/下划线与退格叠加转成 `**…**`/`*…*`，其余控制序列剥除，非 UTF-8 字节按 U+FFFD 有损解码。产物 `pre` 是送模型的唯一内容，也是回退输出；原始渲染字节不送模型、不写入输出。

## 确定性失败（deterministic failure）

重试不会改变结果的失败：未配置、确定性 4xx（429 以外的 4xx）、重试一次后仍不可达的端点。发生后停止提交新块。

## 占位行（placeholder line）

翻译开始后、首块到达前阅读器顶部显示的一行暗色 `Translating… (waiting for the first block)`；首块写出即消失。

## 规范语言标签（canonical language tag）

按 BCP-47 形态校正大小写、`_` 换成 `-` 后的标签，例如 `zh-Hant-TW`。缓存键与显示名映射只使用规范形式。

## 缓存键 / 缓存条目（cache key / entry）

缓存键是条目文件首行 HTML 注释里的字段集合（协议版本、提示词版本、规范化端点、
模型、思考强度、预转换全文 BLAKE3-128）；缓存条目是注释行之后的整页译文 Markdown，
文件名 `<页名>.<section>.md` 放在语言目录下。

## 采用 / 回退（adopted / fallback）

采用指该块的译文被写出（整页缓存命中时是全部块一次性写出）；回退指该块写出预转换原文。尾行按块统计两者。

## 状态行（status line）

阅读器底部常驻单行，按固定顺序展示整页指标 `elapsed · out · tps · cached · in`；
整页缓存命中时只显示流逝时间与 `cache hit`。超宽不折行，从右向左省略字段，
至少保留 elapsed 与 out。无键位提示、无 `?` 帮助界面。

## 整页流逝时间（page elapsed time）

从阅读器接管终端到最后一个块处置完成且无在途请求的墙钟时长；不含 stdin 读取、
切块、缓存写入、尾行写出与用户滚动。磁盘缓存命中页同样从接管起算。

## 输出 TPS（output TPS）

输出 token 总数 ÷ 全部被采用请求的生成窗口之和。生成窗口是单个请求从发出到
流读完的墙钟时长；回退、失败尝试与被取消请求不进分子分母。不用「吞吐量」指代它。

## 替换方式（replacement method）

让日常 `man` 走 `man-ai` 的用户侧配置：导出 `MANPAGER` 或定义 shell 函数。PATH shim 与软链不在支持范围。
