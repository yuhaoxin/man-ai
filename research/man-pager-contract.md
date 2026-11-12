# 事实核查：BSD man/mandoc 与 GNU man-db/groff 的分页器契约差异

- 日期：2026-11-12
- 对应票据：yuhaoxin/man-ai#10（地图 #1）
- 目的：给 man-ai 的 `MANPAGER` 设计确定可依赖的事实边界
- 标注约定：
  - **实测**：本机或本地容器内跑出的命令与原始输出
  - **源码**：主项目源码，给出文件与行号
  - **文档**：项目自带 man page 或官方站点，给出来源
  - **未实测**：只有源码/文档依据，本次没有在对应平台上执行

---

## 0. 结论速查

| 事实 | macOS 26.6.2（BSD man + mandoc） | Debian bookworm（man-db 2.11.2 + groff） |
|---|---|---|
| 分页器来自哪里 | `-P` > `MANPAGER` > （`MANCOLOR` → `less -sR`）> `PAGER` > `less -s`（**实测**） | `-P` > `MANPAGER` > `PAGER` > 编译期 `PROG_PAGER`（`pager`），找不到则 `cat`（**文档/源码**） |
| `-P` 与 `MANPAGER` 谁赢 | `-P` 覆盖 `MANPAGER`；`-P` 同时设置 `MANPAGER` 和 `WHATISPAGER`（**实测/源码**） | `-P` 覆盖 `MANPAGER`；但 `-f`/`-k` 不使用分页器（**文档**） |
| 分页器字符串如何解释 | `eval` 交给 shell：可带参数、引号、**管道**（**实测**） | `pipecmd_new_argstr`：可带参数和引号，**不允许管道**（**文档/源码**） |
| 分页器 argv | 页面内容永远走 stdin，argv 只有用户字符串拆出的词（**实测**） | 同左（**文档**：值可为“命令加参数”） |
| 分页器 stdin | 管道（`S_ISFIFO`），非普通文件（**实测**） | 管道（**源码**：pipeline 输出接分页器） |
| stdin 的格式 | 纯退格叠加（`X\bX`、`_\bc`），**没有 SGR**；`MAN_KEEP_FORMATTING` 无影响（**实测**） | 非 tty 默认去掉格式化字符（`col -b -p -x`）；设 `MAN_KEEP_FORMATTING` 后保留退格叠加（**实测**）；tty 下 groff 输出 SGR（**源码推断，未实测**） |
| 非 TTY 时是否还起分页器 | **仍然起**（无条件管道到 `$MANPAGER`）（**实测**） | **不起**：`if (isatty(STDOUT_FILENO))` 才挂分页器（**源码**，容器实测 piped 时 0 次调用） |
| `man -k`/`-f` | 走 `WHATISPAGER`（默认 `more -E`），**不看 `MANPAGER`**（**实测**） | **不走分页器**（**文档/源码**，未实测） |
| `man -w` | 只打印路径，不分页（**实测**） | 不分页（**文档**） |
| `man -t` | 直接向 stdout 输出 PostScript，不分页（**实测**） | 输出到 stdout，不分页（**文档**：`man -t bash | lpr -Pps`） |
| `man -a` | 每个匹配页各起一次分页器（**实测**） | 每页依次处理（**文档**），分页器行为同单页 |
| 宽度 | `MANWIDTH=tty`（仅 stdin+stdout 都是 tty）→ 终端列数−2；数字照用；非法值退回 mandoc 默认 78（**实测**） | `MANWIDTH`，否则 `COLUMNS`/ioctl，再否则 80（**文档**） |
| locale | `LC_ALL=C` 输出 ASCII 连字符；`C.UTF-8` 输出 U+2014（**实测**） | 同一页（纯 ASCII 内容）两者无差异（**实测**） |
| 分页器退出码 | 成为 `man` 的退出码（探针 `exit 3` → man 退出 3）（**实测**） | 分页器是管道末段，退出码即 man 的退出码（**源码**语义，未实测） |
| `LESS` 环境变量 | `man` 不设置，由 less 自己读（**实测**：分页器环境里只有 `MANPAGER`） | `man` 主动设置 `LESS`，含 `$MAN_PN` 提示串（**源码** `setenv_less`） |

---

