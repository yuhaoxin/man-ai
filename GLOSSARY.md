# 术语表

本仓库的领域术语。规格、ADR、issue 标题与实现命名以此为准，不要使用表里「不用」列的说法。

## 分页器模式（pager mode）

stdin 不是 TTY 时 `man-ai` 的运行模式：`man` 把渲染后的页面写入 stdin，`man-ai` 翻译后交给内层分页器或 stdout。页面内容只在 stdin，argv 里没有页名。

## 薄壳模式（shell mode）

stdin 是 TTY 时 `man-ai` 的运行模式：设置 `MANPAGER` 后 `exec` 平台自带的 `man`。薄壳不是 `man` 的替代实现。

## 薄壳（thin shell）

`man-ai` 二进制在薄壳模式下的角色：只处理自有参数、`MANPAGER` 与少数入口判定，页面检索和渲染全部交给平台 `man`。不用「包装器」「前端」指代它。

## 内层分页器（inner pager）

`man-ai` 在分页器模式或 `-k` 拦截路径中启动的分页器，按 `MAN_AI_PAGER` → `PAGER` → `less -sR` 解析。不要与 `man` 启动的 `man-ai` 混称「分页器」。

## 块（block）

渲染文本中由连续非空行组成的翻译单元。空行是唯一块边界，块不可再分，一次请求一个块。

## 页首行 / 尾行（header line / tail line）

`man-ai` 写入内层分页器的第一行与最后一行，标明译文语言与整页翻译结果。`-k` 路径只有页首行。

## 形态（form）

缓存键中的翻译单元形态：`page`（整页块）或 `k`（`-k`/`-f` 描述批）。两种形态互不命中。

## 原因（reason）

翻译失败的用户可见分类，固定五项：`not configured`、`model unreachable`、`timeout`、`backend error`、`interrupted`。

## 确定性失败（deterministic failure）

重试不会改变结果的失败：未配置、确定性 4xx（429 以外的 4xx）、重试一次后仍不可达的端点。发生后停止提交新块。

## 首屏就绪（first screen ready）

已按序写入的行数达到终端高度，或全部块处理完毕。此时启动内层分页器。

## 规范语言标签（canonical language tag）

按 BCP-47 形态校正大小写、`_` 换成 `-` 后的标签，例如 `zh-Hant-TW`。缓存键与显示名映射只使用规范形式。

## 缓存键 / 缓存条目（cache key / entry）

缓存键是 BLAKE3-128 摘要的 32 位小写十六进制串；缓存条目是键对应文件里的译文原始字节。

## 采用 / 回退（adopted / fallback）

采用指该块的译文或缓存命中被写出；回退指该块写出原文。尾行按块统计两者。

## 替换方式（replacement method）

让日常 `man` 走 `man-ai` 的用户侧配置：导出 `MANPAGER` 或定义 shell 函数。PATH shim 与软链不在支持范围。
