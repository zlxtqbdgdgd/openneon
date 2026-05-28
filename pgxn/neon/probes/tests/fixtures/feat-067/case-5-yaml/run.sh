#!/usr/bin/env bash
# Case 5 · whitelist.yaml + denylist.yaml + invalid fixture 整套 schema 校验
# (issue#31 + issue#32 验收门)
#
# 跟 pgxn/neon/probes/tests/run_tests.sh 区别: 那个跑的是 anchor PR 的 example
# fixture (whitelist.example.yaml / denylist.example.yaml), 本 case 跑实际部署
# 用的 whitelist.yaml (53 entry + 7 deny pattern). 二者都要 pass.

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../../.." && pwd)"

PROBES_DIR="$REPO_ROOT/pgxn/neon/probes"
VALIDATE="$REPO_ROOT/scripts/validate_whitelist.py"

if [[ ! -x "$VALIDATE" ]] && [[ ! -f "$VALIDATE" ]]; then
    echo "FAIL: 没找到 $VALIDATE (anchor PR 应已提供)"
    exit 1
fi

pass=0; fail=0

run_case() {
    local label="$1"; shift
    if "$@"; then
        echo "  PASS  $label"
        pass=$((pass + 1))
    else
        echo "  FAIL  $label"
        fail=$((fail + 1))
    fi
}

echo "== 实际部署 yaml (应通过) =="
run_case "whitelist.yaml  (feat-067 实际 53 probe)" \
    python3 "$VALIDATE" "$PROBES_DIR/whitelist.yaml"
run_case "whitelist.example.yaml (anchor 例子)" \
    python3 "$VALIDATE" "$PROBES_DIR/whitelist.example.yaml"
run_case "denylist.example.yaml  (anchor 例子)" \
    python3 "$VALIDATE" "$PROBES_DIR/denylist.example.yaml"

echo
echo "== anchor 负面 fixture (应被拒) =="
for f in "$PROBES_DIR"/tests/invalid/*.yaml; do
    run_case "$(basename "$f")" \
        python3 "$VALIDATE" --expect-fail "$f"
done

echo
echo "总计: PASS=$pass · FAIL=$fail"
[[ $fail -eq 0 ]]
