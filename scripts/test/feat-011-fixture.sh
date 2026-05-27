#!/usr/bin/env bash
# ============================================================================
# scripts/test/feat-011-fixture.sh
#
# feat-011 验证 fixture · 独立端到端(详设 §7)。
# 不需要真 dev server:在临时目录 mock 一个最小 repo(几个 .rs + 一份
# metric-registry.yaml)· 跑 scripts/check-metric-registry.sh · 断言退出码 + stdout。
#
# 治理模型(Datadog 式:保留身份标签硬管 + 普通字段放行 WARN)下的用例:
#   1. 合规 metric                        → exit 0 · stdout 含 "PASS"
#   2. 普通未注册字段(status/url/value)  → exit 0(只 WARN class 2b · 整体仍 PASS)
#   3. 保留身份漂移 endpoint_uuid          → exit 1 · stdout 含 "class 2a" + endpoint_uuid
#   4. 未注册 metric(name 拼写错)         → exit 0(只 WARN class 1 · 整体仍 PASS)
#   5. metric 缺 USR 三件套                → exit 1 · stdout 含 "class 3" + service
#   6. 保留身份驼峰漂移 shardId            → exit 1 · stdout 含 "class 2a" + shardId
#   7. 带注释 / 多行的 register 宏 typo    → exit 0(只 WARN class 1 · 整体仍 PASS)
#   8. registry YAML 格式错                → exit 1(不得静默 PASS)
#
# 用法:bash scripts/test/feat-011-fixture.sh
# 退出码:0 = 全部通过 · 非 0 = 有用例失败。
# ============================================================================

# set -e:heredoc / mkdir / cat 写入失败立即 abort · 避免在残缺 mock YAML 上误判 PASS。
set -euo pipefail

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
# set -e 下 command substitution 失败会中断脚本 · 用 || 捕获退出码而不触发 abort。
OUT1="$(run_checker "$R1")" && EXIT1=0 || EXIT1=$?
assert_case "用例1 合规 metric 通过" 0 "$EXIT1" "$OUT1" "PASS"
rm -rf "$R1"

# ==========================================================================
# 用例 2 · 普通未注册字段(status / url / value 等存量字段)→ 只 WARN · 整体 PASS
#   Datadog 式治理:保留集之外的普通字段放行 · 不阻断。十万行老库的几百个既有
#   合法字段不该把 CI 判红。期望:exit 0 · stdout 含 "PASS" 且含 "class 2b"(WARN)。
# ==========================================================================
R2="$(mktemp -d)"; scaffold "$R2"; write_good_registry "$R2"
cat > "$R2/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
fn emit() {
    // status / url / value 都是普通业务字段 · 不在 tracing_known_fields · 但非保留身份
    tracing::info!(status = 200, url = "/x", value = 42, "got request");
}
RS
OUT2="$(run_checker "$R2")" && EXIT2=0 || EXIT2=$?
assert_case "用例2 普通未注册字段只 WARN 整体 PASS" 0 "$EXIT2" "$OUT2" "PASS" "class 2b" "status"
rm -rf "$R2"

# ==========================================================================
# 用例 3 · 保留身份标签漂移(endpoint_uuid 应为 endpoint_id)→ FAIL class 2a
#   endpoint 是 USR 钦定保留身份 · 非 _id 规范形一律硬拦。
# ==========================================================================
R3a="$(mktemp -d)"; scaffold "$R3a"; write_good_registry "$R3a"
cat > "$R3a/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
fn emit() {
    tracing::info!(endpoint_uuid = "ep-1", "got request");
}
RS
OUT3a="$(run_checker "$R3a")" && EXIT3a=0 || EXIT3a=$?
assert_case "用例3 保留身份漂移 endpoint_uuid CI fail" nonzero "$EXIT3a" "$OUT3a" "class 2a" "endpoint_uuid"
rm -rf "$R3a"

# ==========================================================================
# 用例 4 · 未注册 metric(foo_ttoal · name 拼写错)→ 只 WARN class 1 · 整体 PASS
#   Datadog 式治理:未注册 metric 是「未纳管」而非错误 · 放行不阻断。
#   期望:exit 0 · stdout 含 "PASS" 且含 "class 1" + foo_ttoal(WARN 行)。
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
OUT3="$(run_checker "$R3")" && EXIT3=0 || EXIT3=$?
assert_case "用例4 未注册 metric(拼写错)只 WARN 整体 PASS" 0 "$EXIT3" "$OUT3" "PASS" "class 1" "foo_ttoal"
rm -rf "$R3"