## 1. 环境与版本

| 项 | macOS（本机实测） | Linux 容器（实测环境） |
|---|---|---|
| OS | macOS 26.6.2，Darwin 25.6.0 arm64 | debian:bookworm-slim（Debian 12） |
| `man` | `/usr/bin/man`，FreeBSD man(1) 的 **POSIX shell 脚本**，Apple 变体，1490 行；不支持 `man --version` | man-db **2.11.2** |
| 格式化器 | Apple `mandoc`（`/usr/bin/mandoc`，`mandoc.1` rev 1.166）；**无 groff/nroff/troff** | GNU groff **1.22.4**；另装 Debian `mandoc` 用于对照 |
| 分页器 | `/usr/bin/less` **668**（POSIX regex 版） | `/usr/bin/less` **590** |
| 终端 | `TERM=xterm-256color`，`LANG=LC_ALL=C.UTF-8`；pty 为 100×30（`TIOCSWINSZ`） | 容器内 `script(1)` 分配 pty，`COLUMNS` 默认 |

版本命令与输出：

```console
$ sw_vers
ProductName:            macOS
ProductVersion:         26.6.2
BuildVersion:           25G83

$ file /usr/bin/man
/usr/bin/man: POSIX shell script text executable, ASCII text

$ man --version
/usr/bin/man: illegal option -- -
Usage:
 man [-adho] [-t | -w] [-M manpath] [-P pager] [-S mansect]
 ...

$ less --version
less 668 (POSIX regular expressions)

$ head -3 /usr/share/man/man1/mandoc.1
.\" $OpenBSD: mandoc.1,v 1.166 2020/02/15 15:28:01 schwarze Exp $

$ command -v groff nroff troff; echo $?
（无输出，退出码 1：macOS 无 groff）

# 容器内
$ man --version
man 2.11.2
$ groff --version | head -1
GNU groff version 1.22.4
$ less --version | head -1
less 590 (GNU regular expressions)
```

来源：本机命令输出；`/usr/bin/man` 文件本体（SPDX 头写的是 `BSD-2-Clause-FreeBSD`，与 FreeBSD `usr.bin/man/man.sh` 同源，Apple 用 `#ifdef __APPLE__` 打补丁）。

---

## 2. 方法

### 2.1 分页器探针

用一个脚本冒充分页器，记录 argv、fd 类型、cwd、环境，并把 stdin 原样落盘：

```sh
#!/bin/sh
t0=n; t1=n; [ -t 0 ] && t0=y; [ -t 1 ] && t1=y
{ echo "argv=$# tty0=$t0 tty1=$t1 cwd=$PWD"; echo "args=<$*>"; } >> "$LOG"
cat > "$RAW"          # stdin 原样保存
env | sort > "$ENVF"
```

> 注意：`[ -t 1 ]` 必须写在 `if`/普通语句里；写成 `"$([ -t 1 ] && …)"` 会因为命令替换把 fd 1 变成管道，得到假的 `n`。本次第一次探测就踩了这个坑，后续已改正。

### 2.2 真 tty

`script(1)` 在 macOS 上行为不稳定（探针看到的 stdout 仍不是 tty），因此 macOS 侧改用 Python `pty.openpty()` + `TIOCSCTTY` + `TIOCSWINSZ(30,100)` 起子进程；Linux 侧用 util-linux `script -q -c`。

### 2.3 Linux 侧

```console
$ docker run -d --name manres -v /tmp/lin:/w debian:bookworm-slim sleep infinity
$ docker exec manres sh -c 'sed -i "/path-exclude/d" /etc/dpkg/dpkg.cfg.d/docker;
    apt-get update -qq && apt-get install -y --no-install-recommends \
      man-db groff less mandoc manpages
    apt-get install --reinstall -y coreutils manpages'
```

> 官方 `debian:bookworm-slim` 镜像的 `/etc/dpkg/dpkg.cfg.d/docker` 里有 `path-exclude /usr/share/man/*`，不删掉就装不进任何 man page（`man ls` 会报 “No manual entry”）。这是镜像问题，不是 man-db 行为。

---

## 3. `MANPAGER` 被调用时的形态（macOS 实测）

### 3.1 argv

