#!/usr/bin/env bash
# Case 2 · readelf -n <postgres> 看到 ≥ 30 NT_STAPSDT note (issue#38 验收门)
#
# 实际跑前必须先跑 case 1 · 编出 pg_install/v17/bin/postgres binary

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../../.." && pwd)"

BIN="$REPO_ROOT/pg_install/v17/bin/postgres"
if [[ ! -x "$BIN" ]]; then
    echo "SKIP: $BIN 不存在 · 先跑 case-1-build/run.sh"
    exit 0
fi

# readelf -n 打 NT_STAPSDT note · 每个 USDT probe 一条
# 期望 ≥ 30 个 (PG 上游标准约 40+ · 我们 yaml 白名单列了 53 个常用的)
echo "step 1: readelf -n $BIN | grep stapsdt"
count=$(readelf -n "$BIN" 2>/dev/null | grep -c 'stapsdt' || true)
echo "  stapsdt note 数: $count"

if [[ $count -lt 30 ]]; then
    echo "FAIL: 期望 ≥ 30 个 stapsdt note · 实际 $count"
    echo "  可能原因 1: configure 没加 --enable-dtrace (回看 case 1)"
    echo "  可能原因 2: PG 源码 src/backend/utils/probes.d 被裁剪"
    exit 1
fi

# 顺手列前 5 个 probe 名做 sanity check (跟 whitelist.yaml 应对得上)
echo "step 2: 前 5 个 probe 名 sanity check"
readelf -n "$BIN" 2>/dev/null | grep -A 3 'NT_STAPSDT' | grep 'Name:' | head -5

echo
echo "Case 2 PASS · stapsdt note=$count (≥ 30)"
