# 认证与凭据

Zuno 把另一些工具混在一起的两件事分开了：一条模型路由属于哪个 provider，以及该 provider 的凭据来自哪里。`zuno.json` 中的一个 provider 条目是一份目录和一次传输方式选择。凭据是一个独立对象，默认存放在配置之外，在请求时解析。

[Provider 与凭据](/zh/config/providers)是 provider 传输方式、各 provider 的登录方法以及请求路径的权威文档。本页覆盖配置面与存储模型。

## 通往凭据的两条路径

API key 是通常情况。Zuno 把它存在某个 provider id 之下，并作为 bearer 凭据发送到已配置的端点。

OAuth 是 provider 特有的。内置的 `openai` provider 拥有 ChatGPT 登录、它的刷新协议、ChatGPT 端点重写和账户头。在一个自定义 provider 上设置 `transport: "openai"` 并不会赋予它那套流程。一个自定义 OAuth provider 需要它自己注册的登录方法以及一个请求侧的消费者；仅有一个 OAuth 形状的 JSON 对象不构成集成。

```sh
zuno auth methods openai
zuno auth login openai --method chatgpt-browser
zuno auth login openai --method chatgpt-device
printf '%s' "$OPENAI_API_KEY" | zuno auth login openai --method api-key
```

`zuno auth` 是 `zuno providers` 的别名。先列出方法是值得多敲这一条命令的：一个配置的 provider id 只有在它解析出的原生传输方式确实会消费该凭据时，才会获得 API-key 方法；而一个任意的或仅有凭据的 id 会在 Zuno 读取标准输入之前就被拒绝。

## 声明凭据来自哪里

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `provider.<id>.env` | `string[]` \| `null` | 无 | 提供该 provider 凭据的环境变量 |
| `provider.<id>.api` | `string` \| `null` | 无 | 该 provider 的基础 API URL |
| `provider.<id>.id` | `string` \| `null` | 映射的键 | 覆盖 provider id |
| `provider.<id>.name` | `string` \| `null` | 无 | 显示名 |
| `provider.<id>.transport` | enum \| `null` | 无 | 由 Zuno 实现的原生请求传输方式 |
| `provider.<id>.surface` | `chat` \| `responses` \| `messages` \| `null` | 无 | 该 provider 下各模型的默认请求 surface |
| `provider.<id>.options` | object \| `null` | 无 | provider 级选项，包括本 schema 未具名的 SDK 选项 |
| `provider.<id>.headers` | object of `string` \| `null` | 无 | 该 provider 下每个模型都附加的默认 HTTP 头 |
| `provider.<id>.models` | map of model \| `null` | 无 | 逐模型的配置与覆盖 |
| `provider.<id>.whitelist` | `string[]` \| `null` | 无 | 要保留的模型，其余全部排除 |
| `provider.<id>.blacklist` | `string[]` \| `null` | 无 | 要丢弃的模型 |

`env` 是一个列表，不是单个名称，第一个非空的变量胜出。这就是让同一个 provider 条目能在给同一份密钥取不同名字的多台机器上工作的原因：

```json
{
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY", "OPENAI_API_KEY"],
      "options": { "baseURL": "https://gateway.example.com/v1" }
    }
  }
}
```

## 优先级

凭据解析是有序的，没有隐藏的回退：

1. `provider.<id>.options.apiKey`，包括显式的空字符串；
2. `auth.json` 中匹配的条目；
3. `provider.<id>.env` 声明的第一个非空变量；
4. 没有凭据。

因此一个显式为空的 `apiKey` 会胜出，并产生「无凭据」的结果。这是刻意的 —— 它给了你一种方式来证明某个 provider 未经认证，而不是让它静默捡起一个环境里恰好存在的变量。

来自环境变量的 key 会被直接使用，绝不会复制进 `auth.json`。这就是为什么在一台没人跑过登录命令的新机器上，某个 provider 也可能已经处于已认证状态。

## 凭据存放在哪里

| 内容 | 路径 |
| --- | --- |
| 凭据存储 | `$XDG_DATA_HOME/zuno/auth.json`，通常是 `~/.local/share/zuno/auth.json` |
| Unix 上的权限 | `0600` |
| 覆盖方式 | `ZUNO_AUTH_CONTENT` 用一个 JSON 对象取代凭据读取 |

`ZUNO_AUTH_CONTENT` 是面向临时与受管环境的机制 —— 容器、CI，或者在启动时注入的密钥管理器。当凭据来自那个变量时，Zuno 不会把轮换后的 OAuth token 写回磁盘，因为它并不拥有任何文件。

这个变量不会交给 `shell` 工具，因此模型组装出来的命令读不到注入的凭据。被扣留的是整个
`ZUNO_*` 命名空间，而不只是这一个变量，这同时把同一处泄漏的另一条路径也堵上了：通过
`ZUNO_CONFIG_CONTENT` 内联提供的 `provider.<id>.options.apiKey` 同样是一份 provider
凭据，它也会被扣留。因此从这类命令内部启动的嵌套 `zuno` 两者都不会继承，而是按普通方式
解析配置与凭据，需要自己的凭据存储或配置。在只靠环境提供凭据的容器里要为这一点做好安排。
参见[工具](/zh/guide/tools#一条-shell-命令继承什么)。

把 `apiKey` 直接放进 `zuno.json` 是受支持的，但会把密钥暴露给配置备份与源码管理。优先使用凭据存储或注入的 `ZUNO_AUTH_CONTENT`。如果你确实要用 `options.apiKey`，请把它放在任何会被提交的层之外；改为从文件或环境读取值的做法见[变量与替换](/zh/config/variables)。

## 检查而不泄露

```sh
zuno auth list
```

它打印活跃的凭据种类、存储路径和匹配的环境变量名，不打印密钥值。一份已存储但当前没有可登录 provider 路由与之对应的凭据会被保留，并标记为 `orphan`，以便你用 `zuno auth logout` 移除它。

ChatGPT OAuth 会把 access token、refresh token、过期时间和账户 id 存在同一个文件里。在发出请求之前，Zuno 会刷新接近过期的 token 并把轮换后的 token 落盘，除非凭据来自 `ZUNO_AUTH_CONTENT`。

Codex 与 Claude Code 产品子 Agent 是独立的。它们继承对应原生命令已有的登录，绝不出现在 `zuno auth login` 中，它们的凭据也不会被复制进 `auth.json`。

## 哪些 provider 被启用

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `enabled_providers` | `string[]` \| `null` | 无 | 设置后，只启用其中列出的 provider |
| `disabled_providers` | `string[]` \| `null` | 无 | 即使凭据存在也要丢弃的 provider |

`disabled_providers` 回答的是「一个环境里恰好存在的变量正在认证一个我不想在这个项目里用的 provider」。即使凭据能解析成功，它也会丢弃该 provider。

## 参见

- [Provider 与凭据](/zh/config/providers)
- [模型路由](/zh/config/models)
- [变量与替换](/zh/config/variables)
- [诊断](/zh/operate/diagnostics)
