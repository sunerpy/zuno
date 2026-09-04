# 常见问题

## Zuno 的 Shell 已经受操作系统沙箱约束了吗？

取决于所选的沙箱模式：

- `read-only` 约束对宿主的写入。
- `workspace-write`（默认值）把写入约束在工作区以及受信的额外根目录内。
- `danger-full-access` 刻意以 Zuno 用户身份使用原生 shell，拥有宿主文件系统、进程、
  凭据和网络。它不是一道操作系统安全边界。

两种受约束模式只有在当前平台后端通过完整能力探测时才真正受操作系统沙箱约束。否则
Shell 默认失败即拒绝。受信的全局、显式配置、环境、CLI 或受管层可以把
`sandbox.onUnavailable` 设为 `run-unconfined`；它只允许具备写能力的 Agent 所请求的
`workspace-write`，在平台不支持、缺少受信启动器或命名空间/容器策略不可用等类型化错误下
使用原生后端。只读 Agent、不受信启动器、无效策略/路径、helper/内部错误和命令执行错误
绝不降级。

不可用降级不等于选择 `danger-full-access`。它保留已配置的权限模式、所有显式拒绝与
灾难性命令硬拒绝，同时记录请求的文件系统和网络限制没有得到 OS 强制执行。
显式 `danger-full-access` 会跳过受限后端发现，并把生效权限模式设为 `allow_all`。

在受支持的 Linux 宿主上，Zuno 会定位一个固定的、root 所有的系统 `bwrap`，路径为
`/usr/bin/bwrap` 或 `/bin/bwrap`，探测所需的命名空间，编译生效的 Agent 策略，并且只把
一个不透明的 `PreparedCommand` 传给进程层。

用与 Shell 相同的后端来验证一次部署：

```sh
zuno debug sandbox --mode workspace-write --network deny --check
```

该 JSON 报告包含分阶段检查、规范化的启动器路径、UID/GID/模式/设备/inode、root 归属、
组/全局可写性、特殊位与文件 capability 的信任判断、后端能力，以及确切的探测失败原因。
它会检查启动器的每一级祖先目录，然后通过与 Shell 相同的 bubblewrap、capability 丢弃、
`PR_SET_NO_NEW_PRIVS` 和 seccomp 路径执行 `/usr/bin/true`。当请求的策略无法部署时，
`--check` 以非成功状态退出，即使已配置的降级允许运行时继续。报告会分别显示请求/实际
权限、降级资格、解析类型与类型化原因。不要用 `danger-full-access` 做部署验证：该模式会
被报告为原生执行旁路，并刻意跳过启动器信任检查与约束自测。

Linux 后端会：

- 以只读方式挂载宿主根目录，只覆盖挂载确切的可写根目录；
- 对已存在的 `.git`、`.zuno`、`.agents`、外部 Git 元数据、已配置的受保护路径以及辅助
  可执行文件重新施加只读；
- 使用私有 `/tmp` 与 `/var/tmp`、全新的 `/proc` 与 `/dev`，以及独立的 user、PID、UTS、
  IPC 命名空间，并且默认还包括 network 命名空间；
- 丢弃全部 capability，设置 `PR_SET_NO_NEW_PRIVS`，并在执行所请求的 shell 之前安装
  seccomp；
- 在网络访问被拒绝时阻断 `ptrace`、`process_vm_readv`/`process_vm_writev`、`io_uring`
  以及网络系统调用。

Agent 契约可以收窄已配置的模式。因此一个只读 Agent 即使在调用时选择了更宽的模式，也
没有可写的宿主根目录。具备写能力的 Agent 只在 `workspace-write` 下获得工作区写入权限；
除非具体命令通过了单独的 Git 变更授权，Git 元数据始终受保护。

`permission.mode: "allow_all"` 会跳过 Zuno 的每一次工具准入提示，但不改变沙箱模式。
TUI `--auto` 仍然更窄，无法满足只能由人类回答的请求。选择 `danger-full-access` 刻意把
原生宿主执行与 `allow_all` 的生效权限模式组合在一起，因此 Zuno 不会在 TUI、ACP、
server 或 headless 界面上弹出准入卡片。显式的权限拒绝以及 Shell 风险门禁的灾难性硬拒绝
仍然是终态；它们直接失败，而不是询问。结构化的用户提问不是准入，仍然可能被展示。

