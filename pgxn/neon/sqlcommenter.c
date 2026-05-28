/*-------------------------------------------------------------------------
 *
 * sqlcommenter.c
 *	  SQLCommenter v1 lexer / injector. See sqlcommenter.h for API contract.
 *
 * Implementation strategy (matches sqlcommenter spec §"Format" and the
 * existing sqlcommenter-python / sqlcommenter-ruby behaviour):
 *
 *	  1. Locate the trailing star-slash of the SQL. If the candidate is
 *	     not the very last non-whitespace token of the input, there is no
 *	     sqlcommenter block (spec §"Output prepending or appending"
 *	     mandates trailing position).
 *	  2. Walk backwards (handling nested-comment depth per PostgreSQL
 *	     lexer semantics; PG's block comments are nested-aware via
 *	     slash-star and star-slash) until the matching opener is found.
 *	  3. Inside the block, tokenize `key='value'` pairs separated by `,`.
 *	     Whitespace around `=` and `,` is tolerated.
 *	  4. For the `traceparent` key, URL-decode the value and feed it to
 *	     trace_context_parse_lenient() (feat-033 anchor).
 *
 * Zero external dependencies on purpose: linked from libpq, the backend
 * extension hook (#23), and walproposer (#24) without dragging in
 * postgres.h. Memory uses bare malloc()/free() so the unit tests can run
 * without a postmaster.
 *
 *-------------------------------------------------------------------------
 */
#include "sqlcommenter.h"

#include <ctype.h>
#include <stdlib.h>
#include <string.h>

/* ---------------------- URL-decode helpers ----------------------------- */

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
 * URL-decode `src` of length `src_len` into a freshly-malloc'd
 * NUL-terminated buffer. Returns NULL on invalid %XX escape or alloc
 * failure. `+` is NOT translated to space (sqlcommenter values use raw
 * percent-encoding, not form-encoding).
 */
static char *
url_decode(const char *src, size_t src_len)
{
	char	   *out = (char *) malloc(src_len + 1);

	if (out == NULL)
		return NULL;

	size_t		o = 0;

	for (size_t i = 0; i < src_len; i++)
	{
		unsigned char c = (unsigned char) src[i];

		if (c == '%')
		{
			if (i + 2 >= src_len)
			{
				free(out);
				return NULL;
			}
			int			hi = hex_nibble((unsigned char) src[i + 1]);
			int			lo = hex_nibble((unsigned char) src[i + 2]);

			if (hi < 0 || lo < 0)
			{
				free(out);
				return NULL;
			}
			out[o++] = (char) ((hi << 4) | lo);
			i += 2;
		}
		else
		{
			out[o++] = (char) c;
		}
	}
	out[o] = '\0';
	return out;
}

/* ---------------------- URL-encode (for inject) ------------------------ */

static const char hex_lower[] = "0123456789abcdef";

/*
 * URL-encode `src` into a malloc'd NUL-terminated buffer. Characters
 * outside the unreserved set (ALPHA / DIGIT / `-` / `_` / `.` / `~`)
 * become %XX. We are intentionally conservative: every byte outside
 * unreserved is percent-encoded so the produced comment cannot break
 * out of its `'...'` single-quoted value.
 */
static char *
url_encode(const char *src)
{
	if (src == NULL)
		return NULL;

	size_t		src_len = strlen(src);
	char	   *out = (char *) malloc(src_len * 3 + 1);

	if (out == NULL)
		return NULL;

	size_t		o = 0;

	for (size_t i = 0; i < src_len; i++)
	{
		unsigned char c = (unsigned char) src[i];
		bool		unreserved =
			(c >= 'A' && c <= 'Z') ||
			(c >= 'a' && c <= 'z') ||
			(c >= '0' && c <= '9') ||
			c == '-' || c == '_' || c == '.' || c == '~';

		if (unreserved)
		{
			out[o++] = (char) c;
		}
		else
		{
			out[o++] = '%';
			out[o++] = hex_lower[(c >> 4) & 0x0f];
			out[o++] = hex_lower[c & 0x0f];
		}
	}
	out[o] = '\0';
	return out;
}

