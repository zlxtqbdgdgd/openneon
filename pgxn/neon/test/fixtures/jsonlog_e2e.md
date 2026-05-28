# feat-036 · PG jsonlog + Neon 字段注入 · e2e 6 case fixture

> 详设: zlxtqbdgdgd/openneon-design#50 · feat-036-L3-neon-compute-jsonlog.html §3.4 + §7
>
> 这份 fixture 是**剧本式**契约描述, 不是 pg_regress 跑批; 对应 `make check-jsonlog-ext`
> 已覆盖的 unit-level 拼接 + escape + mask 解析 21/21 PASS; 本文件描述**真 PG 启动 +
> 真 jsonlog 文件输出 + 真 OTel 桥** 这一层的 6 个验证场景, B 线 buffer 期内在 dev
> server (epyc-256c.e8.luyouxia.net) 跑全编 + 启动 PG 时同步落实.
>
> 命名跟设计稿对齐: `feat-036-jsonlog-neon-fields-fixture.test.rs` (Rust harness) +
> `feat-036-jsonlog-c-hook-fixture.c` (C 端 PG regress · 走 vendor patch 后).

---

## 前置准备

```bash
# 1. apply vendor patch (一次性, fork 已 ship)
cd vendor/postgres-v17 && patch -p1 < ../../pgxn/neon/vendor-patches/feat-036-jsonlog-hook.patch
cd vendor/postgres-v16 && patch -p1 < ../../pgxn/neon/vendor-patches/feat-036-jsonlog-hook.patch
cd vendor/postgres-v15 && patch -p1 < ../../pgxn/neon/vendor-patches/feat-036-jsonlog-hook.patch
# NOTE: v14 不含 jsonlog.c (PG15+ native feature). v14 编译时跳过 patch.

# 2. neon_local 起一个最小 cluster + compute
neon_local init
neon_local pageserver start && neon_local safekeeper start
neon_local endpoint create main --pg-version 17
neon_local endpoint start main

# 3. 把 USR GUC 与 jsonlog 配上 (compute_tools 会自动写入, 这里只列预期)
cat <<EOF | psql -p 55432
SELECT current_setting('neon.endpoint_id');   -- ep-mycluster
SELECT current_setting('neon.branch_id');     -- br-main
SELECT current_setting('neon.project_id');    -- proj-test
SELECT current_setting('log_destination');    -- jsonlog
SELECT current_setting('neon.jsonlog_extra_fields'); -- endpoint_id,branch_id,project_id,trace_id
EOF
```

---

## case 1 · 上游 PG 18 字段全在 (raw jsonlog baseline)

**目的**: 验 PG 15+ 原生 jsonlog 启用后, **上游 18 个标准字段**全数输出, vendor patch
**没破坏 backward compat** (hook 默认 NULL 时, write_jsonlog 末尾 '}' 之前不调 hook).

**注**: case 1 与 case 2 区别是, case 1 模拟"Neon extension 没加载 / mask=0", 走的
是 PG 上游纯净行为, 输出**只有 PG 18 字段** + 不含任何 Neon 字段.

**做**:

```sql
ALTER SYSTEM SET neon.jsonlog_extra_fields = '';
SELECT pg_reload_conf();
SELECT 1/0;  -- 触发 ERROR
```

**期望**:

输出 `~/.neon/endpoints/main/log/postgresql-*.log` 中找到一条 LOG, JSON 解析后:

```json
{
  "timestamp": "2026-05-28 12:00:00.000 CST",
  "user": "cloud_admin",
  "dbname": "postgres",
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
  "query_id": 0
}
```

JSON key 集合 **不含** `endpoint_id` / `branch_id` / `project_id` / `trace_id`.

---

## case 2 · Neon 字段注入 (USR 全局结构有效时)

**目的**: 验 GUC 默认 (`endpoint_id,branch_id,project_id,trace_id`) 全开时, Neon 字段
出现在 jsonlog **同一对象** 末尾, 紧贴上游 18 字段之后, **逗号顺序正确**.

**做**:

```sql
ALTER SYSTEM SET neon.jsonlog_extra_fields = 'endpoint_id,branch_id,project_id,trace_id';
SELECT pg_reload_conf();
SELECT 1/0;
```

**期望**:

```json
{
  ...(18 PG 字段)...,
  "query_id": 0,
  "endpoint_id": "ep-mycluster",
  "branch_id": "br-main",
  "project_id": "proj-test"
}
```

注意: case 2 不包含 `trace_id`, 因为本场景没业务 trace_id 注入 (path α 不触发).
trace_id 单独看 case 3.

---

## case 3 · trace_id 注入 (PgBackendStatus.trace_context 有 trace_id 时输出)

**目的**: 验 path α SQLCommenter 来的 traceparent → backend slot → jsonlog trace_id
端到端串接.

**做**: 业务 SQL 带 SQLCommenter (feat-034 path α 已 ship):

```sql
SELECT 1/0 /*traceparent='00-0123456789abcdef0123456789abcdef-aabbccddeeff0011-01',tracestate='neon=app'*/;
```

**期望**: 同一条 ERROR jsonlog 输出:

```json
{
  ...,
  "query_id": 0,
  "endpoint_id": "ep-mycluster",
  "branch_id": "br-main",
  "project_id": "proj-test",
  "trace_id": "0123456789abcdef0123456789abcdef"
}
```

**反例**: 同 case 3 但**不带** SQLCommenter, `trace_id` 字段不出现 (符合 issue body
"无时 null" — 我们采用更严的 "无时 缺字段", 因 jsonlog 字段是可选的; agent 用
`COALESCE(trace_id, 'untraced')` 兜底).

---

## case 4 · GUC `neon.jsonlog_extra_fields=` 空 → 退化到上游 PG jsonlog

