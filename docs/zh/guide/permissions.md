# 权限与沙箱

在任何触碰你机器的操作之前，Zuno 有两道互相独立的门。**沙箱**决定一条命令在物理上能做什么。**权限**决定这次调用是否被准入。两者不是二选一，任何一方都不能替代另一方。

## 两道门

```
model asks to run a command
        │
        ▼
  permissions ── deny ──▶ refused, nothing runs
        │
      allow / after asking you
        │
        ▼
  sandbox resolver
        ├── confined backend ready ──▶ command runs, confined
        ├── eligible unavailable error + trusted fallback
        │                              └──▶ warning, then native execution
        └── otherwise ───────────────▶ refused, nothing runs
```

权限决策关乎意图：这次调用该不该发生。沙箱关乎能力：一旦发生，这个进程能触达什么。
准入一次调用不会放宽沙箱，宽松沙箱也不会抹掉显式拒绝或硬安全检查。显式的
`danger-full-access` 还会选择生效的 `allow_all`，因此按设计跳过普通审批提示。

## 沙箱模式

通过配置中的 `sandbox.mode` 设置，或在命令行使用 `--sandbox`。

| 模式 | 文件系统 | 网络默认值 |
| --- | --- | --- |
| `read-only` | 读取宿主文件系统，不允许对宿主写入 | `deny` |
| `workspace-write` | 可写当前工作区以及显式受信的额外根目录 | `deny` |
| `danger-full-access` | 以 Zuno 用户身份运行，拥有宿主文件系统、进程与网络 | 宿主网络 |

`sandbox.mode` 缺省时默认为 `workspace-write`。只读的 Agent 契约仍会进一步收窄它 —— 参见 [Agent](/zh/guide/agents)。

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "deny"
  }
}
```

### 如何选择无沙箱执行

无 OS 约束执行有三种不同含义。请按部署意图选择：

| 意图 | 设置 | 实际行为 |
| --- | --- | --- |
| 必须有沙箱 | `workspace-write` 加 `onUnavailable: "deny"` | 默认行为。后端不可用时停止组装 Shell。 |
| 优先使用沙箱，仅在不可用时允许降级 | `workspace-write` 加 `onUnavailable: "run-unconfined"` | Zuno 先探测并验证受限后端，只在具备写能力的请求遇到符合条件的类型化不可用错误时才降级。 |
| 让每个 Agent 都原生运行，同时保留权限模式 | `backend: "native"` | Zuno 跳过受限后端发现，让每个 Agent 的 Shell（包括只读契约）原生运行；已配置的权限模式、规则、审批与风险门禁全部保留，请求的契约被记录为未强制执行。 |
| 始终使用宿主进程后端且不弹审批提示 | `danger-full-access` | Zuno 跳过受限后端发现，在所有受支持平台上直接原生执行，并把生效权限模式设为 `allow_all`。 |

`backend: "native"` 是为没有 OS 沙箱的主机（今天的 macOS 与 Windows）而准备的选择，用于
权限层必须继续生效的场景。它是一项受信的主机声明，而不是降级：没有任何探测，也没有任何
失败在先。在它之下，像 `plan` 这样的只读 Agent 仍保留工具白名单、权限规则与 Shell 风险
门禁，而“只读”此时的含义正是这一点——一道角色边界，而不是 OS 边界。可以在受信层设置，
也可以用 `zuno --sandbox-backend native` 或 `ZUNO_SANDBOX_BACKEND=native` 只作用于一次调用：

```json
{
  "sandbox": {
    "backend": "native"
  }
}
```

一次性显式使用无沙箱模式：

```sh
zuno run --sandbox danger-full-access "run the local build"
```

也可以在受信配置层中设置：

```json
{
  "sandbox": {
    "mode": "danger-full-access"
  }
}
```

如果容器或宿主应当尽量使用沙箱，但沙箱不可用时可以接受宿主执行：

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": "deny",
    "onUnavailable": "run-unconfined"
  }
}
```

对应的一次性参数和环境变量是：

```sh
zuno run \
  --sandbox workspace-write \
  --sandbox-on-unavailable run-unconfined \
  "run the local build"

ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined zuno run "run the local build"
```

