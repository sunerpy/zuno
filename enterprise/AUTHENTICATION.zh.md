# 企业身份适配器

`zuno-identity` 提供统一的 `AccessTokenVerifier -> VerifiedIdentity`。应用宿主依赖
该接口，下面分别接通用 JWT、RFC 7662 introspection 和 Entra 声明策略；模型提供商
凭据继续由 `zuno-auth` 管理。

这是对[已批准计划](PLAN.zh.md)的 2026-09-11 补充：以通用 OAuth2／OIDC 为认证边界，
Entra 作为首个提供商适配器。原始计划全文保留。

本次实现是身份验证库。以下配置是库的输入文档，**不是已经注册的正式 `zuno.json`
字段**。企业 BFF、登录回调／会话存储、当前组织授权和操作审批仍需后续接入。

## 协议与职责

OAuth2 负责授权访问 API；OIDC 在其上补充登录和身份令牌。浏览器 BFF 应负责
Authorization Code＋PKCE、state／nonce、精确回调地址和受保护 cookie。ID token
不能作为 API access token，Graph access token 也不能访问 Zuno API。

由可信部署配置选择验证器。JWT 校验失败不会自动改用 introspection，也不根据令牌
外观尝试更宽松的验证路径。

| 适配器                 | 已实现的检查                                                                                                            |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| RFC 9068 JWT           | RS256、`at+jwt`／`application/at+jwt`、issuer、audience、有效期、`sub`、`client_id`、`iat`、`jti`、作用域及主体类型证明 |
| 提供商 JWT             | RS256、`JWT`、公共信任和有效期边界，以及显式 access-token 标记和声明映射                                                |
| RFC 7662 introspection | 经过客户端认证的 HTTPS POST、`active=true`、本 API audience、有效期、主体、应用白名单、作用域和主体类型证明             |
| Entra v2               | 固定租户 issuer、API 应用 GUID audience、`tid + oid`、`azp` 白名单，以及独立的用户委派／服务身份策略                    |

当前 JWT 签名实现为 RS256。对称签名、无签名、加密令牌及 DPoP 等发送方绑定令牌
不进入这些 Bearer 适配器。额外算法和证明机制需要独立实现及验证。

## 通用提供商配置

```json
{
  "authority": {
    "issuer": "https://identity.example.test/realms/company"
  },
  "claims": {
    "tenantId": "company",
    "audience": "https://zuno.example.test/api",
    "allowedClients": ["company-web"],
    "requiredScopes": ["session:access"],
    "principalKind": "user",
    "actorClaim": { "claim": "principal_type", "value": "user" }
  },
  "profile": { "type": "rfc9068" }
}
```

`principal_type` 是**身份提供商管理的示例声明**，不是 OAuth2 标准字段。应使用实际
提供商保证的用户／服务身份标记。OAuth2 没有统一的用户与服务身份分类声明，因此
不按邮箱、名称或字段缺失猜测类型。主体证明、作用域和调用应用白名单同时校验。

旧格式 JWT access token 使用 `provider` profile，并配置实际存在的
`accessTokenClaim`，例如 `token_use=access`。`scopeClaim` 和 `clientClaim` 可以
映射提供商的顶层字符串字段。RFC 9068 固定使用 `scope` 和 `client_id`；
作用域以空格分隔并区分大小写。

通用主体由配置的企业租户，以及 issuer、原始 `sub`、主体类型经长度分帧后的
SHA-256 标识确定。调用应用使用独立的 issuer 作用域标识。不同 issuer、用户／服务
类型、大小写和包含分隔符的主体不会通过字符串拼接发生混用。邮箱变化不改变数据
归属；变更 issuer 应按身份迁移处理。

## 密钥发现与轮换

`OAuth2Authority` 固定预期 issuer，支持 OIDC discovery；未提供 OIDC discovery 的
OAuth2 服务可指定 `jwksUrl`。所有端点必须使用 HTTPS，禁止 URL 凭据和 fragment。
Discovery 返回的 issuer 必须与配置完全一致。

