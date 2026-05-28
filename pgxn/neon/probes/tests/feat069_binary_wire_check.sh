#!/usr/bin/env bash
# feat-069-binary-wire (D4) · 验 neon_probes USDT note 进 3 binary ELF
#
# 跑法 (Linux 物理机):
#   cd <repo root> && cargo build --release -p pageserver -p safekeeper -p proxy
#   bash pgxn/neon/probes/tests/feat069_binary_wire_check.sh
#
# 通过判据 (实测 USDT note 数 = A5 #48 lib.rs provider 定义数):
#   pageserver USDT note >= 4   (get_page_at_lsn × 2 + layer_download × 2)
#   safekeeper USDT note >= 2   (wal_append × 2)
#   proxy      USDT note >= 4   (auth × 2 + connection × 2)
#
# 注: A5 #48 PR body 提的11是 uprobe 数 (走 mangled symbol · 不需要 stapsdt note) ·
# 本 fixture 验的是 USDT 段 10 entry · 不是 uprobe · 二者机制不同。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
PASS=0
FAIL=0
RESULTS=()

check_binary() {
    local name="$1"
    local min_count="$2"
    local bin_path="${REPO_ROOT}/target/release/${name}"

    if [[ ! -x "$bin_path" ]]; then
        echo "[FAIL] $name · binary not found at $bin_path (先跑 cargo build --release -p $name)"
        FAIL=$((FAIL+1))
        RESULTS+=("FAIL $name not-built")
        return
    fi

    local actual
    actual=$(readelf -n "$bin_path" 2>/dev/null | grep -c 'stapsdt' || true)
    # readelf -n 每个 SDT note 含 4 行: Owner / Data size / Type / Description (含 stapsdt) ·
    # 用更精准的 'NT_STAPSDT' 计数 (一次 note 一行)
    local stap_count
    stap_count=$(readelf -n "$bin_path" 2>/dev/null | grep -c 'NT_STAPSDT' || true)

    if [[ "$stap_count" -ge "$min_count" ]]; then
        echo "[PASS] $name · NT_STAPSDT count = $stap_count (>= $min_count)"
        PASS=$((PASS+1))
        RESULTS+=("PASS $name=$stap_count")
    else
        echo "[FAIL] $name · NT_STAPSDT count = $stap_count (< $min_count)"
        echo "       readelf -n $bin_path | grep -A2 stapsdt | head -40:"
        readelf -n "$bin_path" 2>/dev/null | grep -A2 stapsdt | head -40 || true
        FAIL=$((FAIL+1))
        RESULTS+=("FAIL $name=$stap_count<$min_count")
    fi
}

echo "feat-069-binary-wire (D4) USDT note 接入验证"
echo "repo: $REPO_ROOT"
echo "------------------------------------------"

check_binary pageserver 4
check_binary safekeeper 2
check_binary proxy      4

echo "------------------------------------------"
echo "summary: $PASS PASS / $FAIL FAIL"
printf '  %s\n' "${RESULTS[@]}"

if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
