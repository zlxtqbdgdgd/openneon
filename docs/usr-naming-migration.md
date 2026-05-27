# USR 命名统一 · 迁移指南（feat-010）

> **BREAKING CHANGE**：openneon 4 组件（pageserver / safekeeper / compute_ctl / proxy）的
> telemetry 出口（Prometheus metric label / OpenTelemetry attribute / 日志字段）统一到
> cornerstone 定义的 `openneon.usr.*` 命名规范。已部署的 dashboard / alert / 日志脚本若依赖旧字段名，
> 需按本指南升级。

## 1. canonical 命名（4 组件统一）

| canonical 字段 | 出口组件 | 禁止使用的别名（CI grep 拦） |
|---|---|---|
| `openneon.usr.tenant_id` | 4 组件 | `tenant` / `tenantId` / `tenant_uuid` |
| `openneon.usr.timeline_id` | pageserver / safekeeper / compute | `timeline` / `timelineId` / `timeline_uuid` |
| `openneon.usr.endpoint_id` | compute / proxy | `endpoint` / `endpointId` / `endpoint_uuid` / `epId` |
| `openneon.usr.shard_id` | pageserver / safekeeper | `shard` / `shardId` / `shard_index` / `shard_num` |
| `openneon.usr.project_id` | compute / proxy | `project` / `projectId` / `project_uuid` |

- Prometheus label 用 underscore（`tenant_id` / `endpoint_id` 等），即 OTel attribute 去掉 `openneon.usr.` 前缀。
- `shard_id` 取值格式 `<index><count>` 4 char hex（如 `0204`；`0000` = unsharded），多 shard 逗号分隔。
- `service.name` / `service.version` / `deployment.environment` 是 OTel resource attribute，沿用 OTel spec，不加 `openneon.usr.` 前缀。

## 2. dashboard / alert 升级

### PromQL（metric label rename）

```sh
# 自建脚本 / alert 规则里的批量替换
sed -i 's/endpoint=/endpoint_id=/g' your_alerts.yml
```

```promql
# 旧聚合（按 endpoint）
sum(proxy_connections_total) by (endpoint)
# 新（canonical label）
sum(proxy_connections_total) by (endpoint_id)
```

### OTel collector（attribute rename）

若 collector 后端短期内还认旧 attribute 名，可在 collector 端加 `attributes` processor 做兼容重写：

```yaml
processors:
  attributes/usr_compat:
    actions:
      - key: endpoint_id
        from_attribute: openneon.usr.endpoint_id
        action: insert
      - key: tenant_id
        from_attribute: openneon.usr.tenant_id
        action: insert
```

### Datadog

Datadog dashboard / monitor 里把 tag key 从 `endpoint` 改为 `endpoint_id`，或在 ingestion pipeline
加 remapper 把 `openneon.usr.endpoint_id` 映射到既有 facet。

## 3. 紧急回退

升级 dashboard 期间，可临时设环境变量回到旧字段命名：

| env flag | 作用 |
|---|---|
| `OPENNEON_USR_NAMING_LEGACY=true` | 4 组件退化到上游 baseline 字段名（不注入 `openneon.usr.*`） |
| `OPENNEON_<COMPONENT>_USR_DISABLED=true` | 单组件禁用 USR layer（如 `OPENNEON_PROXY_USR_DISABLED`） |
| `OPENNEON_SAFEKEEPER_USR_DISABLED=true` | safekeeper shard_id 退化为 `0000`（feat-009） |

## 4. CI grep guard

`scripts/usr_naming_guard.sh`（由 `.github/workflows/usr-naming-guard.yml` 触发）会在 PR 上拦截
非 canonical 命名。确为误伤（struct field / FFI binding / vendored 代码）时，在该行加注释豁免：

```rust
let endpoint = ffi_get_endpoint(); // USR-LINT-IGNORE: 上游 FFI binding 字段名，非 telemetry 出口
```