# ==========================================================================
# 用例 5 · metric 缺 USR 三件套(registry 里 bar_total 只填 tenant_id)→ FAIL class 3
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
OUT4="$(run_checker "$R4")" && EXIT4=0 || EXIT4=$?
assert_case "用例5 缺 USR 三件套 CI fail" nonzero "$EXIT4" "$OUT4" "class 3" "service"
rm -rf "$R4"

# ==========================================================================
# 用例 6 · 保留身份驼峰漂移(shardId 应为 shard_id)→ FAIL class 2a
#   覆盖:① 驼峰大小写漂移要被 mixedcase 抽取抓到 · ② shardId 与豁免名 shard_index
#   不冲突(shardId 仍 FAIL · shard_index 才豁免)。同时用简写形式验证简写路径。
# ==========================================================================
R5="$(mktemp -d)"; scaffold "$R5"; write_good_registry "$R5"
cat > "$R5/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
fn emit() {
    // 驼峰漂移:shardId 应为规范的 shard_id
    tracing::info!(shardId = 3, "request handled");
}
RS
OUT5="$(run_checker "$R5")" && EXIT5=0 || EXIT5=$?
assert_case "用例6 保留身份驼峰漂移 shardId CI fail" nonzero "$EXIT5" "$OUT5" "class 2a" "shardId"
rm -rf "$R5"

# ==========================================================================
# 用例 6b · 豁免名 shard_index → 不得 FAIL(分片序号是合法既有字段 · §11 OQ5)
#   保护性断言:shard_index 只能走 WARN(若未注册)· 绝不被 class 2a 误伤为漂移。
# ==========================================================================
R5b="$(mktemp -d)"; scaffold "$R5b"; write_good_registry "$R5b"
cat > "$R5b/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
fn emit() {
    tracing::info!(shard_index = 3, "request handled");
}
RS
OUT5b="$(run_checker "$R5b")" && EXIT5b=0 || EXIT5b=$?
assert_case "用例6b 豁免名 shard_index 不 FAIL(整体 PASS)" 0 "$EXIT5b" "$OUT5b" "PASS"
rm -rf "$R5b"

# ==========================================================================
# 用例 7 · 带注释 / 多行的 register 宏 metric typo → 只 WARN class 1 · 整体 PASS
#   覆盖修复 1:宏调用与 metric 名之间夹行注释 / 多行空白也要抽到(抽取能力不变);
#   抽到的未注册 metric 现按新语义只 WARN 不 fail。
#   期望:exit 0 · stdout 含 "PASS" 且含 "class 1" + foo_ttoal(证明确实抽到了)。
# ==========================================================================
R6="$(mktemp -d)"; scaffold "$R6"; write_good_registry "$R6"
cat > "$R6/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        // 见 RFC-001:metric 命名规约
        "foo_ttoal",
        "typo: should be foo_total"
    ).unwrap()
});
RS
OUT6="$(run_checker "$R6")" && EXIT6=0 || EXIT6=$?
assert_case "用例7 带注释多行 register typo 只 WARN 整体 PASS" 0 "$EXIT6" "$OUT6" "PASS" "class 1" "foo_ttoal"
rm -rf "$R6"

# ==========================================================================
# 用例 8 · registry YAML 格式错(metrics 段缩进/语法坏)→ 不得静默 PASS
#   覆盖修复 3 + 4:yq 解析失败必须显式报 class 3 FAIL · 不能误报 PASS。
# ==========================================================================
R7="$(mktemp -d)"; scaffold "$R7"
cat > "$R7/metric-registry.yaml" <<'YAML'
version: 1
required_tags:
  - name: service
    type: enum
metrics:
  - name: foo_total
    component: pageserver
   type: counter
      unit: requests
    required_tags_subset: [service, env, version]
tracing_known_fields: []
audit_events: []
YAML
cat > "$R7/pageserver/src/metrics.rs" <<'RS'
pub static FOO: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("foo_total", "help").unwrap()
});
RS
OUT7="$(run_checker "$R7")" && EXIT7=0 || EXIT7=$?
assert_case "用例8 坏 YAML 不得静默 PASS" nonzero "$EXIT7" "$OUT7" "FAIL"
rm -rf "$R7"

# ==========================================================================
echo "======================================================================"
echo "结果:PASS=$PASS_COUNT · FAIL=$FAIL_COUNT(共 9 用例)"
if [[ "$FAIL_COUNT" -ne 0 ]]; then
  exit 1
fi
echo "全部用例通过"
exit 0
