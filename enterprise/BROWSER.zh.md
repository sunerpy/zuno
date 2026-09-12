# 企业浏览器认证

BFF 复用[通用 OAuth2／OIDC 适配器](AUTHENTICATION.zh.md)，Entra 是其中一种提供商
配置。`EnterpriseBrowser` 提供真实的登录、回调、会话查询和注销处理器。企业控制面
角色装配 BFF，并可从已校验静态资源包提供 [React 工作台](WEB.zh.md)。

## 接口与归属

| 组件 | 职责 |
| --- | --- |
| `OidcLoginClient` | 授权码、PKCE S256、nonce、issuer／client 绑定和 API 身份验证 |
| `AuthorizationCodeExchange` | 一次有界 token POST；HTTPS 实现支持 client secret Basic／Post |
| `LoginStateCipher` | AES-256-GCM、随机 nonce、坐标认证和显式保留旧 key 的轮换 |
| `LoginTransactionStore` | 保存加密事务，在网络调用前原子消费 |
| `BrowserSessionStore` | 创建、查询和撤销不透明浏览器会话 |
| `BrowserLoginService` | 组合这些接口，不依赖数据库和 HTTP 框架 |
| `EnterpriseBrowser` | 同源 HTTP 边界、受保护 Cookie 和身份提取 |
| `PostgresBrowserStore` | 固定租户、数据库时间、容量及会话／审计原子写入 |

身份凭据和加密密钥属于 BFF／数据所有者，不能进入 Worker、命令容器或公共 DTO。
PostgreSQL 加密保存短期 PKCE／state／nonce，仅保存随机 256 位会话凭据的 SHA-256
摘要。浏览器会话行及 Web 响应均不保存或返回 access token、ID token、refresh token。

登录与回调可以由不同 BFF 实例处理，实例共享固定租户、验证后的提供商配置、数据库
和密钥环。必须先提交 state 的原子消费，再兑换授权码；并发回调只能有一个兑换。
错误浏览器绑定不会消费其他浏览器的合法事务。

## 可信配置

issuer 和 callback 必须是精确 HTTPS 地址，例如
`https://agent.example.test/auth/callback`。Discovery 有大小／超时限制，要求精确
issuer，禁止重定向。登录端点默认限制在 issuer 的 origin；提供商使用单独认证／token
域名时，管理员通过 `additionalEndpointOrigins` 显式允许。Metadata 不能自行扩大
白名单；JWKS 使用独立的 `additionalJwksOrigins`。

`OidcLoginOptions` 是经过验证的库配置文档：

```json
{
  "transactionLifetimeSeconds": 300,
  "sessionLifetimeSeconds": 3600,
  "clockSkewSeconds": 30,
  "maxAuthenticationAgeSeconds": null,
  "additionalEndpointOrigins": []
}
```

登录事务允许 30–600 秒，会话允许 60–86,400 秒，时钟偏差允许 0–120 秒。
认证年龄配置允许 1–86,400 秒，发送 `max_age` 并要求对应 `auth_time`。
实际会话有效期同时受两个已验证 token 的期限和会话上限约束。

宿主分别解析客户端 secret 和 32 字节加密 key。密钥环最多保留八个明确命名的 key，
新事务使用 current key，旧 key 仅用于尚未回调的短期事务。Identity HTTP client 可
接收管理员配置的私有 CA，仍验证主机名，不接受请求携带的信任根。

scope 必须包含 `openid` 和本 API 所需的委派作用域。ID token 校验 issuer、audience、
nonce、时间、authorized party，以及存在时的 access-token hash。资源 token 还需
独立通过本 API verifier，证明委派用户和本次登录的精确 OAuth client。通用适配器
分别保留提供商原始 client ID 和组织授权使用的 issuer 作用域应用 ID；不能将原始 ID
直接与内部摘要比较。

这些配置不是正式 `zuno.json` 字段。宿主显式装配 client、store 和 service，不通过
失败回退启用其他协议。只有 OAuth2、没有 OIDC 的提供商可以接入 API 认证，但不能
通过此登录流程提供浏览器身份认证证明。

## HTTP 契约

| 请求 | 行为 |
| --- | --- |
| `POST /auth/login` | 持久化登录事务并设置绑定 Cookie；`Accept: application/json` 返回 `{authorizationUrl}`，其他请求携带 code＋PKCE 参数跳转 |
| `GET /auth/callback` | 校验绑定 state、单次消费、兑换并验证 token、创建不透明会话，跳转 `/` |
| `GET /auth/session` | 返回经过认证的租户、主体、应用和到期时间 |
| `POST /auth/logout` | 撤销当前会话并使 Cookie 到期 |

Host 必须匹配配置的公开 authority，不根据转发头选择 origin。修改请求要求精确
`Origin` 和 `X-Zuno-CSRF: 1`；提供 `Sec-Fetch-Site` 时必须为 `same-origin`。
不开放携带凭据的跨域 CORS。登录回调从 IdP 返回，通过服务端 state 和浏览器绑定认证。

Cookie 名为 `__Host-zuno_preview_login` 和 `__Host-zuno_preview_session`，
均带 `Secure`、`HttpOnly`、`Path=/`，没有 Domain。短期登录 Cookie 使用
`SameSite=Lax` 接收顶层认证回调，会话 Cookie 使用 `SameSite=Strict`。同一浏览器
最后发起的登录拥有当前绑定 Cookie；失败或未绑定的回调不能覆盖它，成功后使其到期。

响应附 `Cache-Control: no-store`、`Pragma: no-cache`、
`Referrer-Policy: no-referrer`、`X-Content-Type-Options: nosniff`。
拒绝重复回调字段／Cookie、错误 issuer；不支持请求指定跳转目标。
反向代理访问日志应省略回调 query，避免记录授权码。

`authenticate_routes` 为已装配的浏览器路由附加验证后的身份。资源处理器仍需在数据
所有者事务中检查当前组织策略，不能凭该中间件授予工具或 Memory 权限。外部 Bearer
API 和内部 Worker 接口保留独立认证边界。

Web 登录请求接收 JSON，再通过顶层导航进入返回的 HTTPS 认证地址，避免 fetch 跟随
跨域 IdP 重定向。已登录 Web 请求还通过 `x-zuno-browser-context` 携带 JSON
`[tenantId, principalId, clientId]`；提供该头时必须与 Cookie 验证后的身份完全一致，
重复或不匹配均拒绝。它防止旧标签页在 Cookie 换号后借新账号提交，不能替代认证及当前
组织授权。

## 恢复与验证

Token POST 不重定向、不自动重试。响应丢失或失败后重新登录，不能重放已消费的授权
事务。状态服务故障返回不可用；注销未提交时保留 Cookie，允许用户重试。

会话按期限到期并支持本地注销。本阶段未实现 refresh token、IdP back-channel logout
或真实 Entra 应用注册。当前组织授权独立于上游 token 有效期；真实提供商仍需单独
验收，原生浏览器／服务验证见 [Web](WEB.zh.md)。

运行 `cargo test -p zuno-identity` 和 `python3 scripts/check_enterprise_postgres.py`。
后者使用临时 TLS 证书、RSA 签名的真实 HTTPS OIDC 测试服务、两个 BFF 实例及
PostgreSQL，验证通用提供商、PKCE、POST 不重放、应用绑定、未绑定回调、私有会话、
CSRF、注销及格式 6 迁移保留／回滚。它证明网络与存储行为，不代表真实企业租户验收。

另见 [English](BROWSER.md)、[PostgreSQL](POSTGRES.zh.md) 和[授权](AUTHORIZATION.zh.md)。
