# feat-067 · USDT 5 case fixture · RUNBOOK

本目录 (`pgxn/neon/probes/tests/fixtures/feat-067/`) 是 issue#32 五件套 fixture ·
跟 `tests/run_tests.sh` 跑的 yaml schema fixture 分开放——schema fixture 是
**作者机器跑的纯文本校验**, 不依赖 build / 内核 / 二进制; 本目录的 5 case 是
**CI matrix 跑的真实 PG 二进制行为验证**, 需要 Linux + systemtap-sdt-dev +
bpftrace 才能完整跑。

Case 1 / Case 5 在任何机器都能跑 · Case 2 / Case 3 / Case 4 必须在 Linux + perf
工具栈下跑。每个 case 一个目录, 含独立 README + 触发脚本 + 期望输出。

## 总览

| Case | 验收门要点 | 触发脚本 | 期望 | 跑得动的环境 |
| ---- | ---------- | -------- | ---- | ------------ |
| 1    | `make postgres-install-v17` 加 `--enable-dtrace` 后 build 通过 | `case-1-build/run.sh` | exit 0 + log 含 `--enable-dtrace`     | CI Linux runner (装好 systemtap-sdt-dev) |
| 2    | `readelf -n` 看到 ≥ 30 stapsdt note  | `case-2-readelf/run.sh` | stdout 含 ≥ 30 行 `stapsdt`  | Linux + readelf (binutils) |
| 3    | `bpftrace` 能列 `postgresql:query__start` probe | `case-3-bpftrace/run.sh` | stdout 含 `usdt:.*postgres.*:postgresql:query__start` | Linux + bpftrace + root  |
| 4    | overhead 估算 pgbench attach hot path < 5% | `case-4-overhead/run.sh` | TPS 退化 < 5% | Linux + pgbench + root |
| 5    | 整套 yaml schema 校验通过 (whitelist + denylist + invalid) | `case-5-yaml/run.sh` | exit 0 + `PASS=N FAIL=0`  | 任何机器 |

## 总入口

```bash
bash pgxn/neon/probes/tests/fixtures/feat-067/run_all.sh
```

`run_all.sh` 会自动 skip 当前环境跑不动的 case (例 Linux 没装 bpftrace 跳 case-3)
并最后打 `PASS=X · SKIP=Y · FAIL=0`。FAIL>0 直接 exit 1。

## CI 集成 (issue#32 验收门最后一条)

PR #39 已经在 `Makefile` 加了 `make -C pgxn/neon check-probes` target · CI yaml
matrix 加 `with-dtrace` (default · `WITH_DTRACE=yes`) 和 `without-dtrace`
(`WITH_DTRACE=no`) 两条 lane:

```yaml
- name: PG build (with USDT)
  run: WITH_DTRACE=yes make postgres-install-v17
- name: PG build (without USDT)
  run: WITH_DTRACE=no make postgres-install-v17
- name: USDT fixture (CI-friendly subset)
  run: bash pgxn/neon/probes/tests/fixtures/feat-067/run_all.sh --ci
```

`--ci` 模式 skip 需要 root 的 case (case-3 bpftrace, case-4 overhead),
保留 case-1/2/5 三个 deterministic 的 lane.

## 跟 anchor PR 的关系

- anchor PR (#39) 出: schema + example yaml + validate.py + tests/invalid/*.yaml
- 本目录 (feat-067 impl PR · #38+#31+#32 聚合) 出:
  - `pgxn/neon/probes/whitelist.yaml`            实际 53 个 probe 白名单
  - `pgxn/neon/probes/tests/fixtures/feat-067/`  5 case 跑跑看 fixture
- 共用 `scripts/validate_whitelist.py` (anchor PR 出 · 不动它)

## 已知坑

1. **submodule 拉慢**: vendor/postgres-v17 是 git submodule · CI 首次跑
   `make postgres-install-v17` 前必须 `git submodule update --init --recursive
   --depth 2`. 推荐 CI cache pg_install/. CI runner 缺 submodule 会导致 Makefile
   `configure` 阶段就退失败.
2. **macOS DTrace**: PG `--enable-dtrace` 在 macOS 走 Apple DTrace · 但 `readelf`
   是 ELF 工具不能看 mach-o 二进制 · macOS 上 case-2 应自动 skip · 用 `dtrace -l
   -P postgresql$<pid>` 替代验证 (case-2-readelf/macos-fallback.sh 给一个示例).
3. **bpftrace USDT 名字格式**: bpftrace 接 USDT 时格式是 `usdt:<binary>:<provider>:
   <probe>` · 注意 binary 这一段不在 yaml 里 (因为同一 binary 装到 pageserver /
   compute 路径都行) · feat-068 mcp tool 现场拼路径.
4. **PG 14+ 改名**: `xlog-insert` -> `wal-insert` 是 PG 14 才改 · 本白名单 pg_version_min=14
   且只列新名 · 老版本接入需要扩白名单.
5. **probe args 字段**: yaml 里的 `args` 字段是文档化用 · readelf 看 NT_STAPSDT
   note 时确实能看到 args (USDT 把 dtrace 探针参数静态编码进 binary 段) · 但
   bpftrace 现场 attach 时按 arg0/arg1 数字下标取值 · 不读 yaml 的 args 字面值.