只有受信的全局、显式配置、环境、CLI 或受管层可以启用 `run-unconfined`。
项目 `zuno.json[c]` 与 `.zuno` 配置只能设置 `deny`；被检入仓库的配置不能让自己
获得宿主执行权限。受管策略拥有最终决定权，仍可把它强制改回 `deny`。
如果要持久设置，请把 JSON 写入 `zuno debug paths` 所显示配置根目录下的全局
`zuno.json`，通常是 `$XDG_CONFIG_HOME/zuno/zuno.json` 或
`~/.config/zuno/zuno.json`。

仓库点名的程序同理：项目层设置 `shell`、本地 `mcp.*.command`、`lsp.*.command`、
`formatter.*.command` 以及 `productAgent.*.command` 都会被直接拒绝，把该条目用
`enabled: false` 或 `disabled: true` 关掉也没有区别，因为这个开关就在检出自己控制的
那一层里。只有受信层里的 `trust.project_host_commands` 能接纳那个检出。
详见[配置文件与优先级](/zh/config/files)。

### 网络权限

`sandbox.network` 取 `deny` 或 `allow`。在受约束模式下默认为 `deny`，它会创建一个私有网络命名空间并拒绝网络系统调用 —— 不是一条能被执意而为的进程绕过的防火墙规则。

`danger-full-access` 继承宿主网络，并且**拒绝显式的 `deny`**，因为它无法强制执行。这个拒绝是刻意的：一份静默地没能提供其所声明的隔离的配置，比一份直接拒绝加载的配置更糟。

不可用降级同样继承宿主网络。请求的 `deny` 仍会被记录，但命令原生运行期间，它不是
实际生效的网络边界。

### 受保护路径

