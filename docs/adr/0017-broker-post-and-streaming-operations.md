# ADR-0017: Broker POST and streaming operations

- Status: Accepted
- Date: 2026-10-18

ADR-0009 lets a sandboxed artifact call operator-configured HTTP APIs through the live-data broker.
Every operation is a GET with typed query parameters: at most 16 parameters, strings of at most 256
characters, one JSON response. A chat-style artifact (the road-trip concierge) needs longer input
and wants to show progress while a slow upstream works. Removing the iframe sandbox for such
artifacts was rejected: artifact code would run with the viewer's session on the artifact origin
(ADR-0003). Instead the broker gains two operation options. The sandbox, CSP and the rule that only
operators and administrators choose upstream URLs and headers are unchanged.

## POST operations

An HTTP operation may set `"method": "POST"` (default `"GET"`). Parameters are declared and
validated exactly as for GET, with larger bounds: at most 32 parameters and string `max_length` up
to 16384. Path placeholders work as before. Every other supplied or defaulted parameter is sent as
one JSON object body (`content-type: application/json`), keeping its declared type: strings as
strings, integers as numbers, booleans as booleans. A serialized body over 64 KiB is rejected with
`too_large`. GET operations keep the old bounds. Push sources accept neither option.

## Streaming operations

An HTTP operation may set `"stream": true`. Its upstream must answer `200` with newline-delimited
JSON (`application/x-ndjson`): one JSON value per non-empty line. The operation's `max_bytes`
bounds the whole stream. `timeout_ms` bounds the whole exchange and may be up to 120000 for a
streaming operation (30000 otherwise). A gap of more than 30 seconds between lines ends the stream.

The viewer calls `POST /:id/data/stream` with the same body as `/data/query`. The response is
`application/x-ndjson`: `{"chunk": <value>}` for each upstream line, then `{"done": true}`. A
failure after the response has started is sent as one `{"error": "<reason>"}` line before the
stream ends; a failure before it starts uses the normal JSON error statuses. A non-streaming
operation on this route returns `400 bad_params`. A streaming operation called through
`/data/query` is collected and returns its last line as the result, so older clients keep working.
The stream route spends the same data request budget as a query.

In the shell, an artifact sends `data:query` with `stream: true`. The shell posts `data:chunk
{ requestId, data }` for each chunk, then `data:result { requestId, data }` carrying the last
chunk. Errors use `data:error` as before, and `data:cancel` aborts the request and the upstream
call.

## Discovery

`list_data_sources` and the administration views report `method` and `stream` for each operation,
so a publisher can see how to call it. Managed sources (ADR-0010) accept the same fields and
validation as operator-file sources.