/* ---------------------- trailing comment locator ----------------------- */

/*
 * Find the leading `/ *` of the trailing block comment of `sql`. Returns
 * the index of the `/` byte on success, or SIZE_MAX if there is no
 * trailing block comment (handles nested-block-comment depth per PG
 * lexer semantics, where `/ * / *` increases depth and `* /` decreases).
 *
 * On success, *body_start receives the offset of the first byte after
 * the opening `/ *`, and *body_len receives the length of the comment
 * body (excluding the closing `* /`).
 *
 * Slashes and stars are spelled with explicit spaces in this comment to
 * keep the surrounding C compiler from misparsing the block.
 */
static size_t
locate_trailing_block_comment(const char *sql,
							  size_t sql_len,
							  size_t *body_start,
							  size_t *body_len)
{
	/* Skip trailing whitespace + trailing `;`. */
	size_t		end = sql_len;

	while (end > 0)
	{
		unsigned char c = (unsigned char) sql[end - 1];

		if (c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == ';')
			end--;
		else
			break;
	}
	if (end < 4)
		return (size_t) -1;
	if (sql[end - 2] != '*' || sql[end - 1] != '/')
		return (size_t) -1;

	/*
	 * Walk backwards to find matching opener. PG block comments are
	 * nested: track depth.
	 */
	size_t		depth = 1;
	size_t		i = end - 2;	/* points at '*' of closing */

	while (i >= 2)
	{
		if (sql[i - 2] == '*' && sql[i - 1] == '/')
		{
			/* Saw another closing while scanning backwards — depth+1. */
			depth++;
			i -= 2;
			continue;
		}
		if (sql[i - 2] == '/' && sql[i - 1] == '*')
		{
			depth--;
			if (depth == 0)
			{
				size_t		open_idx = i - 2;

				*body_start = open_idx + 2;
				*body_len = (end - 2) - (open_idx + 2);
				return open_idx;
			}
			i -= 2;
			continue;
		}
		i--;
	}
	return (size_t) -1;
}

/* ---------------------- KV iterator ------------------------------------ */

/*
 * Trim ASCII whitespace from both ends of [s, s+len). Updates *s and
 * *len in place.
 */
static void
trim(const char **s, size_t *len)
{
	while (*len > 0 && isspace((unsigned char) (*s)[0]))
	{
		(*s)++;
		(*len)--;
	}
	while (*len > 0 && isspace((unsigned char) (*s)[*len - 1]))
		(*len)--;
}

/*
 * Iterate KV pairs inside the comment body. For each `key='value'`
 * found, invokes the visitor with the key (NUL-terminated, lowercased,
 * malloc'd) and the URL-decoded value (NUL-terminated, malloc'd). The
 * visitor must free both buffers. Iteration stops as soon as the
 * visitor returns false.
 */
typedef bool (*kv_visitor) (const char *key, char *value, void *ctx);