`sandbox.protectedPaths` 会在授予可写根目录之后重新以只读方式施加，因此可以在一个原本可写的目录中挖出一条例外路径。

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "writableRoots": ["/srv/build-cache"],
    "protectedPaths": [".git", "secrets"]
  }
}
```

在不可用降级期间，可写根目录与受保护路径仍属于请求策略和诊断信息，但宿主进程后端
无法强制执行它们。

## 沙箱默认失败即拒绝

`read-only` 与 `workspace-write` 都**要求一个已验证的 OS 约束后端**。当后端不可用时，Zuno 不会退回到以无约束方式运行你的命令 —— 它拒绝启动会话：

```
no trusted system bubblewrap executable was found
```

默认的 `onUnavailable: "deny"` 会让后端不可用直接停止 Shell 组装。

受信的 `run-unconfined` 只改变具备写能力的 Agent 所请求的 `workspace-write`，并且
只接受符合条件的不可用错误。可降级原因包括平台不受支持、没有受信启动器、启动器缺少
所需能力，或命名空间/容器策略导致部署不可用。

以下情况绝不触发降级：

- 启动器存在但不受信；
- 沙箱配置或路径无效；
- seccomp、helper 或内部错误；
- 命令准备或执行错误；
- 任意只读 Agent 或 `read-only` 请求。

降级激活时，Zuno 会输出一次宿主警告，并记录请求模式、网络策略、实际宿主权限、
解析类型和类型化原因。它仍保留已配置的权限模式、显式权限拒绝、灾难性命令硬拒绝、
后台生命周期、超时、取消和至多一次执行；但无法保留请求的 OS 文件系统与网络限制。

显式的 `danger-full-access` 与此不同：它完全跳过沙箱探测，从一开始就使用原生后端，
并把生效权限模式设为 `allow_all`。显式拒绝与灾难性命令拒绝仍然是终态。

### Linux 后端需要什么

后端是 bubblewrap，而它需要的不只是二进制文件存在：

- **bubblewrap 0.8.0 或更新。** Zuno 需要 `--disable-userns` 与 `--assert-userns-disabled`，它们是 0.8.0 引入的。Ubuntu 22.04 自带 0.6.x，能正常安装，然后在选项检查处失败。
- **创建用户命名空间的权限。** 一个禁止非特权用户命名空间的容器根本无法承载该沙箱，探测会以 `No permissions to create new namespace` 失败。装一个更新的 bubblewrap 也没用；是内核在拒绝。

用这条命令同时验证两点：

```sh
zuno debug sandbox
```

### 其他平台

OS 约束后端已在 Linux 上实现。macOS 与 Windows 根本没有受约束后端，所以受限模式在这两个
平台上是失败即拒绝，而不是悄悄降级。这条拒绝信息是写给人照着做的：它会点明平台、说明受信的
`run-unconfined` 降级是否适用于**当前这次**请求、逐条列出补救方式以及可以设置它的配置层，
并且明确说明这些补救方式都不是沙箱隔离。它的开头仍是早先版本单独打印的那条类型化原因：

```
OS sandbox is not implemented for platform `macos`: macos has no confined sandbox
backend, so the Shell tool cannot be registered under the requested
`workspace-write` authority. …
```

- 具备写能力的 `workspace-write` 请求符合降级条件，因此拒绝信息会给出
  `--sandbox-on-unavailable run-unconfined`、`ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`，
  以及在受信层（全局、受管、环境、CLI）里设置
  `"sandbox": {"onUnavailable": "run-unconfined"}` —— 项目层无法启用它。
- 只读请求永远不会降级，拒绝信息会直接这么说，而不是列出一条其实不适用的补救方式。它的
  补救方式是显式选择原生后端：`--sandbox-backend native`、`ZUNO_SANDBOX_BACKEND=native`，
  或在受信层设置 `"sandbox": {"backend": "native"}`（项目层无法选择它），让这个 Agent 的
  Shell 原生运行，同时保留你的权限模式。此时请求的 `read-only` 权限会被记录但不由 OS 强制
  执行：留下的是 Agent 的工具契约、你的权限规则与 Shell 风险门禁——这是一道角色边界，
  不是 OS 边界。
- 具备写能力的请求的拒绝信息会在降级方式旁边一并列出原生后端，并说明 `danger-full-access`
  还会额外把生效权限模式设为 `allow_all`。

在这类主机上交互式启动 `zuno` 时，会在终端进入 raw mode 之前询问一次，是否以原生方式运行
本次会话。只要请求在这台主机上无法被约束就会询问——只读 Agent 的请求也包括在内——但前提是
没有任何配置层设置过 `sandbox.onUnavailable` 或 `sandbox.backend`，并且标准输入与
标准错误都是终端。回答 yes 时，本进程的解析结果与传入 `--sandbox-backend native` 完全一致（`resolutionKind`
为 `trusted_native`），并对该进程之后的每一次组合都生效，包括之后切换到只读 Agent；回答 no
则以上面那条拒绝信息退出。`run`、`acp`、`serve` 以及任何没有终端的启动都不会询问，仍然需要
命令行标志、环境变量或受信配置层。

这个回答只属于当前这个进程。在 macOS 上，命令行标志会由启动时那一次 re-exec 写入真实环境
变量，因此工具启动的嵌套 `zuno` 会继承它；而在提示里输入的回答发生在那次 re-exec 之后，
不会被继承。如果嵌套的 Zuno 进程也需要同样的答案，请设置环境变量或受信配置层。参见
[沙箱模式与后端不可用策略](/zh/config/reference#沙箱模式与后端不可用策略)。

切换到一个无法注册 Shell 的 Agent 时，Zuno 会保留当前 Agent 并在 transcript 上给出同样的
拒绝信息，而不是因为一次可以撤销的切换就结束会话。

`danger-full-access` 始终直接选择原生执行。它和降级都不是沙箱隔离：命令以 Zuno 进程用户的
宿主权限运行，但已配置的权限模式、显式拒绝与灾难性命令拒绝依然生效。

## 权限模式

通过 `permission.mode` 设置。

| 模式 | 行为 |
| --- | --- |
| `standard` | 应用配置的规则和常规风险门禁。默认值。 |
| `strict` | 对每一次有副作用的调用都要求一次新的决定。 |
| `allow_all` | 跳过询问，同时保留显式拒绝与沙箱校验。 |

注意 `allow_all` **不会**做什么。它不会关闭沙箱，也不会覆盖一条 `deny` 规则。显式拒绝在任何模式下都是终态的，包括这一种。

## 一条已保存的 “always” 只属于一个 session

用 **always** 回答一次询问，保存的是你回答的那个 session 的决定，而不是整个进程的决定。同样的调用在另一个 session 里 —— 包括之后才创建的 session，以及另一个客户端在驱动的 session —— 仍会再问一次。一次没有可保存模式的一次性确认既不会装上任何授权，也永远不会被先前的 `always` 满足。

一条已保存的 `always` 活在运行中的进程里，而不在数据库里，并且随 session 结束而结束。通过 HTTP 时，用 `POST /api/session/prune` 归档或删除一个 session 会撤销该 session 授予的每一条授权，重启 `zuno serve` 会清空全部；断开再重连事件流不会丢失它们，因为一条流不等于那个 session。想让一个决定活得比一个 session 更久，应该写进 `permission.rules`。参见[会话保留](/zh/operate/session-retention#归档会终止该-session-的常驻-http-授权)。

通过 HTTP 时，常驻 `always` 预先批准的每一次调用都会被记录为一条独立的、已结算的请求行，响应为 `{"reply":"once","source":"standing"}`；授权本身从不落盘。如果一条回复到达时它的提问方已经消失 —— 回合被中断，或发起调用的进程已重启 —— 这条回复依然会被记录，答案会通过持久 inbox 进入会话，但不会安装任何常驻授权：只有真正收到该回复的调用才会让授权被保存。

## 逐工具规则

`permission.rules` 是有序的，**最后一条匹配的规则胜出**。一条规则要么是对整个工具的单一动作，要么是按模式匹配的多个动作。

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "edit": "ask",
      "shell": {
        "*": "ask",
        "git *": "allow",
        "git push*": "deny",
        "rm -rf*": "deny"
      }
    }
  }
}
```

