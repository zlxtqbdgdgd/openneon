# feat-036 ↔ feat-037 v2 联动 contract (jsonlog 阶段聚类)

> Sibling feat-037 design issue: zlxtqbdgdgd/openneon-design#51
>
> feat-037 是 hybrid (skill+mcp) log pattern clustering · 走 staged delivery:
>   - **v1**: raw stderr (PG 文本 log) → LLM 主 + Drain3 备 双路径聚类 (先 ship)
>   - **v2**: jsonlog (本 feat-036 输出) 路径 · 字段级聚类 + trace_id filter
>
> feat-036 是 feat-037 v2 阶段的**字段层 enabler** —— v1 阶段 feat-036 不在
> critical path, 可以先 stub; feat-037 v1 ship 后, 本 feat-036 上线开 GUC,
> feat-037 v2 拉 jsonlog 走字段路径.

## feat-036 → feat-037 v2 提供的接口

feat-036 不直接给 feat-037 调函数, 它**只是把 jsonlog 输出格式定下来**.
feat-037 v2 通过 `compute_tools` 的 jsonlog reader / `cluster_neondb_logs`
mcp tool, 按以下 schema 解析每行 jsonlog event:

```json
{
  "timestamp": "...",
  "user": "...",
  "dbname": "...",
  "pid": 12345,
  "session_id": "...",
  "line_num": 42,
  "vxid": "...",
  "txid": 0,
  "error_severity": "ERROR",
  "state_code": "22012",
  "message": "division by zero",
  "application_name": "psql",
  "backend_type": "client backend",
  "query_id": 0,

  // feat-036 注入 (默认全开):
  "endpoint_id": "ep-mycluster",
  "branch_id":   "br-main",
  "project_id":  "proj-test",
  "trace_id":    "0123456789abcdef0123456789abcdef"   // optional · 业务 SQL 不带 SQLCommenter 时缺
}
```

## feat-037 v2 期望 (contract stub)

feat-037 v2 走 jsonlog 路径时, 期望 `cluster_neondb_logs` 这样:

```typescript
// openneon-mcp 仓 src/tools/cluster_neondb_logs.ts
async function cluster_neondb_logs(args: {
  endpoint_id: string;
  trace_id?: string;      // feat-036 提供, 给 v2 用; v1 阶段忽略
  since: string;
  pattern_strategy?: 'auto' | 'llm' | 'drain3';
}): Promise<{
  patterns: Array<{
    template: string;
    count: number;
    trace_ids: string[];  // feat-036 注入的 trace_id list
    sample: string;
    severity_top: 'ERROR' | 'WARNING' | 'LOG' | 'NOTICE' | 'FATAL';
  }>;
  coverage: number;       // 95%+ 覆盖率, 含 tail aggregate
  source_format: 'jsonlog' | 'stderr';  // v2='jsonlog', v1='stderr'
}>
```

**契约要点**:

1. `source_format='jsonlog'` 时, feat-036 必须保证 `trace_id` 字段存在 (空时缺字段而不是空字符串 — 让 feat-037 v2 用 missing key 而不是空字符串区分有/无)
2. 4 Neon 字段命名跟 OTel resource attribute 一致 (`endpoint_id` 不是 `endpointId`), feat-037 v2 直接当 OTel attribute 透传
3. GUC `neon.jsonlog_extra_fields` 关到空时, feat-037 v2 自动退回 v1 raw stderr 路径 (fallback 由 feat-037 实现, 不在 feat-036 scope)

## feat-037 v2 启用顺序 (staged delivery)

```
T+0 (now)         feat-037 v1 ship (raw stderr → LLM/Drain3 聚类) — 不依赖 feat-036
T+0..T+1          feat-036 ship (jsonlog 字段注入) — 默认开 GUC, jsonlog 输出
T+1               feat-037 v2 ship (jsonlog reader + trace_id filter)
                  · 联动 fixture case 6 真跑
```

design#50 §scope 已编码: "feat-036 不在 critical path · feat-037 先 ship v1
raw text". 当前 PR 进 feat-036 = T+0..T+1 阶段, **不阻塞** feat-037 v1.

## 联动测试落位

- 当前 feat-036 PR: 落 `pgxn/neon/test/fixtures/jsonlog_e2e.md` case 6 stub
  (本 md 链入 ↑)
- feat-037 v2 ship 时: 在 openneon-mcp 仓 e2e test `cluster_neondb_logs.feat-037-v2.e2e.test.ts`
  真跑, 联动 feat-036 jsonlog 输出. 跨仓 PR sequence:
  1. feat-036 ship (本 PR)
  2. feat-037 v1 ship (openneon-mcp 仓)
  3. feat-037 v2 ship (openneon-mcp 仓 · 把 source_format 切到 jsonlog)
