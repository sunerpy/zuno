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

URL 登录（`zuno auth login https://gateway.example.com`）会运行由该主机选择的命令并先请求确认；提示与 `--trust-remote-command` 见 `providers login` 参考。

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

由 `zuno auth login` 创建的凭据按 provider id 存放在 `$XDG_DATA_HOME/zuno/auth.json`（通常是 `~/.local/share/zuno/auth.json`），Unix 上权限为 `0600`。对临时或受管环境，`ZUNO_AUTH_CONTENT` 可以用一个 JSON 对象取代凭据读取。被中断的写入清空后的凭据文件仍然可读，并会报告损坏，下一次写入会替换它；Zuno 无法解码的条目会被保留，而不会被一次无关的登录删除。

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
| `toolCalls` | 仅 Bedrock：对该 provider 条目服务的所有模型给出 `true` 或 `false`，覆盖内置的 Converse 工具调用表 |
| `modelCapabilities` | 仅 Bedrock：`<model id>.tool_calls` 只决定这一个模型 id，不影响其他模型 |
| `responsesTextBlocks` | Responses 文本投影：默认 `multiple`，对只暴露一个上游文本字段的网关使用 `single` |
| `reasoningReplay` | 默认 `off`；对会封装推理的 Responses 端点使用 `encrypted` |
| `reasoningReplayMaxAge` | Zuno 仍会重放的最旧封装推理信封年龄（毫秒） |
| `extraBody` | 在受保护字段组装完成之后追加的请求字段 |

`responsesTextBlocks: "single"` 是一项兼容性声明，不是从 provider id 推断出的模型能力。它让 Zuno 的持久提示词 part 保持类型化，但会在构建 compatible Responses 请求之前，用一个空行把它们的文本投影连接起来。内联图像仍然是独立的内容块。只有当目标端点拒绝一条消息中出现多个 `input_text` 块时才使用它；符合标准的端点应当保持默认的 `multiple` 行为。不要把它用于 2026-08-28 的 `kiro-provider` 构建：那个 provider 现在会把连续的全文本块逐字节拼接、不加分隔符，而这个选项会有意插入一个空行。

### 工具声明与 `toolChoice`

一轮可以调用哪些工具，由 turn loop 锁定，而不是由 provider 选项决定；每个原生
provider 都会发送这份逐轮工具集。`options.tools` 是给那些由 *端点* 代你执行的工具用的
——例如 Anthropic 的 `web_search_20250305` 以及 Vertex Anthropic 上的对应项——这些是
turn loop 无法表达的。

当一轮带有锁定的工具集时，配置里的条目只有同时满足两点才会保留：它是这类由端点执行的
工具（判据是它不带 `input_schema`），并且它的名字没有被锁定的工具占用——因为两个同名条目
本身就是一个永久 400。配置里的 *自定义* 声明会被取代：工具派发是按锁定集校验的，因此第二份
不一致的同名声明只会招来 Zuno 一定会拒绝的调用。当一轮不带锁定集时——标题、摘要、压缩请求
——`options.tools` 会原样发送。

`toolChoice` 只在请求体确实带着它所指名的那个工具时才发送。Anthropic 与 Vertex Anthropic
surface 上的 `{"type": "tool", "name": "…"}`，以及 Gemini 上限定单个函数的写法，在被指名的
工具不在请求里时都是永久 400，因此不可满足的选择会被丢弃，而不是变成一轮失败。此时模型自行
决定——在 Gemini 上就是不发送 `toolConfig` 时的 `AUTO` 模式——并且永远不会被推向一个与你要求
的不同的工具。

### Bedrock 的逐模型工具声明

Bedrock 是一个端点后面接着所有厂商的模型，而 Converse 对不支持工具调用的家族返回
`toolConfig` 时会给出 `ValidationException`——这是永久失败，不可重试。因此 Zuno 只对
Bedrock 明确记载 Converse 支持工具调用的家族发送工具声明：`anthropic.claude`（除
`anthropic.claude-v2` 与 `anthropic.claude-instant`）、`amazon.nova-micro`、
`amazon.nova-lite`、`amazon.nova-pro`、`amazon.nova-premier`、`cohere.command-r`、
`meta.llama3-1`、`meta.llama3-2`、`meta.llama3-3`、`meta.llama4`、
`mistral.mistral-large`、`mistral.mistral-small`、`mistral.pixtral-large` 与
`ai21.jamba-1-5`。匹配方式是对小写模型 id 做子串匹配，因此跨区域前缀
（`us.anthropic.claude-…`）以及资源名里带有模型 id 的 inference profile ARN，都会与裸 id
解析成同一结果。