顺序很关键，上面的例子依赖它。因为后面的规则会覆盖前面的规则，catch-all `*` 要写在**最前面**，而从它当中划出例外的窄模式要写在**最后面**：`git *` 覆盖 catch-all，`git push*` 再覆盖 `git *`，于是 push 被拒绝。把顺序倒过来不只是风格差异，它会让保护失效：写在最后的 `*` 会覆盖它上面的每一条规则，`rm -rf /` 会重新变成一次询问。

既然顺序本身就是策略，Zuno 在此前会丢掉顺序的两个环节上都保住了你写下的顺序。Markdown Agent 的 `permission.rules` 会按 frontmatter 中的顺序到达评估器；把一个配置层合并到另一层之上时，基础层的规则顺序会被保留，而不是重新排序。这两个环节此前都会把键按字母排序，而把上面那个例子排一遍序就足以让它失效：`$HOME/.ssh/*` 排在 `*` 之前，所以排序后的 `{"*": "allow", "$HOME/.ssh/*": "deny"}` 会把 deny 放到 catch-all 之上，于是 catch-all 反而胜出。

合并规则值得了解，因为你可以在 `zuno debug permissions` 的输出里看到它：两层都设置的键在基础层给它的位置上被替换，只有覆盖层设置的键则追加在基础层各键之后。因此覆盖层的模式会压过基础层的 catch-all：项目层或 Agent 层可以从一条宽规则里划出例外，而不必把那条宽规则重写一遍。

`edit` 这个键同时管 `write`、`edit` 和 `apply_patch` 三个工具，它们都在这个键下申请授权。不存在单独的 `write` 或 `apply_patch` 规则键，而且 `permission.rules` 会直接拒绝它们：写在 `write`、`apply_patch`、`list_mcp_resources`、`list_mcp_resource_templates` 或 `read_mcp_resource` 下的规则会导致配置校验失败，并指出应当改用哪个键——前两个用 `edit`，三个 MCP 资源工具用 `read`。这五个键此前会被接受，却什么都不评估。其他键仍然合法，因为 MCP、插件与 Skill 工具的名字在运行时才确定，键本身也可以是通配模式。

顶层 `tools` 开关按工具名索引，同一套折叠也适用于它，因此同一个配置层内的两个 `tools` 条目可能落到同一条合成规则上，此时它们必须一致。`{"tools": {"edit": false, "write": true}}` 会校验失败，错误信息会同时点名两种拼法和起管辖作用的那个键：

