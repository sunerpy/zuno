# Rootless 执行环境

预览库 `zuno-environment` 实现 `EnvironmentProvider` 与 `OperationGateway`，尚未注册为
Agent 工具或企业启动命令。持久审批等待、Gateway 认证服务与 Worker Profile 装配仍需接入。

## 隔离与归属

后端只接受 rootless Docker Unix socket，并检查 systemd cgroup、内存、CPU 和 PID 限制。
镜像要求不可变 digest。每个会话使用带所有者／环境／定义标签的持久工作区卷；每个命令使用
独立容器，名称和标签绑定稳定 operation ID 与请求摘要。网关重启后读取原容器终态与日志。

命令容器使用只读根文件系统、无网络、清空 capabilities、no-new-privileges、有界内存／CPU／
PID 及不可执行的临时目录，仅工作区卷可写。不继承 Gateway 环境变量、数据库凭证、模型密钥或
Docker socket。当前后端不启用网络访问，也未提供可靠的墙钟 watchdog；CPU 限制是速率上限，
不是累计 CPU 时间预算。

Gateway 数据目录须为私有 `0700`，SQLite 账本和快照文件独立于 Zuno 主数据库。进程实例锁
防止两个 Gateway 控制同一账本；每环境控制锁协调开始、取消、快照及释放，不锁住其他环境。

## 操作与恢复

账本先提交开始准入，再调用 Docker；该转移只能成功一次。旧 `created` 查询不能将 `starting`
降回可再次启动。确认丢失时查询原容器，不重试 Docker start。确认退出只记录一次真实 exit code；
无法确认的开始、丢失的在途容器进入 `uncertain` 并保留工作区占用，等待核查。

`OperationAuthority` 必须显式提供。`OrganizationOperationAuthority` 复用现有 `RuntimeStore`
和 `OrganizationStore`，核对稳定审批绑定、实际环境版本、命令摘要及当前租约；没有生产
allow-all 默认。它用于可信装配入口，独立 Gateway 部署仍须通过内部认证接口访问授权服务。
当前取消也要求该授权；租约撤销后的管理员取消属于尚未完成的分布式控制集成。

输出分页使用逻辑字节偏移及包含 stdout／stderr 身份的前缀摘要，流式解析 Docker frames，
限制单页内存。前缀缺失或改变时返回冲突，不静默拼接日志。日志在命令容器保留期间可读；
长期产物导出及磁盘／日志配额仍需部署集成。

## 快照与释放

快照要求环境空闲且版本匹配。归档流式写入私有临时文件，校验摘要、大小与工作区 tar 边界后
才发布元数据；拒绝特殊文件、set-id 权限和逃逸链接。当前快照上限为 512 MiB。

派生环境核对已存快照身份，恢复到新卷。归档重定位到可写工作区挂载点，不在 Gateway 宿主解压；
父工作区保持不变。释放核验归属标签，只删除本环境保留的命令容器和卷，随后提交 tombstone。
重复释放安全；不能静默重建已释放环境或已经丢失卷的工作区。

当前快照保存在 Gateway 私有文件系统。可替换远程产物后端、派生中断后的持久恢复，以及快照
保留／GC 策略仍是完整企业部署的待实现能力。

## 原生验证

使用已有的隔离 rootless daemon：

```sh
ZUNO_ROOTLESS_DOCKER_SOCKET=/run/user/1000/zuno-preview/docker.sock \
  python3 scripts/check_enterprise_docker.py
```

未指定 socket 时，脚本使用独立 socket／data／exec 目录和用户 systemd D-Bus 启动私有 daemon。
不修改 Docker context，不停止宿主 daemon。前置条件是 `uidmap`、rootless Docker extras 及
已委派 cgroup 的用户 systemd 会话。

测试覆盖根文件系统／网络／资源限制、不可变命令、重开账本后不重复执行、输出游标、取消、
快照、分支隔离和释放。预览 CI／发布门禁在 Linux amd64、arm64 原生运行；只有实际成功的 CI
结果才算平台证据。

另见 [English](ENVIRONMENTS.md)、[授权](AUTHORIZATION.zh.md)和[实施状态](STATUS.md)。

## 网关传输

后端的 Worker／控制面 HTTP 适配见[网关传输](GATEWAY.zh.md)。请求凭证不绕过操作
审批，Docker gate 还通过临时 PostgreSQL／TLS 控制面验证真实 HTTP 执行路径。
独立角色启动、Agent 工具装配及管理级取消仍有单独交付要求。

账本格式 2 增加持久完成结果与确认。结果所有者确认前，环境释放会保留输出；
有界投递扫描可在重启后恢复，响应不可用不会丢弃待投递条目。详见
[操作结果](OPERATION_RESULTS.zh.md)。
