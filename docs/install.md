# 安装 man-ai

`man-ai` 是单个二进制，只支持 macOS 与 Linux。它不替代 `man`：页面检索与渲染仍由平台自带的 `man`（macOS 的 BSD man + mandoc，Linux 的 man-db + groff）完成，`man-ai` 只包装渲染后的文本。

发布目标只有四个：

| target | 平台 |
|---|---|
| `aarch64-apple-darwin` | Apple Silicon macOS |
| `x86_64-apple-darwin` | Intel macOS |
| `aarch64-unknown-linux-gnu` | arm64 Linux |
| `x86_64-unknown-linux-gnu` | x86_64 Linux |

不提供 Windows、musl 静态构建与 `aarch64-unknown-linux-musl`。

## 三种安装方式

### 1. cargo

```sh
cargo install man-ai
# 仓库内开发版本：
cargo install --git https://github.com/yuhaoxin/man-ai
cargo install --path .
```

### 2. GitHub Release tarball

Release 资产是 `man-ai-<version>-<target>.tar.gz` 与同名的 `.sha256`，由 cargo-dist 生成，包内只有一个 `man-ai` 二进制（无 shell 补全、无 man 手册）。

```sh
VERSION=0.1.0
TARGET=aarch64-apple-darwin   # 换成上表中的 target
base="https://github.com/yuhaoxin/man-ai/releases/download/v${VERSION}"
curl -fLO "${base}/man-ai-v${VERSION}-${TARGET}.tar.gz"
curl -fLO "${base}/man-ai-v${VERSION}-${TARGET}.tar.gz.sha256"

# 校验：macOS 用 shasum，Linux 用 sha256sum
shasum -a 256 -c "man-ai-v${VERSION}-${TARGET}.tar.gz.sha256"
# sha256sum -c "man-ai-v${VERSION}-${TARGET}.tar.gz.sha256"

tar -xzf "man-ai-v${VERSION}-${TARGET}.tar.gz"
mkdir -p ~/.local/bin
install -m 755 "man-ai-v${VERSION}-${TARGET}/man-ai" ~/.local/bin/man-ai
```

`~/.local/bin` 需要在 `PATH` 里。

### 3. Homebrew tap

```sh
brew install yuhaoxin/tap/man-ai
```

formula 只有 `man-ai` 一个名字，不提供 `man` 名下的 formula。它用 `url` + `sha256` 引用上面同一份 Release tarball，不自建 bottle。

## 启用翻译：四种替换方式

前两种是正式支持的方式；安装器与 formula 只打印建议，不修改你的 shell 启动文件。

| 方式 | 做法 | 支持 | 影响面 | 撤销 |
|---|---|---|---|---|
| 1. 导出 `MANPAGER` | `export MANPAGER=man-ai` 写进 shell 启动文件 | 支持 | 所有经 man 分页器的路径；`man -P` 等自带分页器的调用让路 | 删掉该行，或 `unset MANPAGER` |
| 2. shell 函数/别名 | `man() { MANPAGER=man-ai command man "$@"; }` | 支持 | 只影响交互式 shell；脚本与其它程序不受影响 | 删掉函数定义，`unset -f man` |
| 3. PATH 里的 `man` shim | 放一个名为 `man` 的脚本或软链到 PATH 前部 | 不支持 | 依赖 PATH 顺序；薄壳 `exec man` 会命中自身导致递归 | 从目录删除文件；改过 PATH 顺序还要恢复 |
| 4. 软链 `/usr/bin/man` | 把 `man` 换成指向 `man-ai` 的软链 | 不支持 | macOS 受 SIP 保护不可行；Linux 上会被 man-db 升级覆盖，同样有递归问题 | 删软链并重装 man-db |

方式 3、4 明确不支持：PATH 顺序因机器而异，薄壳必须跳过自身去找真 `man`，软链会被包管理器覆盖。不要用它们。

### 方式 1：导出 MANPAGER

```sh
# ~/.zshrc 或 ~/.bashrc
export MANPAGER=man-ai
```

`man` 按 shell 词法拆词 `MANPAGER`，支持空格分词与引号，不支持管道与命令替换。因此可以带参数：

```sh
export MANPAGER="man-ai --lang zh-CN"
export MANPAGER="man-ai --lang ja --translate"
```

`man-ai` 所在目录不一定在 `PATH` 前部时，用绝对路径；绝对路径由 shell 求值一次：

```sh
export MANPAGER="$(command -v man-ai)"
```

### 方式 2：shell 函数

