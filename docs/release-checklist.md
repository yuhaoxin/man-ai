# 发布前检查清单

发布只验流程，不验译文质量：不设译文黄金文件，不设覆盖率门槛。

## 1. 版本与提交

```sh
# Bump [package] version in Cargo.toml, then refresh the lockfile.
cargo update -p man-ai
cargo fmt --check
cargo test
cargo build --release
cargo deny check licenses advisories
```

- `deny.toml` 必须允许全部实际依赖的宽松许可，且不包含 GPL/AGPL/SSPL（未列入 `allow` 即拒绝）。
- 提交信息用中文 Conventional Commits，不加 AI 署名。

## 2. 打包产物核对

```sh
# Requires cargo-dist (0.34.x). The config lives in dist-workspace.toml.
# First release only: `cargo dist init` generates .github/workflows/release.yml
# from this config (that file is not committed yet).
cargo dist plan
cargo dist build --artifacts=local
```

`cargo dist plan` 应列出且只列出四个 target 的 `<app>-<target>.tar.gz` 与 `.sha256`：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

逐个解包确认产物只含 `man-ai` 二进制：没有 shell 补全、没有 man 手册、没有 README/LICENSE、没有源码 tarball。

```sh
tar -tzf man-ai-<target>.tar.gz
shasum -a 256 -c man-ai-<target>.tar.gz.sha256   # Linux: sha256sum -c
```

## 3. 发布与 tap

1. Push tag `v<version>`；cargo-dist 生成的 release workflow 构建四个 target 并把 tar.gz + sha256 上传到 GitHub Release。
2. Homebrew tap `yuhaoxin/homebrew-tap`：
   - 正常路径：release 后 cargo-dist 用 `HOMEBREW_TAP_TOKEN`（对 tap 有写权限）更新 `Formula/man-ai.rb`。
   - 手工路径：以 `packaging/homebrew/man-ai.rb` 为模板，填入 `version` 与两个 target 的 `sha256`，提交到 tap。
3. 验证 formula：

   ```sh
   brew update
   brew install yuhaoxin/tap/man-ai
   brew test man-ai
   brew audit --strict man-ai
   ```

   formula 名与二进制名一致，不提供 `man` 名下的 formula；不发布 bottle，`url` + `sha256` 指向同一份 Release tarball；caveats 只打印 `MANPAGER` 与 shell 函数建议。
4. `brew upgrade man-ai` 后确认 `man-ai --version` 与新 tag 一致。

## 4. CI 四行矩阵

| 行 | Runner | 内容 |
|---|---|---|
| 1 | `ubuntu-24.04` | `cargo test`、`cargo build --release`、`cargo-deny check licenses advisories` |
| 2 | `macos-15`（arm64） | `cargo test`、`cargo build --release` |
| 3 | `macos-15-intel` | 同上；2027-08 runner 退役时删除该行 |
| 4 | Alpine 容器（musl） | 构建、单元测试、非 TTY 路径、`-w`/`-k`/`-t` 透传；不做差分 |

Alpine 不做差分：其 `man` 来自 mandoc，契约与 GNU man-db 不同。四行全绿才可发布。

## 5. 发布前手测（不进 CI）

在 macOS（BSD man + mandoc）与 Linux（man-db + groff）各做一次，终端里跑：

1. 真 less：`MANPAGER="man-ai --lang zh-CN" man ls`
   - 页首行 `── man-ai: translation (Simplified Chinese) ──` 与尾行可见。
   - less 内 `/` 搜索、`n`/`N` 跳转、空格/`b` 翻页正常；`q` 退出后退出码为 less 的退出码。
   - 粗体/下划线等样式与原文一致，表格列没有明显错位。
2. Ctrl-C 两阶段：翻译途中按一次 Ctrl-C
   - 已写出的块保留，未完成部分回退原文，尾行显示 `── man-ai: translation failed (interrupted), remaining blocks left untranslated ──`。
   - 分页器照常启动；再按一次 Ctrl-C 退出分页器。
3. 超时中断：把 `timeout_seconds` 调到 `0.5` 或指向慢端点
   - 单块超时只回退该块，尾行显示 `partially translated (...)`，界面不假死。
   - 超时不重试。
4. 非 TTY：`man ls | MANPAGER=cat man-ai` 与 `man ls` 逐字节一致（`--no-translate`、`--lang en` 同样）。

## 6. 发布后抽查

```sh
# Fresh machine or clean PATH:
curl -fLO .../man-ai-<target>.tar.gz
shasum -a 256 -c man-ai-<target>.tar.gz.sha256
tar -xzf man-ai-<target>.tar.gz
export MANPAGER="$PWD/man-ai-<target>/man-ai"
man ls
```

macOS 浏览器下载的副本需要 `xattr -d com.apple.quarantine man-ai`；curl 与 brew 的副本不需要。
