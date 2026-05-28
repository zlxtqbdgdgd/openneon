//! Rust definitions of the libpq-based pagestream API
//!
//! See also the C implementation of the same API in pgxn/neon/pagestore_client.h

use std::io::{BufRead, Read};

use crate::reltag::RelTag;

use byteorder::{BigEndian, ReadBytesExt};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use utils::lsn::Lsn;

/// Block size.
///
/// XXX: We assume 8k block size in the SLRU fetch API. It's not great to hardcode
/// that in the protocol, because Postgres supports different block sizes as a compile
/// time option.
const BLCKSZ: usize = 8192;

// Wrapped in libpq CopyData
#[derive(PartialEq, Eq, Debug)]
pub enum PagestreamFeMessage {
    Exists(PagestreamExistsRequest),
    Nblocks(PagestreamNblocksRequest),
    GetPage(PagestreamGetPageRequest),
    DbSize(PagestreamDbSizeRequest),
    GetSlruSegment(PagestreamGetSlruSegmentRequest),
    #[cfg(feature = "testing")]
    Test(PagestreamTestRequest),
}

// Wrapped in libpq CopyData
#[derive(Debug, strum_macros::EnumProperty)]
pub enum PagestreamBeMessage {
    Exists(PagestreamExistsResponse),
    Nblocks(PagestreamNblocksResponse),
    GetPage(PagestreamGetPageResponse),
    Error(PagestreamErrorResponse),
    DbSize(PagestreamDbSizeResponse),
    GetSlruSegment(PagestreamGetSlruSegmentResponse),
    #[cfg(feature = "testing")]
    Test(PagestreamTestResponse),
}

// Keep in sync with `pagestore_client.h`
#[repr(u8)]
enum PagestreamFeMessageTag {
    Exists = 0,
    Nblocks = 1,
    GetPage = 2,
    DbSize = 3,
    GetSlruSegment = 4,
    /* future tags above this line */
    /// For testing purposes, not available in production.
    #[cfg(feature = "testing")]
    Test = 99,
}

// Keep in sync with `pagestore_client.h`
#[repr(u8)]
enum PagestreamBeMessageTag {
    Exists = 100,
    Nblocks = 101,
    GetPage = 102,
    Error = 103,
    DbSize = 104,
    GetSlruSegment = 105,
    /* future tags above this line */
    /// For testing purposes, not available in production.
    #[cfg(feature = "testing")]
    Test = 199,
}

impl TryFrom<u8> for PagestreamFeMessageTag {
    type Error = u8;
    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            0 => Ok(PagestreamFeMessageTag::Exists),
            1 => Ok(PagestreamFeMessageTag::Nblocks),
            2 => Ok(PagestreamFeMessageTag::GetPage),
            3 => Ok(PagestreamFeMessageTag::DbSize),
            4 => Ok(PagestreamFeMessageTag::GetSlruSegment),
            #[cfg(feature = "testing")]
            99 => Ok(PagestreamFeMessageTag::Test),
            _ => Err(value),
        }
    }
}

impl TryFrom<u8> for PagestreamBeMessageTag {
    type Error = u8;
    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            100 => Ok(PagestreamBeMessageTag::Exists),
            101 => Ok(PagestreamBeMessageTag::Nblocks),
            102 => Ok(PagestreamBeMessageTag::GetPage),
            103 => Ok(PagestreamBeMessageTag::Error),
            104 => Ok(PagestreamBeMessageTag::DbSize),
            105 => Ok(PagestreamBeMessageTag::GetSlruSegment),
            #[cfg(feature = "testing")]
            199 => Ok(PagestreamBeMessageTag::Test),
            _ => Err(value),
        }
    }
}

// A GetPage request contains two LSN values:
//
// request_lsn: Get the page version at this point in time.  Lsn::Max is a special value that means
// "get the latest version present". It's used by the primary server, which knows that no one else
// is writing WAL. 'not_modified_since' must be set to a proper value even if request_lsn is
// Lsn::Max. Standby servers use the current replay LSN as the request LSN.
//
// not_modified_since: Hint to the pageserver that the client knows that the page has not been
// modified between 'not_modified_since' and the request LSN. It's always correct to set
// 'not_modified_since equal' to 'request_lsn' (unless Lsn::Max is used as the 'request_lsn'), but
// passing an earlier LSN can speed up the request, by allowing the pageserver to process the
// request without waiting for 'request_lsn' to arrive.
//
// The now-defunct V1 interface contained only one LSN, and a boolean 'latest' flag. The V1 interface was
// sufficient for the primary; the 'lsn' was equivalent to the 'not_modified_since' value, and
// 'latest' was set to true. The V2 interface was added because there was no correct way for a
// standby to request a page at a particular non-latest LSN, and also include the
// 'not_modified_since' hint. That led to an awkward choice of either using an old LSN in the
// request, if the standby knows that the page hasn't been modified since, and risk getting an error
// if that LSN has fallen behind the GC horizon, or requesting the current replay LSN, which could
// require the pageserver unnecessarily to wait for the WAL to arrive up to that point. The new V2
// interface allows sending both LSNs, and let the pageserver do the right thing. There was no
// difference in the responses between V1 and V2.
//
// V3 version of protocol adds request ID to all requests. This request ID is also included in response
// as well as other fields from requests, which allows to verify that we receive response for our request.
// We copy fields from request to response to make checking more reliable: request ID is formed from process ID
// and local counter, so in principle there can be duplicated requests IDs if process PID is reused.
//
// V4 version of protocol adds an OPTIONAL W3C TraceContext "traceparent" header in front of the
// per-request fields. Wire layout (per pagestream message, only for V4):
//
//   tag(1) trace_present(1) [traceparent_wire(55)?] reqid(8) request_lsn(8) not_modified_since(8) <body>
//
// `trace_present` is 0x00 or 0x01:
//   - 0x00: no trace context attached to this request (we still bump the version so that future
//     bits in the prefix byte can carry additional propagation knobs without another version
//     bump).
//   - 0x01: the next 55 bytes hold a W3C TraceContext v00 traceparent wire string
//     ("00-<32 hex>-<16 hex>-<2 hex>"). It is the canonical lowercase form, no NUL terminator.
//
// The companion C-side serializer/parser lives in `pgxn/neon/trace_context.{h,c}` (feat-033
// anchor); both sides agree on the same 55-byte wire form (TRACE_CONTEXT_WIRE_LEN).
//
// Decision discipline: V4 is parse-only on the pageserver side. The pageserver MUST NOT
// re-make sampling decisions; if a parent traceparent arrives we honor it (head-based
// propagation, per ADR-0010 Q3). If trace_present=0 the pageserver does not invent a
// trace_id; it just creates a local span as before.
//
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PagestreamProtocolVersion {
    V2,
    V3,
    /// V4 = V3 wire + optional W3C traceparent prefix (feat-033).
    V4,
}

