# 原生 ACP

Zuno 将 Agent Client Protocol（ACP）作为原生 stdio 前端提供：

```sh
zuno acp
```

ACP 只是 Zuno 进程内 App Server 的协议投影，不会再启动第二套 agent loop。
ACP session 映射到 App Server 使用的同一份持久化 thread 和 turn，因此取消、
审批、沙箱策略、历史记录与持久化状态仍由原有运行时统一管理。

## 配置

该命令使用普通运行时配置分层。例如：

```sh
# 选择用户 profile。
zuno --profile kiro acp

# 为未由客户端指定模型或目录的 session 设置默认值。
zuno acp --model gpt-5.6-sol -c model_provider='"kiro-local"' --cd /work/project

# 设置权限默认值并增加可写目录。
zuno acp --sandbox workspace-write --add-dir /work/shared

# 遇到未知配置字段时直接失败。
zuno acp --strict-config
```

任意配置覆盖继续使用全局 `-c key=value` 语法。Provider 定义与凭证应放在
配置文件中；`zuno acp` 会明确拒绝 `--oss` 和 `--local-provider`。如果 ACP
客户端支持对应操作，也可以通过协议选择模型、mode 或 session 工作目录。

ACP 传输独占 stdout；日志与诊断信息不得写入协议流。

## 协议范围

当前适配器实现稳定 ACP v1 的初始化、session 新建/加载/恢复/分叉/列表、
prompt、同进程 steer、模型与配置更新、取消、关闭与删除，并在 ACP 权限请求、
session/turn 更新和 App Server 事件之间进行转换。

Zuno 专属能力必须通过显式协议元数据逐步演进。仅凭 ACP SDK 包版本号，不能
视为连接已经启用不稳定的 wire protocol。
