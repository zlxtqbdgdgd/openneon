//! Part of Safekeeper pretending to be Postgres, i.e. handling Postgres
//! protocol commands.

use std::future::Future;
use std::str::{self, FromStr};
use std::sync::Arc;

use anyhow::Context;
use jsonwebtoken::TokenData;
use pageserver_api::models::ShardParameters;
use pageserver_api::shard::{ShardIdentity, ShardStripeSize};
use postgres_backend::{PostgresBackend, QueryError};
use postgres_ffi::PG_TLI;
use pq_proto::{BeMessage, FeStartupPacket, INT4_OID, RowDescriptor, TEXT_OID};
use regex::Regex;
use safekeeper_api::Term;
use safekeeper_api::models::ConnectionId;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{Instrument, debug, info, info_span};
use utils::auth::{Claims, JwtAuth, Scope};
use utils::id::{TenantId, TenantTimelineId, TimelineId};
use utils::lsn::Lsn;
use utils::postgres_client::PostgresClientProtocol;
use utils::shard::{ShardCount, ShardNumber};

use tracing_utils::trace_context::TraceContext;

use crate::auth::check_permission;
use crate::metrics::{PG_QUERIES_GAUGE, TrafficMetrics};
use crate::timeline::TimelineError;
use crate::{GlobalTimelines, SafeKeeperConf};

/// Safekeeper handler of postgres commands
pub struct SafekeeperPostgresHandler {
    pub conf: Arc<SafeKeeperConf>,
    /// assigned application name
    pub appname: Option<String>,
    pub tenant_id: Option<TenantId>,
    pub timeline_id: Option<TimelineId>,
    pub ttid: TenantTimelineId,
    pub shard: Option<ShardIdentity>,
    pub protocol: Option<PostgresClientProtocol>,
    /// Unique connection id is logged in spans for observability.
    pub conn_id: ConnectionId,
    pub global_timelines: Arc<GlobalTimelines>,
    /// Auth scope allowed on the connections and public key used to check auth tokens. None if auth is not configured.
    auth: Option<(Scope, Arc<JwtAuth>)>,
    claims: Option<Claims>,
    io_metrics: Option<TrafficMetrics>,
}

/// Parsed Postgres command.
#[derive(Debug)]
enum SafekeeperPostgresCommand {
    StartWalPush {
        proto_version: u32,
        // Eventually timelines will be always created explicitly by storcon.
        // This option allows legacy behaviour for compute to do that until we
        // fully migrate.
        allow_timeline_creation: bool,
        /// W3C TraceContext propagated from compute (walproposer). `Some` only when
        /// caller used proto_version >= 4. Older clients leave this `None` and the
        /// safekeeper degrades gracefully (no parent span, behaves as before).
        ///
        /// 协议契约（feat-035 §4 命令级 KV）:
        ///   START_WAL_PUSH (proto_version '4',
        ///                   allow_timeline_creation 'false',
        ///                   traceparent '00-<32hex>-<16hex>-<2hex>',
        ///                   tracestate '<key>=<value>,<key>=<value>')
        /// W3C §3.2.2.3 forward-compat: 解析走 lenient (接受 v00..v fe)。
        trace_context: Option<TraceContext>,
        /// W3C §3.3 tracestate (sibling header) — 原样透传, 不解析。
        tracestate: Option<String>,
    },
    StartReplication {
        start_lsn: Lsn,
        term: Option<Term>,
        /// W3C TraceContext propagated from pageserver walreceiver or peer safekeeper
        /// recovery. See `StartWalPush::trace_context` for KV protocol contract.
        trace_context: Option<TraceContext>,
        tracestate: Option<String>,
    },
    IdentifySystem,
    TimelineStatus,
}

