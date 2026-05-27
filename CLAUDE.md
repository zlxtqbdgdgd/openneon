# CLAUDE.md · openneon

> 本仓库是 **neon-autopilot 项目的 `neon` 模块**（数据库内核本地修改 · fork from [`neondatabase/neon`](https://github.com/neondatabase/neon)）

## ⚠️ 启动前必读 · 设计 + AI 协作 source of truth

**开 Claude Code 时第一步**：读以下 3 份文档（否则会漂移回老坑——机械陈述 / 字面意思 / 商业可比叙事 / 单一百分比覆盖度 / "同事 review" 模糊指代等）：

1. [openneon-design/CLAUDE.md](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/CLAUDE.md) —— 12 习惯 + 6 P 规则 + 触发警惕清单（**所有 AI 协作 + 项目设计原则**）
2. [openneon-design/features/overview.html](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html) —— Phase B 概要设计 source of truth
3. [openneon-design/features/feature-registry.yaml](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feature-registry.yaml) —— 57 条 feature 单一事实表（**本仓涉及的 18 条 neon 内核 feature**，grep `module: neon`）

## 本仓在 neon-autopilot 项目中的角色

`neon` 模块——满足 [§3.2.1 物理 / 协议判定](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html) 的 **18 条 feature 实施**（USR 全栈贴标 / SQL view 拓宽 / libpq+traceparent / SQLCommenter / WAL replication / PG compute jsonlog / autosuspend & warming_up 状态机 / Rust profiler / pg_stat_get_backend_io / backup verification 等——都是**必须在数据产生地 / 协议层 / Neon 独有能力**实施的特性）。

**改造原则**：本地 fork 不向 `neondatabase/neon` 提 PR（详 [openneon-design §3.1](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html)）。

## 本仓特定（跨仓不复用 · 本仓 build / 测试 / commit / 部署）

### 上游 Neon 内核 codebase 操作

详 [neondatabase/neon 官方文档](https://github.com/neondatabase/neon)（Rust toolchain · PostgreSQL build · pageserver / safekeeper / compute 启动方式等）

### Commit 规范

- commit message 用中文 + `feat(neon-<submodule>):` / `fix(neon-<submodule>):` 前缀
- submodule 取自 [§2.3 子模块清单](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html)：`pageserver` / `safekeeper` / `compute` / `proxy` / `metric` / `sql-view` / `log` / `audit` / `baseline-state`
- 每个 PR 带 `feat-NNN reference` + 改动锚点（详 design [§10.2.4 规约 4](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html)）

### 跟上游 rebase 策略

- 长期分支：`feat/neon-autopilot`（本项目所有 neon 内核改造）
- 上游 `main` 定期 fetch 但不强制 rebase（详 design §3.1.3 时间线根因）
- 本仓 default branch 跟随上游 / `feat/neon-autopilot` 是本项目工作分支

### 本仓特定 caveat · dev server 部署

详 [`docs/agents/test-infra.md`](docs/agents/test-infra.md) —— `epyc-256c.e8.luyouxia.net` 部署位置 + 编译入口 + 改码-pull-restart workflow + 8 条踩过的坑（含 BINDGEN / libseccomp / pg_crc32c_sse42 / hot-reload 禁忌等）。

## Agent skills

> 由 `/setup-matt-pocock-skills` 生成（2026-05-19）。供 mattpocock/skills 套件下的工程类 skill（`to-issues` / `triage` / `to-prd` / `qa` / `improve-codebase-architecture` / `diagnose` / `tdd` 等）查询 per-repo 配置。

### Issue tracker

GitHub Issues · 仓 `zlxtqbdgdgd/openneon` · 用 `gh` CLI。详 [`docs/agents/issue-tracker.md`](docs/agents/issue-tracker.md)。

### Triage labels

5 个 canonical role 全用默认 label 名（`needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`）。详 [`docs/agents/triage-labels.md`](docs/agents/triage-labels.md)。

### Domain docs

**single-context** · 本仓属 openneon 项目（5 仓 multirepo · 本仓非主词典宿主）· 跨仓共享词汇查 [openneon-design](https://github.com/zlxtqbdgdgd/openneon-design)。详 [`docs/agents/domain.md`](docs/agents/domain.md)。
