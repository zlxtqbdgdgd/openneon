# Agent 说明 · Rust 同步函数 uprobe 白名单

> 本文供 feat-068 mcp tool description / agent prompt / 用户文档直接引用 ·
> 设计哲学 + 接口契约 + 已知 caveat 全集中在这里 · 不再分散到多份 README。

## 一句话

**只允许把 uprobe attach 到 Rust *同步* 函数符号上 · async fn 内部 L3 不做。**

## 为什么要这条限制

Rust async fn 在编译期被翻译成 state machine + `Future::poll()` 实现。
任何对 async fn 入口/出口的 uprobe 测量到的都是 *poll() 单次执行* 的耗时,
而不是 *逻辑步骤* 的耗时:

| 想测的 | uprobe 实测到的 | 差距 |
| --- | --- | --- |
| `Timeline::get_page_at_lsn` 从入口到 page 返回的总耗时 | 第一个 poll 调用耗时 | 漏掉所有 await 之后的 poll · 漏掉 waker 唤醒间隔 |
| 单步逻辑 (例 IO 等待 100ms) | 100ms 被切成 N 个 poll · 每个 poll 几 μs | 完全没法对应到一次逻辑等待 |

这是 *语义* 问题不是 *工程* 问题——uprobe 技术上能 attach,但读出来的数据无法解释。

## 三道屏障

为了让"不允许 attach async fn"这条约束坚不可破,落了三层防护:

| 屏障 | 落点 | 触发时机 | 失败行为 |
| --- | --- | --- | --- |
| 1. Schema enum 强约束 | `pgxn/neon/probes/whitelist.schema.json` `is_async` 字段 `enum: [false]` | schema 校验 (本仓 `make check-probes`) | YAML 配 `is_async: true` 直接 schema 校验失败 (退出非零) |
| 2. mcp tool 加载断言 | 当前实现: `pgxn/neon/probes/tests/feat069_fixtures.sh` case 4 (negative · 自动跑) · runtime 部分留 follow-up PR `feat-069-binary-wire` 在 binary 接入 `pub use neon_probes` 后加 `load_whitelist()` 启动断言 | schema 校验 + Linux CI fixture / 未来 runtime 启动 | YAML 配 `is_async: true` 直接 schema 校验失败 (退出非零) · runtime 加上之后命中即 panic 拒绝启动 |
| 3. Agent description | 本文 + feat-068 tool description | 用户调用 mcp tool 时看到 | 用户被告知 async fn 不支持 · 引导转用同步函数 / hot path USDT |

> 屏障 1 是 schema 层硬拦 (`enum: [false]`) · 屏障 2 是 runtime 二次确认 · 屏障 3 是 UX 提示。
> 三层冗余 · 任一坏掉都不致命。

## 怎么选 attach 目标

按优先级:

1. **首选 USDT 探针** (provider = `neon_pageserver` / `neon_safekeeper` / `neon_proxy`):
   - 已加点的 hot path 见 `rust-whitelist.yaml` 的 `usdt:` 段
   - bpftrace 命令: `bpftrace -e 'usdt:./pageserver:neon_pageserver:get_page_at_lsn__start { @[arg2] = count() }'`
   - 探针名 `<provider>:<event>__<边界>` · 边界统一为 `__start` / `__done` / `__established` / `__closed`

2. **其次同步函数 uprobe** (见 `rust-whitelist.yaml` 的 `uprobe:` 段):
   - 所有 entry `is_async: false` · 已经过 schema 校验
   - 例: `pageserver::tenant::storage_layer::delta_layer::sort_delta`
     (compaction 排序 hot path · 纯 CPU · 同步)

3. **永远不允许**: async fn / 含 `.await` 的同步 wrapper / denylist 命中的密码/密钥相关符号

## denylist 已盖的安全洞 + 已知缺口

当前 denylist (`rust-denylist.yaml` + `rust-whitelist.yaml` 内嵌 denylist):

