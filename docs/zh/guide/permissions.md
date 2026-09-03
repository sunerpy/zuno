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

无 OS 约束执行有两种不同含义。请按部署意图选择：

| 意图 | 设置 | 实际行为 |
| --- | --- | --- |
| 必须有沙箱 | `workspace-write` 加 `onUnavailable: "deny"` | 默认行为。后端不可用时停止组装 Shell。 |
| 优先使用沙箱，仅在不可用时允许降级 | `workspace-write` 加 `onUnavailable: "run-unconfined"` | Zuno 先探测并验证受限后端，只在符合条件的类型化不可用错误下才降级。 |
| 始终使用宿主进程后端 | `danger-full-access` | Zuno 跳过受限后端发现，在所有受支持平台上直接原生执行。 |

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

OS 约束后端已在 Linux 上实现。在 macOS 与 Windows 上，受限模式报告：

```
OS sandbox is not implemented for platform `macos`
```

默认仍然失败即拒绝。受信的 `run-unconfined` 可以让具备写能力的
`workspace-write` Agent 原生继续；只读 Agent 仍会拒绝。`danger-full-access`
始终直接选择原生执行。

## 权限模式

通过 `permission.mode` 设置。

| 模式 | 行为 |
| --- | --- |
| `standard` | 应用配置的规则和常规风险门禁。默认值。 |
| `strict` | 对每一次有副作用的调用都要求一次新的决定。 |
| `allow_all` | 跳过询问，同时保留显式拒绝与沙箱校验。 |

注意 `allow_all` **不会**做什么。它不会关闭沙箱，也不会覆盖一条 `deny` 规则。显式拒绝在任何模式下都是终态的，包括这一种。

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

`edit` 这个键同时管 `write`、`edit` 和 `apply_patch` 三个工具，它们都在这个键下申请授权。不存在单独的 `write` 或 `apply_patch` 规则键，因此写成这两个名字的规则永远不会匹配到任何东西。

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

## Agent 契约只收窄，绝不放宽

无论配置要求什么，只读 Agent 都被钉在 `read-only`。这个方向按设计是单向的：Agent 契约只能削减权限，因此选择一个只读 Agent 是一项保证，而不是一个可被配置悄悄反转的默认值。这也意味着只读 Agent 永远不会使用 `run-unconfined`。

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## 参见

- [Agent](/zh/guide/agents) —— Agent 契约以及每个 Agent 被允许做什么
- [配置项参考](/zh/config/reference) —— 每一个 `sandbox` 与 `permission` 键
- [zuno debug](/zh/cli/debug) —— `debug sandbox` 与 `debug permissions`
- [常见问题](/zh/operate/faq) —— 沙箱启动失败