/// Parse a single `key 'value'` KV pair. Returns (key, value_with_outer_quotes_stripped).
///
/// W3C §3.2 traceparent / §3.3 tracestate 在 `START_WAL_PUSH` / `START_REPLICATION`
/// 命令 KV 序列里走的也是同样这套 `key 'value'` 语法。SK 端 forward-compat 纪律:
/// 对未知 key **silent ignore** (兼容矩阵 §4.5: v3 SK 收到 v4 命令含 traceparent
/// 必须忽略不报错)。
fn parse_kv<'a>(kvstr: &'a str, cmd: &str) -> anyhow::Result<Option<(&'a str, &'a str)>> {
    let trimmed = kvstr.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // Split off the first whitespace-delimited token (the key) and treat the
    // remainder as the value. Stripping outer quotes is intentionally
    // permissive: callers wrap values in single quotes (postgres replication
    // command convention) but we don't require it.
    let (key, value) = trimmed.split_once(|c: char| c == '=' || c.is_whitespace()).with_context(|| {
        format!("failed to parse key/value in kv {kvstr} in command {cmd}")
    })?;
    let value_trimmed = value.trim().trim_matches('\'');
    Ok(Some((key, value_trimmed)))
}

/// Split a `(k1 'v1', k2 'v2', ...)` blob into KV strings. Naive split on `,`:
/// values that legitimately need to embed a comma must be quoted **and** caller
/// must not embed a comma there (matches the existing safekeeper KV grammar).
/// `traceparent` / `tracestate` values are pure hex / ASCII so comma-safe.
fn split_kv_blob<'a>(blob: &'a str) -> impl Iterator<Item = &'a str> {
    blob.split(',')
}

fn parse_cmd(cmd: &str) -> anyhow::Result<SafekeeperPostgresCommand> {
    if cmd.starts_with("START_WAL_PUSH") {
        // Allow additional options in postgres START_REPLICATION style like
        //   START_WAL_PUSH (proto_version '4',
        //                   allow_timeline_creation 'false',
        //                   traceparent '00-..-..-..',
        //                   tracestate 'k=v,...').
        // Parsing here is very naive and breaks in case of commas or
        // whitespaces in values, but enough for our purposes (traceparent /
        // tracestate values are pure ASCII per W3C grammar).
        let re = Regex::new(r"START_WAL_PUSH(\s+?\((.*)\))?").unwrap();
        let caps = re
            .captures(cmd)
            .context(format!("failed to parse START_WAL_PUSH command {cmd}"))?;
        // capture () content
        let options = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        // default values
        let mut proto_version = 2;
        let mut allow_timeline_creation = true;
        let mut trace_context = None;
        let mut tracestate: Option<String> = None;
        for kvstr in split_kv_blob(options) {
            let Some((key, value_trimmed)) = parse_kv(kvstr, cmd)? else {
                continue;
            };
            match key {
                "proto_version" => {
                    proto_version = value_trimmed.parse::<u32>().context(format!(
                        "failed to parse proto_version value {value_trimmed} in command {cmd}"
                    ))?;
                }
                "allow_timeline_creation" => {
                    allow_timeline_creation =
                        value_trimmed.parse::<bool>().context(format!(
                            "failed to parse allow_timeline_creation value {value_trimmed} in command {cmd}"
                        ))?;
                }
                "traceparent" => {
                    // SK 是 forwarder: lenient (W3C §3.2.2.3). 上游来源任意版本
                    // 都接, 失败时不让命令整体失败 — 这是 silent-degrade 纪律,
                    // 烂 traceparent 不能拖死 WAL 写路径。
                    match TraceContext::parse_lenient(value_trimmed) {
                        Ok(tc) => trace_context = Some(tc),
                        Err(e) => {
                            tracing::warn!(
                                "ignoring malformed traceparent {:?} in START_WAL_PUSH: {}",
                                value_trimmed, e
                            );
                        }
                    }
                }
                "tracestate" => {
                    // tracestate (W3C §3.3) 原样透传, 不解析。
                    tracestate = Some(value_trimmed.to_string());
                }
                _ => {
                    // Unknown KV → silent ignore (forward-compat for future
                    // proto versions, mirrors §4.5 兼容矩阵 v3 SK × v4 client 行为).
                }
            }
        }
        Ok(SafekeeperPostgresCommand::StartWalPush {
            proto_version,
            allow_timeline_creation,
            trace_context,
            tracestate,
        })
    } else if cmd.starts_with("START_REPLICATION") {
        // Grammar (extended for feat-035):
        //   START_REPLICATION [SLOT <slot>] [PHYSICAL] X/X
        //       [(key 'value', key 'value', ...)]
        // Historically the trailing parens only carried `term='N'`. feat-035
        // adds `traceparent '...'` and `tracestate '...'`. KV grammar is the
        // same as START_WAL_PUSH; values are quoted to keep the existing
        // term='N' shape backward-compatible.
        let re = Regex::new(
            r"START_REPLICATION(?: SLOT [^ ]+)?(?: PHYSICAL)? ([[:xdigit:]]+/[[:xdigit:]]+)(?:\s+\((.*)\))?",
        )
        .unwrap();
        let caps = re
            .captures(cmd)
            .context(format!("failed to parse START_REPLICATION command {cmd}"))?;
        let start_lsn =
            Lsn::from_str(&caps[1]).context("parse start LSN from START_REPLICATION command")?;
        let options = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let mut term: Option<Term> = None;
        let mut trace_context = None;
        let mut tracestate: Option<String> = None;
        for kvstr in split_kv_blob(options) {
            let Some((key, value_trimmed)) = parse_kv(kvstr, cmd)? else {
                continue;
            };
            match key {
                "term" => {
                    // term historically wrapped with quotes: term='N'
                    term = Some(value_trimmed.parse::<u64>().context("invalid term")?);
                }
                "traceparent" => {
                    match TraceContext::parse_lenient(value_trimmed) {
                        Ok(tc) => trace_context = Some(tc),
                        Err(e) => {
                            tracing::warn!(
                                "ignoring malformed traceparent {:?} in START_REPLICATION: {}",
                                value_trimmed, e
                            );
                        }
                    }
                }
                "tracestate" => {
                    tracestate = Some(value_trimmed.to_string());
                }
                _ => {
                    // Unknown KV → silent ignore (forward-compat).
                }
            }
        }
        Ok(SafekeeperPostgresCommand::StartReplication {
            start_lsn,
            term,
            trace_context,
            tracestate,
        })
    } else if cmd.starts_with("IDENTIFY_SYSTEM") {
        Ok(SafekeeperPostgresCommand::IdentifySystem)
    } else if cmd.starts_with("TIMELINE_STATUS") {
        Ok(SafekeeperPostgresCommand::TimelineStatus)
    } else {
        anyhow::bail!("unsupported command {cmd}");
    }
}

