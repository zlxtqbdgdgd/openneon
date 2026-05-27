# Audit log OTel attribute schema (feat-031)

> Cross-repo single source of truth for the `openneon.audit.*` OpenTelemetry attribute
> schema. Both the Neon kernel components (this repo) and the MCP Server
> ([zlxtqbdgdgd/openneon-mcp#110](https://github.com/zlxtqbdgdgd/openneon-mcp/issues/110))
> emit audit events using **exactly** these attributes so a DBA can join them on a single
> `traceparent` (W3C trace context) in Datadog / Grafana / Honeycomb.
>
> Design: [feat-031 §3.2](https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feat-031-L2-neon-audit-log-otel-export.html).

## Routing

Every audit event is emitted on the `openneon::audit` tracing target
(`utils::logging::AUDIT_TARGET`). On the kernel side this is done via the
`utils::audit_event!` macro, which expands to:

```rust
tracing::info!(target: "openneon::audit", event_type, op_class, principal, outcome, ...);
```

The OTel collector filters on `target=openneon::audit` to split audit events from ordinary
traces (see `openneon-mcp/docs/audit-otel-deployment.md` for sample collector configs).

## Required attributes

| Attribute | Meaning |
|---|---|
| `openneon.audit.event_type` | one of the 13 event types below |
| `openneon.audit.op_class` | feat-056 `OpClass` (e.g. `CREATE_OR_RESTORE_BRANCH`) |
| `openneon.audit.principal` | `human:<id>` / `system:<component>` / `agent:<key-last-4>` |
| `openneon.audit.outcome` | `allow` / `deny` / `override` / `approved` / `rejected` |

## Event types (`openneon.audit.event_type`)

`g1_cross_project_deny`, `g4_destructive_deny`, `g9_rate_limit_deny`, `plan_mode_required`,
`plan_mode_approved`, `plan_mode_rejected`, `confirm_token_issued`, `confirm_token_verified`,
`confirm_token_rejected`, `claim_override`, `destructive_classified`, `ddl_executed`,
`compute_audit_log_record`.

Defined as constants in `libs/utils/src/logging.rs::audit_event_type`. On the kernel side
L2a mainly emits:

- `ddl_executed` — `pageserver/src/http/routes.rs::timeline_create_handler` (branch/timeline create).
- `compute_audit_log_record` — `compute_tools/src/audit_otel.rs` tails `<pgdata>/log/audit*.log`.

The rest (`plan_mode_*`, `confirm_token_*`, `g*_deny`, `claim_override`,
`destructive_classified`) are emitted by the MCP Server.

## DB / OTel semantic conventions

| Attribute | Notes |
|---|---|
| `db.system` | always `postgresql` |
| `db.statement.sha256` | SHA-256 of the SQL — **never the full statement (PII redact, §6)** |
| `db.user` | role name (optional) |

## USR namespace (feat-008-011, L2b)

`openneon.usr.{tenant_id,timeline_id,endpoint_id,shard_id}` is reserved. L2a fills only what
is already available (e.g. pageserver fills tenant/timeline/shard ids; compute fills
endpoint/project ids). Missing USR fields are **not** a failure. When feat-008-011 ships in
L2b, the remaining fields propagate automatically through the 4 components' tracing context,
which is backward-compatible (older events simply have no USR fields).