macOS 与 Windows 的受约束模式目前返回一个带类型的不支持平台错误。在默认 `deny` 下不会
注册 Shell；受信的 `run-unconfined` 可以让具备写能力的 Agent 原生继续，而显式的
`danger-full-access` 始终可以独立使用原生进程后端。拒绝信息会点明平台、说明降级是否适用于
本次请求，并列出全部补救方式：`zuno --sandbox-on-unavailable run-unconfined`、
`zuno --sandbox danger-full-access`、`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`，或在
受信层（全局、受管、环境、CLI）设置 `sandbox.onUnavailable`；项目层无法启用它。在这类主机上
交互式启动 `zuno` 时，会在进入 raw mode 之前询问一次是否以原生方式运行本次会话——仅限具备
写能力的请求、仅当没有任何层设置过 `sandbox.onUnavailable`、且标准输入与标准错误都是终端；
回答 yes 时本进程的解析结果与命令行标志完全一致，回答 no 则以该拒绝信息退出。这个回答只对当前
进程生效：在 macOS 上，命令行标志会通过启动时的 re-exec 写入真实环境变量，从而被嵌套的 Zuno
进程继承，而提示里的回答不会；如果嵌套的 `zuno` 也需要同样的答案，请设置
`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined` 或在受信层设置 `sandbox.onUnavailable`。
`run`、`acp` 与 `serve` 永远不会询问，仍然需要标志或环境变量。以上任何一种都不是沙箱隔离。
切换到一个无法注册 Shell 的 Agent 时，会保留当前 Agent 并给出同样的提示，而不是结束会话。参见
[Shell sandbox roadmap](https://github.com/sunerpy/zuno/blob/main/docs/design/shell-sandbox-roadmap.md)。

## 为什么 `bwrap` 会以 `loopback: Failed RTM_NEWADDR: Operation not permitted` 失败？

`bwrap --unshare-net` 会在运行所请求的命令之前创建一个网络命名空间并初始化其 loopback
设备。`RTM_NEWADDR` 是添加地址的那个 netlink 操作。此处的 `EPERM` 意味着外层内核、LSM、
容器或虚拟化宿主策略拒绝了这个命名空间内的局部操作。它并不意味着 Zuno 应该静默地省略
网络命名空间。

### 当前 Ubuntu 宿主的诊断

在 2026-08-27，当前的 Ubuntu 24.04 EC2 宿主报告：

```text
/usr/bin/bwrap
bubblewrap 0.9.0
kernel.unprivileged_userns_clone = 1
user.max_user_namespaces = 252820
kernel.apparmor_restrict_unprivileged_userns = 1
```

该二进制文件归 root 所有，模式为 `0755`，既没有 setuid 也没有文件 capability。
随后以下两个独立探测失败：

```sh
# User, mount, and PID namespace setup
/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-pid --unshare-uts --unshare-ipc \
  --ro-bind / / \
  -- /usr/bin/true

# Network namespace and loopback setup
/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-net \
  --ro-bind / / \
  -- /usr/bin/true
```

在修复 AppArmor 之前，第一条返回 `setting up uid map: Permission denied`；第二条返回
`loopback: Failed RTM_NEWADDR: Operation not permitted`。user 命名空间在数值上是启用的，
但 Ubuntu 的 AppArmor 限制把本来未受限的进程切换进了通用的 `unprivileged_userns`
profile。该 profile 拒绝了 `bwrap` 构造沙箱所需的 capability。

在加载专用 profile 并把其归属修正为 `root:root` 之后，两个探测以及 Zuno 真实后端的 E2E
在该宿主上都通过。一个已经运行在另一层受限沙箱内的 Zuno 进程仍可能无法创建嵌套命名空间；
那个外层运行时必须允许完整探测。

外层 user 命名空间也可能把宿主上归 root 所有的 `/usr/bin/bwrap` 显示为 `uid=65534`
（`nobody`）。在那种执行上下文中 Zuno 正确地失败并拒绝，因为它无法从自身的权限视角证明
启动器归 root 所有。请直接在目标宿主的服务上下文中运行 `zuno debug sandbox ... --check`
来确认部署就绪状态；不要重新解释被映射的 UID，也不要弱化信任检查。

### 推荐的 Ubuntu 24.04 修复方式

Ubuntu 的 `apparmor-profiles` 包附带一个专为此场景准备的额外
`bwrap-userns-restrict` profile。该 profile 授予受信的 `/usr/bin/bwrap` 可执行文件所需的
构建权限，然后把它的子进程叠加进 `unpriv_bwrap`，在那里 capability 又被拒绝。

启用前先审阅该 profile，然后安装并加载它：

```sh
sudo apt-get update
sudo apt-get install apparmor-profiles

sudo install -o root -g root -m 0644 \
  /usr/share/apparmor/extra-profiles/bwrap-userns-restrict \
  /etc/apparmor.d/bwrap-userns-restrict

sudo /usr/sbin/apparmor_parser -r \
  /etc/apparmor.d/bwrap-userns-restrict
```

重新运行上面两个探测。如果仍有一个失败，先查看实际的拒绝记录，再去改动策略：

```sh
sudo journalctl -k --since '-10 minutes' \
  -g 'apparmor="DENIED"'
```

该 profile 只附着到 `/usr/bin/bwrap`。把二进制文件安装到别处的发行版需要一条单独审阅过的
路径规则；不要信任由工作区控制的 `PATH` 条目，也不要把任意可执行文件复制进受信位置。

以下做法都不能用作生产环境的修复手段：

- 全局设置 `kernel.apparmor_restrict_unprivileged_userns=0`；
- 给 `bwrap` 添加 setuid 或文件 capability；
- 以完全特权运行 Zuno 或其容器；
- 从后端要求中移除网络隔离、capability 丢弃、受保护路径规则或 seccomp。

启用 AppArmor profile 只解除了 AppArmor 这一处阻塞；其他所有探测要求依然适用。Zuno
继续强制 `PR_SET_NO_NEW_PRIVS`、capability 丢弃、seccomp 策略、只读的宿主根目录、精确的
可写根目录，以及受保护子路径的覆盖挂载。

### 容器、WSL 与其他 Linux 宿主

在 Docker、Podman、Kubernetes、dev container 或另一层受管沙箱内部，外层运行时可以独立
拒绝 `clone`、`unshare`、UID/GID 映射、route-netlink 操作，或嵌套的 mount/network 命名
空间。请修正那个运行时具体的 seccomp、AppArmor/SELinux、user namespace 和命名空间设置，
或者把 Zuno 运行在完整探测能够通过的专用虚拟机/裸金属环境中。不要用一刀切的
`--privileged` 代替一份经过审阅的策略。

对于刻意不使用沙箱的自动化容器，可以在受信的宿主级配置中设置：

```json
{"sandbox":{"onUnavailable":"run-unconfined"}}
```

这个选择对只读 Agent 无效，也不会掩盖 `debug sandbox --check` 的失败。项目配置不能启用
它，受管策略仍可强制使用 `deny`。如果容器应当始终原生运行，而不是先尝试沙箱，请直接使用
`"mode": "danger-full-access"`。

WSL1 不受支持。WSL2 是一台 Linux 虚拟机，只有在相同的 user、mount、PID、network、
文件系统和 seccomp 探测都通过时才可以使用 Linux 后端。

### 上游参考资料

- [Bubblewrap project and security model](https://github.com/containers/bubblewrap)
- [Ubuntu restricted unprivileged user namespaces](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)
- [Ubuntu AppArmor documentation](https://documentation.ubuntu.com/server/how-to/security/apparmor/)
- [AppArmor `bwrap-userns-restrict` profile](https://gitlab.com/apparmor/apparmor/-/blob/master/profiles/apparmor/profiles/extras/bwrap-userns-restrict)

## 为什么 Windows 上受 guard 保护的程序在运行前就以 125 退出？

Zuno 在 Windows 上监督的每个子进程——Shell 命令、LSP server、插件宿主或产品 Agent——都
运行在子进程 guard 之后，而 guard 必须盯住自己的父进程，才能保证它启动的进程树不会活过
拥有它的那个 Zuno 进程。工作区禁止 `unsafe` 代码，guard 自己打不开进程句柄，于是它从
`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe` 启动一个 Windows
PowerShell 助手，由助手持有父进程的真实句柄并在其上等待。句柄指向进程对象而不是 PID，
所以父进程退出后被复用的 PID 冒充不了活着的父进程，每次轮询也不再产生新进程。

助手在被监督的程序之前启动，因为一个看不住父进程的 guard 不能开始它无法收尾的工作。如果
助手无法启动，guard 会在运行任何东西之前以退出码 `125` 失败关闭，并向 stderr 写出一条
具名诊断：可执行文件缺失时是 `child-process guard failed: Windows PowerShell is required
to watch the guard's parent process; <path> does not exist`，存在但无法启动时是
`child-process guard failed: cannot start the parent-process watch helper: <error>`。
被监督的程序从未运行。路径依次从 `%SystemRoot%`、`%windir%`、字面的 `C:\Windows`
解析，从不查 `PATH`，因此工作区控制的 `PATH` 无法换入自己的助手；把 Windows PowerShell
恢复到该位置后重试即可。

助手已启动、随后却没有给出判定就结束，是另一种情况。guard 只记录一条诊断，让程序继续
运行，并只监督它自身的退出；未知的父进程状态绝不会杀死一条健康的命令。只有明确的判定——
助手报告父进程已退出，或在助手武装时父进程已经不在——才会终止被监督的进程树。

## 为什么一次 Kiro 提示词会以 `unsupported_content_block_projection` 失败？

2026-08-28 的 `kiro-provider` 构建接受连续的纯文本 Responses 块。它在自己的规范化请求中
保留这些块的边界，并且只在 Kiro 的标量文本边界处按字节原样拼接，不插入任何分隔符。
因此一个 Zed `resource_link` 加上普通文本不再需要 Zuno 侧的单文本投影。

使用常规的 Provider 选项：

```json
{
  "provider": {
    "kiro-local": {
      "transport": "openai",
      "surface": "responses",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "maxTokens": null,
        "timeout": false,
        "headerTimeout": 330000,
        "chunkTimeout": 210000,
        "reasoningReplay": "encrypted",
        "reasoningReplayMaxAge": 86400000
      }
    }
  }
}
```

`reasoningReplay: "encrypted"` 才会让网关封装推理、让 Zuno 重放它。没有它时每个请求依然成功，
但网关会记录 `reasoning_replay_locked: false`，多次工具调用的一轮会在步骤之间丢掉模型自己的推理。
`reasoningReplayMaxAge` 对应网关 24 小时的信封有效期。像这样的网关条目必须显式写出 `transport` 与 `surface`：端点来自 `baseURL` 的 provider
在没有声明 surface 时会解析成 Chat Completions，配置校验会拒绝这种组合。目录里的
`openai` provider 两个键都不需要。

升级时请移除过时的 `responsesTextBlocks: "single"` 设置。那个通用的 Zuno 兼容模式会用一个
空行连接文本，因此相比 Provider 当前的无损投影，它改变了字节内容。

这些超时值让 kiro-provider 的 300 秒请求截止时间和 180 秒流空闲截止时间先触发。于是
Zuno 收到一个带类型的网关错误，而不是在同一边界上取消连接。

当多个文本块与图片、文档、工具内容或其他非文本块交错，且其顺序无法由 Kiro 的单个文本
字段表达时，这个错误仍然是刻意的。Zuno 与 Provider 会失败并拒绝，而不是重排或压平提示词。
如果连续的纯文本块仍然失败，请核实正在运行的 Provider 二进制文件，而不要添加隐式的提示词
改写。