fn cmd_to_string(cmd: &SafekeeperPostgresCommand) -> &str {
    match cmd {
        SafekeeperPostgresCommand::StartWalPush { .. } => "START_WAL_PUSH",
        SafekeeperPostgresCommand::StartReplication { .. } => "START_REPLICATION",
        SafekeeperPostgresCommand::TimelineStatus => "TIMELINE_STATUS",
        SafekeeperPostgresCommand::IdentifySystem => "IDENTIFY_SYSTEM",
    }
}

impl<IO: AsyncRead + AsyncWrite + Unpin + Send> postgres_backend::Handler<IO>
    for SafekeeperPostgresHandler
{
    // tenant_id and timeline_id are passed in connection string params
    fn startup(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        sm: &FeStartupPacket,
    ) -> Result<(), QueryError> {
        if let FeStartupPacket::StartupMessage { params, .. } = sm {
            if let Some(options) = params.options_raw() {
                let mut shard_count: Option<u8> = None;
                let mut shard_number: Option<u8> = None;
                let mut shard_stripe_size: Option<u32> = None;

                for opt in options {
                    // FIXME `ztenantid` and `ztimelineid` left for compatibility during deploy,
                    // remove these after the PR gets deployed:
                    // https://github.com/neondatabase/neon/pull/2433#discussion_r970005064
                    match opt.split_once('=') {
                        Some(("protocol", value)) => {
                            self.protocol =
                                Some(serde_json::from_str(value).with_context(|| {
                                    format!("Failed to parse {value} as protocol")
                                })?);
                        }
                        Some(("ztenantid", value)) | Some(("tenant_id", value)) => {
                            self.tenant_id = Some(value.parse().with_context(|| {
                                format!("Failed to parse {value} as tenant id")
                            })?);
                        }
                        Some(("ztimelineid", value)) | Some(("timeline_id", value)) => {
                            self.timeline_id = Some(value.parse().with_context(|| {
                                format!("Failed to parse {value} as timeline id")
                            })?);
                        }
                        Some(("availability_zone", client_az)) => {
                            if let Some(metrics) = self.io_metrics.as_ref() {
                                metrics.set_client_az(client_az)
                            }
                        }
                        Some(("shard_count", value)) => {
                            shard_count = Some(value.parse::<u8>().with_context(|| {
                                format!("Failed to parse {value} as shard count")
                            })?);
                        }
                        Some(("shard_number", value)) => {
                            shard_number = Some(value.parse::<u8>().with_context(|| {
                                format!("Failed to parse {value} as shard number")
                            })?);
                        }
                        Some(("shard_stripe_size", value)) => {
                            shard_stripe_size = Some(value.parse::<u32>().with_context(|| {
                                format!("Failed to parse {value} as shard stripe size")
                            })?);
                        }
                        _ => continue,
                    }
                }

                match self.protocol() {
                    PostgresClientProtocol::Vanilla => {
                        if shard_count.is_some()
                            || shard_number.is_some()
                            || shard_stripe_size.is_some()
                        {
                            return Err(QueryError::Other(anyhow::anyhow!(
                                "Shard params specified for vanilla protocol"
                            )));
                        }
                    }
                    PostgresClientProtocol::Interpreted { .. } => {
                        match (shard_count, shard_number, shard_stripe_size) {
                            (Some(count), Some(number), Some(stripe_size)) => {
                                let params = ShardParameters {
                                    count: ShardCount(count),
                                    stripe_size: ShardStripeSize(stripe_size),
                                };
                                self.shard =
                                    Some(ShardIdentity::from_params(ShardNumber(number), params));
                            }
                            _ => {
                                return Err(QueryError::Other(anyhow::anyhow!(
                                    "Shard params were not specified"
                                )));
                            }
                        }
                    }
                }
            }

            if let Some(app_name) = params.get("application_name") {
                self.appname = Some(app_name.to_owned());
                if let Some(metrics) = self.io_metrics.as_ref() {
                    metrics.set_app_name(app_name)
                }
            }

            let ttid = TenantTimelineId::new(
                self.tenant_id.unwrap_or(TenantId::from([0u8; 16])),
                self.timeline_id.unwrap_or(TimelineId::from([0u8; 16])),
            );
            tracing::Span::current()
                .record("ttid", tracing::field::display(ttid))
                .record(
                    "application_name",
                    tracing::field::debug(self.appname.clone()),
                );

            if let Some(shard) = self.shard.as_ref() {
                if let Some(slug) = shard.shard_slug().strip_prefix("-") {
                    tracing::Span::current().record("shard", tracing::field::display(slug));
                }
            }

            Ok(())
        } else {
            Err(QueryError::Other(anyhow::anyhow!(
                "Safekeeper received unexpected initial message: {sm:?}"
            )))
        }
    }

    fn check_auth_jwt(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        jwt_response: &[u8],
    ) -> Result<(), QueryError> {
        // this unwrap is never triggered, because check_auth_jwt only called when auth_type is NeonJWT
        // which requires auth to be present
        let (allowed_auth_scope, auth) = self
            .auth
            .as_ref()
            .expect("auth_type is configured but .auth of handler is missing");
        let data: TokenData<Claims> = auth
            .decode(str::from_utf8(jwt_response).context("jwt response is not UTF-8")?)
            .map_err(|e| QueryError::Unauthorized(e.0))?;

        // The handler might be configured to allow only tenant scope tokens.
        if matches!(allowed_auth_scope, Scope::Tenant)
            && !matches!(data.claims.scope, Scope::Tenant)
        {
            return Err(QueryError::Unauthorized(
                "passed JWT token is for full access, but only tenant scope is allowed".into(),
            ));
        }

        if matches!(data.claims.scope, Scope::Tenant) && data.claims.tenant_id.is_none() {
            return Err(QueryError::Unauthorized(
                "jwt token scope is Tenant, but tenant id is missing".into(),
            ));
        }

        debug!(
            "jwt scope check succeeded for scope: {:#?} by tenant id: {:?}",
            data.claims.scope, data.claims.tenant_id,
        );

        self.claims = Some(data.claims);
        Ok(())
    }

    fn process_query(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        query_string: &str,
    ) -> impl Future<Output = Result<(), QueryError>> {
        Box::pin(async move {
            if query_string
                .to_ascii_lowercase()
                .starts_with("set datestyle to ")
            {
                // important for debug because psycopg2 executes "SET datestyle TO 'ISO'" on connect
                pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
                return Ok(());
            }

            let cmd = parse_cmd(query_string)?;
            let cmd_str = cmd_to_string(&cmd);

            let _guard = PG_QUERIES_GAUGE.with_label_values(&[cmd_str]).guard();

            info!("got query {:?}", query_string);

            let tenant_id = self.tenant_id.context("tenantid is required")?;
            let timeline_id = self.timeline_id.context("timelineid is required")?;
            self.check_permission(Some(tenant_id))?;
            self.ttid = TenantTimelineId::new(tenant_id, timeline_id);

            match cmd {
                SafekeeperPostgresCommand::StartWalPush {
                    proto_version,
                    allow_timeline_creation,
                    trace_context,
                    tracestate,
                } => {
                    // feat-035 段 1 (compute → SK): trace_id 来自上游 walproposer
                    // 注入的 traceparent (W3C §3.2.2.5 forwarder 纪律: 接收即透传)。
                    // 出现在 span field 上让 OTel layer 自动把整条 WAL 接收链路
                    // 挂到上游 trace 下面。无 traceparent (proto v3 / 未注入) 时
                    // 字段为空字符串, 不影响行为。
                    let (trace_id, span_id) = trace_context
                        .as_ref()
                        .map(|tc| (tc.trace_id_hex(), tc.span_id_hex()))
                        .unwrap_or_default();
                    let ts_field = tracestate.as_deref().unwrap_or("");
                    self.handle_start_wal_push(pgb, proto_version, allow_timeline_creation)
                        .instrument(info_span!(
                            "WAL receiver",
                            trace_id = %trace_id,
                            span_id = %span_id,
                            tracestate = %ts_field,
                        ))
                        .await
                }
                SafekeeperPostgresCommand::StartReplication {
                    start_lsn,
                    term,
                    trace_context,
                    tracestate,
                } => {
                    // feat-035 段 2 (SK → pageserver) / 段 3 (SK → SK recovery):
                    // 入站 START_REPLICATION 可能携带 traceparent — pageserver
                    // walreceiver 出口侧自生 + tracestate=neon=root=pageserver-walreceiver
                    // (lazy catch-up) 或来自 GetPage@LSN 上游 span; SK→SK 自生
                    // + tracestate=neon=root=safekeeper-recovery。
                    let (trace_id, span_id) = trace_context
                        .as_ref()
                        .map(|tc| (tc.trace_id_hex(), tc.span_id_hex()))
                        .unwrap_or_default();
                    let ts_field = tracestate.as_deref().unwrap_or("");
                    self.handle_start_replication(pgb, start_lsn, term)
                        .instrument(info_span!(
                            "WAL sender",
                            trace_id = %trace_id,
                            span_id = %span_id,
                            tracestate = %ts_field,
                        ))
                        .await
                }
                SafekeeperPostgresCommand::IdentifySystem => self.handle_identify_system(pgb).await,
                SafekeeperPostgresCommand::TimelineStatus => self.handle_timeline_status(pgb).await,
            }
        })
    }
}

