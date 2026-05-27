#!/usr/bin/env bash
# ============================================================================
# scripts/check-metric-registry.sh
#
# feat-011 · Metric Registry CI 检查(USR 套 + feat-031 audit schema 验证层)
#
# 在 openneon fork repo root 跑 · GitHub Actions workflow
# .github/workflows/metric-registry-check.yml 调用 · 本地开发也可直接跑。
#
# 把 4 组件源码(pageserver / safekeeper / compute_tools / proxy)实际 emit 的
# metric / tracing field · diff metric-registry.yaml 的期望集:
#   class 1 · 未注册 metric            → FAIL(exit 1)
#   class 2 · 未注册 tracing field     → FAIL(exit 1 · typo gate)
#   class 3 · metric 缺 USR 三件套     → FAIL(exit 1)
#   warn    · registry stale(源码已删) → WARN(不 fail · 允许 rollback 灵活性)
#   class 4(联动)· audit_events 缺核心 attr → FAIL(exit 1)
#
# 退出码:0 = pass · 非 0 = 至少一类 hard violation。
# 0 副作用:不改任何源文件 · 只读 + 临时目录。
#
# 依赖:ripgrep(rg)· yq(mikefarah/yq v4)· comm/sort/grep(coreutils)。
# OQ1:register_* 宏 / tracing 宏的 grep pattern 已按 2026-05 neon baseline 实测校准
#      (pageserver/src/metrics.rs 用 register_int_counter! / _vec! / register_uint_gauge! 等)。
# ============================================================================

set -euo pipefail

# ---- 定位 repo root(脚本在 scripts/ 下)----
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="${REGISTRY:-metric-registry.yaml}"

# 扫描的源码根(只扫 4 组件 · 跟 workflow paths filter 对齐)
SRC_DIRS=()
for d in pageserver/src safekeeper/src compute_tools/src proxy/src; do
  [[ -d "$d" ]] && SRC_DIRS+=("$d")