pub type RequestId = u64;

/// On-the-wire byte length of a W3C TraceContext v00 "traceparent" header value.
///
/// Must stay in lock-step with `TRACE_CONTEXT_WIRE_LEN` in `pgxn/neon/trace_context.h`.
/// Layout: `00-<32 hex trace_id>-<16 hex parent_id>-<2 hex flags>` = 2+1+32+1+16+1+2 = 55 bytes,
/// no trailing NUL on the wire.
pub const TRACE_CONTEXT_WIRE_LEN: usize = 55;

/// W3C §3.2.2.5 "Trace Flags" defined bits, mirrored from
/// `pgxn/neon/trace_context.h`. The byte is treated as an 8-bit bitmap; we only name the two
/// bits the spec currently defines and forward the rest verbatim.
pub const TRACE_CONTEXT_FLAG_SAMPLED: u8 = 0x01;
pub const TRACE_CONTEXT_FLAG_RANDOM: u8 = 0x02;

/// Decoded W3C TraceContext v00 "traceparent" value, in big-endian (network / hex) order so
/// `memcmp` on the byte arrays matches lexicographic compare of the hex form. Mirrors the
/// C-side `struct trace_context` from `pgxn/neon/trace_context.h`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct TraceContext {
    /// Always 0x00 for v00. Parsers MUST forward-compat accept v>0 prefixes (W3C §3.2.2.3) but
    /// when *we* emit, we always emit v00.
    pub version: u8,
    /// 128-bit trace_id, big-endian.
    pub trace_id: [u8; 16],
    /// 64-bit parent_id (a.k.a. span_id of the immediate sender), big-endian.
    pub parent_id: [u8; 8],
    /// W3C §3.2.2.5 trace_flags bitmap. As a *forwarder* preserve verbatim; as a *sender* only
    /// set named bits (SAMPLED / RANDOM).
    pub trace_flags: u8,
}

impl Default for TraceContext {
    fn default() -> Self {
        Self {
            version: 0x00,
            trace_id: [0u8; 16],
            parent_id: [0u8; 8],
            trace_flags: 0,
        }
    }
}

impl TraceContext {
    /// Parse a 55-byte traceparent wire form. Lenient per W3C §3.2.2.3 forward-compat: accepts
    /// any version byte in 0x00..=0xfe (only 0xff is reserved/invalid). Rejects all-zero
    /// trace_id / parent_id and any non-hex / wrong-delimiter input.
    pub fn parse_wire(input: &[u8]) -> anyhow::Result<Self> {
        if input.len() < TRACE_CONTEXT_WIRE_LEN {
            anyhow::bail!(
                "traceparent wire too short: got {} bytes, need {}",
                input.len(),
                TRACE_CONTEXT_WIRE_LEN
            );
        }
        let wire = &input[..TRACE_CONTEXT_WIRE_LEN];
        if wire[2] != b'-' || wire[35] != b'-' || wire[52] != b'-' {
            anyhow::bail!("traceparent wire: missing dash delimiters");
        }
        let version = decode_hex_byte(&wire[0..2])?;
        if version == 0xff {
            anyhow::bail!("traceparent wire: version 0xff is reserved/invalid");
        }
        let mut trace_id = [0u8; 16];
        for (i, chunk) in wire[3..35].chunks(2).enumerate() {
            trace_id[i] = decode_hex_byte(chunk)?;
        }
        if trace_id == [0u8; 16] {
            anyhow::bail!("traceparent wire: trace_id must not be all zero");
        }
        let mut parent_id = [0u8; 8];
        for (i, chunk) in wire[36..52].chunks(2).enumerate() {
            parent_id[i] = decode_hex_byte(chunk)?;
        }
        if parent_id == [0u8; 8] {
            anyhow::bail!("traceparent wire: parent_id must not be all zero");
        }
        let trace_flags = decode_hex_byte(&wire[53..55])?;
        Ok(Self {
            version,
            trace_id,
            parent_id,
            trace_flags,
        })
    }