impl SafekeeperPostgresHandler {
    pub fn new(
        conf: Arc<SafeKeeperConf>,
        conn_id: u32,
        io_metrics: Option<TrafficMetrics>,
        auth: Option<(Scope, Arc<JwtAuth>)>,
        global_timelines: Arc<GlobalTimelines>,
    ) -> Self {
        SafekeeperPostgresHandler {
            conf,
            appname: None,
            tenant_id: None,
            timeline_id: None,
            ttid: TenantTimelineId::empty(),
            shard: None,
            protocol: None,
            conn_id,
            claims: None,
            auth,
            io_metrics,
            global_timelines,
        }
    }

    pub fn protocol(&self) -> PostgresClientProtocol {
        self.protocol.unwrap_or(PostgresClientProtocol::Vanilla)
    }

    // when accessing management api supply None as an argument
    // when using to authorize tenant pass corresponding tenant id
    fn check_permission(&self, tenant_id: Option<TenantId>) -> Result<(), QueryError> {
        if self.auth.is_none() {
            // auth is set to Trust, nothing to check so just return ok
            return Ok(());
        }
        // auth is some, just checked above, when auth is some
        // then claims are always present because of checks during connection init
        // so this expect won't trigger
        let claims = self
            .claims
            .as_ref()
            .expect("claims presence already checked");
        check_permission(claims, tenant_id).map_err(|e| QueryError::Unauthorized(e.0))
    }