其他情况——预置吞吐量 ARN、比本次构建更新的模型——会在不带任何工具声明的情况下发送，这与
既有版本对所有 Bedrock 模型的行为一致。有两个选项可以覆盖它：

- `toolCalls: true` 让该 provider 条目服务的所有模型都带上工具声明。`toolCalls: false`
  则一个都不带，并且同时撤回该 provider 的工具调用能力，于是 turn loop 不再去组装一份
  链路无法承载的工具集。
- `modelCapabilities: {"<model id>": {"tool_calls": true}}` 只决定这一个模型 id，其余模型
  仍由 `toolCalls` 或上面的表决定。键是配置里写的完整模型 id，拼写与 OpenAI-compatible
  provider 已经读取的那一份完全相同。

`bedrock-mantle` 发送的是 OpenAI Chat 或 Responses 请求体而不是 Converse 请求体，因此
Converse 的那张表并不描述它：除非 `toolCalls: false` 或逐模型条目另有说明，这两个 surface
会照常携带工具声明。

### 加密推理重放

`reasoningReplay: "encrypted"` 声明该端点会封装推理。此后 Zuno 会为该 provider 的每个 Responses 请求加上 `include: ["reasoning.encrypted_content"]`，把每个封装推理项持久化为独立的 part，并按模型产出时的位置逐字节重放这些项：每一项都紧挨在它所解释的输出之前。这是端点能力，不是 provider 身份；官方 Responses API 与本地回环网关用同一种方式声明它。

这个选项是否生效取决于请求最终落在哪个 surface 上。目录里的 `openai` provider 什么都不用声明：它的 transport 来自目录，surface 保持 OpenAI adapter 的默认值，也就是 Responses。而端点来自 `options.baseURL` 或 `options.endpoint` 的网关必须同时声明 `"transport": "openai"` 与 `"surface": "responses"`，因为只要 provider 选项里带了端点、又没有声明 surface，就会解析成 Chat Completions；`openai-compatible` 则按 provider id 规则解析 surface，而不是按你声明的值。这两种情况下会话都会请求封装推理却永远拿不到，链路上也没有任何提示。

配置校验只拒绝它能证明不可能承载封装项的路由，并指出出错键路径：transport 不是 `openai`、surface 不是 `responses`，或者带自定义端点却在任何层级都没有声明 surface。对本来就会解析成 Responses 的 provider，它不会要求多余的声明。`models.<id>.provider.surface`、`.transport` 与 `models.<id>.options.reasoningReplay` 会按模型逐个校验，因为决定请求走向的是模型自己的路由。

封装信封绑定到铸造它的模型，并会在上游过期。Zuno 只会把信封重放给同一个目录 provider 与同一个模型，且只在它比 `reasoningReplayMaxAge` 更新时重放。其他情况下信封会离开 provider 请求，而持久行保留原密文，因此会话中途换模型只会降级质量，不会让这一轮失败。请把年龄上限设为端点自身的有效期，例如 24 小时写成 `86400000`；省略它则重放所有已存信封。标题、摘要、压缩以及其他辅助请求运行在不同的模型上，永远不会携带封装信封。

重放还必须与端点的指纹一致。工具调用会用 provider 自己的 `arguments` 字节重放，而不是把解析后的值重新序列化，因为键顺序与空格也是被签名内容的一部分。如果某个步骤的封装项后面没有任何输出（例如步骤被打断，或整份输出预算都花在推理上），这一项会被扣留而不是单独发出，因为 Responses 端点会拒绝这种形状；它计入被扣留数，而不算作一次重放。项 id 不会回送：重放项只带 `type`、`summary`、`encrypted_content` 与 `status`。

默认值 `off` 表示请求既不带 `include`，也不带任何封装项，包括同一会话在选项为 `encrypted` 时存下的信封。它并不承诺请求字节与既有版本一致：本次发布还会按模型流出的顺序发送每个 assistant 轮次的 Responses `input`，因此先写文本再调用工具的一轮，现在会先发文本项再发 function call —— 这对所有 Responses provider 生效，与 `reasoningReplay` 的取值无关。这个顺序正是封装端点会校验的内容；一次性代价是仅追加的提示词缓存前缀会失效一次。