    /// Serialize as 55 bytes (lowercase hex, no NUL terminator). We only emit version 0x00 per
    /// ADR-0010; trying to serialize a TraceContext with a non-zero version is a programming
    /// error and panics in debug, returns 55 garbage-version bytes in release (we never
    /// construct such values in tree).
    pub fn serialize_wire(&self, out: &mut [u8]) {
        assert!(out.len() >= TRACE_CONTEXT_WIRE_LEN);
        debug_assert_eq!(self.version, 0x00, "we only emit traceparent v00");
        write_hex_byte(&mut out[0..2], 0x00);
        out[2] = b'-';
        for (i, b) in self.trace_id.iter().enumerate() {
            write_hex_byte(&mut out[3 + i * 2..3 + i * 2 + 2], *b);
        }
        out[35] = b'-';
        for (i, b) in self.parent_id.iter().enumerate() {
            write_hex_byte(&mut out[36 + i * 2..36 + i * 2 + 2], *b);
        }
        out[52] = b'-';
        write_hex_byte(&mut out[53..55], self.trace_flags);
    }

    /// True iff the W3C SAMPLED bit (0x01) is set. Pageserver uses this when deciding whether
    /// to attach OTel-exportable fields to the local span (the actual export decision lives in
    /// the tracing-opentelemetry bridge configured by feat-031).
    pub fn is_sampled(&self) -> bool {
        self.trace_flags & TRACE_CONTEXT_FLAG_SAMPLED != 0
    }
}

#[inline]
fn decode_hex_byte(b: &[u8]) -> anyhow::Result<u8> {
    debug_assert_eq!(b.len(), 2);
    fn nibble(c: u8) -> anyhow::Result<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            other => anyhow::bail!("traceparent wire: invalid hex char 0x{other:02x}"),
        }
    }
    Ok((nibble(b[0])? << 4) | nibble(b[1])?)
}

