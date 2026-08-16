# 资源门禁

六项门禁支撑本实现的资源声明。下列数据均在 Linux 上实测，不是推算值。其中两项需要显式启用，
不属于普通测试套件；另有一项的一半从未在当前环境运行，限制见后文。

这一页从 README 移出，因为它是测量记录而不是项目介绍。README 只保留一条指向本页的链接。

## G1 与 G2 — 峰值常驻内存

<!-- generated:BEGIN memory-gate-measurement -->
Derived from the newest committed measurement artefact,
[`.omo/evidence/task-123-opencode-rust.txt`](../.omo/evidence/task-123-opencode-rust.txt).
The ceilings are not measured here:
[`benchmarks/ts-baseline.json`](../benchmarks/ts-baseline.json) freezes each
one at half the TypeScript median for the same workload, and every other column
below is computed from the five per-repetition Rust peaks the artefact records.

| gate | workload | Rust median peak | frozen ceiling | margin | five-run spread | Rust / TypeScript | verdict |
|---|---|---:|---:|---:|---:|---:|---|
| G1 | `W-idle` | 20,380 KiB | 477,120 KiB | 456,740 KiB | 444 KiB | 0.0214 | PASS |
| G2 | `W-real` | 1,494,024 KiB | 1,513,496 KiB | 19,472 KiB | 17,032 KiB | 0.4936 | PASS |

G2's five `W-real` peaks were 1,493,496 · 1,493,948 · 1,494,024 · 1,510,444 ·
1,510,528 KiB. Every one of the five is under the ceiling, and the median's
19,472 KiB margin — 1.29% of the ceiling — is 2,440 KiB wider than the 17,032
KiB five-run spread. That ordering is the claim worth checking: a margin
narrower than the spread is a coin flip that landed, not a pass. The superseded
measurement in
[`.omo/evidence/task-122-opencode-rust.txt`](../.omo/evidence/task-122-opencode-rust.txt)
is the shape being avoided: a 164,552 KiB spread around a median that finished
13,692 KiB over the same ceiling — FAIL.
<!-- generated:END memory-gate-measurement -->

## G3 至 G6

| 门禁 | 约束对象 | 实测值 | 上限 | 结论 |
| --- | --- | --- | --- | --- |
| G3 | 500 轮 soak 中每轮内存增长 | 0.0001775568 MiB/turn | 1.0 MiB/turn | PASS |
| G3 | 最终/中段峰值比 | 0.9938255268 | 1.5 | PASS |
| G4 | soak 期间的活性 | 两个上限均未触发 | 120 秒无状态进展；每轮 1800 秒硬截止 | PASS |
| G5 | 生产者/消费者边界的无界 channel | 17 个有界 + 2 个已声明例外，0 个未声明 | — | PASS |
| G6 | 父进程退出后的孤儿进程 | Linux 上 0 个孤儿，正常关闭和 `SIGKILL` 均验证 | — | Linux 上 PASS；Windows 部分未执行 |

## 四项明确限制

**G2 上限不会随测试对象缩放。** 它是固定值：同一 session 的一次 TypeScript 中位数的一半。
因此，即使代码不变，换成明显更大的 session 也可能使门禁转为 FAIL。上方 margin 与五次运行
spread 决定真实余量，两者的大小关系比任何单个数字更重要。

**G6 的 Windows 部分从未执行。** 上方实测结果来自
`crates/zuno-process/tests/containment.rs`，该文件受 `#![cfg(target_os = "linux")]` 限制。
Windows Job Object 路径位于 `crates/zuno-process/tests/windows_containment.rs`，受
`#![cfg(windows)]` 限制；它在 Linux 主机上是 **NOT EXECUTED**，不是“跳过但视为通过”，也不能
由 Linux 结果推断。只有在原生 Windows CI 或 Windows 主机上执行后，才能声明 G6 跨平台通过。

**`cargo test --workspace` 通过不代表 G1-G6 通过。** 高成本门禁需要显式启用，普通套件会跳过
或忽略它们：

```sh
# G1 + G2：仅当 mode 为 `run` 时执行。
ZUNO_MEMORY_GATE_MODE=run cargo test -p zuno-testkit --test memory -- --nocapture --test-threads=1

# G3 + G4：真实 driver soak。该测试被 #[ignore]，会占用两个真实 language server、
# 一个 50,000 文件 watcher、一个 PTY，以及两小时 wall clock。
ZUNO_MEMORY_GATE_MODE=skip cargo test -p zuno-testkit --test soak \
  g3_and_g4_real_driver_soak_stays_bounded_and_live -- \
  --ignored --exact --nocapture --test-threads=1

# G5 与 G6 会在普通套件中运行。
cargo test -p zuno-testkit --test backpressure
cargo test -p zuno-process --test containment
```

**G2 测试对象已固定，在其他环境复现需要重新捕获。** 实测 session 为
`ses_2bcaee257ffeFZNJrmtpi3ZglR`（931 条消息、3,620 个 part、105,118,812 part bytes），位于
一个以 sha256 标识的 2.6 GB 数据库快照中。`crates/zuno-testkit/src/perf/subject.rs` 保存该 pin；
发生不匹配时会打印四步重新捕获流程，第四步要求重测 TypeScript 基线，因为测试对象与上限必须
来自同一次测量。没有该快照的机器会在 pin 校验处失败，而不会测量其他对象并称其为 G2。

测量方法、公式与冻结版本见 [perf-methodology.md](perf-methodology.md)。
