# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root (lazily created by `/grill-with-docs`)
- **`docs/adr/`** — read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The producer skill (`/grill-with-docs`) creates them lazily when terms or decisions actually get resolved.

## File structure: single-context

```
/
├── CONTEXT.md           ← local domain glossary (lazily created)
├── docs/adr/            ← architectural decisions (lazily populated)
└── docs/agents/         ← this folder
```

This repo is **single-context** — one `CONTEXT.md` covers everything in this repo. (`openneon` project is a 5-repo multirepo, **not** a monorepo with `CONTEXT-MAP.md`.)

## Cross-repo glossary (重要 · 跨仓共享词汇)

This repo is **fork of `neondatabase/neon` (database kernel · Rust · USR 全栈贴标 / SQL view 拓宽 / libpq+traceparent 等数据库内核改造在此)** — part of the **`openneon` project**, a 5-repo AI-agent-driven cloud-native PostgreSQL (fork of Neon).

**The shared domain glossary lives in the design hub repo `openneon-design`** (not in this repo). When a concept is **not** in this repo's local `CONTEXT.md`, engineering skills should next consult:

- **Master glossary**: https://github.com/zlxtqbdgdgd/openneon-design/blob/main/CONTEXT.md (when it exists · lazily created)
- **Phase B 概要 + 60 feature registry**: https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/overview.html
- **Feature single source of truth**: https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feature-registry.yaml
- **Phase A 调研**: https://github.com/zlxtqbdgdgd/openneon-design/tree/main/research

Cross-repo shared vocabulary held in the master glossary includes: USR / pageserver / safekeeper / endpoint_id / LFC / SAE J3016 L0-L5 / Datadog DBM 11+1 tool / OWASP LLM Top 10 mapping etc. (per `openneon-design/features/overview.html`).

### Sibling repos (5-repo openneon project)

| Repo | Role | GitHub URL |
|------|------|------------|
| `openneon-design` | **设计枢纽 · 主词典宿主** | https://github.com/zlxtqbdgdgd/openneon-design |
| `openneon` | fork of `neondatabase/neon` · database kernel (Rust) | https://github.com/zlxtqbdgdgd/openneon |
| `openneon-mcp` | fork of `neondatabase/mcp-server-neon` · MCP Server (TypeScript · 21+ modifications) | https://github.com/zlxtqbdgdgd/openneon-mcp |
| `openneon-plugin` | new · Claude Code skill plugin | https://github.com/zlxtqbdgdgd/openneon-plugin |
| `openneon-autopilot` | fork of `OpenHarness-SQL` · OHSQL 专用 agent (Codex skill) | https://github.com/zlxtqbdgdgd/openneon-autopilot |

Skills that operate cross-repo (e.g. a refactor that touches both `openneon-mcp` and `openneon-plugin`) should use the GitHub URLs above to navigate — never assume sibling repos are cloned to a specific local path (per `openneon-design/CLAUDE.md` 习惯 8).

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md` (this repo's local, then `openneon-design`'s master). Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/grill-with-docs`).

## Flag ADR conflicts

If your output contradicts an existing ADR (this repo's `docs/adr/` or `openneon-design/docs/adr/`), surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (event-sourced orders) — but worth reopening because…_