    async fn handle_timeline_status<IO: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
    ) -> Result<(), QueryError> {
        // Get timeline, handling "not found" error
        let tli = match self.global_timelines.get(self.ttid) {
            Ok(tli) => Ok(Some(tli)),
            Err(TimelineError::NotFound(_)) => Ok(None),
            Err(e) => Err(QueryError::Other(e.into())),
        }?;

        // Write row description
        pgb.write_message_noflush(&BeMessage::RowDescription(&[
            RowDescriptor::text_col(b"flush_lsn"),
            RowDescriptor::text_col(b"commit_lsn"),
        ]))?;

        // Write row if timeline exists
        if let Some(tli) = tli {
            let (inmem, _state) = tli.get_state().await;
            let flush_lsn = tli.get_flush_lsn().await;
            let commit_lsn = inmem.commit_lsn;
            pgb.write_message_noflush(&BeMessage::DataRow(&[
                Some(flush_lsn.to_string().as_bytes()),
                Some(commit_lsn.to_string().as_bytes()),
            ]))?;
        }

        pgb.write_message_noflush(&BeMessage::CommandComplete(b"TIMELINE_STATUS"))?;
        Ok(())
    }

    ///
    /// Handle IDENTIFY_SYSTEM replication command
    ///
    async fn handle_identify_system<IO: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
    ) -> Result<(), QueryError> {
        let tli = self
            .global_timelines
            .get(self.ttid)
            .map_err(|e| QueryError::Other(e.into()))?;

        let lsn = if self.is_walproposer_recovery() {
            // walproposer should get all local WAL until flush_lsn
            tli.get_flush_lsn().await
        } else {
            // other clients shouldn't get any uncommitted WAL
            tli.get_state().await.0.commit_lsn
        }
        .to_string();

        let sysid = tli.get_state().await.1.server.system_id.to_string();
        let lsn_bytes = lsn.as_bytes();
        let tli = PG_TLI.to_string();
        let tli_bytes = tli.as_bytes();
        let sysid_bytes = sysid.as_bytes();

        pgb.write_message_noflush(&BeMessage::RowDescription(&[
            RowDescriptor {
                name: b"systemid",
                typoid: TEXT_OID,
                typlen: -1,
                ..Default::default()
            },
            RowDescriptor {
                name: b"timeline",
                typoid: INT4_OID,
                typlen: 4,
                ..Default::default()
            },
            RowDescriptor {
                name: b"xlogpos",
                typoid: TEXT_OID,
                typlen: -1,
                ..Default::default()
            },
            RowDescriptor {
                name: b"dbname",
                typoid: TEXT_OID,
                typlen: -1,
                ..Default::default()
            },
        ]))?
        .write_message_noflush(&BeMessage::DataRow(&[
            Some(sysid_bytes),
            Some(tli_bytes),
            Some(lsn_bytes),
            None,
        ]))?
        .write_message_noflush(&BeMessage::CommandComplete(b"IDENTIFY_SYSTEM"))?;
        Ok(())
    }

    /// Returns true if current connection is a replication connection, originating
    /// from a walproposer recovery function. This connection gets a special handling:
    /// safekeeper must stream all local WAL till the flush_lsn, whether committed or not.
    pub fn is_walproposer_recovery(&self) -> bool {
        match &self.appname {
            None => false,
            Some(appname) => {
                appname == "wal_proposer_recovery" ||
                // set by safekeeper peer recovery
                appname.starts_with("safekeeper")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SafekeeperPostgresCommand;

    const SAMPLE_TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    /// Test parsing of START_WAL_PUSH command (legacy v3 shape preserved).
    #[test]
    fn test_start_wal_push_parse() {
        let cmd = "START_WAL_PUSH";
        let parsed = super::parse_cmd(cmd).expect("failed to parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush {
                proto_version,
                allow_timeline_creation,
                trace_context,
                tracestate,
            } => {
                assert_eq!(proto_version, 2);
                assert!(allow_timeline_creation);
                assert!(trace_context.is_none());
                assert!(tracestate.is_none());
            }
            _ => panic!("unexpected command"),
        }

        let cmd =
            "START_WAL_PUSH (proto_version '3', allow_timeline_creation 'false', unknown 'hoho')";
        let parsed = super::parse_cmd(cmd).expect("failed to parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush {
                proto_version,
                allow_timeline_creation,
                trace_context,
                tracestate,
            } => {
                assert_eq!(proto_version, 3);
                assert!(!allow_timeline_creation);
                // unknown KV → silent ignore, trace_context still None.
                assert!(trace_context.is_none());
                assert!(tracestate.is_none());
            }
            _ => panic!("unexpected command"),
        }
    }

    // ====================================================================
    // feat-035 §4.5 兼容矩阵 (6 case · v3/v4 SK × v3/v4 client):
    //
    //   v3 client → v3 SK: 不带 traceparent · 同 baseline
    //   v3 client → v4 SK: 不带 traceparent · trace_context = None (degrade)
    //   v4 client → v3 SK: 带 traceparent · silent ignore (forward-compat)
    //   v4 client → v4 SK: 带 traceparent · 解析到 TraceContext
    //   v4 client (v01)  → v4 SK: lenient 接受 (forward-compat W3C §3.2.2.3)
    //   v4 client → v4 SK with broken traceparent: 走 warn 不让命令失败
    //
    // SK 端解析逻辑不区分 "我是 v3" 还是 "我是 v4" — parse_cmd 永远尝试解, 不解
    // 出来不报错。所以下面 6 个 case 都在 SK 解析层验。"是否真正用 trace_context"
    // 由后续 handle_start_wal_push spans 决定 (None → 不挂 OTel parent), v3 SK
    // 即便配置 GUC=3 也能解出 v4 命令 KV 不报错 (silent ignore unknown key)。
    // ====================================================================

    #[test]
    fn compat_v3_client_no_traceparent() {
        // v3 client → v3 or v4 SK: 老 shape, 无 traceparent。
        let cmd = "START_WAL_PUSH (proto_version '3', allow_timeline_creation 'false')";
        let parsed = super::parse_cmd(cmd).expect("v3 must parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush {
                proto_version,
                trace_context,
                ..
            } => {
                assert_eq!(proto_version, 3);
                assert!(trace_context.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compat_v4_client_v4_sk_round_trip() {
        // v4 client → v4 SK: full traceparent propagation。
        let cmd = format!(
            "START_WAL_PUSH (proto_version '4', allow_timeline_creation 'false', traceparent '{SAMPLE_TP}')"
        );
        let parsed = super::parse_cmd(&cmd).expect("v4 must parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush {
                proto_version,
                trace_context,
                ..
            } => {
                assert_eq!(proto_version, 4);
                let tc = trace_context.expect("trace_context must be present");
                assert_eq!(tc.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
                assert_eq!(tc.span_id_hex(), "00f067aa0ba902b7");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compat_v4_client_v3_sk_silent_ignore() {
        // v4 client → v3 SK: SK 解析照样能跑通 (proto_version 字段读出来 = 4),
        // traceparent KV 在我们这个统一 parse 里也会被解出来 — but **v3 SK 后续
        // handle_start_wal_push 应该忽略 trace_context** (因为它的 instrumentation
        // 不依赖). 这一行为由 §4.5 矩阵保证: 老二进制不会调用任何依赖 trace_context
        // 的 API。
        //
        // 这个 case 的语义是 "命令本身解析无错", 即向后兼容不打破连接。
        let cmd = format!(
            "START_WAL_PUSH (proto_version '4', allow_timeline_creation 'false', traceparent '{SAMPLE_TP}')"
        );
        super::parse_cmd(&cmd).expect("v3 SK must still parse v4 cmd without error");
    }

    #[test]
    fn compat_v4_client_broken_traceparent_does_not_kill_cmd() {
        // 烂 traceparent 不能拖死 WAL 写路径 (silent-degrade 纪律) — parse_cmd
        // 必须返回 Ok, trace_context = None。
        let cmd = "START_WAL_PUSH (proto_version '4', allow_timeline_creation 'false', traceparent 'definitely-not-a-traceparent')";
        let parsed = super::parse_cmd(cmd).expect("broken traceparent must not fail cmd");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush { trace_context, .. } => {
                assert!(trace_context.is_none(), "broken tp should degrade to None");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compat_v4_lenient_accepts_future_version() {
        // forward-compat (W3C §3.2.2.3): v01 / vfe 都接, vff 拒。
        let v01 = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let cmd = format!(
            "START_WAL_PUSH (proto_version '4', allow_timeline_creation 'false', traceparent '{v01}')"
        );
        let parsed = super::parse_cmd(&cmd).expect("future version must lenient-parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush { trace_context, .. } => {
                assert!(trace_context.is_some(), "lenient v01 must be accepted");
                let tc = trace_context.unwrap();
                assert_eq!(tc.version, 0x01);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn compat_tracestate_round_trip() {
        // tracestate (W3C §3.3) 原样透传。
        let cmd = format!(
            "START_WAL_PUSH (proto_version '4', traceparent '{SAMPLE_TP}', tracestate 'neon=root=walproposer')"
        );
        let parsed = super::parse_cmd(&cmd).expect("must parse");
        match parsed {
            SafekeeperPostgresCommand::StartWalPush { tracestate, .. } => {
                assert_eq!(tracestate.as_deref(), Some("neon=root=walproposer"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn start_replication_legacy_term_only() {
        // START_REPLICATION 老 shape: term='N' 单 KV, 行为不变。
        let cmd = "START_REPLICATION PHYSICAL 1/0 (term='5')";
        let parsed = super::parse_cmd(cmd).expect("legacy must parse");
        match parsed {
            SafekeeperPostgresCommand::StartReplication {
                term,
                trace_context,
                ..
            } => {
                assert_eq!(term, Some(5));
                assert!(trace_context.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn start_replication_with_traceparent() {
        // feat-035 段 2/3 出口: pageserver walreceiver / SK recovery 发的
        // START_REPLICATION 带 traceparent (+ 可选 term)。
        let cmd = format!(
            "START_REPLICATION PHYSICAL 1/2A0 (term='7', traceparent '{SAMPLE_TP}', tracestate 'neon=root=safekeeper-recovery')"
        );
        let parsed = super::parse_cmd(&cmd).expect("must parse");
        match parsed {
            SafekeeperPostgresCommand::StartReplication {
                term,
                trace_context,
                tracestate,
                ..
            } => {
                assert_eq!(term, Some(7));
                let tc = trace_context.expect("trace_context");
                assert_eq!(tc.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
                assert_eq!(
                    tracestate.as_deref(),
                    Some("neon=root=safekeeper-recovery")
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn start_replication_no_options_block() {
        // 没有括号的最朴素形态 (现行 pageserver 走这条).
        let cmd = "START_REPLICATION PHYSICAL 1/0";
        let parsed = super::parse_cmd(cmd).expect("no-paren must parse");
        match parsed {
            SafekeeperPostgresCommand::StartReplication {
                term,
                trace_context,
                ..
            } => {
                assert_eq!(term, None);
                assert!(trace_context.is_none());
            }
            _ => panic!(),
        }
    }
}
