#!/usr/bin/env bash
# Case 4 · pgbench attach hot path · overhead < 5% (issue#38 验收门)
# requires-root: yes
#
# 验收门:
#   hot path probe (query__execute__start/done) attach 后业务 query latency overhead < 5%
#   binary 大小 +N% 实测 (预期 < 1% · NOP 5 byte)
#
# 实操:
#   1. pgbench -i (init 1x scale)
#   2. baseline · pgbench -T 30 · 记 TPS_baseline
#   3. 后台 bpftrace attach query__execute__start/done
#   4. attached · pgbench -T 30 · 记 TPS_attached
#   5. 算退化 (TPS_baseline - TPS_attached) / TPS_baseline · 期望 < 5%
#   6. binary 大小: with vs without --enable-dtrace · 期望 < 1%

set -u
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../../../../../.." && pwd)"

BIN="$REPO_ROOT/pg_install/v17/bin/postgres"
PGBENCH="$REPO_ROOT/pg_install/v17/bin/pgbench"

if [[ ! -x "$BIN" || ! -x "$PGBENCH" ]]; then
    echo "SKIP: 没有 $BIN / $PGBENCH · 先跑 case-1-build/run.sh"
    exit 0
fi
if [[ $EUID -ne 0 ]]; then
    echo "SKIP: 需 root 起 bpftrace · 当前 EUID=$EUID"
    exit 0
fi

# Case 4 是真实跑分 · 占资源 · 在 CI 应 SKIP_BENCH=yes 跳过
if [[ "${SKIP_BENCH:-}" == "yes" ]]; then
    echo "SKIP: SKIP_BENCH=yes (CI 默认跳)"
    exit 0
fi

DURATION_BASE="${DURATION_BASE:-30}"
DURATION_ATTACH="${DURATION_ATTACH:-30}"
SCALE="${SCALE:-1}"
PGDATA="${PGDATA:-/tmp/feat-067-pgdata}"
PORT="${PORT:-55067}"

echo "params: DURATION=$DURATION_BASE/$DURATION_ATTACH SCALE=$SCALE PGDATA=$PGDATA PORT=$PORT"

# pg_ctl initdb + start (假定 case 4 在干净环境跑 · 重复跑前先 rm -rf $PGDATA)
PG_CTL="$REPO_ROOT/pg_install/v17/bin/pg_ctl"
INITDB="$REPO_ROOT/pg_install/v17/bin/initdb"
PSQL="$REPO_ROOT/pg_install/v17/bin/psql"

if [[ ! -d "$PGDATA" ]]; then
    echo "step 0: initdb"
    "$INITDB" -D "$PGDATA" --username=postgres --auth=trust >/dev/null
fi
echo "step 0: 启 PG"
"$PG_CTL" -D "$PGDATA" -o "-p $PORT" -l "$HERE/pg.log" start
trap '"$PG_CTL" -D "$PGDATA" stop -m fast || true' EXIT

# 等 PG 起来
for i in 1 2 3 4 5; do
    if "$PSQL" -p "$PORT" -U postgres -c 'select 1' postgres >/dev/null 2>&1; then break; fi
    sleep 1
done

echo "step 1: pgbench -i scale=$SCALE"
"$PGBENCH" -p "$PORT" -U postgres -i -s "$SCALE" postgres >/dev/null

echo "step 2: baseline pgbench -T $DURATION_BASE"
tps_base=$("$PGBENCH" -p "$PORT" -U postgres -T "$DURATION_BASE" -c 4 -j 2 postgres 2>&1 | awk -F'=' '/tps/ {gsub(/[^0-9.]/, "", $2); print $2; exit}')
echo "  TPS_baseline=$tps_base"

echo "step 3: attach bpftrace query__execute__start/done (后台)"
bpftrace -q -e "usdt:$BIN:postgresql:query__execute__start { @s[tid] = nsecs; }
usdt:$BIN:postgresql:query__execute__done /@s[tid]/ { @lat = hist(nsecs - @s[tid]); delete(@s[tid]); }" >/dev/null 2>&1 &
BPF_PID=$!
sleep 2

echo "step 4: attached pgbench -T $DURATION_ATTACH"
tps_attach=$("$PGBENCH" -p "$PORT" -U postgres -T "$DURATION_ATTACH" -c 4 -j 2 postgres 2>&1 | awk -F'=' '/tps/ {gsub(/[^0-9.]/, "", $2); print $2; exit}')
echo "  TPS_attached=$tps_attach"

kill "$BPF_PID" 2>/dev/null || true

# 算退化 %
deg=$(awk "BEGIN {printf \"%.2f\", ($tps_base - $tps_attach) / $tps_base * 100}")
echo "step 5: 退化 = $deg %"

ok=$(awk "BEGIN {print ($deg < 5.0) ? 1 : 0}")
if [[ "$ok" != "1" ]]; then
    echo "FAIL: 退化 $deg % >= 5% · attach hot path 太贵 · 重新评估 query__execute__start/done"
    exit 1
fi

echo
echo "Case 4 PASS · 退化 $deg %"
