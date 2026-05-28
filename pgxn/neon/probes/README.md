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
| `tests/invalid/*.yaml`  | 6 个非法 fixture · 期望被 schema 拒绝 |
| `tests/run_tests.sh`    | 跑全套合法 + 非法 fixture |

校验脚本在仓根 `scripts/validate_whitelist.py` (PyYAML + jsonschema)。

## Schema 顶层

```yaml
version: 1                       # 必填 · 当前只接受 1
usdt:                            # 可选 · USDT probe 白名单
  - target: postgresql           # enum: postgresql/pageserver/safekeeper/proxy/local_proxy/pg_sni_router
    probe_name: query__start    # PG 上游约定: 双下划线分段
    subsystem: executor         # enum: parser/executor/lock/lwlock/wal/...
    pg_version_min: 14          # 可选
    pg_version_max: null        # 可选
    sample_overhead_ns_estimate: 50
    args: ["char *query_string"]
    notes: "..."
uprobe:                          # 可选 · Rust 同步函数 uprobe 白名单
  - binary: pageserver           # enum: pageserver/safekeeper/proxy/local_proxy/pg_sni_router
    symbol: "pageserver::tenant::timeline::Timeline::get_page_at_lsn"
    module: "pageserver::tenant::timeline"
    type: method                 # enum: sync_fn/method/trait_impl/closure
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
- `subsystem` 字段必须命中 enum (有 14 种 · 不够再扩 schema)
- denylist 7 类 hard-deny pattern 放 `denylist.usdt_probe_patterns`

## 给 feat-069/A5 的接口契约

- Rust 同步函数白名单塞 `uprobe:` 段 · `binary` 字段四选一
- usdt crate 选择性加点也可同时在 `usdt:` 段加 entry · `target` 用对应 Rust binary 名
- `is_async` 字段当前 schema 写死必须 `false` · 将来 L4 支持 async fn 时 bump schema version 并松绑
- Rust crypto 黑名单 pattern 放 `denylist.uprobe_symbol_patterns`

## 跑测试

```bash
# 需先装依赖
pip install --user pyyaml jsonschema

bash pgxn/neon/probes/tests/run_tests.sh
```