static void
iterate_kvs(const char *body, size_t body_len, kv_visitor visit, void *ctx)
{
	size_t		i = 0;

	while (i < body_len)
	{
		/* Skip whitespace + commas. */
		while (i < body_len &&
			   (isspace((unsigned char) body[i]) || body[i] == ','))
			i++;
		if (i >= body_len)
			break;

		/* Key: [A-Za-z0-9_-]+ (sqlcommenter spec). */
		size_t		key_start = i;

		while (i < body_len)
		{
			unsigned char c = (unsigned char) body[i];

			if ((c >= 'A' && c <= 'Z') ||
				(c >= 'a' && c <= 'z') ||
				(c >= '0' && c <= '9') ||
				c == '_' || c == '-')
				i++;
			else
				break;
		}
		size_t		key_len = i - key_start;

		if (key_len == 0)
			break;

		/* Skip whitespace, then '='. */
		while (i < body_len && isspace((unsigned char) body[i]))
			i++;
		if (i >= body_len || body[i] != '=')
			break;
		i++;
		while (i < body_len && isspace((unsigned char) body[i]))
			i++;

		/* Must be single-quoted value (sqlcommenter spec). */
		if (i >= body_len || body[i] != '\'')
			break;
		i++;
		size_t		val_start = i;

		while (i < body_len && body[i] != '\'')
			i++;
		if (i >= body_len)
			break;
		size_t		val_len = i - val_start;

		i++;					/* skip closing quote */

		/* Build a lowercase NUL-terminated key copy. */
		char	   *key_buf = (char *) malloc(key_len + 1);

		if (key_buf == NULL)
			return;
		for (size_t k = 0; k < key_len; k++)
		{
			unsigned char c = (unsigned char) body[key_start + k];

			if (c >= 'A' && c <= 'Z')
				c = (unsigned char) (c - 'A' + 'a');
			key_buf[k] = (char) c;
		}
		key_buf[key_len] = '\0';

		char	   *val_buf = url_decode(body + val_start, val_len);

		if (val_buf == NULL)
		{
			free(key_buf);
			/* Silent skip on bad encoding; continue scanning. */
			continue;
		}

		bool		keep_going = visit(key_buf, val_buf, ctx);

		free(key_buf);
		free(val_buf);
		if (!keep_going)
			return;
	}
}

/* ---------------------- public: extract -------------------------------- */

struct extract_ctx
{
	struct trace_context *out;
	bool		traceparent_ok;
	char	  **tracestate_out;
};

static bool
extract_visit(const char *key, char *value, void *vctx)
{
	struct extract_ctx *ctx = (struct extract_ctx *) vctx;

	if (strcmp(key, "traceparent") == 0 && !ctx->traceparent_ok)
	{
		struct trace_context parsed;

		if (trace_context_parse_lenient(value, &parsed))
		{
			*ctx->out = parsed;
			ctx->traceparent_ok = true;
		}
	}
	else if (strcmp(key, "tracestate") == 0 &&
			 ctx->tracestate_out != NULL &&
			 *ctx->tracestate_out == NULL)
	{
		char	   *dup = (char *) malloc(strlen(value) + 1);

		if (dup != NULL)
		{
			strcpy(dup, value);
			*ctx->tracestate_out = dup;
		}
	}
	return true;
}

bool
sqlcommenter_extract_traceparent(const char *sql,
								 struct trace_context *out,
								 char **tracestate_out)
{
	if (sql == NULL || out == NULL)
		return false;
	if (tracestate_out != NULL)
		*tracestate_out = NULL;

	size_t		sql_len = strlen(sql);
	size_t		body_start = 0;
	size_t		body_len = 0;

	if (locate_trailing_block_comment(sql, sql_len, &body_start, &body_len)
		== (size_t) -1)
		return false;

	struct extract_ctx ctx = {
		.out = out,
		.traceparent_ok = false,
		.tracestate_out = tracestate_out
	};

	iterate_kvs(sql + body_start, body_len, extract_visit, &ctx);

	if (!ctx.traceparent_ok && tracestate_out != NULL && *tracestate_out != NULL)
	{
		/*
		 * No valid traceparent but we filled tracestate; per spec
		 * tracestate without traceparent is meaningless, so undo.
		 */
		free(*tracestate_out);
		*tracestate_out = NULL;
	}
	return ctx.traceparent_ok;
}

/* ---------------------- public: inject --------------------------------- */

/*
 * If the very end of `sql` already carries a sqlcommenter block, strip
 * it so the caller can append a fresh one. Returns a malloc'd copy
 * (caller frees), or NULL on alloc failure. If there is no trailing
 * block, returns a duplicate of `sql` unchanged.
 */
