# 企业远程 ACP 桥接

该 Linux 服务将编辑器的 ACP stdio 连接接入企业公共 HTTPS API。会话、输入、
审批、模型和命令执行仍由企业控制面、Worker 与执行网关负责。桥接不会将编辑器
的文件系统、终端或 MCP 服务委托给远端任务，不需要数据库或 Worker 凭证。
个人 `zuno acp` 的 Windows／macOS／Linux 行为保持独立。

## 配置

```json
{
  "stateDirectory": "/var/lib/zuno-enterprise-preview/acp-alice",
  "service": {
    "kind": "acp_bridge",
    "api": {
      "endpoint": "https://agent.example/api/v1/",
      "accessTokenFile": "/run/secrets/alice-access-token",
      "rootCertificate": null
    },
    "workspaceId": "workspace",
    "localDirectory": "/home/alice/project",
    "maxSessions": 32,
    "pollMillis": 500
  }
}
```

使用 `zuno-enterprise --config /absolute/bridge.json` 启动，stdout 只输出 ACP
JSON-RPC，日志写入 stderr 及私有状态目录。`localDirectory` 仅校验编辑器 cwd
映射，不同步文件、不授予远端访问该路径的权限。每个进程绑定一个经过验证的
主体；令牌文件可刷新，但租户、主体类型、所有者和调用应用必须保持一致。
OAuth2 access token 由登录客户端取得，本服务不会把 ID token 当作 API 凭证。

## 已实现协议

支持 initialize、会话创建／载入／恢复／列表／关闭、文本 prompt 和取消。
未提供附件、客户端 MCP、模型切换、steer 或删除的占位能力。企业配置仍决定
模型及工具。`session/resume` 只接回会话，已有 Job 的观察通过 `_zuno/observe`
进行，不会为接续重新提交输入。

标准 ACP 消息、思考摘要、工具状态及 Plan 来自公共持久事实。完整六组 enum、
授权操作和替换／移除语义通过 `_zuno/activity` 的 `CommittedFrame` 保留；临时
进度通过 `_zuno/live` 发送，绝不当作已提交历史。载入返回最近 100 项与固定
`through`、`before`，更早内容通过 `_zuno/history` 分页读取。

扩展请求均要求当前连接已打开 sessionId：

| 方法 | 参数／作用 |
| --- | --- |
| `_zuno/history` | sessionId、through、before；返回较早历史页 |
| `_zuno/job` | sessionId、jobId；读取本人任务状态 |
| `_zuno/request` | sessionId、requestId；核查持久输入接纳回执 |
| `_zuno/observe` | sessionId、jobId；继续观察原 Job 至终态或暂停 |

## 审批、取消与断线

ACP 权限回复不是企业批准。桥接只展示持久审批事实；实际答复需通过当前
获准审核应用调用企业审批 API。编辑器的 allow-always、文件和终端能力不会
改变组织策略，执行始终留在企业环境。

输入先读取版本，再使用 CAS 和稳定 requestId 提交。可在
`_meta.zuno.requestId` 明确提供跨连接的核查标识；缺省标识只在该连接内稳定。
POST 不自动重放，响应丢失时查询原回执。未确认结果返回核查标识，不表示输入
一定未接纳。取消绑定当前 Job 和 turn；在提交期间取消，也会在取得接纳结果
后取消原 Job。断开连接仅终止观察，不自动取消已接纳任务。

当前为后端桥接交付，不包含 App/UI。完整 App 设计仍需先使用 Penpot。
