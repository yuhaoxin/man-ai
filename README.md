# man-ai

`man-ai` 包装平台自带的 `man`，把渲染后的手册页按块翻译成目标语言，再交给用户原有的分页器显示。翻译走 OpenAI 兼容的 `POST <endpoint>/chat/completions`。

```sh
brew install yuhaoxin/tap/man-ai      # 或 cargo install man-ai
export MANPAGER=man-ai
man ls
```

- 规格（唯一契约）：[`docs/spec/man-ai.md`](docs/spec/man-ai.md)
- 安装、替换方式对比与配置模板：[`docs/install.md`](docs/install.md)
- 架构决策：[`docs/adr/`](docs/adr)
- 许可：MIT，见 [`LICENSE`](LICENSE)

只支持 macOS 与 Linux，二进制名固定 `man-ai`；不提供 shell 补全、man 手册、自更新与版本检查。已知限制见规格 §15。
