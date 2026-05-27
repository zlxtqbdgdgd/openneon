# Metric Registry + CI 检查(feat-011)

USR 套(feat-008/009/010)与 feat-031 audit schema 的**机械化验证层**:用一份
`metric-registry.yaml`(单一事实表)+ 一个 CI 检查锁住 4 组件(pageserver /
safekeeper / compute / proxy)的 metric / tracing field / audit event schema,
防止任何开发者一手抖手造成跨组件命名漂移(schema drift)。

## 文件

| 文件 | 作用 |
|---|---|
| `metric-registry.yaml`(repo root) | 单一事实表:metric name + 必填 tag schema + audit event attr |
| `scripts/check-metric-registry.sh` | CI 检查脚本(ripgrep + yq + bash · 0 副作用) |
| `.github/workflows/metric-registry-check.yml` | GitHub Actions PR gate(paths filter 限定触发) |
| `scripts/test/feat-011-fixture.sh` | 4 用例独立验证 fixture(不需真 dev server) |

## 4 类违规

| 类别 | 触发 | 结果 |
|---|---|---|
| class 1 | 源码 emit 的 metric 不在 registry | CI **fail** |
| class 2 | tracing field 不在 known 集(typo gate · 如 `endpoint_uuid` / `tenantId`) | CI **fail** |
| class 3 | registry 里某 metric 的 `required_tags_subset` 缺 USR 三件套(service/env/version) | CI **fail** |
| stale | registry 有但源码已删 | **warn**(允许 rollback 灵活性 · 后续 PR 清理) |
| class 4 | `audit_events` 某条 `required_attrs` 缺核心 attr | CI **fail** |

## 开发者工作流(schema evolution)

新加 metric / tracing field 时**必须同 PR 改 registry**,否则 CI fail:

```
改 pageserver/src/metrics.rs:
  + register_int_counter!("pageserver_layer_eviction_total", "...")
        ↓ 发 PR · CI 触发
  FAIL · class 1 · 未注册 metric: pageserver_layer_eviction_total
        ↓ 同 PR 加 metric-registry.yaml entry
  - name: pageserver_layer_eviction_total
    component: pageserver
    type: counter
    unit: count
    required_tags_subset: [service, env, version, tenant_id, timeline_id]
    source_file: pageserver/src/metrics.rs
        ↓ push 同 PR · CI re-run
  PASS · reviewer 在同份 diff 里看到「代码 + registry」1:1 对应
```

field 拼写错(`tenantId` 应为 `tenant_id`)同理被 class 2 拦下,改源码而非加 registry。

## 本地跑

```bash
# 在 repo root
bash scripts/check-metric-registry.sh        # 全量检查
bash scripts/test/feat-011-fixture.sh         # 跑 4 用例 fixture
```

依赖:`ripgrep`(neon CI 已用)、`yq`(mikefarah v4)。

## schema 版本

- `version: 1` 首版。增量加 metric / tag 不算 breaking。
- 改顶层结构(加/删 top-level key)需 bump version,并同 PR 改
  `check-metric-registry.sh` 的兼容判定。
- 删 metric / 改 `required_tags_subset` = breaking,走 §8 回滚兼容窗口。

## 跨仓 audit schema(§11 OQ3)

`audit_events` 节跟 `openneon-mcp` 仓的 `audit-registry.yaml` 副本同源(feat-031)。
两仓各自 CI 检查自己仓内的 audit emission,**不跨仓 fetch**;cross-repo drift 通过
手动 grep + PR 同步(L4 期再自动化)。

## 回滚

- 紧急 unblock:`.github/workflows/metric-registry-check.yml` 设 `if: false` 或临时删
  (schema drift 风险回归 · 必须配 issue 跟踪)。
- CI 是软门:admin 可强制 merge 绕过,但 commit history 留痕,review 期可发现 + revert。
