# 单一二进制与严格审批模式

Zuno 可以以一个静态链接的可执行文件在服务器上运行，并在每条命令执行前停下来等人审批。
这覆盖了最常见的排障场景：把一个文件拷到主机上、登录、让模型提出诊断步骤，由人逐条放行。

## 单一二进制

每次发布会在六个平台包之外额外发布一个静态链接的 Linux 可执行文件：

- `zuno-standalone-x86_64-unknown-linux-musl`

它没有动态加载器，也不依赖任何共享库，可以在任意 x86_64 Linux 内核上运行，不受发行版
glibc 版本影响，包括 `FROM scratch` 容器。拷到 `PATH` 下任意位置、`chmod +x`，以 `zuno`
运行即可。

单文件不携带的东西，以及对服务器的影响：

- **沙箱辅助程序。** 平台包内置 Linux 沙箱用的 `bwrap`，单文件没有。请使用
  `sandbox_mode = "danger-full-access"`（或安装发行版的 `bubblewrap`，Zuno 会从 `PATH`
  找到它）。严格审批模式下安全边界是审批提示，而不是沙箱。
- **Code Mode host。** 已内嵌。模型目录里标为 `tool_mode = "code_mode_only"` 的模型
  （gpt-5.6 各族，Kiro provider 提供的全部模型都属于此类）会把每一次工具调用都经由
  `codex-code-mode-host` 执行。平台包把它作为同目录的伴生二进制发布；单文件把它编进了
  自身，并通过 `$ZUNO_HOME/tmp/arg0` 下每会话辅助目录里的 arg0 别名来启动。发布版构建
  在 `$ZUNO_HOME` 位于系统临时目录之下时会拒绝创建辅助程序，所以主目录要放在别处
  （默认的 `~/.zuno` 即可）；否则这些模型会在审批提示之后回答
  "the shell tool failed to start"。
- **TLS 根证书。** HTTPS 使用主机的证书库（`/etc/ssl/certs` 或 `SSL_CERT_FILE`）。
  `scratch` 容器访问远端 provider 时需要挂入 CA 证书。

本地构建同一产物。内嵌的 host 需要链接 Codex 为 musl 发布的 V8 预编译包：从 `openai/codex`
的 `rusty-v8-v<版本>` release 下载静态库、bindgen 输出和校验清单（`.github/actions/setup-rusty-v8`
列出了确切文件名，并用 `third_party/v8/` 里的可信清单校验）：

```sh
rustup target add x86_64-unknown-linux-musl
cd codex-rs
RUSTY_V8_ARCHIVE=/path/to/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.a.gz \
RUSTY_V8_SRC_BINDING_PATH=/path/to/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.rs \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_x86_64_unknown_linux_musl=musl-gcc \
AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
cargo build --locked --release --target x86_64-unknown-linux-musl -p codex-cli --bin zuno
```

选择内嵌 host 的是 musl 目标本身：`codex-cli` 把 `codex-code-mode-host` 列为仅 musl 的依赖
（与 jemalloc 分配器并列），不涉及 cargo feature，glibc、macOS、Windows 包不受影响。

## 严格审批模式

`approval_policy = "untrusted"` 要求每条 shell 命令和每次文件修改都先审批，除非
`$ZUNO_HOME/rules/*.rules` 里有显式的 `allow` 规则匹配。默认不附带任何规则，所以不会有
任何东西不经提示就执行。Codex 上游已把这个值从用户可选项中移除；Zuno 在 `config.toml`、
profile 文件和命令行上重新接受它：

```sh
# 临时使用
zuno -a untrusted -s danger-full-access

# 作为可复用 profile：把 examples/zuno-config/server-strict.config.toml 复制到
# $ZUNO_HOME/server-strict.config.toml，填好 provider，然后
zuno --profile server-strict
```

该 profile 同时设置 `approvals_reviewer = "user"`，自动审阅器永远不能替你放行。审批提示里的
"本会话内允许"仍然可以让你对已经审过的同一条命令不再被反复询问。

持久终端同样受控。模型用 `exec_command` 保持一个 shell 打开、之后再用 `write_stdin` 往里输入时，
严格模式把每次输入都当作新命令重新审批（只有单独的 Ctrl-C 例外），不依赖实验性的
`write_stdin_approval` 特性，终端权限没变也一样。

仍能跳过提示的只有操作者自己写下的东西：`$ZUNO_HOME/rules/*.rules` 里的 `allow` 规则，或
`hooks.json` 里回答 `allow` 的 `PermissionRequest` hook。严格 profile 两者都不附带；在要求每条
命令都审批的主机上不要添加它们。

`zuno exec` 是无头模式，始终以 `approval_policy = "never"` 运行；严格模式作用于交互式
TUI、`zuno app-server` 与 `zuno acp`。

## 冒烟测试

`scripts/zuno_standalone_smoke.py` 只依赖 Python 标准库。它校验 ELF 没有动态加载器、运行
`zuno --version`，然后用本地 mock 模型分四段驱动 `zuno app-server`：严格模式下对提出的 `ls`
必须发出 `item/commandExecution/requestApproval` 且拒绝后什么都没执行；`approval_policy = "never"`
下同样的 `ls` 必须不问直接运行；批准 `/bin/sh -i` 之后，后续的 `write_stdin` 必须再次审批
（kind 为 `writeStdin`），拒绝后不留任何痕迹；换成 `code_mode_only` 模型 slug 后，mock 发来的
JavaScript 程序必须到达内嵌的 host，其中的 `exec_command` 必须停在审批提示上，输出必须回传给模型。

```sh
python3 scripts/zuno_standalone_smoke.py --binary ./zuno-standalone-x86_64-unknown-linux-musl

# 在只有二进制的容器里做同样的检查
docker run --rm -v "$PWD/zuno:/zuno:ro" scratch /zuno --version
docker run --rm -v "$PWD:/work:ro" -w /work python:3.12-alpine \
  python3 scripts/zuno_standalone_smoke.py --binary ./zuno
```

PR 门禁会对每个候选执行构建、在 runner 上跑冒烟，并在 `python:3.13-slim` 容器里再跑一遍同样的
冒烟；发布晋升会把该二进制与平台包一起发布。

`scripts/zuno_standalone_live_check.py` 用真实的 Responses 兼容 provider（例如本机的 Kiro
provider）重复严格模式检查，这是端到端验证真正的 `code_mode_only` 模型的唯一途径：

```sh
export KIRO_PROVIDER_API_KEY=...   # provider 的 bearer token
python3 scripts/zuno_standalone_live_check.py \
  --binary ./zuno-standalone-x86_64-unknown-linux-musl \
  --base-url http://127.0.0.1:8787/v1 --model gpt-5.6-sol-low
```

它先让模型运行 `ls -la`（必须出现审批，接受后命令必须执行），再让模型创建一个文件（必须出现
审批，拒绝后不能留下文件）。在容器里运行时加 `--network host`，以便访问宿主机上的 provider。