分页器命令是 shell 字符串，`man` 用 `eval` 执行：`eval "$cattool \"$manpage\" | $pipeline"`，其中 `pipeline` 末尾是 `$MANPAGER`（源码：`man_display_page()`、`man_display_page_groff()`）。

实测：

```console
$ MANPAGER=/tmp/pm2/probe3.sh man ls        # pty 100 列
argv=0
tty0=n tty1=y
cwd=/usr/share/man
MANWIDTH=unset
args:<>

$ MANPAGER="/tmp/pm2/probe3.sh -R -X" man ls
argv=2
tty0=n tty1=y
...
args=<-R -X>
```

- 页面内容**不进 argv**，只走 stdin。
- `MANPAGER` 里的空格按 shell 规则拆词；引号生效：`man -P "/tmp/pm2/probe3.sh --weird-flag 'x y'"` 得到 `args=<--weird-flag x y>`、`argc=2`（第二个参数内部含空格）。
- 因为是 `eval`，**管道也能用**：

```console
$ MANPAGER='cat | wc -l' man ls
     525
$ man -P 'cat | wc -c' ls
   27493
```

对照 man-db：`-P` 的值“可以是带参数的命令，支持 shell 引号，但**不允许用管道连接多个命令**；需要就写 wrapper 脚本，脚本从参数或 stdin 取文件”（来源：man-db man(1) `-P` 段，<https://manpages.debian.org/bookworm/man-db/man.1.en.html>）。

### 3.2 stdin

```
pager stdin type: dev=no fifo=yes reg=no
```

`fd 0` 是 FIFO，不是普通文件。含义：分页器不能 seek、不能 `mmap`、不能用 `-o logfile` 这类“输入是普通文件”才有的能力；需要重读时得自己缓存。

管道内是已经格式化好的终端文本：`ls.1` 在 80 列下 27493 字节，在 100 列 pty 下 26442 字节，首行长度 98（=100−2）。

### 3.3 cwd 与环境

```
cwd=/usr/share/man
```

`man` 在页面所在目录里执行整条管道（源码：`(cd "$path" && eval ...)`），相对路径会以 man 目录为基准。分页器环境里 `man` **只注入 `MANPAGER` 本身**，不导出 `MANPATH`、`MANWIDTH`、`LESS`、`PAGER`（实测 `env` 过滤结果只有 `MANPAGER=…`）。`LESS` 若存在是用户自己的，由 less 读取。

man-db 相反：`setenv_less()` 会给分页器设置 `LESS`（含 `$MAN_PN` 展开的提示串），`man -r` 就是改这个串（源码：`src/man.c:1547-1570`；文档：man-db man(1) `-r` 段）。

### 3.4 退出码

探针 `exit 3`，`man` 随之退出 3（`waitpid` 状态 768 = 3<<8）：

```console
man exit status with pager exit 3: 768
```

原因：分页器是管道最后一段，`ret=$?` 直接成为 `man` 的退出码。

---

## 4. 优先级：`-P` / `MANPAGER` / `PAGER` / `MANCOLOR` / `LESS`

macOS 的决策全部在 `/usr/bin/man` 的 `setup_pager()`：

```sh
if [ -z "$MANPAGER" ]; then
    if [ -n "$MANCOLOR" ]; then
        MANPAGER="$LESS_CMD -sR"
    else
        if [ -n "$PAGER" ]; then
            MANPAGER="$PAGER"
        else
            MANPAGER="$LESS_CMD -s"
        fi
    fi
fi
```

`-P` 在参数解析阶段就写进 `MANPAGER`，并且**同时**写 `WHATISPAGER`：

```sh
P)  MANPAGER=$OPTARG; WHATISPAGER=$OPTARG ;;
```

`man -d` 的实测结果（`-d` 会打印 `-- Using pager:` 与完整命令行）：

| 环境 | 实际命令 |
|---|---|
| 干净环境（无 `MANPAGER`/`PAGER`/`MANCOLOR`） | `zcat -f /usr/share/man/man1/ls.1 \| mandoc \| /usr/bin/less -s` |
| `PAGER=more` | `… \| mandoc \| more` |
| `MANCOLOR=1` | `… \| mandoc \| /usr/bin/less -sR` |
| `MANCOLOR=1 PAGER=more` | `… \| mandoc \| /usr/bin/less -sR`（`MANCOLOR` 压过 `PAGER`） |
| `MANPAGER=/tmp/pm/a.sh PAGER=more` | `… \| mandoc \| /tmp/pm/a.sh` |
| `MANPAGER=/tmp/pa.sh man -P /tmp/pb.sh` | `pager=b`（`-P` 胜） |
| `MANPAGER=''`（空串） | 视为未设，退回默认 `less -s` |

