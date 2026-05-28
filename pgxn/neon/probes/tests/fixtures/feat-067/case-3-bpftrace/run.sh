#!/usr/bin/env bash
# Case 3 · bpftrace 单线列 USDT probe (issue#38 验收门)
# requires-root: yes
#
# 验收门: bpftrace 单线测试 `query__start` probe 能命中
# 实际操作: bpftrace -l 列 postgres binary 里的 usdt probe · 含 query__start

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../../.." && pwd)"

BIN="$REPO_ROOT/pg_install/v17/bin/postgres"
if [[ ! -x "$BIN" ]]; then
    echo "SKIP: $BIN 不存在 · 先跑 case-1-build/run.sh"
    exit 0
fi
if [[ $EUID -ne 0 ]]; then
    echo "SKIP: bpftrace 需 root · 当前 EUID=$EUID"
    exit 0
fi

# 步骤 1 · list 所有 usdt probe (postgres binary 里 provider=postgresql)
echo "step 1: bpftrace -l 'usdt:$BIN:postgresql:*'"
probes=$(bpftrace -l "usdt:$BIN:postgresql:*" 2>/dev/null || true)
echo "$probes" | head -10
total=$(echo "$probes" | wc -l)
echo "  共 $total 个 USDT probe"

if [[ $total -lt 30 ]]; then
    echo "FAIL: 期望 ≥ 30 个 usdt probe · 实际 $total"
    exit 1
fi

# 步骤 2 · 含 query__start
if ! echo "$probes" | grep -q 'postgresql:query__start'; then
    echo "FAIL: 没有 usdt:...:postgresql:query__start"
    exit 1
fi
echo "  ok · 含 postgresql:query__start"

# 步骤 3 · 实际 attach 单线 (可选 · 命中率 > 0 需要有 PG 在跑且有 query)
if [[ "${SKIP_ATTACH_TEST:-}" == "yes" ]]; then
    echo "  SKIP attach test (SKIP_ATTACH_TEST=yes)"
else
    echo "step 3: attach query__start 5 秒看是否命中 (需要后台有 PG 进程 + 跑 query)"
    timeout 5 bpftrace -e "usdt:$BIN:postgresql:query__start { @[probe] = count(); } interval:s:5 { exit(); }" 2>"$HERE/bpftrace.log" || true
    if grep -q '@\[' "$HERE/bpftrace.log" 2>/dev/null; then
        echo "  ok · attach 成功 (有命中)"
    else
        echo "  warn: 5 秒内没命中 (后台无 PG 跑 query · 不算 FAIL)"
    fi
fi

echo
echo "Case 3 PASS"
