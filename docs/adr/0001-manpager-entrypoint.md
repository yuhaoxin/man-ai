# 0001 接入点：MANPAGER 包装与 man-ai 薄壳

- 状态：接受
- 日期：2026-11-12
- 决策来源：[#2](https://github.com/yuhaoxin/man-ai/issues/2)、[#3](https://github.com/yuhaoxin/man-ai/issues/3)、[#10](https://github.com/yuhaoxin/man-ai/issues/10)

## 背景

目标平台已有成熟的 `man` 与 roff 渲染器：macOS 用 BSD man + mandoc，Linux 用 GNU man-db + groff。man-ai 要替换的是渲染后的显示内容，不是页面检索或排版。自建 roff 渲染器需要覆盖两套平台的全部 man 宏包、表格、断行、宽字符与 locale 行为，工作量与兼容风险远超翻译本身。

候选接入点：

1. 自解析 roff 并自建渲染器：完全可控，但要重新实现 man 的核心兼容面。
2. PATH 里放名为 `man` 的 shim：用户无感，但依赖 PATH 顺序；薄壳 `exec man` 会命中自身；`.cargo/bin` 与 `/opt/homebrew/bin` 的相对位置因机器而异。
3. 软链或替换 `/usr/bin/man`：macOS 受 SIP 保护；Linux 会被包管理器升级覆盖；同样有递归问题。
4. 以 `MANPAGER` 为主，另给 `man-ai` 薄壳：包装平台 man 的渲染结果，接入点稳定，非分页路径可透传。

## 决策

采用方案 4。

- `man-ai` 是单一二进制，按 stdin 是否 TTY 分为薄壳模式与分页器模式。
- 薄壳模式保存用户原有的 `MANPAGER` 到 `MAN_AI_PAGER`，设 `MANPAGER=man-ai`，然后 `exec man "$@"`；只摘除自有长参数，其余参数按原序交回 man。
- 分页器模式从 stdin 读渲染后的页面，翻译后交给内层分页器链：`MAN_AI_PAGER` → `PAGER` → `less -sR`。
- 只正式支持两种替换方式：导出 `MANPAGER`、定义 shell 函数。PATH 同名 shim 与软链 `/usr/bin/man` 明确不支持，理由写入安装文档。
- `-k`/`-f` 不经过 man 的分页器（macOS 走 `WHATISPAGER`，man-db 不分页），由薄壳拦截并设 `WHATISPAGER=cat` 捕获结果；`-w`/`-M`/`-S`/`-t`/`-T` 直接透传，不发起翻译。

## 后果

- man-ai 的兼容面绑定两套 man 的分页器契约；平台差异（BSD man 非 TTY 仍起分页器、man-db 只在 stdout 是 TTY 时起分页器、格式字符是否保留）必须写进规格并以实测为准。
- 非分页路径的字节一致性可直接通过与平台 man 做差分测试验收。
- 用户必须主动配置 `MANPAGER` 或 shell 函数；安装器不修改 shell 配置，卸载也不回滚它。
- 薄壳依赖 `man-ai` 在 PATH 中；`MANPAGER` 值写二进制名，绝对路径写法留给用户自己求值。
- `-P` 与用户自带分页器的调用必须让路；薄壳不与之争夺分页器选择权。
- 支持的平台限于 macOS 与 Linux；Windows 不在范围内。
