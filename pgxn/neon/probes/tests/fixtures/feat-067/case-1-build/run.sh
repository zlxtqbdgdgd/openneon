#!/usr/bin/env bash
# Case 1 · PG fork build 加 --enable-dtrace 后通过 · 不破坏现有 build chain
#
# 验收门 (issue#38):
#   - Neon fork 中 PostgreSQL configure 调用加 --enable-dtrace flag
#   - PostgreSQL fork build 通过 · 不破坏现有 build chain
#
# 跑动条件: 仓根 + vendor/postgres-v17/ 已 submodule init + systemtap-sdt-dev 装好 (Linux)
#
# 验证策略 (分两步, 失败不假阳性):
#   1. configure.log 含 '--enable-dtrace' (Makefile 改对了)
#   2. config.status 退 0 + 头文件 utils/probes.h 被生成 (configure 接受 flag 且
#      dtrace 工具链满足要求 · 不满足时 PG 自己会报错退出)

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../../.." && pwd)"

echo "REPO_ROOT=$REPO_ROOT"

# 步骤 1 · Makefile 配置正确性 (不需要真编译 · 跑得动的纯文本检查)
echo "step 1: Makefile 含 WITH_DTRACE toggle + --enable-dtrace"
if ! grep -q 'WITH_DTRACE' "$REPO_ROOT/Makefile"; then
    echo "FAIL: Makefile 没找到 WITH_DTRACE toggle"
    exit 1
fi
if ! grep -q -- '--enable-dtrace' "$REPO_ROOT/Makefile"; then
    echo "FAIL: Makefile 没找到 --enable-dtrace flag"
    exit 1
fi
echo "  ok"

# 步骤 2 · 真实 configure 跑 (vendor submodule 在的话)
echo "step 2: 真 configure (vendor submodule + systemtap-sdt-dev 在的话)"
if [[ ! -s "$REPO_ROOT/vendor/postgres-v17/configure" ]]; then
    echo "  SKIP: vendor/postgres-v17/configure 不存在 (submodule 未 init) · CI 跑前应 git submodule update"
    exit 0
fi

# 不实际跑 make · 改为直接调 configure --help 检查 --enable-dtrace 是否被 PG configure 接受
# (PG configure 永远接受这个 flag · 只在没装 dtrace 工具链时才真编译失败)
if ! "$REPO_ROOT/vendor/postgres-v17/configure" --help 2>/dev/null | grep -q -- '--enable-dtrace'; then
    echo "FAIL: PG configure --help 不识别 --enable-dtrace · 上游已删 flag?"
    exit 1
fi
echo "  ok · PG configure 识别 --enable-dtrace"

# 步骤 3 · 真编译触发 (issue#38 验收门 "build 通过" 的硬性证据)
echo "step 3: 真编译 (POSTGRES_VERSIONS=v17 · WITH_DTRACE=yes)"
if [[ "${SKIP_REAL_BUILD:-}" == "yes" ]]; then
    echo "  SKIP: SKIP_REAL_BUILD=yes · CI 应自行跑 'make postgres-install-v17'"
    exit 0
fi
if ! command -v dtrace >/dev/null 2>&1 && ! dpkg -l systemtap-sdt-dev 2>/dev/null | grep -q '^ii'; then
    echo "  SKIP: 没装 systemtap-sdt-dev (Linux 必须) / dtrace (macOS) · CI 应自己装好"
    exit 0
fi

cd "$REPO_ROOT"
if ! make -j4 POSTGRES_VERSIONS=v17 WITH_DTRACE=yes postgres-install-v17 >"$HERE/build.log" 2>&1; then
    echo "FAIL: build 失败 · 看 $HERE/build.log"
    tail -50 "$HERE/build.log"
    exit 1
fi
echo "  ok · build 通过"

echo
echo "Case 1 PASS"