```text
tools "edit" is false and tools "write" is true, but both are governed by permission "edit"; one rule cannot be both, so set them alike or write the rule under permission.rules.edit
```

把两个条目设成相同的值仍然可以加载。**这是一处不兼容变更**：这样自相矛盾的 `tools` 块此前是可以加载的，写在后面的那个条目会静默胜出，于是一个读起来像是禁用的块，实际上可能正在放开那个工具。请在 `permission.rules.<key>` 下把意图写一次，用错误信息点名的那个键。

分处两层的分歧则是另一回事：这属于覆盖，而不是矛盾，解析方式和其他任何配置键一样——在点名了该 permission 键的各层中，优先级最高的那一层胜出，所以项目层的 `edit: false` 会压过全局的 `write: true`。只有同一层内部的分歧没有顺序可以援引，因此只有它会被拒绝。

路径规则会同时按调用给出的原样路径和它的规范化拼写来匹配：分隔符被统一，`.` 段与重复分隔符被去掉，因此 `./src/main.rs`、`src//main.rs` 以及反斜杠写法 `src\main.rs` 都能匹配一条写作 `src/main.rs` 的规则。`deny` 刻意伸得更远：它还覆盖 `..` 解析后的路径，而写成绝对路径的 deny 也覆盖该路径的相对尾段，所以 deny 无法靠改写路径拼法绕过。`allow` 在这两个方向上都不会被放宽，因为放宽一条 allow 就等于授权了规则没有点名的文件。

按这种方式处理的键是 `read`、`edit`、`write`、`list` 与 `lsp`。`lsp` 也在其中，是因为它的资源同样是一个文件：语言服务器工具会把该文件命名为相对于包含本次会话的工作区的路径，而当会话目录与工作区不构成嵌套关系时，则回退为解析后的绝对路径。因此写作 `{"lsp": {"secrets.rs": "deny"}}` 的 deny 在两种布局下都能覆盖这个文件。

写 `allow` 时要照这个不对称来规划。`read`、`edit`、`write`、`apply_patch` 的文档约定接收绝对路径，所以 `"read": {"src/main.rs": "allow"}` 覆盖不到调用实际传入的绝对路径，而同样的模式写成 `deny` 却能覆盖。请用 `~`、`$HOME` 或绝对前缀来写 allow，或者用 `*`（它可以跨分隔符匹配），例如 `{"*/src/*": "allow"}`。

打印解析后的策略 —— 即在配置和任何 Agent 契约都已应用之后，生效的模式与每一条规则：

```sh
zuno debug permissions
```

它的输出还会说明一个宽松模式仍然强制执行了什么，这是确认上述保证、而不是凭信任接受它们的最快方式：

```json
{
  "configuredMode": "allow_all",
  "mode": "allow_all",
  "allowAllStillEnforces": [
    "explicit deny",
    "catastrophic shell denial",
    "sandbox authority",
    "argument validation"
  ]
}
```

## 一次路径授权保证了什么

对某个路径批准 `write`、`edit`、`apply_patch` 或 `read`，授权的是一个文件系统对象，
而不是一个字符串。解析从授权边界开始 —— 工作区根目录，或者你授予的外部目录 —— 逐段
向下，对每一级目录持有一个打开的句柄，并拒绝遇到的每一个符号链接。随后的操作通过
保留下来的句柄执行，而不是从文件系统根重新按名字寻址。

由此得到的保证是精确的：调用要么到达你批准的那个目录对象，要么失败。在你批准之后把
某一级祖先目录换成符号链接无法改变字节的落点，因为那个名字不会被解析第二次。把被授权
的目录改名，仍然只能到达你批准的那个对象。把它删掉会得到一个失败，因为已删除的目录
不接受新条目。

最后一段是符号链接则是另一回事：它会被有意地、恰好跟随一次，且发生在询问你之前。因此
你授权的是链接指向的那个文件，而不是链接本身。留在工作区内部的链接只需要普通的 `edit`
提示。指向工作区外部的链接需要一次 `external_directory` 授权，命名的是目标所在的目录，
而不是链接所在的目录。链接本身在写入之后始终保留。