文档一致：macOS man(1) 写 “If unset, and color support is enabled, `less -sR` is used. If unset, and color support is disabled, then `PAGER` is used. If that has no value either, `less -s` is used.”（`/usr/share/man/man1/man.1:413-425`；`-P` 在 124-130 行，默认值注明 `less -sR`/`less -s`）。

man-db 的顺序在源码里写死（`src/man.c:4258-4266`）：

```c
if (pager == NULL)
    pager = getenv ("MANPAGER");
if (pager == NULL)
    pager = getenv ("PAGER");
if (pager == NULL)
    pager = get_def_user ("pager", NULL);
if (pager == NULL) { … PROG_PAGER … }
```

`PROG_PAGER` 由 configure 从 `pager less more` 里挑（`configure.ac:121-122`）；Debian 上 `man -d` 打出 `using pager as pager`，`/usr/bin/pager` 是 alternatives 指向 less。文档：`-P` “overrides the $MANPAGER environment variable, which in turn overrides the $PAGER environment variable. It is not used in conjunction with -f or -k.”

`LESS` 不参与 `man` 的挑选，只在 less 启动后生效（less 自己读 `LESS`）；man-db 例外，它会写 `LESS`。

---

## 5. 格式化字符：SGR 还是退格叠加

### 5.1 macOS：只有退格叠加，`MAN_KEEP_FORMATTING` 无效（实测）

```console
$ MANPAGER=/tmp/pm/a.sh man ls        # stdout 是管道
stdin_bytes=27493 ESC=0 BS=786

$ MAN_KEEP_FORMATTING=1 MANPAGER=/tmp/pm/a.sh man ls
$ cmp k0.raw k1.raw && echo identical
identical bytes with and without MAN_KEEP_FORMATTING (no effect)
```

原始字节（`cat -v`）：

```
LS(1)                       General Commands Manual                      LS(1)

N^HNA^HAM^HME^HE
     l^Hls^Hs M-bM-^@M-^S list directory contents

S^HSY^HYN^HNO^HOP^HPS^HSI^HIS^HS
     l^Hls^Hs [-^H-@^H@A^HAB^HBC^HCF^HFG^HGH^HHI^HIL^HLO^HOP^HPR^HRS^HST^HTU^HUW^HWa^Hab^Hbc^Hcd^Hde^Hef^Hfg^Hgh^Hhi^Hik^Hkl^Hlm^Hmn^Hno^Hop^Hpq^Hqr^Hrs^Hst^Htu^Huv^Hvw^Hwx^Hxy^Hy1^H1%^H%,^H,] ...
```

- `ESC = 0`：没有 SGR，连伪终端下也没有（`mandoc` 直接跑在 pty 上同样 `ESC=0 BS=788`）。
- 粗体 = `c\bc`，下划线 = `_\bc`，与 mandoc(1) 的说明一致：“Font styles are applied by using back-spaced encoding … Emboldened characters are rendered as `c\bc`”（`/usr/share/man/man1/mandoc.1:250-260`）。
- `MAN_KEEP_FORMATTING` 在这个实现里根本没有代码路径：全脚本 grep 不到该变量；本机所有 man page 也 grep 不到（`grep -rl MAN_KEEP_FORMATTING /usr/share/man` 无输出）。
- `MANCOLOR=1` 只把分页器换成 `less -sR`，并不要求 mandoc 输出颜色：实测此时 stdin 仍 `ESC=0 BS=786`。所以 macOS 上 `-R` 是为**未来/其他页面**留的口子，当前渲染没有 SGR。

### 5.2 Linux man-db：非 tty 默认剥掉格式，`MAN_KEEP_FORMATTING` 保留（容器实测）

```console
$ man ls | head -150 > a.out
$ MAN_KEEP_FORMATTING=1 man ls | head -150 > b.out
a: ESC=0 BS=0
b: ESC=0 BS=706
```

