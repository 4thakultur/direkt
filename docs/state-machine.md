# Download state machine

Each download has two independent states. They are stored in the `downloads`
table, returned by the API, and shown in the UI under the same names.

## Transfer state (`state`)

```
                 ┌──────────── resume ─────────────┐
                 ▼                                 │
  submit ──► queued ──claim──► downloading ──► completed
              │  ▲               │  │  │  │
              │  │  interrupted  │  │  │  └──► failed ──retry──► queued
              │  └───────────────┘  │  │
              │                     │  └──► blocked ──retry──► queued
              │          transient  ▼
              │          error   retrying ──backoff elapsed──► downloading
              │
              └─► paused / cancelled   (also from downloading and retrying)
                     paused ──resume──► queued
                  cancelled ──retry───► queued
```

| From          | Allowed next states                                                    |
|---------------|------------------------------------------------------------------------|
| `queued`      | `downloading`, `paused`, `cancelled`                                   |
| `downloading` | `completed`, `retrying`, `blocked`, `failed`, `paused`, `cancelled`, `queued` |
| `retrying`    | `downloading`, `paused`, `cancelled`                                   |
| `paused`      | `queued`, `cancelled`                                                  |
| `blocked`     | `queued`, `cancelled`                                                  |
| `failed`      | `queued`                                                               |
| `cancelled`   | `queued`                                                               |
| `completed`   | — (final)                                                              |

What the states mean:

- **retrying**: a transient failure happened, such as a network error or a
  temporary provider restriction. The job runs again once `next_attempt_at`
  has passed. Each retry increments `retry_count`. When the budget
  (`downloads.max_retries`) runs out, the job becomes `failed`.
- **blocked**: the job can't continue until the user does something, for
  example supplying credentials. A temporary provider restriction is
  *not* `blocked`; it is `retrying`.
- **failed**: a permanent failure, or the retry budget ran out. A manual
  retry moves the job back to `queued` and resets `retry_count`.
- **downloading → queued**: only startup recovery and shutdown use this. A job
  that was running when the process stopped goes back into the queue. Its
  partial file is kept and checked before resuming. A job is never marked
  `completed` just because a partial file exists.

Only a worker that has claimed a job (`downloading`) can complete or fail it.
The claim is a single atomic `UPDATE … RETURNING`, so two workers can never
own the same job.

## Post-processing state (`post_state`)

```
none ──transfer completed──► pending ──► processing ──► completed
                               │  ▲          │
                               │  └─restart──┤
                               ▼             ▼
                            skipped        failed ──retry──► pending
```

Post-processing starts only after the transfer is `completed`. A post-processing
failure is recorded in `post_error` and **never** changes the transfer state.
The downloaded file stays where it is.

## Enforcement

`Store::transition` and `Store::transition_post` check the transition table
and then run a compare-and-swap update (`WHERE state = <expected>`). The state
change and its entry in `download_events` are written in the same transaction.
If another actor changed the state first, the call returns a `conflict` error
and overwrites nothing.