JWKS 默认只能来自 issuer 的 origin。管理员可以显式增加最多八个
`additionalJwksOrigins`；metadata 自身不能扩大白名单。禁止 HTTP 重定向，限制请求
超时、文档大小和密钥数量。标准 JWK 的 `use` 和 issuer 元数据可省略；用途矛盾、
重复 key ID、私钥参数及非法 RSA 公钥会被拒绝。

密钥缓存默认一小时过期，未知 key ID 最多每 60 秒触发一次刷新。并发刷新合并，
取消或失败保留冷却时间。未知密钥的慢请求不阻塞仍有效的已知密钥；过期后无法刷新
则拒绝验证，不无限沿用旧密钥。

## 不透明令牌

`OAuth2IntrospectionConfig` 固定 `issuer`、HTTPS `endpoint` 和相同的声明策略。
宿主另外从密钥存储解析 `IntrospectionClientAuth`，支持 `client_secret_basic` 和
`client_secret_post`；不会把密钥放进可序列化配置。

令牌在表单 body 中提交，并附 `token_type_hint=access_token`，不进入 URL。
关闭重定向并限制请求时间、响应大小。每次验证重新检查 active，不保留正向缓存；
撤销立即影响下一次验证。服务不可用产生类型化错误，不能沿用先前成功结果。

虽然 RFC 7662 中部分字段可选，本企业适配器要求提供 `aud`、`exp`、`sub`、
调用应用、作用域及主体类型证明。如果响应包含 `iss`，必须与配置匹配。缺少这些事实
的提供商需要显式映射集成，不能猜测补齐权限信息。

## Entra 特定规则

Entra 复用通用 JWT 验证、discovery 和缓存，只补充：

- Microsoft 公有云租户级 v2 issuer、GUID、固定 JWKS 路径及签名密钥 issuer 校验。
- 本 API 的应用 GUID audience；应用注册需选择 v2 access token。
- `tid + oid` 的稳定归属及 `azp` 调用应用白名单。
- 用户必须有委派 `scp`；Worker 必须有 `idtyp=app` 及所需应用角色，二者不混用。

v1 令牌、国家云和自定义应用签名密钥不会自动套用此配置。真实 Entra 登录及应用
注册仍需测试租户／应用配置；本地生成签名的测试不能替代真实租户验收。

## 授权与验证

`VerifiedIdentity` 没有公开构造器和 Deserialize，不保存 Bearer token。宿主从当前
授权服务取得策略版本后才记录 attribution；JWT 和浏览器不能指定该版本。

OAuth scope／role 只用于 API 准入，不直接授予命令、Memory、HITL 或环境权限。
任务接纳和执行阶段仍需复核组织策略、资源归属和当前租约。

运行 `cargo test -p zuno-identity`。测试使用临时 RSA 密钥和真实签名，覆盖伪造签名、
audience／issuer／应用错误、身份类型混用、过期、跨 issuer 主体隔离、密钥轮换、
取消和并发，以及 introspection 撤销、不可用、凭据编码和脱敏。

规范依据：[OAuth2](https://www.rfc-editor.org/rfc/rfc6749)、
[JWT access token](https://www.rfc-editor.org/rfc/rfc9068)、
[introspection](https://www.rfc-editor.org/rfc/rfc7662)、
[OIDC discovery](https://openid.net/specs/openid-connect-discovery-1_0.html) 和
[Entra 声明校验](https://learn.microsoft.com/en-us/entra/identity-platform/claims-validation)。
另见 [English](AUTHENTICATION.md) 和[实施状态](STATUS.md)。

身份服务的 HTTP client 通过 `zuno-network` 创建，遵守控制面进程的代理策略及 `NO_PROXY`；适配器继续强制 HTTPS、禁止重定向并限制请求。

组织策略和持久审批存储已独立实现，见[授权指南](AUTHORIZATION.zh.md)；BFF／Driver 集成仍待完成。
