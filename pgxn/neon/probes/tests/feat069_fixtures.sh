#!/usr/bin/env bash
# feat-069/#35 · 5 case 端到端 fixture
#
# 验收门 (issue#35):
#   (1) pageserver/safekeeper/proxy release build 保留 debug info
#       (Cargo profile.release: strip=none / split-debuginfo=packed / debug=limited)
#   (2) uprobe attach 到白名单函数 (Timeline::get_page_at_lsn 等) 能命中
#   (3) USDT note 在 binary ELF 可见 + bpftrace attach 能命中
#   (4) 白名单 / denylist 静态文件就位 (供 feat-068 加载)
#   (5) async fn 屏障: 故意配 is_async:true → schema 校验直接拒
#
# 分工:
#   - 本 PR 范围 (#33+#34+#35): case (1) 通过 Cargo.toml 文本断言验证 ·
#                              case (4)(5) 走 schema 校验脚本验证 ·
#                              case (2)(3) 因本仓 CI 暂未配 Rust release build +
#                                  bpftrace · 留 stage marker 文档化命令
#   - feat-068 mcp tool PR:    把 (2)(3) 接入端到端 CI (远端 Linux 物理机 ·
#                              cargo build --release + readelf -n + bpftrace -l)

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROBES_DIR="$(cd "$HERE/.." && pwd)"
REPO_ROOT="$(cd "$HERE/../../../.." && pwd)"
VALIDATE="$REPO_ROOT/scripts/validate_whitelist.py"

pass=0
fail=0
stage=0

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

stage_case() {
    local label="$1"; shift
    echo "  STAGE $label"
    echo "        cmd: $*"
    stage=$((stage + 1))
}

echo "== feat-069 5-case fixture =="
echo

# -------------------------------------------------------------------
# Case 1: build profile 保留 debug info (Cargo.toml 文本断言)
# -------------------------------------------------------------------
echo "[case 1] Cargo profile.release: strip + split-debuginfo + debug=limited"
run_case "strip = \"none\"" \
    bash -c "grep -qE '^strip *= *\"none\"' $REPO_ROOT/Cargo.toml"
run_case "split-debuginfo = \"packed\"" \
    bash -c "grep -qE '^split-debuginfo *= *\"packed\"' $REPO_ROOT/Cargo.toml"
run_case "debug = \"limited\"" \
    bash -c "grep -qE '^debug *= *\"limited\"' $REPO_ROOT/Cargo.toml"
echo

# -------------------------------------------------------------------
# Case 2: uprobe attach 命令骨架 (stage · 留 feat-068 CI 跑)
# -------------------------------------------------------------------
echo "[case 2] uprobe attach to Timeline::get_page_at_lsn (stage)"
stage_case "cargo build --release (pageserver)" \
    "cargo build --release -p pageserver"
stage_case "readelf -W --symbols pageserver (grep get_page_at_lsn / new_for_path)" \
    "readelf -W --symbols target/release/pageserver | grep -E '(get_page_at_lsn|new_for_path)'"
stage_case "bpftrace 列出可 attach 符号" \
    "sudo bpftrace -l 'uprobe:target/release/pageserver:*new_for_path*'"
echo

# -------------------------------------------------------------------
# Case 3: USDT note + bpftrace attach (stage)
# -------------------------------------------------------------------
echo "[case 3] USDT note section + bpftrace probe (stage)"
stage_case "cargo build --release -p neon_probes" \
    "cd $PROBES_DIR/rust && cargo build --release"
stage_case "readelf -n libneon_probes.rlib (期望: neon_pageserver / neon_safekeeper / neon_proxy 3 个 provider)" \
    "readelf -n $PROBES_DIR/rust/target/release/libneon_probes.rlib"
stage_case "bpftrace 列 USDT probe" \
    "sudo bpftrace -l 'usdt:target/release/pageserver:neon_pageserver:*'"
echo

# -------------------------------------------------------------------
# Case 4: 白名单 + denylist 静态文件就位 (schema 校验全绿)
# -------------------------------------------------------------------
echo "[case 4] 白名单 / denylist 静态文件就位"
run_case "rust-whitelist.yaml schema 合法" \
    python3 "$VALIDATE" "$PROBES_DIR/rust-whitelist.yaml"
run_case "rust-denylist.yaml schema 合法" \
    python3 "$VALIDATE" "$PROBES_DIR/rust-denylist.yaml"
run_case "rust-whitelist.yaml usdt 段 ≥ 10 entry (issue#34 验收门 ~10)" \
    bash -c "python3 -c \"import yaml,sys; d=yaml.safe_load(open('$PROBES_DIR/rust-whitelist.yaml')); n=len(d.get('usdt') or []); sys.exit(0 if n>=10 else 1)\""
run_case "rust-whitelist.yaml uprobe 段 ≥ 10 entry (issue#34 验收门 ~10)" \
    bash -c "python3 -c \"import yaml,sys; d=yaml.safe_load(open('$PROBES_DIR/rust-whitelist.yaml')); n=len(d.get('uprobe') or []); sys.exit(0 if n>=10 else 1)\""
run_case "rust-whitelist.yaml 所有 uprobe entry is_async=false (屏障 1)" \
    bash -c "python3 -c \"import yaml,sys; d=yaml.safe_load(open('$PROBES_DIR/rust-whitelist.yaml')); rows=d.get('uprobe') or []; bad=[r for r in rows if r.get('is_async') is not False]; sys.exit(0 if not bad else 1)\""
echo

# -------------------------------------------------------------------
# Case 5: async fn 屏障 (故意 is_async:true → schema 拒)
# -------------------------------------------------------------------
echo "[case 5] async fn 屏障 (invalid fixture 必须被拒)"
run_case "tests/invalid/async_uprobe.yaml 被拒" \
    python3 "$VALIDATE" --expect-fail "$HERE/invalid/async_uprobe.yaml"
echo

echo "总计: PASS=$pass · FAIL=$fail · STAGE=$stage (留 feat-068 CI 验)"
[[ $fail -eq 0 ]]
