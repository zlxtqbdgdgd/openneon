/*-------------------------------------------------------------------------
 *
 * trace_context.c
 *	  W3C TraceContext (traceparent header v00) serializer / parser.
 *
 * See trace_context.h for the API contract and validation rules.
 *
 * Zero external dependencies on purpose: this file is linked from libpq,
 * the neon backend extension, and standalone unit tests. It must NOT
 * include postgres.h or anything from src/include/.
 *
 *-------------------------------------------------------------------------
 */
#include "trace_context.h"

#include <string.h>

/*
 * Offsets inside the 55-byte traceparent value:
 *	  pos 0..1   version (2 hex)
 *	  pos 2	     '-'
 *	  pos 3..34  trace_id (32 hex)
 *	  pos 35     '-'
 *	  pos 36..51 parent_id (16 hex)
 *	  pos 52     '-'
 *	  pos 53..54 flags (2 hex)
 */
#define OFF_VERSION		0
#define OFF_DASH1		2
#define OFF_TRACE_ID	3
#define OFF_DASH2		35
#define OFF_PARENT_ID	36
#define OFF_DASH3		52
#define OFF_FLAGS		53

#define LEN_TRACE_ID_HEX	32
#define LEN_PARENT_ID_HEX	16

/*
 * Decode a single hex digit. Returns -1 on invalid input.
 *
 * Parameter type is 'unsigned char' (not plain 'char') so callers don't
 * trigger implementation-defined sign-extension when feeding bytes
 * sourced from a possibly-signed 'char *' buffer. Callers should pass
 * '(unsigned char) input[i]' explicitly to make the cast visible.
 */
static int
hex_nibble(unsigned char c)
{
	if (c >= '0' && c <= '9')
		return c - '0';
	if (c >= 'a' && c <= 'f')
		return 10 + (c - 'a');
	if (c >= 'A' && c <= 'F')
		return 10 + (c - 'A');
	return -1;
}

/*
 * Decode hex_len hex characters from src into dst (dst_len bytes).
 * hex_len must equal 2 * dst_len. Returns true on success.
 */
static bool
hex_decode(const char *src, size_t hex_len, uint8_t *dst, size_t dst_len)
{
	if (hex_len != 2 * dst_len)
		return false;

	for (size_t i = 0; i < dst_len; i++)
	{
		int			hi = hex_nibble((unsigned char) src[2 * i]);
		int			lo = hex_nibble((unsigned char) src[2 * i + 1]);

		if (hi < 0 || lo < 0)
			return false;
		dst[i] = (uint8_t) ((hi << 4) | lo);
	}
	return true;
}

/* Lowercase hex alphabet (W3C requires lowercase on the wire). */
static const char hex_lower[] = "0123456789abcdef";

static void
hex_encode(const uint8_t *src, size_t src_len, char *dst)
{
	for (size_t i = 0; i < src_len; i++)
	{
		dst[2 * i] = hex_lower[(src[i] >> 4) & 0x0f];
		dst[2 * i + 1] = hex_lower[src[i] & 0x0f];
	}
}

static bool
is_all_zero(const uint8_t *buf, size_t len)
{
	for (size_t i = 0; i < len; i++)
		if (buf[i] != 0)
			return false;
	return true;
}

/*
 * Shared parsing core. Validates the 55-byte wire grammar (length,
 * NUL terminator, dashes, hex digits, all-zero id rejection) and
 * decodes the prefix fields. Version-policy decisions live in the
 * trampoline wrappers below (strict vs lenient).
 */
static bool
parse_common(const char *input, struct trace_context *out)
{
	if (input == NULL || out == NULL)
		return false;

	/*
	 * Length check: must be exactly TRACE_CONTEXT_WIRE_LEN bytes, and
	 * the next byte must be NUL. Longer inputs are rejected to keep the
	 * wire grammar tight (callers that want to tolerate trailing data
	 * should slice first).
	 */
	for (size_t i = 0; i < TRACE_CONTEXT_WIRE_LEN; i++)
		if (input[i] == '\0')
			return false;
	if (input[TRACE_CONTEXT_WIRE_LEN] != '\0')
		return false;

	/* Dashes at the right places. */
	if (input[OFF_DASH1] != '-' ||
		input[OFF_DASH2] != '-' ||
		input[OFF_DASH3] != '-')
		return false;

	uint8_t		version;

	if (!hex_decode(input + OFF_VERSION, 2, &version, 1))
		return false;

	uint8_t		trace_id[16];
	uint8_t		parent_id[8];
	uint8_t		flags;

	if (!hex_decode(input + OFF_TRACE_ID, LEN_TRACE_ID_HEX,
					trace_id, sizeof(trace_id)))
		return false;
	if (!hex_decode(input + OFF_PARENT_ID, LEN_PARENT_ID_HEX,
					parent_id, sizeof(parent_id)))
		return false;
	if (!hex_decode(input + OFF_FLAGS, 2, &flags, 1))
		return false;

	/* W3C §3.2.2.2 / §3.2.2.3: all-zero ids are invalid. */
	if (is_all_zero(trace_id, sizeof(trace_id)))
		return false;
	if (is_all_zero(parent_id, sizeof(parent_id)))
		return false;

	out->version = version;
	memcpy(out->trace_id, trace_id, sizeof(out->trace_id));
	memcpy(out->parent_id, parent_id, sizeof(out->parent_id));
	out->trace_flags = flags;
	return true;
}

bool
trace_context_parse(const char *input, struct trace_context *out)
{
	if (!parse_common(input, out))
		return false;

	/*
	 * Strict v00: reject anything else. Forward-compat callers should
	 * use trace_context_parse_lenient() instead (W3C §3.2.2.3).
	 */
	if (out->version != 0x00)
		return false;
	return true;
}

bool
trace_context_parse_lenient(const char *input, struct trace_context *out)
{
	if (!parse_common(input, out))
		return false;

	/*
	 * W3C §3.2.2.3: "Vendors MUST NOT reject a value due to an
	 * unrecognized version." Accept any version 0x00..0xfe; only the
	 * spec-reserved 0xff sentinel is rejected as invalid.
	 */
	if (out->version == 0xff)
		return false;
	return true;
}

int
trace_context_serialize(const struct trace_context *in,
						char *buf, size_t buflen)
{
	if (in == NULL || buf == NULL)
		return -1;
	if (buflen < TRACE_CONTEXT_BUF_SIZE)
		return -1;

	/*
	 * We only emit W3C v00 (per ADR-0010). Refuse to serialize a
	 * trace_context whose version field has been set to anything else;
	 * otherwise the caller would silently produce a wire value whose
	 * leading "00" disagrees with its semantic version.
	 */
	if (in->version != 0x00)
		return -1;

	/* Version is always emitted as "00". */
	buf[OFF_VERSION] = '0';
	buf[OFF_VERSION + 1] = '0';
	buf[OFF_DASH1] = '-';
	hex_encode(in->trace_id, sizeof(in->trace_id), buf + OFF_TRACE_ID);
	buf[OFF_DASH2] = '-';
	hex_encode(in->parent_id, sizeof(in->parent_id), buf + OFF_PARENT_ID);
	buf[OFF_DASH3] = '-';
	buf[OFF_FLAGS] = hex_lower[(in->trace_flags >> 4) & 0x0f];
	buf[OFF_FLAGS + 1] = hex_lower[in->trace_flags & 0x0f];
	buf[TRACE_CONTEXT_WIRE_LEN] = '\0';
	return TRACE_CONTEXT_WIRE_LEN;
}
