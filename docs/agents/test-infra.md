# Dev server · openneon 编译 / 部署 / 改码工作流

> 本仓 `zlxtqbdgdgd/openneon`（fork from `neondatabase/neon`） 部署到 **`epyc-256c.e8.luyouxia.net`** 的 dev server · 替换原 `/home/z1/liqiang/src/neon` 上游 baseline · 2026-05-27 拍板执行 · 本文件是 agent / 协作者操作 SOP

---

## 1. 接入

SSH config（user mac · `~/.ssh/config`）：

```
Host openneon-dev
    HostName epyc-256c.e8.luyouxia.net
    Port 24235
    User liqiang
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
    LocalForward 55432 127.0.0.1:55432
```

⚠️ **2026-05-27 起 mac→dev server 默认 SSH 拒收**（`~/.ssh/id_ed25519` 私钥文件被误覆盖 · agent 里幸存副本）。所有 SSH 调用加 flag：

```bash
ssh -o IdentityFile=/dev/null -o IdentitiesOnly=no openneon-dev '<cmd>'
```

永久修复要 user 把新 mac pub key 追加到 dev server `~/.ssh/authorized_keys`（详 agent memory · 修复前两个 flag 必加）。

`LocalForward 55432` 把 dev compute PG 转到 mac · 本地 `psql -h 127.0.0.1 -p 55432 -U cloud_admin neondb` 直连。

---

## 2. 关键路径

| 路径 | 用途 |
|---|---|
| `/home/z1/liqiang/zlxtqbdgdgd/openneon` | **openneon fork checkout · 编译入口 · cluster 跑这里** |
| `/home/z1/liqiang/zlxtqbdgdgd/openneon-mcp` | mcp fork checkout · port 3344 · `NEON_LOCAL_URL` 指 55432 |
| `/home/z1/liqiang/neon-env.sh` | 编译环境变量 source 脚本 |
| `/home/z1/liqiang/build-openneon.sh` | openneon 编译入口脚本（封装 source env + unset LIBRARY_PATH + make + 时间戳 log）|
| `/home/z1/liqiang/build-openneon.pid` | 最近一次 build 的 PID |
| `/home/z1/liqiang/build-openneon.latest.log` | 内容 = 最新 build log 的绝对路径 |
| `/home/z1/liqiang/tools/local/` | LOCAL_PREFIX · 用户态库（libseccomp 等）|

ssh 默认登 `/home/liqiang/` · 文件全在 `/home/z1/liqiang/`（不同用户家目录）· 不用 `~`，写绝对路径。

---

## 3. Fork 拓扑（不止 openneon 一个仓）

openneon 的 `.gitmodules` 用相对 URL `../postgres.git` · 解析依赖**两个 fork 都在**：

- `zlxtqbdgdgd/openneon` ← fork from `neondatabase/neon`
- `zlxtqbdgdgd/postgres` ← fork from `neondatabase/postgres`（2026-05-27 补 fork · 之前没有）

新加 fork（或 user 换 GitHub account）时必须保证两个 fork 都存在 + 都有 `REL_14_STABLE_neon` / `REL_15_STABLE_neon` / `REL_16_STABLE_neon` / `REL_17_STABLE_neon` 4 个 branch（`gh repo fork` 默认带过来）。

---

## 4. 首次部署（仅一次）

### 4.1 前置（user 一次性配）

```bash
# 1. dev server 给 liqiang 配 GitHub SSH key（HTTPS 大文件 clone 卡 GFW · 增量 fetch 走 HTTPS 可以）
ssh openneon-dev
ssh-keygen -t ed25519 -C "liqiang-dev-server-openneon" -f ~/.ssh/id_ed25519 -N ""
cat ~/.ssh/id_ed25519.pub  # → 加到 https://github.com/settings/keys
ssh -T git@github.com   # 期望 "Hi <user>!"

# 2. 确保 zlxtqbdgdgd/postgres fork 存在
gh repo fork neondatabase/postgres --clone=false  # 跑在 mac 用 zlxtqbdgdgd 账号
```

### 4.2 Clone fork 到规范路径

```bash
# Option A: HTTPS 大文件 clone 卡 GFW · 用 rsync 从 mac 推（mac 已有 clone）
rsync -az --info=progress2 -e ssh \
  /Users/qiang/Code/github.com/zlxtqbdgdgd/openneon/ \
  openneon-dev:/home/z1/liqiang/zlxtqbdgdgd/openneon/

# Option B: dev server 直接 clone（HTTPS 大文件 clone 可能卡 · 视当时网络）
ssh openneon-dev 'git clone https://github.com/zlxtqbdgdgd/openneon.git /home/z1/liqiang/zlxtqbdgdgd/openneon'
```

### 4.3 Init submodule（拉 postgres-v14..v17）

```bash
ssh openneon-dev '
  cd /home/z1/liqiang/zlxtqbdgdgd/openneon
  git submodule update --init --recursive --depth 2 --progress .
'
```