```sh
# ~/.zshrc 或 ~/.bashrc
man() { MANPAGER=man-ai command man "$@"; }
```

只影响交互式 shell，脚本里的 `man` 保持原样。

`man-ai` 也可以直接当薄壳用：`man-ai ls` 会设置 `MANPAGER` 后 `exec man ls`（此时 stdin 是终端）。

## 配置

配置只有一个位置，没有项目级配置，也没有路径覆盖变量：

- `$XDG_CONFIG_HOME/man-ai/config.toml`
- `XDG_CONFIG_HOME` 未设置或为空时：`~/.config/man-ai/config.toml`
- macOS 同样走 XDG，不用 `~/Library/Application Support`

文件不存在时全部取默认值，不报错。没有任何自动生成配置的入口（无 `--init`）。把下面的模板存成上面的路径，按需删改：

```toml
# Target language as a BCP-47 tag. Empty means: --lang, then LC_ALL/LC_MESSAGES/LANG, then "en".
lang = "zh-CN"

# Required. Full URL including the API prefix; http/https only, no query or fragment.
endpoint = "https://api.deepseek.com/v1"

# Required. Sent as the request "model" field; no CLI override.
model = "deepseek-chat"

# Plaintext key. Takes precedence over api_key_env.
api_key = ""

# Name of an environment variable that holds the key; read at request time.
api_key_env = "DEEPSEEK_API_KEY"

# Optional per-request limit; omitted means the field is not sent.
max_tokens = 4096

# Per-request timeout in seconds; must be > 0.
timeout_seconds = 30

# Maximum in-flight requests; >= 1.
concurrency = 4

# false disables the translation cache entirely (no directory is created).
cache_enabled = true

# Cache size cap in MB; 0 means unlimited.
cache_max_mb = 200

# Explicit proxy; http://, https:// or socks5://. Empty means HTTPS_PROXY/HTTP_PROXY/ALL_PROXY.
proxy = ""
```

值的来源与优先级：命令行 `--lang` 覆盖 `lang`；密钥只从 `api_key` 或 `api_key_env` 指向的变量读；`proxy` 未配置时才使用 `HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY`、`NO_PROXY`。

### 密钥、权限与隐私

- `api_key` 以明文存在这个 TOML 里。用 `chmod 600 ~/.config/man-ai/config.toml` 限制可读范围，或者改用 `api_key_env` 把密钥放进环境变量/密钥管理器。
- `endpoint` 的 host 是 `localhost`、`127.0.0.1` 或 `::1` 时视为本地端点，密钥可以留空。
- 缓存目录 `$XDG_CACHE_HOME/man-ai/v1` 下保存的是**译文明文**，不含原文、密钥或诊断信息；`cache_enabled = false` 关闭。
- 程序不写日志文件；诊断只写到 stderr。

## 首次运行

- 不引导、不生成配置、不交互。
- 未配置或必填项缺失时整页回退原文，页首行显示 `translation failed (not configured)`，不发任何请求。
- `--version` 只输出 `man-ai <semver>`，不联网。

## 升级

只有三条路，不做自更新，不在启动时检查版本：

```sh
cargo install man-ai                                                  # 1. 重跑 cargo install
brew upgrade man-ai                                                   # 2. Homebrew
# 3. 重新下载并解压对应 target 的 tarball，覆盖旧的 man-ai
```

升级后旧缓存条目自然不命中（缓存键含提示词版本与内容），不提供迁移或清理命令。若想立刻回收空间，直接删缓存目录，见下文卸载。

## 卸载

```sh
cargo uninstall man-ai        # 或 brew uninstall man-ai
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/man-ai"
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/man-ai"
```

缓存目录只存译文，删除无副作用。shell 启动文件里的 `MANPAGER` 或 `man` 函数由你自己删。

## macOS：quarantine 与签名

v1 不签名、不公证：

- 用 `curl` 或 `brew` 下载的 tarball 不带 `com.apple.quarantine` 属性，解压后可直接运行。
- 通过浏览器下载的 tarball 会带 quarantine，运行时可能被 Gatekeeper 拦截。确认来源后去掉属性：

  ```sh
  xattr -d com.apple.quarantine man-ai
  ```

## 已知限制

完整清单见规格 [`docs/spec/man-ai.md` §15](../docs/spec/man-ai.md)：块级回退粒度、长块整块回退、跨块术语不一致、按序写出的首屏瓶颈、缓存无 TTL、只支持四个 target、无 shell 补全/自更新/版本检查等。
