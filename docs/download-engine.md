# Download engine

## Lifecycle of a transfer

1. **Submit.** `DownloadService::submit` validates the URL (see *Network
   security*), picks a provider and inserts the job as `queued`.
2. **Claim.** The dispatcher runs at most `downloads.max_concurrent` transfers.
   It claims the oldest `queued` job, or a `retrying` job whose backoff has
   elapsed, with a single atomic `UPDATE`.
3. **Resolve.** The job's provider turns the URL into a direct HTTP resource.
   The `http` provider passes it through unchanged.
4. **Transfer.** The body streams into `incomplete_dir/<id>.part`. The engine
   writes progress to the database once a second; live speed is kept in
   memory.
5. **Finalize.** The engine picks a sanitized, collision-free file name in
   `download_dir` and records it. Then it moves the partial file there without
   overwriting anything. Only after that is the job marked `completed` and
   post-processing set to `pending`.

## Resuming

A partial file is resumed only when all of these hold:

- the provider allows it,
- the previous response had a strong `ETag` or a `Last-Modified` date, and
- the server answers `Range` + `If-Range` with a `206` whose `Content-Range`
  starts exactly at the end of the partial file.

If the remote file changed, the server answers `200` and the download starts
over. Without a validator the download always starts over, because appending
bytes from a possibly different file would silently corrupt it. On startup,
the partial file on disk is the source of truth for the resume offset, not the
stored progress counter.

## Failures

| Situation | Result |
|-----------|--------|
| Connection error, timeout, truncated body, HTTP 408/5xx | `retrying`, exponential backoff (`retry_base_delay_secs` · 2ⁿ, capped at `retry_max_delay_secs`) |
| HTTP 429 or 503 with `Retry-After` | `retrying` after the server-requested delay (at most 24 h) |
| Retries exhausted (`max_retries`) | `failed` |
| HTTP 401/403/404/410, other 4xx | `failed` immediately |
| Destination not allowed by the network policy | `failed` immediately, never retried |
| Free space would drop below `storage.min_free_space_mb`, or disk full | `blocked` (partial file kept) |
| Other local filesystem errors (permissions, missing mount) | `blocked` (partial file kept) |

`blocked` and `failed` downloads are retried manually. A manual retry resets
the retry counter.

## Pause, cancel, shutdown

- **Pause** stops the transfer and keeps the partial file. **Resume** puts the
  job back in the queue, and it continues from the partial file when the
  rules above allow.
- **Cancel** stops the transfer and deletes the partial file.
- **Shutdown** (SIGTERM/Ctrl-C) stops every transfer, saves its progress and
  puts it back in the queue. Transfers that don't stop within 30 seconds are
  recovered on the next start.

## Crash safety

- A job that was `downloading` when the process died is re-queued on startup,
  and its partial file is kept.
- The final destination is recorded *before* the move. If the process dies
  after the move but before the job is marked completed, the next run finds
  the complete file at the recorded path, with the expected size and no
  partial file, and marks the job completed without downloading it again.

## Network security (SSRF)

By default, downloads can only reach public internet addresses.
`network.allowed_private_networks` opens up specific private ranges.
Unspecified, broadcast and multicast addresses are never reachable.

These checks apply:

1. At submission, the scheme must be http or https, the URL must not contain
   credentials, and an IP-literal host must be allowed.
2. During DNS resolution, the resolver drops forbidden addresses, and the
   connection uses only the addresses that passed. A hostname therefore can't
   be re-resolved to an internal address between check and connect (DNS
   rebinding).
3. Before the first request and on every redirect hop, the scheme and any
   IP-literal host are checked. hyper doesn't call the resolver for IP
   literals, so they need this separate check.
4. After connecting, the peer address of the response is checked again.

IPv6 forms that embed an IPv4 address (mapped, compatible, NAT64, 6to4) are
judged by the IPv4 address inside them. System proxy variables
(`HTTP_PROXY`, …) are ignored, because a proxy would bypass these checks.

## Files and names

- Partial files are named after the download ID. Their names never come from
  untrusted input, and they are opened with `O_NOFOLLOW` on Linux.
- Final names come from `Content-Disposition`, the provider, or the URL path.
  The name is sanitized: path separators, control characters and leading dots
  are removed, and the length is capped.
- Existing files are never overwritten. A collision produces `name (1).ext`.
  The move uses `renameat2(RENAME_NOREPLACE)`. When that isn't supported (for
  example across filesystems), it falls back to copying into a newly created
  file, then fsync, then removing the source.