### 4.4 Patch Makefile（dev server 本地修改 · 不 commit · 见 §6 caveat）

加入用户态 libseccomp 路径（dev server 系统 `/usr/lib64` 缺 `.so` dev symlink）：

```bash
ssh openneon-dev '
  cd /home/z1/liqiang/zlxtqbdgdgd/openneon
  cp Makefile Makefile.orig
  python3 -c "
with open(\"Makefile\") as f: src = f.read()
old = \"PG_CONFIGURE_OPTS += --with-libseccomp\"
new = \"\"\"PG_CONFIGURE_OPTS += --with-libseccomp
		PG_CONFIGURE_OPTS += --with-includes=/home/z1/liqiang/tools/local/include
		PG_CONFIGURE_OPTS += --with-libraries=/home/z1/liqiang/tools/local/lib\"\"\"
assert old in src
src = src.replace(old, new, 1)
with open(\"Makefile\", \"w\") as f: f.write(src)
"
'
```

### 4.5 编译

```bash
ssh openneon-dev '/home/z1/liqiang/build-openneon.sh'   # 256 core -j64 全新 build · 实测 ~3.5min
```

build 脚本封装 source env + unset LIBRARY_PATH + `make -j64` + 时间戳 log。

跟踪 build：

```bash
# 后台 launch
ssh openneon-dev '
  nohup setsid /home/z1/liqiang/build-openneon.sh </dev/null >/dev/null 2>&1 &
  echo $! > /home/z1/liqiang/build-openneon.pid
'
# 等完成
ssh openneon-dev '
  LOG=$(cat /home/z1/liqiang/build-openneon.latest.log)
  tail -F "$LOG" | grep -m1 "=== make exit"
'
```

### 4.6 Init + start cluster

```bash
ssh openneon-dev '
  cd /home/z1/liqiang/zlxtqbdgdgd/openneon
  source /home/z1/liqiang/neon-env.sh && unset LIBRARY_PATH
  ./target/debug/neon_local init
  ./target/debug/neon_local start
  ./target/debug/neon_local tenant create --set-default
  ./target/debug/neon_local endpoint create main
  ./target/debug/neon_local endpoint start main
  psql -h 127.0.0.1 -p 55432 -U cloud_admin postgres -c "CREATE DATABASE neondb;"
  ./target/debug/neon_local endpoint list
'
```

---

## 5. 日常 workflow · 改码 → push → pull → restart

跟 `openneon-mcp` 同模式：

```bash
# 1. 本地 mac 改代码 → commit → push
git push origin main

# 2. dev server pull + 重启
ssh openneon-dev '
  cd /home/z1/liqiang/zlxtqbdgdgd/openneon
  source /home/z1/liqiang/neon-env.sh && unset LIBRARY_PATH
  ./target/debug/neon_local stop    # caveat #7 · 不能 hot reload
  git pull origin main
  git submodule update --recursive  # 仅 postgres 子模块有变才必要
  /home/z1/liqiang/build-openneon.sh  # 增量编译 · 通常几秒~分钟
  ./target/debug/neon_local start
  ./target/debug/neon_local endpoint list
'
```

⚠️ **`neon_local stop` 必须先于 git pull / make** —— neon_local 不支持 hot reload · 在线改 binary 会触发 `[WP] PANIC: propTermStartLsn != basebackup LSN`（详 §6 caveat #7）。

---

## 6. 编译 / 运行 8 条 caveat（踩过的坑）

每条用户已在之前 sprint 付过代价 · 别再 retry：

1. **不用 sudo / root**。所有依赖（rust / cargo / protoc / libclang）装在用户家目录 · root 跑会断依赖链。
2. **不删 `.neon/`**（除非真要重建 cluster）。里面是 cluster runtime state（pageserver / safekeeper / compute）。
3. **不 export LIBRARY_PATH**。GCC 误读 `./specs` 目录 · build 断。编译前 `unset LIBRARY_PATH || true`。
4. **不 export 全局 CFLAGS**。污染 jemalloc / cc-rs 编译 · 也会覆盖 postgres per-file `-msse4.2`（pg_crc32c_sse42.o "always_inline target specific option mismatch"）。用 scoped CPPFLAGS / LDFLAGS / RUSTFLAGS，或走 postgres 原生 `--with-includes` / `--with-libraries`（详 §4.4 patch）。
5. **bindgen 需要 `LIBCLANG_PATH` + `BINDGEN_EXTRA_CLANG_ARGS`**。两者已写进 `neon-env.sh`（2026-05-27 持久化）· source 即用。值：`-isystem /home/z/tools/gcc/13.3.0/lib/gcc/x86_64-pc-linux-gnu/13.3.0/include -isystem /usr/include`。
6. **`neon_local` 默认不启 PgBouncer / proxy**。Compute PG 在 `127.0.0.1:55432` · 要 pool 自部署 PgBouncer/pgcat。
7. **改码 → 重编 → 重启永远顺序**：`neon_local stop` → `git pull` → `make` → `neon_local start` → `endpoint list` 验证。不能 hot reload。
8. **加 `shared_preload_libraries` extension**：编辑 `.neon/endpoints/<id>/postgresql.conf`（SOURCE · 非 `pgdata/postgresql.conf` · 非 `config.json` · 非 `ALTER SYSTEM`）· 然后 `neon_local endpoint stop+start`（不能 `pg_ctl restart` · 会绕过 safekeeper handshake 触发 panic）· 自定义 ext 还要从 `build/v<N>/contrib/<ext>/` `make install` 安装 .so。

