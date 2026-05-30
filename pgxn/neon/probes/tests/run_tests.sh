#!/usr/bin/env bash
# 校验 schema + 全套 fixture 的红绿测试
# 用法: bash pgxn/neon/probes/tests/run_tests.sh
# 退出: 0 全通过 · 非 0 任何一项失败

set -u
set -o pipefail

# 仓根目录 (本脚本位于 pgxn/neon/probes/tests/run_tests.sh)
HERE="$(cd "$(dirname "$0")" && pwd)"
PROBES_DIR="$(cd "$HERE/.." && pwd)"
REPO_ROOT="$(cd "$HERE/../../../.." && pwd)"
VALIDATE="$REPO_ROOT/scripts/validate_whitelist.py"

pass=0
fail=0

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

echo "== 合法 fixture (应通过) =="
run_case "whitelist.example.yaml" \
    python3 "$VALIDATE" "$PROBES_DIR/whitelist.example.yaml"
run_case "denylist.example.yaml" \
    python3 "$VALIDATE" "$PROBES_DIR/denylist.example.yaml"

# feat-067: 校验操作性文件 (非 .example demo · 真正供 feat-068 mcp tool 加载的)
run_case "whitelist.yaml (operative)" \
    python3 "$VALIDATE" "$PROBES_DIR/whitelist.yaml"
run_case "denylist.yaml (operative · feat-067)" \
    python3 "$VALIDATE" "$PROBES_DIR/denylist.yaml"

echo
echo "== 非法 fixture (应被拒) =="
for f in "$HERE"/invalid/*.yaml; do
    run_case "$(basename "$f")" \
        python3 "$VALIDATE" --expect-fail "$f"
done

echo
echo "总计: PASS=$pass · FAIL=$fail"
[[ $fail -eq 0 ]]