- 默认：`col -b -p -x` 清掉退格/下划线，得到纯文本。
- 设了 `MAN_KEEP_FORMATTING`（任意非空值）：保留 `c\bc` 形式。
- 源码同一处还在非 tty、未设该变量时 `setenv("GROFF_NO_SGR","1")`（`src/man.c:1439-1456`），即 **tty 下 groff 走 SGR**；这一条本次没有 Linux tty 实测，标为源码依据。
- 文档：“Normally, when output is not being directed to a terminal … formatting characters are discarded … if `$MAN_KEEP_FORMATTING` is set to any non-empty value, these formatting characters are retained. This may be useful for wrappers around man that can interpret formatting characters.”（man-db man(1)）

结论：`MAN_KEEP_FORMATTING` 是 **man-db 专属契约**，BSD man 完全忽略。跨平台 wrapper 不能靠设置它来“保证拿到格式”，必须自己检测字节。

---

## 6. 哪些入口经过分页器

macOS 实测（探针统计调用次数）：

| 命令 | 分页器调用 | 说明 |
|---|---|---|
| `man ls` | 1 | `mandoc \| $MANPAGER` |
| `man ls cp` | 2 | 每个 page 参数各 1 次，串行 |
| `man -a printf` | 3 | 每个匹配页各 1 次（本次命中 3 个） |
| `man -w ls` | 0 | 只打印 `/usr/share/man/man1/ls.1` |
| `man -t ls` | 0 | 直接输出 PostScript（61635 字节，`%!PS-Adobe-3.0`） |
| `man -k printf` | 0（用 `MANPAGER`） / ≥1（用 `WHATISPAGER` 或 `PAGER` 或 `-P`） | 走 `search_whatis()`，管道目标是 `$WHATISPAGER` |
| `man -k` 无匹配 | 0 | 只有 stderr 的 “nothing appropriate”，`good` 为空就不进分页器 |

`-k`/`-f` 的细节（macOS）：

```sh
if [ -n "$MANCOLOR" ]; then WHATISPAGER="$MORE_CMD -ER"
else if [ -n "$PAGER" ]; then WHATISPAGER="$PAGER"
     else WHATISPAGER="$MORE_CMD -E"; fi
fi
# 最后： printf "%b\n" "$good" | $WHATISPAGER
```

即：`MANPAGER` 对 apropos/whatis 无效；`-P` 因为同时写 `WHATISPAGER` 而有效；默认 `more -E`，`MANCOLOR` 时 `more -ER`（`/etc/man.conf` 注释也写明 “default WHATISPAGER … remains more(1) for historical compatibility”）。

Linux man-db（未实测，文档+源码）：man(1) 明说 `-P` “is not used in conjunction with -f or -k”；`whatis.c` 里没有任何 pager/`isatty` 逻辑，apropos(1) 手册也没有分页器字样。据此 `man -k`/`apropos` 在 man-db 上不分页。

`-t` 的 man-db 文档示例直接是 `man -t bash | lpr -Pps`，输出到 stdout（man-db man(1) EXAMPLES）。

---

## 7. 非 TTY 行为

这是两套实现最硬的差异：

- **macOS/BSD man：无条件起分页器。** 探针在 stdout 为管道时仍被调用：`tty0=n tty1=n`，并且 `man ls | wc -l` 得到 525 行（less 在非 tty 下原样透传）。`man` 自身没有任何 `isatty` 检查。
- **man-db：只在 stdout 是 tty 时起分页器。** `src/man.c:1691`：

```c
if (isatty (STDOUT_FILENO)) {
    …
    pager_cmd = pipecmd_new_argstr (pager);
}
```

容器实测一致：

```console
$ PROBE=a MANPAGER=/w/probe.sh man ls >/dev/null
calls=0
```

后果：分页器**不能**把“自己被启动了”当作“用户在终端前”的信号；macOS 上这条永远为真。判断 tty 只能自己 `isatty(1)`。

---

## 8. 宽度与 locale

### 8.1 宽度

macOS `man_setup_width()`：

