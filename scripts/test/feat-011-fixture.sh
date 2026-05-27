#!/usr/bin/env bash
# ============================================================================
# scripts/test/feat-011-fixture.sh
#
# feat-011 验证 fixture · 独立端到端(详设 §7)。
# 不需要真 dev server:在临时目录 mock 一个最小 repo(几个 .rs + 一份
# metric-registry.yaml)· 跑 scripts/check-metric-registry.sh · 断言退出码 + stdout。
#
# 4 用例:
#   1. 合规 metric                 → exit 0 · stdout 含 "PASS"
#   2. 未注册 tracing field(typo) → exit 1 · stdout 含 "class 2" + 该 field
#   3. metric name 拼写错(typo)   → exit 1 · stdout 含 "class 1" + 该 metric
#   4. metric 缺 USR 三件套        → exit 1 · stdout 含 "class 3" + service
#
# 用法:bash scripts/test/feat-011-fixture.sh
# 退出码:0 = 4/4 通过 · 非 0 = 有用例失败。
# ============================================================================

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHECKER="$REPO_ROOT/scripts/check-metric-registry.sh"

[[ -f "$CHECKER" ]] || { echo "找不到被测脚本:$CHECKER" >&2; exit 2; }

PASS_COUNT=0
FAIL_COUNT=0

# --- 在一个隔离的 mock repo 里跑 checker ---
# 参数:$1 mock repo 目录
run_checker() {
  local repo="$1"
  ( cd "$repo" && REGISTRY=metric-registry.yaml bash "$CHECKER" ) 2>&1
}

# 断言:退出码 + stdout 子串
# 参数:用例名 / 期望退出码(0|nonzero)/ 实际退出码 / 实际输出 / 期望子串...
assert_case() {
  local name="$1"; shift
  local want_exit="$1"; shift
  local got_exit="$1"; shift
  local out="$1"; shift
  local ok=1

  if [[ "$want_exit" == "0" ]]; then
    [[ "$got_exit" -eq 0 ]] || { ok=0; echo "  [x] 期望 exit 0 · 实际 $got_exit"; }
  else
    [[ "$got_exit" -ne 0 ]] || { ok=0; echo "  [x] 期望 exit 非 0 · 实际 0"; }
  fi
  for needle in "$@"; do
    if ! grep -qF -- "$needle" <<<"$out"; then
      ok=0
      echo "  [x] stdout 缺子串:$needle"
    fi
  done

  if [[ "$ok" -eq 1 ]]; then
    echo "PASS · $name"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    echo "FAIL · $name"
    echo "------ 实际输出 ------"
    echo "$out" | sed 's/^/    /'
    echo "---------------------"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

# mock repo 骨架:建 4 组件 src 目录 + 一份 registry
# 参数:$1 目标目录
scaffold() {
  local repo="$1"
  mkdir -p "$repo/pageserver/src" "$repo/safekeeper/src" \
           "$repo/compute_tools/src" "$repo/proxy/src"
}

# 写一份「合规」registry(含 foo_total + USR 三件套 + 必要 field)
write_good_registry() {
  cat > "$1/metric-registry.yaml" <<'YAML'
version: 1
required_tags:
  - name: service
    type: enum
    values: [neon-pageserver]
  - name: env
    type: string
  - name: version
    type: string
neon_specific_tags:
  - name: tenant_id
    type: string
    components: [pageserver]
  - name: endpoint_id
    type: string
    components: [pageserver]
metrics:
  - name: foo_total
    component: pageserver
    type: counter
    unit: requests
    required_tags_subset: [service, env, version, tenant_id]
    source_file: pageserver/src/metrics.rs
tracing_known_fields:
  - lsn
audit_events:
  - event_type: ddl_executed
    required_attrs:
      - openneon.audit.event_type
      - openneon.audit.outcome
    optional_attrs: []
    component: pageserver
YAML
}

# ==========================================================================
# 用例 1 · 合规 metric → PASS
# ==========================================================================
R1="$(mktemp -d)"; scaffold "$R1"; write_good_registry "$R1"
cat > "$R1/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "foo_total",
        "help text"
    ).unwrap()
});
fn emit() {
    tracing::info!(lsn = 42, "wal applied");
}
RS
OUT1="$(run_checker "$R1")"; EXIT1=$?
assert_case "用例1 合规 metric 通过" 0 "$EXIT1" "$OUT1" "PASS"
rm -rf "$R1"

# ==========================================================================
# 用例 2 · 未注册 tracing field(endpoint_uuid 应为 endpoint_id)→ FAIL class 2
# ==========================================================================
R2="$(mktemp -d)"; scaffold "$R2"; write_good_registry "$R2"
cat > "$R2/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
fn emit() {
    tracing::info!(endpoint_uuid = "ep-1", "got request");
}
RS
OUT2="$(run_checker "$R2")"; EXIT2=$?
assert_case "用例2 未注册 tracing field CI fail" nonzero "$EXIT2" "$OUT2" "class 2" "endpoint_uuid"
rm -rf "$R2"

# ==========================================================================
# 用例 3 · metric name 拼写错(foo_ttoal)→ FAIL class 1
# ==========================================================================
R3="$(mktemp -d)"; scaffold "$R3"; write_good_registry "$R3"
cat > "$R3/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "foo_ttoal",
        "typo: should be foo_total"
    ).unwrap()
});
RS
OUT3="$(run_checker "$R3")"; EXIT3=$?
assert_case "用例3 拼写错 typo CI fail" nonzero "$EXIT3" "$OUT3" "class 1" "foo_ttoal"
rm -rf "$R3"

# ==========================================================================
# 用例 4 · metric 缺 USR 三件套(registry 里 bar_total 只填 tenant_id)→ FAIL class 3
# ==========================================================================
R4="$(mktemp -d)"; scaffold "$R4"
cat > "$R4/metric-registry.yaml" <<'YAML'
version: 1
required_tags:
  - name: service
    type: enum
    values: [neon-pageserver]
  - name: env
    type: string
  - name: version
    type: string
neon_specific_tags:
  - name: tenant_id
    type: string
    components: [pageserver]
metrics:
  - name: bar_total
    component: pageserver
    type: counter
    unit: requests
    required_tags_subset: [tenant_id]
    source_file: pageserver/src/metrics.rs
tracing_known_fields: []
audit_events:
  - event_type: ddl_executed
    required_attrs:
      - openneon.audit.event_type
      - openneon.audit.outcome
    optional_attrs: []
    component: pageserver
YAML
# 源码也 emit bar_total 以免 class 1 先触发(本用例只想验 class 3)
cat > "$R4/pageserver/src/metrics.rs" <<'RS'
pub static BAR: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("bar_total", "help").unwrap()
});
RS
OUT4="$(run_checker "$R4")"; EXIT4=$?
assert_case "用例4 缺 USR 三件套 CI fail" nonzero "$EXIT4" "$OUT4" "class 3" "service"
rm -rf "$R4"

# ==========================================================================
echo "======================================================================"
echo "结果:PASS=$PASS_COUNT · FAIL=$FAIL_COUNT(共 4 用例)"
if [[ "$FAIL_COUNT" -ne 0 ]]; then
  exit 1
fi
echo "全部用例通过"
exit 0