`external_directory` 授权在所有地方只有一种拼法：目录加上 `/*`，使用正斜杠，去掉
Windows 的逐字 `\\?\` 前缀 —— 是 `C:/build-cache/*`，绝不是 `\\?\C:\build-cache\*`。
因此一次常驻授权同时覆盖 shell 工具与文件、搜索工具；在此之前它们各自使用的拼法互相
命中不了。

在 Windows 上，此前的保护是缺失而不只是更弱，因此升级会改变风险门禁在那里拒绝什么。
门禁此前只读 `HOME`，而 `cmd` 与 PowerShell 都不设置它，于是所有 home、用户配置与凭据
规则都自行关闭了：`rm -rf ~/.ssh` 与 `rm -rf $HOME` 只是一次确认提示而不是永久拒绝，在
`allow_all` 下则直接执行。现在 home 目录会回退到平台自己的答案，`HOME` 在被设置时仍然
优先，并且 `%USERPROFILE%` 与 `$env:USERPROFILE` 会被展开。逐字 `\\?\`、设备 `\\.\` 根
拼法以及 UNC 共享根也一并匹配，且不区分盘符与大小写。

针对系统位置的硬拒绝现在对 PowerShell 与 Bash 下绝对拼写的目标一样生效。转义按 shell 自身的语法读取，因此 `C:\Users\you\.ssh` 在进入风险表的路上不再被
削成 `C:Usersyou.ssh`，而 `C:\Windows\System32\rm.exe` 这类绝对程序路径也会进入它本该
命中的破坏性命令表。

## 两者如何相互作用

有几种组合值得说清楚，因为猜错它们会有真实后果。

| 配置 | 实际会发生什么 |
| --- | --- |
| `allow_all` + `read-only` | 不再询问，但写入仍然失败。沙箱不受权限模式影响。 |
| `standard` + `danger-full-access` | 生效权限变为 `allow_all`；跳过普通提示，但显式拒绝与灾难性硬拒绝仍然有效。 |
| `allow_all` + 规则 `"shell": "deny"` | Shell 调用被拒绝。显式拒绝优先。 |
| `workspace-write` + 默认 `deny`，且没有后端 | Shell 不会被组装。什么都不会运行。 |
| `workspace-write` + 受信的 `run-unconfined`，且发生可降级不可用错误 | 命令使用宿主权限；已配置权限模式和硬拒绝仍然保留。 |
| `read-only` + `run-unconfined`，且没有后端 | Shell 不会被组装。只读执行绝不降级。 |
| `read-only` + 受信的 `backend: "native"` | Shell 原生运行，权限模式保持不变。只读契约是工具、权限与风险门禁边界，不是 OS 边界；记录中写明 `trusted_native` 与 `requestedMode: read-only`。 |

## Agent 契约只收窄，绝不放宽

无论配置要求什么，只读 Agent 都被钉在 `read-only`。这个方向按设计是单向的：Agent 契约只能削减权限，因此选择一个只读 Agent 是一项保证，而不是一个可被配置悄悄反转的默认值。这也意味着只读 Agent 永远不会使用 `run-unconfined`。它的 Shell 唯一的原生运行方式是受信的 `sandbox.backend: native` 选择：那是一项显式的主机声明而非降级，契约作为工具与权限边界继续生效，只是不再有 OS 边界。

Agent 契约默认拒绝，因此契约没有点名的工具是被**隐藏**，而不只是未获授权：对一个未被点名的工具 id 来说，契约开头那条 `"*": "deny"` 就是最后一条匹配规则，模型根本不会被提供这个工具。默认授予里有两条正是由此而来。凡是授予 `shell` 的地方都会一并授予 `bg`，只读角色也不例外，因为后台执行由 `shell` 启动、只能通过 `bg` 读回——大到无法完整返回的结果也是如此。`job` 只授予可以委派的 Agent，因为一个 Job 只对创建它的那次 `task` 所属的会话才能解析出来。

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## 参见

- [Agent](/zh/guide/agents) —— Agent 契约以及每个 Agent 被允许做什么
- [配置项参考](/zh/config/reference) —— 每一个 `sandbox` 与 `permission` 键
- [zuno debug](/zh/cli/debug) —— `debug sandbox` 与 `debug permissions`
- [常见问题](/zh/operate/faq) —— 沙箱启动失败
