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
| `scripts/test/feat-011-fixture.sh` | 9 用例独立验证 fixture(不需真 dev server · 覆盖 class 1 WARN/2a/2b/3 + 豁免名 + 坏 YAML) |

## 治理模型:Datadog 式「保留集硬管 + 其余放行」

本检查对齐 **Datadog Unified Service Tagging** 的核心理念——**少而严的保留集强约束,
其余自定义标签自由发挥**。Datadog 只对 `service` / `env` / `version` 这三个钦定保留标签
强制规范,其余标签随便用;我们把同样的边界搬到 neon 的 tracing field 上:

- **受治理的「USR 保留身份标签」**(`endpoint` / `tenant` / `timeline` / `shard` / `project`)
  → **硬管(FAIL)**。这几个概念是跨组件 JOIN 的身份锚点,命名漂移会直接打断关联查询。
- **普通字段**(`status` / `url` / `user` / `value` / `waiters` ... 十万行老库的几百个既有字段)
  → **放行 + WARN**。不在白名单只是「未登记」,不是错误。绝不因此把存量库判红。
- **未注册 metric**(源码 emit 但不在 registry · `pageserver_*` / `compute_ctl_*` ... 几百个存量真实指标)
  → **放行 + WARN**。同普通字段一样,这是「未纳管」而非错误。

> 为什么这么改:旧版把「任何不在白名单的 tracing field」一律判 FAIL,等于要求先把
> neon 几百个既有合法字段全部 seed 进白名单,否则整库 CI 长红——守门员把「USR 保留身份
> 标签」和「普通日志字段」混为一谈。现在两者分开对待,硬拦只盯真正会造成跨组件漂移的保留身份。
>
> metric 也是同款毛病:neon 存量有几百个真实指标未登记进 registry,旧版把它们全判
> class 1 FAIL → CI 长红。现按 Datadog「保留集之外自由发挥」原则,未注册 metric 降为
> WARN 放行——只观测、不强制纳管。**如需强制新指标必须注册**,可后续做「diff-only 硬拦」
> (只对本 PR 新增/改动的 metric 强制要求注册,存量不动),不在本期范围。

## 违规分类(FAIL vs WARN 边界)

| 类别 | 触发 | 结果 |
|---|---|---|
| class 1 | 源码 emit 的 metric 不在 registry | **warn**(放行 · 未纳管 · 不阻断) |
| **class 2a** | **USR 保留身份标签的非规范写法漂移**(如 `endpoint_uuid` / `endpointId` / `tenant_uuid` / `tenantId` / `timeline_uuid` / `timelineId` / `shard_uuid` / `shardId` / `shardIndex`) | CI **fail** |
| **class 2b** | 普通 tracing field 不在 `tracing_known_fields` 白名单(非保留身份) | **warn**(放行 · 不阻断) |
| class 3 | registry 里某 metric 的 `required_tags_subset` 缺 USR 三件套(service/env/version) | CI **fail** |
| stale | registry 有但源码已删 | **warn**(允许 rollback 灵活性 · 后续 PR 清理) |
| class 4 | `audit_events` 某条 `required_attrs` 缺核心 attr | CI **fail** |

### class 2a 是模式判定,不是死名单

保留身份漂移用**正则模式**识别,而非逐个枚举错拼黑名单——任何新造的
`endpoint_xxx` / `tenantFoo` 漂移变体都会被同一条规则拦下:

```
概念词根 (endpoint|tenant|timeline|shard|project)
  + 非 _id 的身份后缀 (uuid|uid|id 驼峰|index|idx|num|guid|key)   → FAIL
规范形 <concept>_id(全小写、snake_case)                          → 合法,放行
豁免名 shard_index / shard_num(分片序号,非身份标识,§11 OQ5)     → 放行
```

判定大小写不敏感,既抓 snake_case 漂移(`endpoint_uuid`)也抓驼峰漂移(`endpointId` / `shardIndex`)。
扫描范围沿用「只扫 metric/tracing 宏出口」,不会误伤源码里普通的 `shard_index` / `ShardIndex` 局部变量。

## 开发者工作流(schema evolution)

新加 **metric** 时,**建议(非强制)同 PR 加 registry entry** 把它纳入治理。
未注册不阻断 CI,只出 class 1 WARN 提示:

```
改 pageserver/src/metrics.rs:
  + register_int_counter!("pageserver_layer_eviction_total", "...")
        ↓ 发 PR · CI 触发
  WARN · class 1 · 未注册 metric: pageserver_layer_eviction_total(放行 · 不阻断)
        ↓ (可选)同 PR 加 metric-registry.yaml entry 纳入治理
  - name: pageserver_layer_eviction_total
    component: pageserver
    type: counter
    unit: count
    required_tags_subset: [service, env, version, tenant_id, timeline_id]
    source_file: pageserver/src/metrics.rs
        ↓ push 同 PR · CI re-run
  PASS(WARN 清掉)· reviewer 在同份 diff 里看到「代码 + registry」1:1 对应
```

> 一旦 metric 被加进 registry 纳入治理,它的 `required_tags_subset` 就**必须含 USR 三件套**
> (service/env/version),否则 class 3 FAIL——纳管即受约束。未纳管的存量 metric 不受此约束。

**保留身份漂移**(`tenantId` 应为 `tenant_id` · `endpoint_uuid` 应为 `endpoint_id`)被 **class 2a**
硬拦,**改源码字段名**而非加 registry。**普通新字段**(如 `cache_state`)不会阻断 CI——只出 class 2b
WARN 提示,想纳入治理可选择性加到 `tracing_known_fields`(不强制)。

## 本地跑

```bash
# 在 repo root
bash scripts/check-metric-registry.sh        # 全量检查
bash scripts/test/feat-011-fixture.sh         # 跑 9 用例 fixture
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