**目的**: 验 GUC 设空后, **彻底退化**到 case 1 行为. 给运维一个"出了事先关 Neon
字段"的快开关.

**做**:

```sql
ALTER SYSTEM SET neon.jsonlog_extra_fields = '';
SELECT pg_reload_conf();
SELECT 1/0;
```

**期望**: 同 case 1 输出 (18 PG 字段, 0 Neon 字段).

**附加子 case**: 部分白名单:

```sql
ALTER SYSTEM SET neon.jsonlog_extra_fields = 'trace_id';
SELECT pg_reload_conf();
SELECT 1/0 /*traceparent='00-aaaa...-bbbb-01'*/;
```

期望: 18 PG 字段 + **只有** `trace_id`, 不含 `endpoint_id` / `branch_id` / `project_id`.

---

## case 5 · feat-031 OTel 集成

**目的**: 验 feat-031 既有 OTel exporter (`compute_tools/src/logger.rs::init_tracing_and_logging`)
读 jsonlog stdout 后, **自动把每条 jsonlog event 转 OTel event**,
**Neon 4 字段一致** 经由 OTel attributes 透传.

**做**:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
neon_local endpoint restart main --remote-extensions=otel-collector

# 触发 case 3 同款 query
psql -p 55432 -c "SELECT 1/0 /*traceparent='00-aabb...-ccdd-01'*/;"

# OTel collector 收 (mock collector / jaeger)
otelctl pull-events --service=neon.compute.pglog --since=1m | grep ERROR
```

**期望**: OTel event log payload 含:

```yaml
attributes:
  service.name: neon.compute.pglog   # 跟 feat-031 audit 用的 neon.compute.audit 区分
  endpoint_id: ep-mycluster
  branch_id: br-main
  project_id: proj-test
  trace_id: aabb...                  # 跟 SpanContext.trace_id 对齐
  log.severity: ERROR
  log.message: "division by zero"
```

**反例**: 不开 OTel exporter (`OTEL_EXPORTER_OTLP_ENDPOINT` 未设),
本 case 自动 skip — jsonlog 文件输出**仍按 case 3 那样有 4 字段**, 这是 feat-036 的
最小可用形态; feat-031 wired 后再启用 OTel routing 这一段.

---

## case 6 · mcp e2e 联动 feat-037 v2 阶段 · cluster_neondb_logs 按 trace_id filter

**目的**: 验 feat-037 v2 启用 `cluster_neondb_logs` mcp tool 后, 按 trace_id 拉聚类
**走的是 jsonlog 字段**而不是 raw stderr regex (这是 feat-036 的核心 v2 收益).

**协调**: 跟 openneon-mcp 仓 feat-037 fixture 协调 (case 6 contract 在 openneon-design#51).

**做** (openneon-mcp 仓 e2e 测试 stub · 等 feat-037 v2 ship 之前先打 stub):

```typescript
// openneon-mcp/tests/cluster_neondb_logs.feat-037-v2.e2e.test.ts (stub)
describe('cluster_neondb_logs · feat-036 jsonlog v2', () => {
  it('按 trace_id filter (v2 jsonlog 路径)', async () => {
    // 准备: 3 条 jsonlog event, 共享同 trace_id, 不同 message
    const trace_id = '0123456789abcdef0123456789abcdef';
    await injectJsonlogFixture([
      { trace_id, error_severity: 'ERROR', message: 'connection refused' },
      { trace_id, error_severity: 'ERROR', message: 'connection refused' },
      { trace_id, error_severity: 'WARNING', message: 'slow query' },
    ]);

    const out = await mcp.callTool('cluster_neondb_logs', {
      endpoint_id: 'ep-mycluster',
      trace_id,
      since: '1m',
    });

    // feat-037 v2 期望 (contract stub · 等 feat-037 ship 后 e2e 实跑):
    expect(out.patterns).toHaveLength(2);
    expect(out.patterns[0]).toMatchObject({
      template: 'connection refused',
      count: 2,
      trace_ids: [trace_id],
    });
    expect(out.coverage).toBeGreaterThanOrEqual(0.95);
  });
});
```

**说明**: 本 case 6 是 feat-037 v2 阶段才真跑; 当前 feat-036 PR 阶段, 把这段 stub
进 fixture md, 等 feat-037 v2 ship 时联动 e2e 实跑. feat-036 的 PR 本身**不**
等 feat-037 — staged delivery 设计 (design issue #50 §scope "feat-036 不在 critical
path · feat-037 先 ship v1 raw text").

---

## 验证状态

| case | 验证位置 | 状态 |
|---|---|---|
| 1 上游 18 字段 | dev server PG17 启动验 jsonlog 输出 | B 线 buffer 跑 |
| 2 Neon 字段注入 | 同上 | B 线 buffer 跑 |
| 3 trace_id 注入 | 依赖 feat-034 B1 #49 (path α) merge | B 线 buffer 跑 (B1 contract stub) |
| 4 GUC 空 → 退化 | 同 case 1 | B 线 buffer 跑 |
| 5 OTel 集成 | 依赖 feat-031 既有 OTel exporter (已 ship) | B 线 buffer 跑 |
| 6 mcp e2e | 依赖 feat-037 v2 ship | stub 落 fixture · 等 feat-037 v2 联动 |

**当前 PR 内立即 PASS**:
- `make check-jsonlog-ext` standalone 21/21 (hook 拼接 + escape + mask 解析逻辑)

**B 线 buffer 期内补**:
- vendor patch apply + PG 全编 + jsonlog 真输出 (case 1-4)
- feat-031 OTel exporter 路由 (case 5)

**feat-037 v2 联动**:
- mcp e2e (case 6)