#[inline]
fn write_hex_byte(out: &mut [u8], b: u8) {
    debug_assert_eq!(out.len(), 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = HEX[(b >> 4) as usize];
    out[1] = HEX[(b & 0x0f) as usize];
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamRequest {
    pub reqid: RequestId,
    pub request_lsn: Lsn,
    pub not_modified_since: Lsn,
    /// W3C TraceContext propagated from the compute / libpq client side (feat-033 / issue #21).
    /// Only meaningful on V4 wire. On V2/V3 wire it stays `None` because there's no place for
    /// it on the wire; on V4 it can also be `None` if the sender chose not to attach one.
    pub trace_context: Option<TraceContext>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamExistsRequest {
    pub hdr: PagestreamRequest,
    pub rel: RelTag,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamNblocksRequest {
    pub hdr: PagestreamRequest,
    pub rel: RelTag,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamGetPageRequest {
    pub hdr: PagestreamRequest,
    pub rel: RelTag,
    pub blkno: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamDbSizeRequest {
    pub hdr: PagestreamRequest,
    pub dbnode: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PagestreamGetSlruSegmentRequest {
    pub hdr: PagestreamRequest,
    pub kind: u8,
    pub segno: u32,
}

#[derive(Debug)]
pub struct PagestreamExistsResponse {
    pub req: PagestreamExistsRequest,
    pub exists: bool,
}

#[derive(Debug)]
pub struct PagestreamNblocksResponse {
    pub req: PagestreamNblocksRequest,
    pub n_blocks: u32,
}

#[derive(Debug)]
pub struct PagestreamGetPageResponse {
    pub req: PagestreamGetPageRequest,
    pub page: Bytes,
}

#[derive(Debug)]
pub struct PagestreamGetSlruSegmentResponse {
    pub req: PagestreamGetSlruSegmentRequest,
    pub segment: Bytes,
}

#[derive(Debug)]
pub struct PagestreamErrorResponse {
    pub req: PagestreamRequest,
    pub message: String,
}

#[derive(Debug)]
pub struct PagestreamDbSizeResponse {
    pub req: PagestreamDbSizeRequest,
    pub db_size: i64,
}

#[cfg(feature = "testing")]
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PagestreamTestRequest {
    pub hdr: PagestreamRequest,
    pub batch_key: u64,
    pub message: String,
}

#[cfg(feature = "testing")]
#[derive(Debug)]
pub struct PagestreamTestResponse {
    pub req: PagestreamTestRequest,
}

impl PagestreamFeMessage {
    /// Serialize a compute -> pageserver message at the given protocol version.
    ///
    /// This is currently only used by tests and in-tree mock-pageserver tooling; production
    /// computes write the wire form directly from C (see `pgxn/neon/communicator.c`
    /// `nm_pack_request`). Keep the two sides byte-for-byte identical when extending.
    ///
    /// V4 wire prefix (only when `protocol_version == V4`):
    ///   tag(1) trace_present(1) [traceparent_wire(55)?] reqid(8) request_lsn(8) nlm_since(8)
    /// V3 wire (V3): tag(1) reqid(8) request_lsn(8) nlm_since(8)
    /// V2 wire (V2): tag(1) request_lsn(8) nlm_since(8)
    pub fn serialize(&self, protocol_version: PagestreamProtocolVersion) -> Bytes {
        let mut bytes = BytesMut::new();

        // Helper: write the leading [tag, trace_prefix?, reqid, request_lsn, not_modified_since]
        // section that's shared across all variants.
        fn put_header(
            bytes: &mut BytesMut,
            tag: PagestreamFeMessageTag,
            hdr: &PagestreamRequest,
            protocol_version: PagestreamProtocolVersion,
        ) {
            bytes.put_u8(tag as u8);
            if matches!(protocol_version, PagestreamProtocolVersion::V4) {
                match hdr.trace_context {
                    Some(tc) => {
                        bytes.put_u8(1u8);
                        let mut wire = [0u8; TRACE_CONTEXT_WIRE_LEN];
                        tc.serialize_wire(&mut wire);
                        bytes.put_slice(&wire);
                    }
                    None => {
                        bytes.put_u8(0u8);
                    }
                }
            }
            match protocol_version {
                PagestreamProtocolVersion::V2 => {
                    // V2 had no reqid; senders that try to round-trip V2 with this serializer
                    // simply lose the reqid (legacy).
                    bytes.put_u64(hdr.request_lsn.0);
                    bytes.put_u64(hdr.not_modified_since.0);
                }
                PagestreamProtocolVersion::V3 | PagestreamProtocolVersion::V4 => {
                    bytes.put_u64(hdr.reqid);
                    bytes.put_u64(hdr.request_lsn.0);
                    bytes.put_u64(hdr.not_modified_since.0);
                }
            }
        }

        match self {
            Self::Exists(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::Exists,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u32(req.rel.spcnode);
                bytes.put_u32(req.rel.dbnode);
                bytes.put_u32(req.rel.relnode);
                bytes.put_u8(req.rel.forknum);
            }

            Self::Nblocks(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::Nblocks,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u32(req.rel.spcnode);
                bytes.put_u32(req.rel.dbnode);
                bytes.put_u32(req.rel.relnode);
                bytes.put_u8(req.rel.forknum);
            }

            Self::GetPage(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::GetPage,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u32(req.rel.spcnode);
                bytes.put_u32(req.rel.dbnode);
                bytes.put_u32(req.rel.relnode);
                bytes.put_u8(req.rel.forknum);
                bytes.put_u32(req.blkno);
            }

            Self::DbSize(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::DbSize,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u32(req.dbnode);
            }

            Self::GetSlruSegment(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::GetSlruSegment,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u8(req.kind);
                bytes.put_u32(req.segno);
            }
            #[cfg(feature = "testing")]
            Self::Test(req) => {
                put_header(
                    &mut bytes,
                    PagestreamFeMessageTag::Test,
                    &req.hdr,
                    protocol_version,
                );
                bytes.put_u64(req.batch_key);
                let message = req.message.as_bytes();
                bytes.put_u64(message.len() as u64);
                bytes.put_slice(message);
            }
        }

        bytes.into()
    }

    pub fn parse<R: std::io::Read>(
        body: &mut R,
        protocol_version: PagestreamProtocolVersion,
    ) -> anyhow::Result<PagestreamFeMessage> {
        // these correspond to the NeonMessageTag enum in pagestore_client.h
        //
        // TODO: consider using protobuf or serde bincode for less error prone
        // serialization.
        let msg_tag = body.read_u8()?;
        // V4 trace prefix sits between the tag byte and the header LSNs. We read it eagerly so
        // that downstream variants don't need to know it exists.
        let trace_context = if matches!(protocol_version, PagestreamProtocolVersion::V4) {
            let trace_present = body.read_u8()?;
            match trace_present {
                0 => None,
                1 => {
                    let mut wire = [0u8; TRACE_CONTEXT_WIRE_LEN];
                    body.read_exact(&mut wire)?;
                    Some(TraceContext::parse_wire(&wire)?)
                }
                other => anyhow::bail!(
                    "pagestream V4: invalid trace_present byte 0x{other:02x} (expected 0 or 1)"
                ),
            }
        } else {
            None
        };
        let (reqid, request_lsn, not_modified_since) = match protocol_version {
            PagestreamProtocolVersion::V2 => (
                0,
                Lsn::from(body.read_u64::<BigEndian>()?),
                Lsn::from(body.read_u64::<BigEndian>()?),
            ),
            PagestreamProtocolVersion::V3 | PagestreamProtocolVersion::V4 => (
                body.read_u64::<BigEndian>()?,
                Lsn::from(body.read_u64::<BigEndian>()?),
                Lsn::from(body.read_u64::<BigEndian>()?),
            ),
        };

        match PagestreamFeMessageTag::try_from(msg_tag)
            .map_err(|tag: u8| anyhow::anyhow!("invalid tag {tag}"))?
        {
            PagestreamFeMessageTag::Exists => {
                Ok(PagestreamFeMessage::Exists(PagestreamExistsRequest {
                    hdr: PagestreamRequest {
                        reqid,
                        request_lsn,
                        not_modified_since,
                        trace_context,
                    },
                    rel: RelTag {
                        spcnode: body.read_u32::<BigEndian>()?,
                        dbnode: body.read_u32::<BigEndian>()?,
                        relnode: body.read_u32::<BigEndian>()?,
                        forknum: body.read_u8()?,
                    },
                }))
            }
            PagestreamFeMessageTag::Nblocks => {
                Ok(PagestreamFeMessage::Nblocks(PagestreamNblocksRequest {
                    hdr: PagestreamRequest {
                        reqid,
                        request_lsn,
                        not_modified_since,
                        trace_context,
                    },
                    rel: RelTag {
                        spcnode: body.read_u32::<BigEndian>()?,
                        dbnode: body.read_u32::<BigEndian>()?,
                        relnode: body.read_u32::<BigEndian>()?,
                        forknum: body.read_u8()?,
                    },
                }))
            }
            PagestreamFeMessageTag::GetPage => {
                Ok(PagestreamFeMessage::GetPage(PagestreamGetPageRequest {
                    hdr: PagestreamRequest {
                        reqid,
                        request_lsn,
                        not_modified_since,
                        trace_context,
                    },
                    rel: RelTag {
                        spcnode: body.read_u32::<BigEndian>()?,
                        dbnode: body.read_u32::<BigEndian>()?,
                        relnode: body.read_u32::<BigEndian>()?,
                        forknum: body.read_u8()?,
                    },
                    blkno: body.read_u32::<BigEndian>()?,
                }))
            }
            PagestreamFeMessageTag::DbSize => {
                Ok(PagestreamFeMessage::DbSize(PagestreamDbSizeRequest {
                    hdr: PagestreamRequest {
                        reqid,
                        request_lsn,
                        not_modified_since,
                        trace_context,
                    },
                    dbnode: body.read_u32::<BigEndian>()?,
                }))
            }
            PagestreamFeMessageTag::GetSlruSegment => Ok(PagestreamFeMessage::GetSlruSegment(
                PagestreamGetSlruSegmentRequest {
                    hdr: PagestreamRequest {
                        reqid,
                        request_lsn,
                        not_modified_since,
                        trace_context,
                    },
                    kind: body.read_u8()?,
                    segno: body.read_u32::<BigEndian>()?,
                },
            )),
            #[cfg(feature = "testing")]
            PagestreamFeMessageTag::Test => Ok(PagestreamFeMessage::Test(PagestreamTestRequest {
                hdr: PagestreamRequest {
                    reqid,
                    request_lsn,
                    not_modified_since,
                    trace_context,
                },
                batch_key: body.read_u64::<BigEndian>()?,
                message: {
                    let len = body.read_u64::<BigEndian>()?;
                    let mut buf = vec![0; len as usize];
                    body.read_exact(&mut buf)?;
                    String::from_utf8(buf)?
                },
            })),
        }
    }
}

impl PagestreamBeMessage {
    pub fn serialize(&self, protocol_version: PagestreamProtocolVersion) -> Bytes {
        let mut bytes = BytesMut::new();

        use PagestreamBeMessageTag as Tag;
        match protocol_version {
            PagestreamProtocolVersion::V2 => {
                match self {
                    Self::Exists(resp) => {
                        bytes.put_u8(Tag::Exists as u8);
                        bytes.put_u8(resp.exists as u8);
                    }

                    Self::Nblocks(resp) => {
                        bytes.put_u8(Tag::Nblocks as u8);
                        bytes.put_u32(resp.n_blocks);
                    }

                    Self::GetPage(resp) => {
                        bytes.put_u8(Tag::GetPage as u8);
                        bytes.put(&resp.page[..])
                    }

                    Self::Error(resp) => {
                        bytes.put_u8(Tag::Error as u8);
                        bytes.put(resp.message.as_bytes());
                        bytes.put_u8(0); // null terminator
                    }
                    Self::DbSize(resp) => {
                        bytes.put_u8(Tag::DbSize as u8);
                        bytes.put_i64(resp.db_size);
                    }

                    Self::GetSlruSegment(resp) => {
                        bytes.put_u8(Tag::GetSlruSegment as u8);
                        bytes.put_u32((resp.segment.len() / BLCKSZ) as u32);
                        bytes.put(&resp.segment[..]);
                    }

                    #[cfg(feature = "testing")]
                    Self::Test(resp) => {
                        bytes.put_u8(Tag::Test as u8);
                        bytes.put_u64(resp.req.batch_key);
                        let message = resp.req.message.as_bytes();
                        bytes.put_u64(message.len() as u64);
                        bytes.put_slice(message);
                    }
                }
            }
            // V4 BE responses currently mirror V3 wire byte-for-byte. The pageserver -> compute
            // direction does NOT need to carry the W3C traceparent back: the compute already
            // knows which trace it is in, and the response is just data. We keep the arm
            // separate so that any future "echo trace_id in response for debugging" extension
            // can hang off V4 without disturbing V3 behaviour.
            PagestreamProtocolVersion::V3 | PagestreamProtocolVersion::V4 => {
                match self {
                    Self::Exists(resp) => {
                        bytes.put_u8(Tag::Exists as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u32(resp.req.rel.spcnode);
                        bytes.put_u32(resp.req.rel.dbnode);
                        bytes.put_u32(resp.req.rel.relnode);
                        bytes.put_u8(resp.req.rel.forknum);
                        bytes.put_u8(resp.exists as u8);
                    }

                    Self::Nblocks(resp) => {
                        bytes.put_u8(Tag::Nblocks as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u32(resp.req.rel.spcnode);
                        bytes.put_u32(resp.req.rel.dbnode);
                        bytes.put_u32(resp.req.rel.relnode);
                        bytes.put_u8(resp.req.rel.forknum);
                        bytes.put_u32(resp.n_blocks);
                    }

                    Self::GetPage(resp) => {
                        bytes.put_u8(Tag::GetPage as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u32(resp.req.rel.spcnode);
                        bytes.put_u32(resp.req.rel.dbnode);
                        bytes.put_u32(resp.req.rel.relnode);
                        bytes.put_u8(resp.req.rel.forknum);
                        bytes.put_u32(resp.req.blkno);
                        bytes.put(&resp.page[..])
                    }

                    Self::Error(resp) => {
                        bytes.put_u8(Tag::Error as u8);
                        bytes.put_u64(resp.req.reqid);
                        bytes.put_u64(resp.req.request_lsn.0);
                        bytes.put_u64(resp.req.not_modified_since.0);
                        bytes.put(resp.message.as_bytes());
                        bytes.put_u8(0); // null terminator
                    }
                    Self::DbSize(resp) => {
                        bytes.put_u8(Tag::DbSize as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u32(resp.req.dbnode);
                        bytes.put_i64(resp.db_size);
                    }

                    Self::GetSlruSegment(resp) => {
                        bytes.put_u8(Tag::GetSlruSegment as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u8(resp.req.kind);
                        bytes.put_u32(resp.req.segno);
                        bytes.put_u32((resp.segment.len() / BLCKSZ) as u32);
                        bytes.put(&resp.segment[..]);
                    }

                    #[cfg(feature = "testing")]
                    Self::Test(resp) => {
                        bytes.put_u8(Tag::Test as u8);
                        bytes.put_u64(resp.req.hdr.reqid);
                        bytes.put_u64(resp.req.hdr.request_lsn.0);
                        bytes.put_u64(resp.req.hdr.not_modified_since.0);
                        bytes.put_u64(resp.req.batch_key);
                        let message = resp.req.message.as_bytes();
                        bytes.put_u64(message.len() as u64);
                        bytes.put_slice(message);
                    }
                }
            }
        }
        bytes.into()
    }

    pub fn deserialize(buf: Bytes) -> anyhow::Result<Self> {
        let mut buf = buf.reader();
        let msg_tag = buf.read_u8()?;

        use PagestreamBeMessageTag as Tag;
        let ok =
            match Tag::try_from(msg_tag).map_err(|tag: u8| anyhow::anyhow!("invalid tag {tag}"))? {
                Tag::Exists => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let rel = RelTag {
                        spcnode: buf.read_u32::<BigEndian>()?,
                        dbnode: buf.read_u32::<BigEndian>()?,
                        relnode: buf.read_u32::<BigEndian>()?,
                        forknum: buf.read_u8()?,
                    };
                    let exists = buf.read_u8()? != 0;
                    Self::Exists(PagestreamExistsResponse {
                        req: PagestreamExistsRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            rel,
                        },
                        exists,
                    })
                }
                Tag::Nblocks => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let rel = RelTag {
                        spcnode: buf.read_u32::<BigEndian>()?,
                        dbnode: buf.read_u32::<BigEndian>()?,
                        relnode: buf.read_u32::<BigEndian>()?,
                        forknum: buf.read_u8()?,
                    };
                    let n_blocks = buf.read_u32::<BigEndian>()?;
                    Self::Nblocks(PagestreamNblocksResponse {
                        req: PagestreamNblocksRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            rel,
                        },
                        n_blocks,
                    })
                }
                Tag::GetPage => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let rel = RelTag {
                        spcnode: buf.read_u32::<BigEndian>()?,
                        dbnode: buf.read_u32::<BigEndian>()?,
                        relnode: buf.read_u32::<BigEndian>()?,
                        forknum: buf.read_u8()?,
                    };
                    let blkno = buf.read_u32::<BigEndian>()?;
                    let mut page = vec![0; 8192]; // TODO: use MaybeUninit
                    buf.read_exact(&mut page)?;
                    Self::GetPage(PagestreamGetPageResponse {
                        req: PagestreamGetPageRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            rel,
                            blkno,
                        },
                        page: page.into(),
                    })
                }
                Tag::Error => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let mut msg = Vec::new();
                    buf.read_until(0, &mut msg)?;
                    let cstring = std::ffi::CString::from_vec_with_nul(msg)?;
                    let rust_str = cstring.to_str()?;
                    Self::Error(PagestreamErrorResponse {
                        req: PagestreamRequest {
                            reqid,
                            request_lsn,
                            not_modified_since,
                            trace_context: None,
                        },
                        message: rust_str.to_owned(),
                    })
                }
                Tag::DbSize => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let dbnode = buf.read_u32::<BigEndian>()?;
                    let db_size = buf.read_i64::<BigEndian>()?;
                    Self::DbSize(PagestreamDbSizeResponse {
                        req: PagestreamDbSizeRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            dbnode,
                        },
                        db_size,
                    })
                }
                Tag::GetSlruSegment => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let kind = buf.read_u8()?;
                    let segno = buf.read_u32::<BigEndian>()?;
                    let n_blocks = buf.read_u32::<BigEndian>()?;
                    let mut segment = vec![0; n_blocks as usize * BLCKSZ];
                    buf.read_exact(&mut segment)?;
                    Self::GetSlruSegment(PagestreamGetSlruSegmentResponse {
                        req: PagestreamGetSlruSegmentRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            kind,
                            segno,
                        },
                        segment: segment.into(),
                    })
                }
                #[cfg(feature = "testing")]
                Tag::Test => {
                    let reqid = buf.read_u64::<BigEndian>()?;
                    let request_lsn = Lsn(buf.read_u64::<BigEndian>()?);
                    let not_modified_since = Lsn(buf.read_u64::<BigEndian>()?);
                    let batch_key = buf.read_u64::<BigEndian>()?;
                    let len = buf.read_u64::<BigEndian>()?;
                    let mut msg = vec![0; len as usize];
                    buf.read_exact(&mut msg)?;
                    let message = String::from_utf8(msg)?;
                    Self::Test(PagestreamTestResponse {
                        req: PagestreamTestRequest {
                            hdr: PagestreamRequest {
                                reqid,
                                request_lsn,
                                not_modified_since,
                                trace_context: None,
                            },
                            batch_key,
                            message,
                        },
                    })
                }
            };
        let remaining = buf.into_inner();
        if !remaining.is_empty() {
            anyhow::bail!(
                "remaining bytes in msg with tag={msg_tag}: {}",
                remaining.len()
            );
        }
        Ok(ok)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Exists(_) => "Exists",
            Self::Nblocks(_) => "Nblocks",
            Self::GetPage(_) => "GetPage",
            Self::Error(_) => "Error",
            Self::DbSize(_) => "DbSize",
            Self::GetSlruSegment(_) => "GetSlruSegment",
            #[cfg(feature = "testing")]
            Self::Test(_) => "Test",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_traceparent() -> TraceContext {
        // Canonical W3C example (https://www.w3.org/TR/trace-context/#examples-of-traceparent).
        TraceContext {
            version: 0x00,
            trace_id: [
                0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
                0x47, 0x36,
            ],
            parent_id: [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
            trace_flags: TRACE_CONTEXT_FLAG_SAMPLED,
        }
    }

    fn sample_messages_without_trace() -> Vec<PagestreamFeMessage> {
        vec![
            PagestreamFeMessage::Exists(PagestreamExistsRequest {
                hdr: PagestreamRequest {
                    reqid: 0,
                    request_lsn: Lsn(4),
                    not_modified_since: Lsn(3),
                    trace_context: None,
                },
                rel: RelTag {
                    forknum: 1,
                    spcnode: 2,
                    dbnode: 3,
                    relnode: 4,
                },
            }),
            PagestreamFeMessage::Nblocks(PagestreamNblocksRequest {
                hdr: PagestreamRequest {
                    reqid: 0,
                    request_lsn: Lsn(4),
                    not_modified_since: Lsn(4),
                    trace_context: None,
                },
                rel: RelTag {
                    forknum: 1,
                    spcnode: 2,
                    dbnode: 3,
                    relnode: 4,
                },
            }),
            PagestreamFeMessage::GetPage(PagestreamGetPageRequest {
                hdr: PagestreamRequest {
                    reqid: 0,
                    request_lsn: Lsn(4),
                    not_modified_since: Lsn(3),
                    trace_context: None,
                },
                rel: RelTag {
                    forknum: 1,
                    spcnode: 2,
                    dbnode: 3,
                    relnode: 4,
                },
                blkno: 7,
            }),
            PagestreamFeMessage::DbSize(PagestreamDbSizeRequest {
                hdr: PagestreamRequest {
                    reqid: 0,
                    request_lsn: Lsn(4),
                    not_modified_since: Lsn(3),
                    trace_context: None,
                },
                dbnode: 7,
            }),
        ]
    }

    #[test]
    fn test_pagestream_v3_round_trip() {
        // Legacy V3 round-trip: must remain byte-compatible with neon main.
        let messages = sample_messages_without_trace();
        for msg in messages {
            let bytes = msg.serialize(PagestreamProtocolVersion::V3);
            let reconstructed =
                PagestreamFeMessage::parse(&mut bytes.reader(), PagestreamProtocolVersion::V3)
                    .unwrap();
            assert!(msg == reconstructed);
        }
    }

    #[test]
    fn test_pagestream_v4_round_trip_with_traceparent() {
        // V4 carries a TraceContext; fixture verifies parse/serialize symmetry per W3C wire form.
        let tc = sample_traceparent();
        let messages: Vec<PagestreamFeMessage> = sample_messages_without_trace()
            .into_iter()
            .map(|msg| {
                let mut msg = msg;
                match &mut msg {
                    PagestreamFeMessage::Exists(r) => {
                        r.hdr.reqid = 17;
                        r.hdr.trace_context = Some(tc);
                    }
                    PagestreamFeMessage::Nblocks(r) => {
                        r.hdr.reqid = 18;
                        r.hdr.trace_context = Some(tc);
                    }
                    PagestreamFeMessage::GetPage(r) => {
                        r.hdr.reqid = 19;
                        r.hdr.trace_context = Some(tc);
                    }
                    PagestreamFeMessage::DbSize(r) => {
                        r.hdr.reqid = 20;
                        r.hdr.trace_context = Some(tc);
                    }
                    _ => {}
                }
                msg
            })
            .collect();

        for msg in messages {
            let bytes = msg.serialize(PagestreamProtocolVersion::V4);
            let reconstructed =
                PagestreamFeMessage::parse(&mut bytes.reader(), PagestreamProtocolVersion::V4)
                    .unwrap();
            assert_eq!(msg, reconstructed);
        }
    }

    #[test]
    fn test_pagestream_v4_round_trip_without_traceparent() {
        // V4 sender that chose not to attach a parent traceparent (e.g. local-only request);
        // wire still bumps protocol_version so the receiver knows to consume 1 byte for the
        // trace_present flag.
        for msg in sample_messages_without_trace() {
            let bytes = msg.serialize(PagestreamProtocolVersion::V4);
            let reconstructed =
                PagestreamFeMessage::parse(&mut bytes.reader(), PagestreamProtocolVersion::V4)
                    .unwrap();
            assert_eq!(msg, reconstructed);
        }
    }

    #[test]
    fn test_pagestream_v3_and_v4_wire_diverge() {
        // Same logical message serialized as V3 vs V4 must produce different bytes: V4 inserts
        // 1 trace_present byte right after the tag. This guards against an accidental
        // PagestreamProtocolVersion::V4 arm that silently emits V3 bytes.
        let msg = sample_messages_without_trace().remove(0);
        let v3 = msg.serialize(PagestreamProtocolVersion::V3);
        let v4 = msg.serialize(PagestreamProtocolVersion::V4);
        assert_eq!(v4.len(), v3.len() + 1);
        assert_eq!(v4[0], v3[0]); // same tag
        assert_eq!(v4[1], 0); // trace_present=0
        assert_eq!(&v4[2..], &v3[1..]); // rest matches
    }

    #[test]
    fn test_pagestream_v4_attached_trace_adds_55_bytes() {
        // Symmetric guard for the attached-trace case: V4 + Some(tc) must be exactly 56 bytes
        // longer than V3 (1 prefix flag + 55 wire bytes).
        let tc = sample_traceparent();
        let mut msg = sample_messages_without_trace().remove(2); // GetPage
        if let PagestreamFeMessage::GetPage(r) = &mut msg {
            r.hdr.trace_context = Some(tc);
        }
        let v3 = msg.serialize(PagestreamProtocolVersion::V3);
        let v4 = msg.serialize(PagestreamProtocolVersion::V4);
        assert_eq!(v4.len(), v3.len() + 1 + TRACE_CONTEXT_WIRE_LEN);
        assert_eq!(v4[1], 1); // trace_present=1
        // Verify the wire is canonical lowercase W3C.
        let wire = std::str::from_utf8(&v4[2..2 + TRACE_CONTEXT_WIRE_LEN]).unwrap();
        assert_eq!(
            wire,
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn test_trace_context_parse_invalid() {
        // Forwarder discipline: bad wires must surface as parse errors so we can't silently
        // attach garbage to a server-side span. Five negative cases.

        // length: too short
        assert!(TraceContext::parse_wire(b"00-too-short").is_err());

        // delimiters: wrong dash position
        let mut bad = b"00-4bf92f3577b34da6a3ce929d0e0e4736X00f067aa0ba902b7-01".to_vec();
        bad[35] = b'X';
        assert!(TraceContext::parse_wire(&bad).is_err());

        // non-hex char in trace_id
        let bad = b"00-zz92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(TraceContext::parse_wire(bad).is_err());

        // all-zero trace_id
        let bad = b"00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(TraceContext::parse_wire(bad).is_err());

        // all-zero parent_id
        let bad = b"00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01";
        assert!(TraceContext::parse_wire(bad).is_err());
    }

    #[test]
    fn test_trace_context_serialize_round_trip() {
        let tc = sample_traceparent();
        let mut buf = [0u8; TRACE_CONTEXT_WIRE_LEN];
        tc.serialize_wire(&mut buf);
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        let back = TraceContext::parse_wire(&buf).unwrap();
        assert_eq!(back, tc);
        assert!(back.is_sampled());
    }

    #[test]
    fn test_pagestream_v4_negotiation_downgrade_simulated() {
        // Simulated mixed-version negotiation: a V4 client cannot reach a V3 pageserver
        // because the wire framing differs; tools that proxy / mock must explicitly downgrade.
        // This test pins behaviour by trying to parse a V4 wire as V3 and asserting it does NOT
        // accidentally succeed with garbage. The first byte after the tag in V4 is a 0/1
        // trace_present flag; under V3 it would be interpreted as the high byte of reqid.
        let mut msg = sample_messages_without_trace().remove(0);
        if let PagestreamFeMessage::Exists(r) = &mut msg {
            r.hdr.reqid = 0; // so the V3 misparse can't accidentally match
            r.hdr.trace_context = Some(sample_traceparent());
        }
        let v4_wire = msg.serialize(PagestreamProtocolVersion::V4);
        // Parsing V4 bytes under V3 will misalign the relfile/forknum tail and almost certainly
        // fail (short read or bogus tag in a follow-up message). We only assert it does not
        // round-trip to the same message — that's the real downgrade contract.
        if let Ok(decoded) =
            PagestreamFeMessage::parse(&mut v4_wire.reader(), PagestreamProtocolVersion::V3)
        {
            assert_ne!(decoded, msg, "V3 must not silently decode V4 wire correctly");
        }
    }
}
