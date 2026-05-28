#!/usr/bin/env bash
# feat-067 · 5 case fixture 总入口
# 用法:
#   bash pgxn/neon/probes/tests/fixtures/feat-067/run_all.sh        # 全跑 · 不能跑的 skip
#   bash pgxn/neon/probes/tests/fixtures/feat-067/run_all.sh --ci   # CI 模式 · skip 需 root 的 case
# 退出:
#   0  · 全 pass / skip
#   1  · 至少一个 case fail
#
# 详情见同目录 RUNBOOK.md

set -u
set -o pipefail

CI_MODE=no
if [[ "${1:-}" == "--ci" ]]; then
    CI_MODE=yes
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
# run_all.sh 位置: pgxn/neon/probes/tests/fixtures/feat-067/run_all.sh
# HERE -> feat-067 · ../fixtures · ../../tests · ../../../probes · ../../../../neon ·
# ../../../../../pgxn · ../../../../../../<repo-root>
PROBES_DIR="$(cd "$HERE/../../.." && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../.." && pwd)"

pass=0
fail=0
skip=0

# 跑单个 case · 自动算 PASS/FAIL/SKIP
# $1 = case 目录名 · $2 = 友好名 · $3.. = case 跑动需要的可执行 (没装就 skip)
run_case() {
    local case_dir="$1"; shift
    local label="$1"; shift
    local needs=("$@")

    # 在 CI 模式且 case 自己声明需要 root, 直接 skip (CI runner 一般非 root)
    local script="$HERE/$case_dir/run.sh"
    if [[ ! -x "$script" ]]; then
        echo "  SKIP  $label (无 run.sh)"
        skip=$((skip + 1))
        return
    fi

    # 检查依赖工具
    for tool in "${needs[@]}"; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            echo "  SKIP  $label ($tool 不可用)"
            skip=$((skip + 1))
            return
        fi
    done

    # CI 模式 skip root case
    if [[ "$CI_MODE" == "yes" ]] && grep -q '^# requires-root: yes' "$script"; then
        echo "  SKIP  $label (CI 模式 skip root case)"
        skip=$((skip + 1))
        return
    fi

    # 真跑
    if bash "$script" >"$HERE/$case_dir/last.log" 2>&1; then
        echo "  PASS  $label"
        pass=$((pass + 1))
    else
        echo "  FAIL  $label · 看 $HERE/$case_dir/last.log"
        fail=$((fail + 1))
    fi
}

echo "== feat-067 · USDT 5 case fixture =="
echo "REPO_ROOT=$REPO_ROOT"
echo "PROBES_DIR=$PROBES_DIR"
echo "CI_MODE=$CI_MODE"
echo

run_case case-1-build    "Case 1 · PG build with --enable-dtrace" make
run_case case-2-readelf  "Case 2 · readelf 看 stapsdt note"        readelf
run_case case-3-bpftrace "Case 3 · bpftrace 单线列 probe"          bpftrace
run_case case-4-overhead "Case 4 · pgbench overhead < 5%"          pgbench bpftrace
run_case case-5-yaml     "Case 5 · YAML schema 校验整套"           python3

echo
echo "总计: PASS=$pass · SKIP=$skip · FAIL=$fail"

[[ $fail -eq 0 ]]