没有 `reasoningReplay: "encrypted"` 的 `reasoningReplayMaxAge` 同样会在配置期被拒绝。不要给会封装推理的端点添加 `reasoningSummary`，它会拒绝 `reasoning.summary`。

信封是不透明的 provider 密文，Zuno 把它当作会话内容而不是可以脱敏的机密：它存放在推理 part 的 `metadata.providerReasoning` 中，会由 HTTP messages 端点返回，还会随一个完整携带密文的流事件转发：SSE 客户端看到的类型是 `provider.reasoning.item`，`zuno run --json` 打印的是 `provider_reasoning_item`，两者的密文都在 `encryptedContent` 字段里。Zuno 绝不会把它发给除封装者以外的模型，`session.provider.request` 事件只记录计数。能读取某个会话的消息或事件流，就等于能读取它的信封。

每个前台请求都会在自己的 `session.provider.request` 事件上记录 `reasoningReplay`、`replayedReasoningCapsules` 与 `withheldReasoningCapsules`，这就是确认重放确实发生的依据。重放计数就是真正上链路的数量；历史提供了、但这次请求没有发出的信封都计入被扣留数，无论原因是别的模型铸造、已经过期，还是它后面没有任何输出。

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
配置的代理不可用时请求直接失败，不会静默重试直连。

有两个环境变量会让匹配的公开目标走直连：

- `NO_PROXY`（或 `no_proxy`）让它匹配到的目标绕过已配置的代理。
- `REQUEST_METHOD` 会整体停用代理环境。CGI 风格的宿主会把请求中传入的 `Proxy:`
  header 映射成 `HTTP_PROXY`（httpoxy），因此只要存在 `REQUEST_METHOD`，Zuno 就会
  在整个进程内忽略 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY`，所有
  公开请求一律直连。如果你因为其他原因导出了 `REQUEST_METHOD` 但仍希望走代理，
  请取消该变量。

有些建连失败是对方给出的答复而不是网络波动，`webfetch` 只失败一次、不再按退避重试：
代理 CONNECT 返回 401、403、404、407、501，以及除 408、429 和其余 5xx 之外的任何
状态；SOCKS5 除 0x01、0x03、0x04、0x05、0x06 之外的回复；SOCKS4 除 0x5b 之外的
回复；以及任何证书或协议层面的 TLS 拒绝，包括 443 端口上以明文应答的强制门户。
其中 403、404 与 SOCKS5 的 0x02、0x08 只针对一个目标地址，因此剩余的已校验地址
仍会被尝试；但如果它们都没能连上，这条拒绝仍然决定整个请求的结果。

建连有两重上限：单个已校验地址在 TCP、代理协商与 TLS 握手上最多用 10 秒；每一跳
（原始 URL，以及随后的每一次重定向）的整轮地址遍历最多 30 秒、最多 8 个已校验地址，
因此再大的 DNS 应答也无法延长它。等待响应 header 不计入该预算，由调用方自己的请求
超时负责。Zuno 其他 HTTP 客户端都带有 30 秒的默认 connect 超时，个别 provider 可以
把它调得更短。

由 shell 工具、格式化器、语言服务器和本地 MCP server 启动的命令会继承 Zuno 的进程环境。它们各自显式的环境配置可以覆盖个别代理变量。

Amazon Bedrock 运行时请求与 AWS SSO 凭据请求使用同一套环境代理策略。这意味着一个只能通过网关访问的 region 不需要 Bedrock 专用的代理选项：

```sh
HTTPS_PROXY=http://127.0.0.1:1080 zuno
```

要做直连与走代理的对比，请把解析出的 Bedrock 主机名加入 `NO_PROXY`，例如 `bedrock-runtime.us-east-1.amazonaws.com`。IMDS 与 AWS 认可的本地 ECS 凭据端点始终直连，即使设置了 `HTTP_PROXY` 或 `ALL_PROXY` 也是如此；把那些元数据请求转发给一个环境中的代理可能会暴露临时 AWS 凭据。远端 HTTPS 的 `AWS_CONTAINER_CREDENTIALS_FULL_URI` 仍然感知代理，并且仍然遵循 `NO_PROXY`。

归属与扩展契约在 [Provider 认证](https://github.com/sunerpy/zuno/blob/main/docs/design/provider-authentication.md)中规定。