```sh
if [ -z "$MANWIDTH" -a -t 0 -a -t 1 ]; then MANWIDTH=tty; fi
case "$MANWIDTH" in
[0-9]*) … use_width=$MANWIDTH ;;
[Tt][Tt][Yy]) … use_width=$(($2-2)) ;;   # stty size 的列数 − 2
esac
```

实测（pty 100 列）：

| `MANWIDTH` | 分页器 stdin 首行长度 |
|---|---|
| 未设 | 98（100−2） |
| `40` | 40 |
| `garbage` | 78（mandoc 默认） |

文档：`MANWIDTH` “numeric … Otherwise, if set to the special value `tty`, and output is to a terminal, the pages may be displayed over the whole width of the screen.”（man.1:405-410）；mandoc 默认宽度 78，窄于 79 列时减到终端宽度−1（mandoc.1:332-338）。

man-db：`MANWIDTH` > `COLUMNS`/ioctl > 80（文档）；容器实测 `MANWIDTH=40` 时页眉被压到 40 列：`LS(1)        User Commands       LS(1)`。

### 8.2 locale

macOS 实测（同一页、同一格式）：

```console
LC_ALL=C        →      l^Hls^Hs - list directory contents
LC_ALL=C.UTF-8  →      l^Hls^Hs M-bM-^@M-^S list directory contents   # U+2014 EM DASH
```

mandoc 按 `LC_ALL`/`LC_CTYPE`/`LANG` 中第一个有值的变量选 UTF-8 或 ASCII 输出（mandoc.1 “Locale Output”，441-456 行）。

man-db + groff：容器内 `ls.1`（纯 ASCII 内容）在 `C` 与 `C.UTF-8` 下第 4 行逐字节相同。差异要看含引号/破折号的页，本次未做逐页 locale 矩阵，标为未覆盖。

---

## 9. mandoc vs groff：同一页的渲染差异

### 9.1 实测（Debian 容器，同一份 `/usr/share/man/man1/ls.1`，80 列）

```console
$ MANWIDTH=80 man ls > g.out        # groff 1.22.4 via man-db
$ mandoc /usr/share/man/man1/ls.1 > m.out
groff  hdr: LS(1)                            User Commands                           LS(1)
mandoc hdr: LS(1)                            User Commands                           LS(1)
maxlen groff=78 mandoc=130
indent groff:  4 行@7 空格, 46 行@14 空格
indent mandoc: 13 行@7 空格, 59 行@14 空格
```

- **页眉**：两者都把 `.TH LS 1` 渲染成 `LS(1) … User Commands … LS(1)`。man-db 用 groff 的 `-mandoc` 宏包，mandoc 在 Debian 上也给出 `User Commands`（OS 字符串来自 `-I os=` 或内置表）。
- **正文缩进**：都是选项名 7 空格、描述 14 空格；差异在折行数量（同一段落在 mandoc 下分成更多行），不是缩进宽度。
- **宽度**：groff 最长行 78；mandoc 出现 130 列的长行。原因是 mandoc 对 literal/示例块不折行（mandoc.1 明说 “lines that are output in literal mode are never wrapped and may exceed the output width”），而 groff 的 man 宏在该处会拆行。**分页器必须能处理超宽行**（横向滚动或截断策略），不能假设 ≤80。

macOS 上的同页页眉不同，因为 Apple 的 mandoc 用 “General Commands Manual” 作为 section 1 的 OS 字符串：

```console
$ MANPAGER=/tmp/pm/probe.sh man ls
LS(1)                       General Commands Manual                      LS(1)
```

同一页在 Apple mandoc 下默认宽度 78，声明了 `MANWIDTH=tty` 才变 98。

### 9.2 `-t` 路径

macOS：`mandoc -Tps`，直接吐 PostScript 到 stdout（实测 61635 字节，`%!PS-Adobe-3.0`，`%%DocumentMedia: man-Letter`），不经过分页器。man-db：`man -t` 输出到 stdout 供打印（文档示例 `man -t bash | lpr -Pps`）。两者都把 `-t` 排除在分页器契约之外。

---

## 10. less 在 SGR 与宽字符下的行为

### 10.1 实测（macOS，less 668，60 列 pty）

输入文件同时含 SGR、日文宽字符、退格叠加：

```
header plain
\033[1mSGR bold\033[0m and \033[31mred\033[0m
日本語のワイド文字テキスト
overstrike B\bBo\bOl\bLd\bD here
end
```

