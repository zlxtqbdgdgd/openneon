# neon dynamic probe whitelist / denylist

本目录是 **feat-067** (PG USDT enable) 和 **feat-069** (Rust uprobe 白名单) 共用的
配置基建 · 供 **feat-068** mcp tool 在 attach 探针前加载校验 · anchor PR 只锁 schema +
fixture + 校验脚本 · 实际 30+ USDT entry / Rust 函数列表分别由 feat-067/#2 和
feat-069/#2 后续 PR 填充。

## 文件

| 文件 | 用途 |
| --- | --- |
| `whitelist.schema.json` | JSON Schema (draft-07) · 锁顶层结构 / enum / pattern / 必填 |
| `whitelist.example.yaml` | 合法 fixture · 含 usdt + uprobe + denylist 三段 |
| `denylist.example.yaml` | 独立 denylist 用法示例 (whitelist 段缺省 = 全 deny) |
| `tests/invalid/*.yaml`  | 非法 fixture · 期望被 schema / validate 脚本拒绝 |
| `tests/run_tests.sh`    | 跑全套合法 + 非法 fixture |

校验脚本在仓根 `scripts/validate_whitelist.py` (PyYAML + jsonschema)。脚本除 JSON
Schema 校验外还做两层语义校验:

1. `denylist.*_patterns` 中每条 regex 必须能被 `re.compile`,否则报 `invalid regex` 退出
   非零 (R2/S1)。
2. `usdt[*].pg_version_min > pg_version_max` 跨字段校验,违反时退出非零 (R1/S2)。

## Schema 顶层

```yaml
version: 1                       # 必填 · 当前只接受 1
usdt:                            # 可选 · USDT probe 白名单
  - target: postgresql           # enum: postgresql/pageserver/safekeeper/proxy/local_proxy/pg_sni_router
    probe_name: postgresql:query__start
                                 # 必须含 <provider>: 前缀 (PG 侧 provider 固定为 postgresql)
    subsystem: executor          # enum: 22 类业务子系统 + other (见下方)
    pg_version_min: 14           # 可选
    pg_version_max: null         # 可选
    sample_overhead_ns_estimate: 50
    args: ["char *query_string"]
    notes: "..."
uprobe:                          # 可选 · Rust 同步函数 uprobe 白名单
  - binary: pageserver           # enum: pageserver/safekeeper/proxy/local_proxy/pg_sni_router (五选一)
    symbol: "pageserver::tenant::timeline::Timeline::get_page_at_lsn"
    module: "pageserver::tenant::timeline"
    type: method                 # enum: sync_fn/method/trait_impl/closure (必填)
    is_async: false              # 必须 false (L3 只允许同步)
    estimated_overhead_ns: 200
denylist:                        # 可选 · 任一 pattern 命中即拒绝 (覆盖 whitelist)
  usdt_probe_patterns:
    - "^pg_md5_hash.*"
  uprobe_symbol_patterns:
    - ".*::scram_.*"
```

## 给 feat-067/A4 的接口契约