---

## 7. ports 速查

| port | 服务 |
|---|---|
| `127.0.0.1:55432` | compute PG（cloud_admin@neondb · 无密码 · 通过 SSH LocalForward 落到 mac 同 port）|
| `127.0.0.1:55433/55434` | compute_ctl external/internal http |
| `127.0.0.1:50051` | storage_broker |
| `127.0.0.1:7676` | safekeeper http |
| `127.0.0.1:5454` | safekeeper pg |
| `127.0.0.1:1234/1235` | storage_controller listen / db |
| `127.0.0.1:64000` | pageserver |
| `*:3344` | openneon-mcp landing Next.js（依赖 55432 · NEON_LOCAL_URL=postgres://cloud_admin:cloud_admin@127.0.0.1:55432/neondb）|

---

## 8. 跟上游 rebase

不直接向 `neondatabase/neon` 提 PR（详 [openneon-design §3.1](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html)）。

```bash
# 主仓 sync 上游
git remote add upstream https://github.com/neondatabase/neon.git
git fetch upstream
git merge upstream/main      # 或 rebase · 看 [openneon-design §10.2.4 规约 4]
# postgres 子模块 sync
cd vendor/postgres-v17
git remote add upstream https://github.com/neondatabase/postgres.git
git fetch upstream
git merge upstream/REL_17_STABLE_neon
```

---

## 9. 故障排查

| 症状 | 原因 / 修复 |
|---|---|
| `bind 127.0.0.1:55432: Address already in use`（ssh 时）| ssh `LocalForward` 失败 · 本地 mac 55432 被另一 ssh 占了 · 无害 · ssh 还是通 |
| `Permission denied (publickey)`（ssh mac→dev）| mac id_ed25519 被覆盖 · 加 `-o IdentityFile=/dev/null -o IdentitiesOnly=no` |
| `Permission denied (publickey)`（ssh dev→github）| dev server `liqiang` 没配 GitHub key · §4.1 |
| `Postgres submodule not found in vendor/postgres-vN` | submodule 没 init · §4.3 |
| `library 'libseccomp' is required` | Makefile 没 patch `--with-libraries` · §4.4 |
| `inlining failed in call to 'always_inline' '_mm_crc32_u64'` | 用了 `make CFLAGS=...` 覆盖了 postgres per-file `-msse4.2` · 走 §4.4 patch 不传 CFLAGS |
| `'stddef.h' file not found` | `BINDGEN_EXTRA_CLANG_ARGS` 没设 · source env script（2026-05-27 已持久化）|
| `[WP] PANIC: propTermStartLsn != basebackup LSN` | 用了 `pg_ctl restart` 直接重启 compute · 必须走 `neon_local endpoint stop+start` |
| mcp `database "neondb" does not exist` | 新 cluster 没建 neondb · `psql ... -c "CREATE DATABASE neondb;"` |

---

## 10. 验证 deployment 健康

```bash
ssh openneon-dev '
  cd /home/z1/liqiang/zlxtqbdgdgd/openneon
  source /home/z1/liqiang/neon-env.sh && unset LIBRARY_PATH

  # 1. 所有服务都活着
  ./target/debug/neon_local endpoint list

  # 2. PG 可连（neon-env.sh 把 pg_install/v17/bin 进 PATH · source 后 psql 直接可调）
  psql -h 127.0.0.1 -p 55432 -U cloud_admin neondb -c "SELECT version();"

  # 3. mcp 可达（如部署在 3344）
  curl -sS -X POST -H "Content-Type: application/json" \
    -d "{\"query\":\"SELECT 1\"}" \
    http://127.0.0.1:3344/api/local-call/run_sql_transaction
'
```

---

## 相关文档

- [openneon-design overview §3.6.1 day-one 运行形态](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html) · 设计上 openneon = 本地自部署 · 非 SaaS
- [openneon-design overview §3.2.1 物理 / 协议判定](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html) · 18 条 neon 内核 feature 在本仓实施
- 上游 neon 内核文档：[neondatabase/neon docs/sourcetree.md](https://github.com/neondatabase/neon/blob/main/docs/sourcetree.md)
- 同套 workflow 的 mcp 部署：`zlxtqbdgdgd/openneon-mcp` README + CLAUDE.md