static char *
strip_trailing_block(const char *sql)
{
	size_t		sql_len = strlen(sql);
	size_t		body_start = 0;
	size_t		body_len = 0;
	size_t		open_idx = locate_trailing_block_comment(sql, sql_len,
														 &body_start,
														 &body_len);

	if (open_idx == (size_t) -1)
	{
		char	   *dup = (char *) malloc(sql_len + 1);

		if (dup == NULL)
			return NULL;
		memcpy(dup, sql, sql_len + 1);
		return dup;
	}

	/* Check whether this trailing block looks like a sqlcommenter KV. */
	bool		looks_like_sc = false;

	{
		const char *p = sql + body_start;
		size_t		n = body_len;

		trim(&p, &n);
		/* very small heuristic: presence of `=` and `'` */
		for (size_t i = 0; i < n; i++)
		{
			if (p[i] == '=' && (i + 1 < n) && p[i + 1] == '\'')
			{
				looks_like_sc = true;
				break;
			}
		}
	}

	if (!looks_like_sc)
	{
		char	   *dup = (char *) malloc(sql_len + 1);

		if (dup == NULL)
			return NULL;
		memcpy(dup, sql, sql_len + 1);
		return dup;
	}

	/* Trim trailing whitespace before the stripped block. */
	size_t		new_end = open_idx;

	while (new_end > 0)
	{
		unsigned char c = (unsigned char) sql[new_end - 1];

		if (c == ' ' || c == '\t' || c == '\n' || c == '\r')
			new_end--;
		else
			break;
	}

	char	   *out = (char *) malloc(new_end + 1);

	if (out == NULL)
		return NULL;
	memcpy(out, sql, new_end);
	out[new_end] = '\0';
	return out;
}

char *
sqlcommenter_inject_traceparent(const char *sql,
								const struct trace_context *tc,
								const char *tracestate)
{
	if (sql == NULL || tc == NULL)
		return NULL;

	char		tp_wire[TRACE_CONTEXT_BUF_SIZE];

	if (trace_context_serialize(tc, tp_wire, sizeof(tp_wire)) < 0)
		return NULL;

	char	   *base = strip_trailing_block(sql);

	if (base == NULL)
		return NULL;

	char	   *ts_enc = NULL;

	if (tracestate != NULL)
	{
		ts_enc = url_encode(tracestate);
		if (ts_enc == NULL)
		{
			free(base);
			return NULL;
		}
	}

	size_t		base_len = strlen(base);
	/* worst-case: base + " /<star>traceparent='...',tracestate='...'<star>/" */
	size_t		alloc = base_len + 1 /* space */ + 2 /* slash-star */
		+ strlen("traceparent='") + TRACE_CONTEXT_WIRE_LEN + 1
		+ (ts_enc != NULL ? strlen(",tracestate='") + strlen(ts_enc) + 1 : 0)
		+ 2 /* star-slash */ + 1 /* NUL */;
	char	   *out = (char *) malloc(alloc);

	if (out == NULL)
	{
		free(base);
		free(ts_enc);
		return NULL;
	}

	size_t		o = 0;

	memcpy(out + o, base, base_len);
	o += base_len;
	if (base_len > 0)
	{
		unsigned char tail = (unsigned char) out[o - 1];

		if (tail != ' ' && tail != '\t' && tail != '\n' && tail != '\r')
			out[o++] = ' ';
	}
	out[o++] = '/';
	out[o++] = '*';
	memcpy(out + o, "traceparent='", strlen("traceparent='"));
	o += strlen("traceparent='");
	memcpy(out + o, tp_wire, TRACE_CONTEXT_WIRE_LEN);
	o += TRACE_CONTEXT_WIRE_LEN;
	out[o++] = '\'';
	if (ts_enc != NULL)
	{
		memcpy(out + o, ",tracestate='", strlen(",tracestate='"));
		o += strlen(",tracestate='");
		size_t		ts_len = strlen(ts_enc);

		memcpy(out + o, ts_enc, ts_len);
		o += ts_len;
		out[o++] = '\'';
	}
	out[o++] = '*';
	out[o++] = '/';
	out[o] = '\0';

	free(base);
	free(ts_enc);
	return out;
}