- `.*::scram_.*` / `.*::sasl::.*` / `.*::tls::handshake.*` / `.*::password::.*`
- `.*::token::verify.*` / `.*::crypto::aead.*`
- **本 PR 补 (R2 元评 issue#54 comment 4564616746)**:
  `.*::authenticate_password.*` / `.*::authenticate_token.*` / `.*::password_hash_verify.*`
  三条精确 pattern,堵 anchor README §deny 优先级 + 安全洞 点名的"非 `::password::`
  命名空间但含敏感字段"函数,不误抓现有合法 `proxy::auth::credentials::*` sync 函数 ·
  L4 候选技术前提 (`feat-068` mcp tool 的 whitelist override 语义) **已具备 PR**:
  feat-068 主 PR `attach_neondb_dynamic_probe` 计划在 mcp 仓 PR #167 (B2) wiring 完成
  后实装,届时 `.*::auth::.*` 整段 deny + whitelist 显式 override 切换是低风险动作。

**已知缺口** (L4 候选 · 暂未扩):

- `.*::auth::.*` 整段 deny 仍未启用 (启用条件: feat-068 实现 whitelist override denylist 语义)
- 启用后,白名单需显式 override 现有 `proxy::auth::credentials::*` 4 个合法 sync 函数

## 给 feat-068 attach 命令的接口契约

每条 uprobe 白名单 entry 提供四元组 (供 mcp tool attach 命令直接使用):

| 字段 | 用途 |
| --- | --- |
| `binary` | 决定 attach 到哪个 ELF · enum 五选一 · feat-068 据此查 release build artifact 路径 |
| `symbol` | demangle 后的 Rust 符号名 · feat-068 用 `addr2line` 或 `rustfilt` 反查实际 mangled name 后传给 `bpftrace` |
| `module` | 顶层 crate / module 路径前缀 · 用于 mcp tool 按模块聚合展示 |
| `type` | `sync_fn` / `method` / `trait_impl` / `closure` · L3 没有 `async_fn` (schema 不接受) |

USDT entry 提供:

| 字段 | 用途 |
| --- | --- |
| `target` | 决定 attach 到哪个 ELF (与 uprobe 的 `binary` 同语义) |
| `probe_name` | `<provider>:<event>__<边界>` 形式 · feat-068 用 `bpftrace -e 'usdt:./<binary>:<provider>:<event>__<边界>'` 直接 attach |
| `subsystem` | 按子系统聚合用 |
| `args` | 参数文档 · 用户写 bpftrace 表达式时参考 |

## 验收门 (feat-069 自检)

- 本 PR 内:
  - `make -C pgxn/neon check-feat069` 全绿 (9 PASS · 6 STAGE · 留 feat-068 CI)
  - `make -C pgxn/neon check-probes` 全绿 (anchor 已有 13 PASS · 本 PR 不动)
- 留 **follow-up PR `feat-069-binary-wire`** (R2 元评 D4 决策 · 不归本 PR · pageserver / safekeeper / proxy 三个 Cargo.toml + lib.rs 加 `pub use neon_probes`):
  - `cargo build --release` 后 `readelf -n target/release/pageserver` 看到 USDT note section
  - `readelf -W --symbols target/release/pageserver | grep new_for_path` 看到具体函数符号
  - `bpftrace -l 'uprobe:target/release/pageserver:*new_for_path*'` 列出可 attach 符号
  - mcp tool runtime `load_whitelist()` 启动断言 (屏障 2 runtime 部分)
- denylist 安全洞 (`.*::authenticate_password.*` / `.*::authenticate_token.*` / `.*::password_hash_verify.*` 三条精确 pattern):
  本 PR 已补,堵 anchor README 留的三类点名函数;`.*::auth::.*` 整段 deny 仍是 L4 议题 (需 feat-068 实现 whitelist override 语义后再扩)。
