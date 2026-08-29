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
    sandbox ── no backend ──▶ session refuses to start
        │
        ▼
   command runs, confined
```

权限决策关乎意图：这次调用该不该发生。沙箱关乎能力：一旦发生，这个进程能触达什么。准入一次调用不会放宽沙箱，而一个宽松的沙箱也不会跳过权限门。

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
    "network": "deny"
  }
}
```

### 网络权限

`sandbox.network` 取 `deny` 或 `allow`。在受约束模式下默认为 `deny`，它会创建一个私有网络命名空间并拒绝网络系统调用 —— 不是一条能被执意而为的进程绕过的防火墙规则。

`danger-full-access` 继承宿主网络，并且**拒绝显式的 `deny`**，因为它无法强制执行。这个拒绝是刻意的：一份静默地没能提供其所声明的隔离的配置，比一份直接拒绝加载的配置更糟。

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

## 沙箱失败即拒绝

这是在把 Zuno 部署到任何地方之前值得先理解的部分。

`read-only` 与 `workspace-write` 都**要求一个已验证的 OS 约束后端**。当后端不可用时，Zuno 不会退回到以无约束方式运行你的命令 —— 它拒绝启动会话：

```
no trusted system bubblewrap executable was found
```

没有任何配置项能把这件事变成警告，受限模式也绝不会降级到无约束后端。如果你想要无约束执行，就必须指名请求，即使用 `danger-full-access`，那是一个显式的信任选择，而不是缺少某个软件包导致的静默后果。

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

这与上面是同一种失败即拒绝行为，不是特例：没有后端就没有受约束的会话。

## 权限模式

通过 `permission.mode` 设置。

| 模式 | 行为 |
| --- | --- |
| `standard` | 应用配置的规则和常规风险门禁。默认值。 |
| `strict` | 对每一次有副作用的调用都要求一次新的决定。 |
| `allow_all` | 跳过询问，同时保留显式拒绝与沙箱校验。 |

注意 `allow_all` **不会**做什么。它不会关闭沙箱，也不会覆盖一条 `deny` 规则。显式拒绝在任何模式下都是终态的，包括这一种。

## 逐工具规则

`permission.rules` 是有序的，并按你书写的顺序求值。一条规则要么是对整个工具的单一动作，要么是按模式匹配的多个动作。

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "read": "allow",
      "write": "ask",
      "shell": {
        "git push*": "deny",
        "git *": "allow",
        "rm -rf*": "deny",
        "*": "ask"
      }
    }
  }
}
```

顺序很关键，上面的例子依赖它：`git push*` 必须在 `git *` 之前，否则更宽的模式会先匹配，push 就会被允许。

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
| `standard` + `danger-full-access` | 仍然会询问你，但被批准的命令拥有完整的宿主权限。 |
| `allow_all` + 规则 `"shell": "deny"` | Shell 调用被拒绝。显式拒绝优先。 |
| 任意受限模式，且没有后端 | 会话不会启动。什么都不会运行。 |

## Agent 契约只收窄，绝不放宽

无论配置要求什么，只读 Agent 都被钉在 `read-only`。这个方向按设计是单向的：Agent 契约只能削减权限，因此选择一个只读 Agent 是一项保证，而不是一个可被配置悄悄反转的默认值。

```sh
# Cannot write, whatever sandbox.mode says.
zuno run --agent plan "audit the retry policy"
```

## 参见

- [Agent](/zh/guide/agents) —— Agent 契约以及每个 Agent 被允许做什么
- [配置项参考](/zh/config/reference) —— 每一个 `sandbox` 与 `permission` 键
- [zuno debug](/zh/cli/debug) —— `debug sandbox` 与 `debug permissions`
- [常见问题](/zh/operate/faq) —— 沙箱启动失败