| 启动方式 | less 写回终端的字节（节选） |
|---|---|
| `less file` | `\x1b[7mESC\x1b[27m[1mSGR bold…` —— ESC 被当作**可见字符**高亮显示，SGR 未生效 |
| `less -R file` | `\x1b[1mSGR bold\x1b[0m and \x1b[31mred\x1b[0m` —— SGR 原样透传，且行尾补 `\x1b[m` 复位 |
| `less -r file` | `\x1b[1m…\x1b[0m` 也透传，但不维护屏幕外观（无复位序列） |
| `less -RX file` | 同 `-R`，只是跳过终端初始化 |

- **`-R` 是 SGR 的必要条件**：不加就是把 `ESC` 当文本显示。文档：`-R` “Like -r, but only ANSI ‘color’ escape sequences and OSC 8 hyperlink sequences are output in ‘raw’ form. Unlike -r, the screen appearance is maintained correctly … Color escape sequences are only supported when the color is changed within one line”（`/usr/share/man/man1/less.1.gz`）。
- **退格叠加不需要任何开关**：`B\bBo\bO…` 在四种模式下都被 less 渲染成粗体 `OLD`。也就是说 mandoc 的输出天然可读，macOS 默认的 `less -s` 够用。
- **CJK 宽字符**：四种模式下日文 UTF-8 字节都原样出现在输出里（`\xe6\x97\xa5…`），没有丢字节；本次没有做“宽字符后光标列是否错位”的逐列断言。

### 10.2 版本差异

macOS 是 less 668，Linux 容器是 less 590。less 590 的 SGR/宽字符组合本次**未实测**（容器内 pty 测试在收尾阶段被叫停）。可依赖的只有文档契约：`-R` 只保证“ANSI 颜色序列按零宽处理”，跨行颜色不保证（less(1) 原文）。宽字符对齐在旧版 less 上的行为没有本文档依据，标为未验证。

另：`LESS` 是 less 自己的环境变量，不是 `man` 的接口。`man` 用 `less -s` 时，用户的 `LESS=FRX` 仍会叠加生效（本次探测环境里就带着 `LESS=FRX`，分页器侧看得到）。

---

## 11. 对 man-ai 规格的直接影响

- **按“shell 命令字符串”实现 `MANPAGER`，不要只接受一个可执行路径**：BSD man 用 `eval`，参数、引号、管道都合法；man-db 用 argstr，参数和引号合法但**管道不合法**。要跨平台，就只能生成“命令 + 参数”形态，并在需要管道时用 wrapper。
- **页面内容永远从 stdin 读，绝不从 argv 读**：两套实现都把格式化结果写进管道（`S_ISFIFO`），argv 里没有文件名。反过来，man-ai 作为被调用方也必须假设 stdin 不可 seek。
- **不要依赖 `MAN_KEEP_FORMATTING`**：BSD man 根本没实现；macOS 上设与不设字节完全相同。要 SGR 只能自己检测输入里有没有 `ESC[`，要粗体则必须解析 `c\bc`。
- **必须同时支持退格叠加和 SGR**：macOS 的 mandoc 只给退格叠加；man-db 非 tty 默认把格式剥光（`col -b`），tty 下 groff 才给 SGR。三种形态都要能渲染，不能只处理 SGR。
- **`-P` 的优先级高于 `MANPAGER`，且会覆盖它**：想当默认分页器只能靠 `MANPAGER`（或包装 `PAGER`），无法阻止用户用 `man -P` 顶掉。macOS 上 `-P` 还会同时顶掉 `WHATISPAGER`。
- **`-k`/`-f` 不要指望走 `MANPAGER`**：macOS 走 `WHATISPAGER`（默认 `more -E`），man-db 干脆不分页。apropos 类输出若要自定义渲染，需要单独接管 `WHATISPAGER` 或包装 `apropos`。
- **`-w`/`-t` 永远不过分页器**：不要把“所有 man 输出都经过我”写进规格；PostScript 路径直接落 stdout。
- **非 tty 判定必须自己 `isatty(1)`**：macOS 在 stdout 是管道时照样启动分页器，man-db 则不会。用“分页器被启动”推断终端存在会在 macOS 上出错。
- **分页器退出码会成为 `man` 的退出码**：正常退出 0，用户 `q` 退出 0；非零会被 `man` 原样返回，脚本里的 `man … || …` 会误判。分页器内部错误不要用非零退出码上报。
- **不要重排宽度**：`man` 已经把宽度算好（macOS `cols-2`，man-db `MANWIDTH`/`COLUMNS`/ioctl/80），mandoc 还会输出超过宽度的 literal 行（实测 130 列）。分页器应横向滚动而不是二次折行。
- **不要假设 cwd**：BSD man 把 cwd 切到页面目录（`/usr/share/man`），man-db 保持调用者 cwd。分页器里解析相对路径必须基于明确的绝对路径。
- **`LESS` 的归属**：BSD man 不设置 `LESS`；man-db 会设置并塞入 `$MAN_PN` 提示串。如果 man-ai 自己设置 `LESS`，在 man-db 下会覆盖掉 man 的提示串，在 macOS 下则是唯一来源。规格里要写清楚谁拥有 `LESS`。