- 把 30+ entry 全部塞到 `whitelist.yaml` 的 `usdt:` 段
- 每个 entry 的 `target` 字段固定为 `"postgresql"`
- **`probe_name` 必须含 `<provider>:` 前缀,PG 侧 provider 固定为 `postgresql`**
  (例: `postgresql:query__start`,匹配 issue#31 验收门字段,R2/M3)
- `subsystem` 字段必须命中 enum (22 类业务子系统 + `other` 兜底,共 23 个 enum 值):
  - PG 编译流水线: `parser` / `rewriter` / `planner` / `executor`
  - 并发原语:     `lock` / `lwlock`
  - 存储与 WAL:    `storage` / `wal` / `checkpoint` / `clog` / `smgr`
  - 事务与语句:    `transaction` / `statement`
  - 后台进程:      `bgwriter` / `buffer` / `autovacuum` / `syscache`
  - 复制:          `wal_sender` / `wal_receiver`
  - Neon Rust:    `pageserver` / `safekeeper` / `proxy`
  - 兜底:          `other` (在 `notes` 字段写实际子系统名,待 enum 扩)
- **注解 (R2 元评 issue#54 comment 4564616746 补)**: subsystem enum 是分类标签,
  不代表每个 subsystem 都对应 PG `probes.d` USDT probe。**`autovacuum` /
  `bgwriter` / `wal_sender` / `wal_receiver` / `syscache` 在 PG 14+ 上游
  `src/backend/utils/probes.d` 里没有 USDT probe**,要观测这几类后台进程
  必须走 feat-069/A5 的 Rust `uprobe` (针对 Rust 改写部分) 或 PG C 函数 uprobe
  (针对 PG 原生 C 部分)。usdt 段当前只列 `parser` / `rewriter` / `planner` /
  `executor` / `lock` / `lwlock` / `wal` / `checkpoint` / `clog` / `smgr` /
  `transaction` / `statement` / `buffer` 13 个真实有 USDT probe 的子系统。
- denylist hard-deny pattern 放 `denylist.usdt_probe_patterns`,语义见下方「denylist 语义」段

## 给 feat-069/A5 的接口契约

- Rust 同步函数白名单塞 `uprobe:` 段 · `binary` 字段五选一 (pageserver / safekeeper /
  proxy / local_proxy / pg_sni_router)
- **uprobe entry 必填字段**: `binary` / `symbol` / `module` / `type` / `is_async`
  (R2/M2 · `type` 是必填,缺省会被拒)
- usdt crate 选择性加点也可同时在 `usdt:` 段加 entry · `target` 用对应 Rust binary 名 ·
  `probe_name` 同样必须含 provider 前缀 (例: `neon_pageserver:get_page_at_lsn__start`)
- `is_async` 字段当前 schema 写死必须 `false` · 将来 L4 支持 async fn 时 bump schema
  version 并松绑
- **字段重命名**: issue#34 验收门写的 `function` 已统一为 `symbol` (anchor PR #39
  R1+R2 评审一致结论 · 与 `readelf -s` 输出术语一致 · 与 USDT 段 `probe_name` 区分)
- Rust crypto 黑名单 pattern 放 `denylist.uprobe_symbol_patterns`

## denylist 语义

denylist 在两个阶段生效:

1. **静态校验 (anchor PR #39)**: `validate_whitelist.py` 对每条 pattern 跑
   `re.compile()` 检查 regex 合法性,非法 regex 直接退出非零。
2. **运行时 (feat-068 mcp tool · 不在本 anchor 范围)**: attach 前用
   **`re.fullmatch(pattern, name)`** 匹配。一旦命中即拒绝 attach,**denylist 优先级
   高于 whitelist**——即便某条 entry 在 whitelist 里,只要被 denylist 任一 pattern
   命中也不允许 attach。

作者写 pattern 时遵循约定:

- 用 `.*` 显式表达前缀/后缀 (`fullmatch` 锚定首尾两端,不锚就是要全字匹配)。
  例如 `.*::scram_.*` 表示"路径中含 `::scram_` 子串",`^pg_md5_hash.*` 在 fullmatch
  下等价于"以 `pg_md5_hash` 开头"。
- denylist 整段缺省或 `denylist: {}` 视作"无 deny 规则",**不等于** schema 不
  启用 deny 段——三态语义统一为"零条 deny 规则"。

### deny 优先级 + 安全洞 (给 A5 的扩充任务)

当前 anchor 阶段的 deny pattern 偏窄 (PG 侧 7 类 / Rust 侧 6 类),仅覆盖 SCRAM /
TLS / GSSAPI / password / token / crypto 这类**显式命名**的函数。

**已知安全洞** (跟 issue#34 给 A5 留扩充任务一并跟踪):

- `proxy::auth::AuthFlow::authenticate_password` 这种**非 `password::` 命名空间但
  含敏感字段的函数**,当前 Rust 侧 deny pattern `.*::password::.*` 抓不到。本 PR
  自带 example `proxy::auth::AuthFlow::authenticate_sync` 演示了同模式合法函数,但
  A5 填白名单时若照搬塞 `authenticate_password` / `authenticate_token` /
  `password_hash_verify` 类同步函数,**全都会漏过 deny**。
- 建议 A5 在 feat-069/#2 把 deny pattern 扩成 `.*::auth::.*` 整段 deny,并在
  whitelist 用「白名单显式覆盖 deny」的方式列出确需 trace 的非敏感同步函数 (
  如本 example `authenticate_sync`)——前提是运行时 deny 拦截逻辑同时实现
  「whitelist override」语义,这点需要 feat-068 mcp tool PR 拍板。

## 跑测试

推荐入口 (跟平行 PR #40 A0a feat-033 `check-trace-context` 同前缀,跨 anchor
测试入口纪律统一):

```bash
# 需先装依赖
pip install --user pyyaml jsonschema

make -C pgxn/neon check-probes
```

底层等价于直接跑脚本 (不依赖 make):

```bash
bash pgxn/neon/probes/tests/run_tests.sh
```