done
if [[ ${#SRC_DIRS[@]} -eq 0 ]]; then
  echo "FAIL · 未找到任何组件源码目录(pageserver/src 等)· 是否在 repo root?" >&2
  exit 2
fi

# ---- 依赖检查 ----
for bin in rg yq comm sort grep; do
  command -v "$bin" >/dev/null 2>&1 || { echo "FAIL · 缺依赖:$bin" >&2; exit 2; }
done
[[ -f "$REGISTRY" ]] || { echo "FAIL · 找不到 registry 文件:$REGISTRY" >&2; exit 2; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

FAILED=0   # 累计 hard violation · 全程不提前退出 · 一次报全所有问题

# ---- 第 0 步:registry schema 版本兼容 ----
REG_VERSION="$(yq -r '.version' "$REGISTRY")"
if [[ "$REG_VERSION" != "1" ]]; then
  echo "FAIL · 不支持的 registry schema version: '$REG_VERSION'(本 CI script 仅支持 version 1)"
  echo "       升级 schema 请同 PR 改 check-metric-registry.sh 兼容判定"
  FAILED=1
fi

# ---- 第 1 步:从 registry 抽 expected 集合 ----
yq -r '.metrics[].name' "$REGISTRY" | grep -v '^null$' | sort -u > "$WORKDIR/expected_metrics.txt"

# 合法 tracing field = tracing_known_fields ∪ required_tags ∪ neon_specific_tags
{
  yq -r '.tracing_known_fields[]' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
  yq -r '.required_tags[].name' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
  yq -r '.neon_specific_tags[].name' "$REGISTRY" 2>/dev/null | grep -v '^null$' || true
} | sort -u > "$WORKDIR/known_fields.txt"

# ---- 第 2 步:grep 源码实际 emit 的 metric name ----
# register_* 宏后第一参数是 metric name(snake_case 字符串) · 调用常跨多行:
#     register_int_counter!(
#         "pageserver_xxx_total",
#         "help text ..."
#     )
# 用 ripgrep multiline 模式:匹配 register 宏 · 非贪婪跨到第一个字符串字面量。
RG_REGISTER='register_(int_|uint_|float_)?(counter|gauge|histogram)(_vec|_pair)?!\s*\(\s*\n?\s*"[a-z_][a-z0-9_]*"'

rg --no-heading --no-line-number --no-filename -U -o -e "$RG_REGISTER" "${SRC_DIRS[@]}" 2>/dev/null \
  | grep -oE '"[a-z_][a-z0-9_]*"[^"]*$' \
  | grep -oE '"[a-z_][a-z0-9_]*"' \
  | tr -d '"' \
  | sort -u > "$WORKDIR/actual_metrics.txt" || true

# class 1 · actual - expected = 未注册 metric
UNREGISTERED="$(comm -23 "$WORKDIR/actual_metrics.txt" "$WORKDIR/expected_metrics.txt" || true)"
if [[ -n "$UNREGISTERED" ]]; then
  echo "FAIL · class 1 · 发现未注册 metric(需同 PR 在 $REGISTRY 加 entry):"
  echo "$UNREGISTERED" | sed 's/^/    - /'
  echo "    Hint: 加 metrics: 列表项 · 含 name / component / type / unit / required_tags_subset(必含 service,env,version)/ source_file"
  echo ""
  FAILED=1
fi

# warn · expected - actual = registry stale(源码已删但 registry 未跟进)
# 仅当源码扫到了至少一条 metric 时才比对(否则 grep pattern 未命中会误报全 stale)
if [[ -s "$WORKDIR/actual_metrics.txt" ]]; then
  STALE="$(comm -13 "$WORKDIR/actual_metrics.txt" "$WORKDIR/expected_metrics.txt" || true)"
  if [[ -n "$STALE" ]]; then
    echo "WARN · registry 中 metric 已不在源码(stale · 允许 rollback · 建议后续 PR 清理):"
    echo "$STALE" | sed 's/^/    - /'
    echo ""
  fi
fi

# ---- 第 3 步:grep tracing / audit 宏里的结构化 field name ----
# tracing::info!(field = value, ...) / info!(field, ...) / audit_event!(field = ...)
# 抽出 `ident =` 形式的 field key(排除比较运算 ==)。
RG_TRACING='(tracing::)?(info|warn|error|debug|trace)!|audit_event!'

rg --no-heading --no-line-number --no-filename -e "$RG_TRACING" "${SRC_DIRS[@]}" 2>/dev/null \
  | grep -oE '[a-z_][a-z0-9_]*[[:space:]]*=[^=]' \
  | grep -oE '^[a-z_][a-z0-9_]*' \
  | sort -u > "$WORKDIR/actual_fields_raw.txt" || true

# 过滤掉明显的非-telemetry 噪声关键字(let/const 等不会出现在此 pattern · 保险起见去常见噪声)
grep -vxE '(let|mut|const|if|else|for|while|match|return|self)' "$WORKDIR/actual_fields_raw.txt" \
  > "$WORKDIR/actual_fields.txt" || true

# class 2 · actual fields - known = 未注册 field(typo gate)
UNKNOWN_FIELDS="$(comm -23 "$WORKDIR/actual_fields.txt" "$WORKDIR/known_fields.txt" || true)"
if [[ -n "$UNKNOWN_FIELDS" ]]; then
  echo "FAIL · class 2 · 发现未注册 tracing field(疑似拼写错或新字段未声明 schema):"
  echo "$UNKNOWN_FIELDS" | sed 's/^/    - /'
  echo "    Hint: 若是合法新字段 · 加到 $REGISTRY 的 tracing_known_fields"
  echo "          若是拼写错(如 endpoint_uuid 应为 endpoint_id · tenantId 应为 tenant_id)· 改源码"
  echo ""
  FAILED=1
fi

# ---- 第 4 步:每条 metric 必须含 USR 三件套(service / env / version)----
while IFS=$'\t' read -r metric tags; do
  [[ -z "$metric" || "$metric" == "null" ]] && continue
  for usr in service env version; do
    case ",$tags," in
      *",$usr,"*) ;;
      *)
        echo "FAIL · class 3 · metric '$metric' 缺 USR 三件套字段 '$usr'(required_tags_subset 必含 service/env/version)"
        FAILED=1
        ;;
    esac
  done
done < <(yq -r '.metrics[] | [.name, (.required_tags_subset // [] | join(","))] | @tsv' "$REGISTRY")

# ---- 第 5 步:audit_events 联动一致性(feat-031 · cross-mcp 同源)----
# neon 仓本地校验:每条 audit_event 的 required_attrs 必须含核心 attribute。
# (现阶段源码无 audit_event! emit · 不做 actual-vs-expected diff · 仅锁 schema 完整性)
AUDIT_CORE=(openneon.audit.event_type openneon.audit.outcome)
AUDIT_COUNT="$(yq -r '.audit_events | length' "$REGISTRY")"
if [[ "$AUDIT_COUNT" != "0" && "$AUDIT_COUNT" != "null" ]]; then
  for ((i = 0; i < AUDIT_COUNT; i++)); do
    ev="$(yq -r ".audit_events[$i].event_type" "$REGISTRY")"
    yq -r ".audit_events[$i].required_attrs[]" "$REGISTRY" | sort -u > "$WORKDIR/audit_req_$i.txt"
    for must in "${AUDIT_CORE[@]}"; do
      if ! grep -qxF "$must" "$WORKDIR/audit_req_$i.txt"; then
        echo "FAIL · class 4 · audit_event '$ev' 的 required_attrs 缺核心 attribute: $must"
        FAILED=1
      fi
    done
  done
fi

# ---- 汇总 ----
echo "----------------------------------------------------------------------"
if [[ "$FAILED" -ne 0 ]]; then
  echo "FAIL · metric-registry.yaml 跟源码 / schema 存在不一致(见上方违规清单)"
  exit 1
fi
echo "PASS · metric-registry.yaml 跟源码一致(扫描组件:${SRC_DIRS[*]})"
exit 0