---

## 12. 未实测与风险

- Linux 侧 man-db 的**分页器启动**只验证了非 tty（0 次调用）；tty 下 `MANPAGER` 的 argv/stdin 形态由源码（`isatty` + `pipecmd_new_argstr` + `setenv_less`）和文档推断，未在容器 pty 里直接抓取。
- groff 在 tty 下输出 SGR、非 tty 下被 `GROFF_NO_SGR` 切成退格叠加：来自 `src/man.c:1439-1456`，未直接观测 grotty 输出字节。
- less 590 的 SGR/宽字符行为未实测（仅 less 668 有 pty 原始字节）。
- 宽字符“列对齐”未做逐列断言，只确认字节不丢失。
- macOS 没有 groff，无法在本机做 groff vs mandoc 同页对照；Linux 侧的对照用的是 Debian 的 groff 1.22.4 与 Debian mandoc。
- claim “man-db 的 `man -k` 不分页”来自 man(1) 文档的 “not used in conjunction with -f or -k”、`whatis.c` 无 pager 代码、apropos(1) 无分页器描述；未在 Linux 上抓包验证。

---

## 附：引用清单

| 编号 | 来源 | 用途 |
|---|---|---|
| S1 | 本机 `/usr/bin/man`（FreeBSD man.sh + Apple 补丁），函数 `setup_pager`/`man_display_page`/`man_setup_width`/`search_whatis` | macOS 分页器选择、eval 管道、宽度、`-k` 走 `WHATISPAGER` |
| S2 | 本机 `/usr/share/man/man1/man.1`（macOS，行 124-130、405-425） | `-P` 默认值、`MANPAGER`/`PAGER`/`MANCOLOR`/`MANWIDTH` 文档语义 |
| S3 | 本机 `/usr/share/man/man1/mandoc.1`（行 250-260、332-338、441-456、644-664） | 退格叠加编码、默认宽度 78、locale 选择、`MANPAGER` 环境变量 |
| S4 | 本机 `/usr/share/man/man1/less.1.gz` | `-R`/`-r` 的 SGR 与屏幕外观语义 |
| S5 | man-db man(1)，<https://manpages.debian.org/bookworm/man-db/man.1.en.html> | `-P` > `MANPAGER` > `PAGER`；`-f`/`-k` 不用分页器；`MAN_KEEP_FORMATTING`；`-r` 设置 `LESS` |
| S6 | man-db 源码 `src/man.c`，<https://gitlab.com/man-db/man-db/-/blob/main/src/man.c>（4258-4266 选择、1691-1706 `isatty` 才挂分页器、1439-1456 `GROFF_NO_SGR`、1547-1570 `setenv_less`） | Linux 分页器契约的实际实现 |
| S7 | man-db `configure.ac:121-122` | `PROG_PAGER` 来自 `pager less more` |
| S8 | apropos(1)，<https://manpages.debian.org/bookworm/man-db/apropos.1.en.html>；`src/whatis.c` | man-db 的 `-k`/apropos 不分页 |
| S9 | FreeBSD `usr.bin/man/man.sh`，<https://cgit.freebsd.org/src/tree/usr.bin/man/man.sh> | macOS man 的上游实现 |
