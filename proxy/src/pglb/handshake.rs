use futures::{FutureExt, TryFutureExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, info, warn};

use crate::auth::endpoint_sni;
use crate::config::TlsConfig;
use crate::context::RequestContext;
use crate::error::ReportableError;
use crate::metrics::Metrics;
use crate::pglb::TlsRequired;
use crate::pqproto::{
    BeMessage, CancelKeyData, FeStartupPacket, ProtocolVersion, StartupMessageParams,
};
use crate::stream::{PqStream, Stream, StreamUpgradeError};
use crate::tls::PG_ALPN_PROTOCOL;
use crate::trace_context::{self, TraceContext, TraceRoot};

#[derive(Error, Debug)]
pub(crate) enum HandshakeError {
    #[error("data is sent before server replied with EncryptionResponse")]
    EarlyData,

    #[error("protocol violation")]
    ProtocolViolation,

    #[error("{0}")]
    StreamUpgradeError(#[from] StreamUpgradeError),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    ReportedError(#[from] crate::stream::ReportedError),
}

impl ReportableError for HandshakeError {
    fn get_error_kind(&self) -> crate::error::ErrorKind {
        match self {
            HandshakeError::EarlyData => crate::error::ErrorKind::User,
            HandshakeError::ProtocolViolation => crate::error::ErrorKind::User,
            HandshakeError::StreamUpgradeError(upgrade) => match upgrade {
                StreamUpgradeError::AlreadyTls => crate::error::ErrorKind::Service,
                StreamUpgradeError::Io(_) => crate::error::ErrorKind::ClientDisconnect,
            },
            HandshakeError::Io(_) => crate::error::ErrorKind::ClientDisconnect,
            HandshakeError::ReportedError(e) => e.get_error_kind(),
        }
    }
}

pub(crate) enum HandshakeData<S> {
    Startup(PqStream<Stream<S>>, StartupMessageParams),
    Cancel(CancelKeyData),
}

/// Establish a (most probably, secure) connection with the client.
/// For better testing experience, `stream` can be any object satisfying the traits.
/// It's easier to work with owned `stream` here as we need to upgrade it to TLS;
/// we also take an extra care of propagating only the select handshake errors to client.
#[tracing::instrument(skip_all)]
pub(crate) async fn handshake<S: AsyncRead + AsyncWrite + Unpin + Send>(
    ctx: &RequestContext,
    stream: S,
    mut tls: Option<&TlsConfig>,
    record_handshake_error: bool,
) -> Result<HandshakeData<S>, HandshakeError> {
    // Client may try upgrading to each protocol only once
    let (mut tried_ssl, mut tried_gss) = (false, false);

    const PG_PROTOCOL_EARLIEST: ProtocolVersion = ProtocolVersion::new(3, 0);
    const PG_PROTOCOL_LATEST: ProtocolVersion = ProtocolVersion::new(3, 0);

    let (mut stream, mut msg) = PqStream::parse_startup(Stream::from_raw(stream)).await?;
    loop {
        match msg {
            FeStartupPacket::SslRequest { direct } => match stream.get_ref() {
                Stream::Raw { .. } if !tried_ssl => {
                    tried_ssl = true;

                    if let Some(tls) = tls.take() {
                        // Upgrade raw stream into a secure TLS-backed stream.
                        // NOTE: We've consumed `tls`; this fact will be used later.

                        let mut read_buf;
                        let raw = if let Some(direct) = &direct {
                            read_buf = &direct[..];
                            stream.accept_direct_tls()
                        } else {
                            read_buf = &[];
                            stream.accept_tls().await?
                        };

                        let Stream::Raw { raw } = raw else {
                            return Err(HandshakeError::StreamUpgradeError(
                                StreamUpgradeError::AlreadyTls,
                            ));
                        };

                        let mut res = Ok(());
                        let accept = tokio_rustls::TlsAcceptor::from(tls.pg_config.clone())
                            .accept_with(raw, |session| {
                                // push the early data to the tls session
                                while !read_buf.is_empty() {
                                    match session.read_tls(&mut read_buf) {
                                        Ok(_) => {}
                                        Err(e) => {
                                            res = Err(e);
                                            break;
                                        }
                                    }
                                }
                            })
                            .map_ok(Box::new)
                            .boxed();

                        res?;

                        if !read_buf.is_empty() {
                            return Err(HandshakeError::EarlyData);
                        }

                        let tls_stream = accept.await.inspect_err(|_| {
                            if record_handshake_error {
                                Metrics::get().proxy.tls_handshake_failures.inc();
                            }
                        })?;

                        let conn_info = tls_stream.get_ref().1;

                        // try parse endpoint
                        let ep = conn_info
                            .server_name()
                            .and_then(|sni| endpoint_sni(sni, &tls.common_names));
                        if let Some(ep) = ep {
                            ctx.set_endpoint_id(ep);
                        }

                        // check the ALPN, if exists, as required.
                        match conn_info.alpn_protocol() {
                            None | Some(PG_ALPN_PROTOCOL) => {}
                            Some(other) => {
                                let alpn = String::from_utf8_lossy(other);
                                warn!(%alpn, "unexpected ALPN");
                                return Err(HandshakeError::ProtocolViolation);
                            }
                        }

                        let (_, tls_server_end_point) =
                            tls.cert_resolver.resolve(conn_info.server_name());

                        let tls = Stream::Tls {
                            tls: tls_stream,
                            tls_server_end_point,
                        };
                        (stream, msg) = PqStream::parse_startup(tls).await?;
                    } else {
                        if direct.is_some() {
                            // client sent us a ClientHello already, we can't do anything with it.
                            return Err(HandshakeError::ProtocolViolation);
                        }
                        msg = stream.reject_encryption().await?;
                    }
                }
                _ => return Err(HandshakeError::ProtocolViolation),
            },
            FeStartupPacket::GssEncRequest => match stream.get_ref() {
                Stream::Raw { .. } if !tried_gss => {
                    tried_gss = true;

                    // Currently, we don't support GSSAPI
                    msg = stream.reject_encryption().await?;
                }
                _ => return Err(HandshakeError::ProtocolViolation),
            },
            FeStartupPacket::StartupMessage { params, version }
                if PG_PROTOCOL_EARLIEST <= version && version <= PG_PROTOCOL_LATEST =>
            {
                // Check that the config has been consumed during upgrade
                // OR we didn't provide it at all (for dev purposes).
                if tls.is_some() {
                    Err(stream.throw_error(TlsRequired, None).await)?;
                }

                // feat-065: path α/β 分流。务必在 break 之前完成，
                // 这样后续 connect_to_compute 链路上的 span 都跑在正确的 trace_id 下。
                classify_and_attach_trace_context(ctx, &params);

                // This log highlights the start of the connection.
                // This contains useful information for debugging, not logged elsewhere, like role name and endpoint id.
                info!(
                    ?version,
                    ?params,
                    session_type = "normal",
                    "successful handshake"
                );
                break Ok(HandshakeData::Startup(stream, params));
            }
            // downgrade protocol version
            FeStartupPacket::StartupMessage { params, version }
                if version.major() == 3 && version > PG_PROTOCOL_LATEST =>
            {
                debug!(?version, "unsupported minor version");

                // no protocol extensions are supported.
                // <https://github.com/postgres/postgres/blob/ca481d3c9ab7bf69ff0c8d71ad3951d407f6a33c/src/backend/tcop/backend_startup.c#L744-L753>
                let mut unsupported = vec![];
                let mut supported = StartupMessageParams::default();

                for (k, v) in params.iter() {
                    if k.starts_with("_pq_.") {
                        unsupported.push(k);
                    } else {
                        supported.insert(k, v);
                    }
                }

                stream.write_message(BeMessage::NegotiateProtocolVersion {
                    version: PG_PROTOCOL_LATEST,
                    options: &unsupported,
                });
                stream.flush().await?;

                // feat-065: 同步 path α/β 分流（downgrade 路径同样需要）。
                classify_and_attach_trace_context(ctx, &supported);

                info!(
                    ?version,
                    ?params,
                    session_type = "normal",
                    "successful handshake; unsupported minor version requested"
                );
                break Ok(HandshakeData::Startup(stream, supported));
            }
            FeStartupPacket::StartupMessage { version, params } => {
                warn!(
                    ?version,
                    ?params,
                    session_type = "normal",
                    "unsuccessful handshake; unsupported version"
                );
                return Err(HandshakeError::ProtocolViolation);
            }
            FeStartupPacket::CancelRequest(cancel_key_data) => {
                info!(session_type = "cancellation", "successful handshake");
                break Ok(HandshakeData::Cancel(cancel_key_data));
            }
        }
    }
}

/// feat-065 path α/β 分流：从 startup params 看上游有没有 traceparent，决定 trace 起源。
///
/// path α：上游 app 已带 `-c neon.traceparent=...` 且 W3C 合法 →
/// 把当前 connect_request span 的 OTel parent 接到上游 SpanContext 上，
/// 这样 connect_request span 共享上游 trace_id。tracestate 标 `neon=root=app`。
///
/// path β：未携带 / 校验失败 → 用当前 span 自己分配的 trace_id（ADR-0011 ODD 承诺）。
/// tracestate 标 `neon=root=proxy`。
///
/// 注意：W3C spec §3.2 + ADR-0010 Q3 ——「sampling 决策只透传不重新决策」，所以这里
/// **不主动改 trace_flags**：path α 时直接用上游 flags；path β 时用 tracing-opentelemetry
/// layer 已经按本地 sampler 算出来的 flags。
fn classify_and_attach_trace_context(ctx: &RequestContext, params: &StartupMessageParams) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    // Step 1: 上游有效 traceparent → path α
    if let Some(upstream) = trace_context::extract_from_startup(params) {
        // 把当前 connect_request span 的 parent 接到上游 SpanContext。
        // tracing-opentelemetry 会把这个 remote context 当作 parent，让本 span
        // 继承 trace_id（但 span_id 是本地新分配的 child —— 满足 #29 "child span_id 由 proxy 生成"）。
        let otel_cx =
            opentelemetry::Context::current().with_remote_span_context(upstream.span_context.clone());
        ctx.span().set_parent(otel_cx);
        ctx.set_trace_context(upstream);
        debug!(path = "α", "feat-065 traceparent inherited from upstream");
        return;
    }

    // Step 2: 自生 path β。从当前 span 的 OTel context 抽 trace_id。
    if let Some(self_tc) = trace_context::from_current_span() {
        // 强制覆盖 root = proxy（from_current_span 默认就是 proxy，这里显式 reaffirm）。
        ctx.set_trace_context(TraceContext {
            span_context: self_tc.span_context,
            root: TraceRoot::Proxy,
        });
        debug!(path = "β", "feat-065 traceparent self-generated by proxy");
    } else {
        // OTel layer 未启用（OTEL_SDK_DISABLED=true 或单元测试场景），不强求。
        debug!("feat-065 no OTel context available; trace_context not set");
    }
}
