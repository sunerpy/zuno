# Provider 与凭据

## 推荐的原生 provider

`myopenai` 只是一个普通的 provider id。在 `zuno.json` 中声明它的端点、原生传输方式、模型和默认模型：

```json
{
  "model": "myopenai/primary-model",
  "small_model": "myopenai/fast-model",
  "provider": {
    "myopenai": {
      "name": "My OpenAI gateway",
      "transport": "openai",
      "surface": "responses",
      "env": ["MYOPENAI_API_KEY"],
      "options": {
        "baseURL": "https://gateway.example.com/v1"
      },
      "headers": {"X-Tenant": "tenant-a"},
      "models": {
        "primary-model": {
          "name": "Primary model",
          "reasoning": true,
          "tool_call": true,
          "limit": {
            "context": 200000,
            "output": 32000
          }
        },
        "fast-model": {
          "name": "Fast model",
          "tool_call": true,
          "limit": {
            "context": 128000,
            "output": 16000
          }
        }
      }
    }
  }
}
```

已检入的起步文件是 [`examples/config/zuno.json`](https://github.com/sunerpy/zuno/blob/main/examples/config/zuno.json)。`transport` 指定一个原生 Rust 线协议实现；它不是 provider 类型、provider 身份、认证方法或具体端点。`surface` 选择 `responses`、`chat` 或 `messages`；一个走 OpenAI 线协议的网关通常应当声明 `"transport": "openai", "surface": "responses"`。某个模型可以在 `models.<id>.provider.surface` 下覆盖 provider 的默认值。只在网关行为与 OpenAI 不同时才使用 `openai-compatible`。两种传输方式都不加载 npm 包、不启动 Node，也不运行 AI SDK。

Provider 级的头是该 provider 下每个已配置模型的默认值。模型可以在 `provider.<id>.models.<model>.headers` 下添加或替换它们；同名时模型级的 `headers` 胜出。一个受信的运行时组件也可以附加请求本地的头，它们最后应用。这是网关路由、租户、特性和厂商版本类头的预期扩展点，因此这些值不需要硬编码在某个 Rust provider 里。

不要用可配置的头来复现另一个产品的 OAuth 身份或服务授权。`Authorization`、`Content-Type` 和 `Accept` 属于 provider 认证与传输分帧；原生的 OpenAI ChatGPT OAuth 还拥有它自己的账户与数据驻留头。请改为通过 provider 登录流程或声明的环境变量来保存 bearer 凭据。

## 首次运行的初始化

Zuno 没有基于 Node 的配置生成器。从已检入的起步文件初始化一份签出、编辑端点与模型 id，然后通过原生 CLI 保存凭据：

```sh
install -d -m 700 "${XDG_CONFIG_HOME:-$HOME/.config}/zuno"
install -m 600 examples/config/zuno.json "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
$EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/zuno/zuno.json"
printf '%s' "$MYOPENAI_API_KEY" | zuno providers login --provider myopenai
zuno debug config
zuno models myopenai --verbose
```

当 Zuno 是在没有源码签出的情况下安装时，请直接在配置根下创建同样的 `zuno.json`。交互式 API key 登录会关闭终端回显；管道登录从标准输入读取。无论哪种方式，密钥都不必出现在 shell 历史中。

## 登录方法

`zuno auth` 是 `zuno providers` 的别名。登录之前，先列出某个 provider 已实现的方法：

```sh
zuno auth methods openai
zuno auth methods myopenai
```

在终端中，不带参数的登录会打开一个可搜索的 provider 选择器。它包含官方 OpenAI 集成，以及那些解析出的模型路由确实有一个真实 API key 消费者的已配置 provider。仅存在于目录中的条目、历史遗留的凭据 id，以及像 Bedrock 这类使用环境凭据的传输方式，都不是登录选项：

```sh
zuno auth login
```

用方向键或输入文字过滤，然后按 Enter。如果所选 provider 有多种认证方法，Zuno 会为方法再打开一个选择器。Escape 或 Ctrl+C 可以取消任一选择器。非交互式调用仍然要求显式指定 provider，这样脚本就不会卡在一个询问上。

内置的 `openai` provider 支持三种方法：

```sh
# Select browser OAuth, device-code OAuth, or API key interactively.
zuno auth login openai

# ChatGPT Plus/Pro in the local browser.
zuno auth login openai --method chatgpt-browser

# ChatGPT Plus/Pro on a headless or remote host.
zuno auth login openai --method chatgpt-device

# OpenAI Platform API key.
printf '%s' "$OPENAI_API_KEY" | zuno auth login openai --method api-key
```

像 `myopenai` 这样的已配置 provider id，只有在它解析出的原生传输方式会消费该凭据时，才会获得 API key 方法：

```sh
printf '%s' "$MYOPENAI_API_KEY" | zuno auth login myopenai
```

登录之前请先配置好自定义 provider。像 `kiro-auth` 这样任意的、或仅有凭据的 id，会在 Zuno 读取标准输入或写入 `auth.json` 之前就被拒绝。

使用 `transport: "openai"` 不会赋予一个自定义 provider OpenAI 的 ChatGPT OAuth 流程。`openai` 这个 id 拥有那套登录、刷新协议、ChatGPT 端点重写和账户头。一个自定义 OAuth provider 需要它自己注册的登录方法和请求侧消费者；仅有一个 OAuth 形状的 JSON 对象不会被视为一次完整集成。

## 凭据存储

由 `zuno auth login` 创建的凭据按 provider id 存放在 `$XDG_DATA_HOME/zuno/auth.json`（通常是 `~/.local/share/zuno/auth.json`），Unix 上权限为 `0600`。对临时或受管环境，`ZUNO_AUTH_CONTENT` 可以用一个 JSON 对象取代凭据读取。

凭据优先级是：

1. `provider.<id>.options.apiKey`，包括显式的空字符串；
2. `auth.json` 中匹配的条目；
3. `provider.<id>.env` 声明的第一个非空变量；
4. 没有凭据。

把 `apiKey` 放进 `zuno.json` 是受支持的，但会把密钥暴露给配置备份与源码管理，所以更可取的是凭据存储或注入的 `ZUNO_AUTH_CONTENT`。

来自环境变量的 key 会被直接使用，不会复制进 `auth.json`。这就是为什么即使用户从未执行过 Zuno 的登录命令，某个 provider 也可能已处于已认证状态。`zuno auth list` 打印活跃的凭据种类、存储路径和匹配的环境变量名，不打印密钥值。一份已存储但当前没有可登录 provider 路由与之对应的凭据会被保留并标记为 `orphan`，以便用 `zuno auth logout` 移除。

ChatGPT OAuth 会把 access token、refresh token、过期时间和账户 id 存在同一个文件里。在发出请求之前，Zuno 会刷新接近过期的 token 并把轮换后的 token 落盘，除非凭据来自 `ZUNO_AUTH_CONTENT`。

## OpenAI API key 与 ChatGPT OAuth 的区别

这是两个彼此独立的认证产品：

- OpenAI Platform API key 作为 bearer 凭据发送到已配置的 OpenAI API 端点。
- ChatGPT OAuth 登录一个 ChatGPT 订阅，要求使用 Responses surface，把请求发往 ChatGPT Codex 后端，并在可用时带上所选的 ChatGPT 账户 id。

选择 `--method api-key` 绝不会触发 ChatGPT 登录。选择任一 ChatGPT 方法，也绝不会把得到的 access token 当作 Platform API key。

Codex 与 Claude Code 产品子 Agent 是一项独立能力。它们继承对应原生命令已有的登录，绝不出现在 `zuno auth login` 中；Zuno 不会把它们的凭据复制进 `auth.json`，也不会替它们选择模型。参见 [Codex 与 Claude Code 产品 Agent](https://github.com/sunerpy/zuno/blob/main/docs/design/product-agents.md)。

## `myopenai` 是如何被调用的

请求路径是原生 Rust：

1. `zuno-config` 解析并合并 `provider.myopenai`。
2. `zuno-llm` 解析模型目录，并构建一个类型化的 provider `Spec`。
3. `zuno-cli` 解析显式的 `surface`；自定义的 OpenAI base URL 由带 OpenAI 请求语义的原生 compatible provider 承载。
4. 该 provider 为那个 surface 构建 Responses 或 Chat Completions JSON，应用请求本地的推理控制，然后用 `reqwest` 发送请求。
5. `zuno-llm` 解析 SSE 分帧，provider crate 把数据块翻译成引擎消费的共享流事件。

`openai-compatible` 传输方式由 `zuno-provider-compatible` 单独实现，默认走 `/chat/completions`；规则驱动的 compatible provider 可以选择 `/responses`。`anthropic`、`bedrock` 和 Google 系传输方式使用各自独立的原生 crate，因为它们的请求与流协议与 OpenAI 不兼容。

对于前台的 Responses 请求，引擎会提供一个私有的类型化路由上下文，其中包含持久的根会话或子会话身份。官方 OpenAI 适配器以及用于自定义 OpenAI `baseURL` 的 compatible 适配器会把它投影为 `metadata.zuno_session_id`；工具续跑复用它，而标题、摘要、压缩、反思和 Council 调用是隔离的。该字段对 `extraBody` 与请求参数覆盖是保留的，并且绝不会被复制进模型输入、instructions、头或工具定义。Chat Completions 与 Messages surface 忽略这个路由上下文。

受支持的配置取值是 `openai`、`openai-compatible`、`openrouter`、`anthropic`、`bedrock`、`bedrock-mantle`、`google`、`google-vertex` 和 `google-vertex-anthropic`。Provider 配置没有 `npm` 字段。

重要选项包括：

| 键 | 含义 |
| --- | --- |
| provider `surface` | 具体的 API 形态：`responses`、`chat` 或 `messages` |
| provider `headers` | 该 provider 下每个模型的默认额外 HTTP 头 |
| model `headers` | 在 provider 头之后应用的逐模型添加与覆盖 |
| `baseURL` 或选项 `endpoint` | API base URL；两者都设置时选项 `endpoint` 胜出 |
| `apiKey` | 配置本地的凭据，优先于凭据存储 |
| `timeout` | 整个请求的超时（毫秒），或 `false` |
| `headerTimeout` | 响应头超时（毫秒），或 `false` |
| `chunkTimeout` | 流式数据块之间的最大间隔（毫秒） |
| `maxTokens`、`temperature`、`topP`、`toolChoice` | 由原生 provider 转发的生成控制项 |
| `responsesTextBlocks` | Responses 文本投影：默认 `multiple`，对只暴露一个上游文本字段的网关使用 `single` |
| `extraBody` | 在受保护字段组装完成之后追加的请求字段 |

`responsesTextBlocks: "single"` 是一项兼容性声明，不是从 provider id 推断出的模型能力。它让 Zuno 的持久提示词 part 保持类型化，但会在构建 compatible Responses 请求之前，用一个空行把它们的文本投影连接起来。内联图像仍然是独立的内容块。只有当目标端点拒绝一条消息中出现多个 `input_text` 块时才使用它；符合标准的端点应当保持默认的 `multiple` 行为。不要把它用于 2026-08-28 的 `kiro-provider` 构建：那个 provider 现在会把连续的全文本块逐字节拼接、不加分隔符，而这个选项会有意插入一个空行。

模型的 `options.reasoningEffort` 是当实时会话与所选 Agent 都没有选择级别时的默认值。在 Responses surface 上，`options.reasoningSummary` 会与它一起降级为 `reasoning: { effort, summary }`；Chat Completions 只收到 `reasoning_effort`，因为它没有推理摘要请求字段。对拒绝 `reasoning.summary` 的 compatible 端点，请省略 `reasoningSummary`；Zuno 不会基于 provider id 静默剥离一个被显式请求的控制项。

运行 `zuno models myopenai --verbose` 检查解析出的模型，运行 `zuno debug config` 在不打开凭据文件的情况下确认合并后的 provider 块。

## 网络代理

Zuno 对一个会话所使用的每个进程内出站 HTTP 客户端应用同一份进程级的环境代理契约：模型 provider、provider 登录与 OAuth、远端 MCP、模型与 skill 目录、远端指令文件、`webfetch` 和 `web_search`。

在启动 Zuno 之前设置这些标准变量：

```sh
export HTTP_PROXY=http://127.0.0.1:1080
export HTTPS_PROXY=http://127.0.0.1:1080
export ALL_PROXY=socks5h://127.0.0.1:1080
export NO_PROXY=127.0.0.1,localhost,::1,.internal.example
zuno
```

小写别名 `http_proxy`、`https_proxy`、`all_proxy` 和 `no_proxy` 也被接受。特定协议的变量优先于 `ALL_PROXY`；`NO_PROXY` 会让匹配的目标绕过所有已配置的代理。支持 HTTP、HTTPS CONNECT、SOCKS4、SOCKS5 以及带代理侧 DNS 的 SOCKS5 URL。改动这些变量之后请重启 Zuno，因为连接池在每个客户端构造时就捕获了代理策略。

`webfetch` 使用同一条路由，但不会把目标 DNS 的权威交给代理。对原始 URL 与每一跳
重定向，Zuno 都会在本地解析并校验全部目标地址，再通过选中的 HTTP、HTTPS、
SOCKS4 或 SOCKS5 路由连接一个已校验 IP，同时保留原始 Host header 与 TLS SNI。
配置的代理不可用时请求直接失败，不会静默重试直连；`NO_PROXY` 是环境层面对匹配
公开目标选择直连的唯一方式。

由 shell 工具、格式化器、语言服务器和本地 MCP server 启动的命令会继承 Zuno 的进程环境。它们各自显式的环境配置可以覆盖个别代理变量。

Amazon Bedrock 运行时请求与 AWS SSO 凭据请求使用同一套环境代理策略。这意味着一个只能通过网关访问的 region 不需要 Bedrock 专用的代理选项：

```sh
HTTPS_PROXY=http://127.0.0.1:1080 zuno
```

要做直连与走代理的对比，请把解析出的 Bedrock 主机名加入 `NO_PROXY`，例如 `bedrock-runtime.us-east-1.amazonaws.com`。IMDS 与 AWS 认可的本地 ECS 凭据端点始终直连，即使设置了 `HTTP_PROXY` 或 `ALL_PROXY` 也是如此；把那些元数据请求转发给一个环境中的代理可能会暴露临时 AWS 凭据。远端 HTTPS 的 `AWS_CONTAINER_CREDENTIALS_FULL_URI` 仍然感知代理，并且仍然遵循 `NO_PROXY`。

归属与扩展契约在 [Provider 认证](https://github.com/sunerpy/zuno/blob/main/docs/design/provider-authentication.md)中规定。
