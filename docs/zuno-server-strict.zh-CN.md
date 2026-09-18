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
- **Code Mode host。** Code Mode 默认关闭且保持关闭；它需要完整包里的
  `codex-code-mode-host`。
- **TLS 根证书。** HTTPS 使用主机的证书库（`/etc/ssl/certs` 或 `SSL_CERT_FILE`）。
  `scratch` 容器访问远端 provider 时需要挂入 CA 证书。

本地构建同一产物：

```sh
rustup target add x86_64-unknown-linux-musl
cd codex-rs
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_x86_64_unknown_linux_musl=musl-gcc \
AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
cargo build --locked --release --target x86_64-unknown-linux-musl -p codex-cli --bin zuno
```

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

`zuno exec` 是无头模式，始终以 `approval_policy = "never"` 运行；严格模式作用于交互式
TUI、`zuno app-server` 与 `zuno acp`。

## 冒烟测试

`scripts/zuno_standalone_smoke.py` 只依赖 Python 标准库。它校验 ELF 没有动态加载器、运行
`zuno --version`，然后用本地 mock 模型驱动 `zuno app-server`：模型提出 `ls`，严格模式下
服务端必须发出 `item/commandExecution/requestApproval`，拒绝后回合必须结束且什么都没执行。

```sh
python3 scripts/zuno_standalone_smoke.py --binary ./zuno-standalone-x86_64-unknown-linux-musl

# 在只有二进制的容器里做同样的检查
docker run --rm -v "$PWD/zuno:/zuno:ro" scratch /zuno --version
docker run --rm -v "$PWD:/work:ro" -w /work python:3.12-alpine \
  python3 scripts/zuno_standalone_smoke.py --binary ./zuno
```

PR 门禁会对每个候选执行构建与冒烟，发布晋升会把该二进制与平台包一起发布。
