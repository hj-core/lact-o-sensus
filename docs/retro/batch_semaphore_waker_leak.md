# Batch Semaphore Waker Leak

## What happened

The leader's tick loop permanently stalled after a successful mutation
propose. The snapshot installation smoke test (`snapshot_threshold=20`,
`VetoMode.MOCK`) failed consistently — the leader would go silent 10+
seconds after a propose, followers would enter endless elections at higher
terms (49–53), and the test would time out.

Three independent runs with different nodes as leader reproduced the same
pattern every time.

## Root cause

`tokio::sync::RwLock` is backed by `batch_semaphore`, a FIFO waker waitlist
with heap-allocated Waker entries. Under sustained contention consisting of
many rapid short write-lock acquisitions (~40µs per tick iteration) followed
by one slightly longer acquisition (~1.7ms sled flush during propose), the
semaphore's internal accounting dropped a waker: the permit was returned to
the counter, but the corresponding Waker in the FIFO queue was never invoked.
The next waiter (the tick loop) parked forever.

Once the tick loop stopped, no heartbeats were sent, no elections were
contested, and the leader was effectively dead. The lock was permanently
consumed from the perspective of all subsequent waiters — every
`RequestVote` handler and future tick iteration also parked forever at the
same `.await`.

## Diagnostic journey

The failure was initially suspected to be a sled I/O issue, a `FuturesUnordered`
waker leak, a `select!` cancellation hazard, or a panic inside the tick loop.
Each was ruled out systematically:

1. **Fine-grained timing diagnostics** were added inside the consensus write
   lock critical section (`pre_lock`, `lock_acquired`, `tick_completed`,
   `lock_dropped`) and inside the `ConsensusShell::write()` method
   (`pre_acquire`, `post_acquire`, `progress_captured`).

2. **Sled I/O** was definitively ruled out: all traces showed sub-millisecond
   sled operations, and the hang was at `state.write().await` (before the
   lock), not inside the guarded section.

3. **`FuturesUnordered` in `broadcast_append_entries`** was traced: futures
   never hold the write lock across async boundaries (the `Acquire` future
   completes before any RPC work).

4. **`select!` in `verify_leadership_quorum`** was ruled out: it's only
   called for read queries, not during mutation flood.

5. **Panic inside tick loop** was ruled out: no `TickAction::Stop` log,
   no panic messages, and the hang was at `state.write().await` (before
   entering the guarded section).

6. **Zero `unsafe` code** in the entire project ruled out memory corruption.

The critical trace from Run 3 (Node 1 leader):

```
15:35:17.336771Z — tick loop count=208: pre_lock
15:35:17.336838Z — post_acquire (tokio RwLock acquired)
15:35:17.336858Z — progress_captured (MutationGuard constructed)
15:35:17.336869Z — tick_completed action=SendHeartbeat
15:35:17.336881Z — lock_dropped (write lock released — 43µs total)
15:35:17.336904Z — replication round: post_acquire, progress_captured (8µs)
15:35:17.336965Z — propose handler: post_acquire, progress_captured (18µs)
15:35:17.337033Z — sled_append_batch_end elapsed=36.74µs
15:35:17.338739Z — sled_append_flush_end elapsed=1.653ms
15:35:17.338836Z — "Mutation index 16 appended."
15:35:17.348029Z — tick loop count=209: pre_lock  ← NEVER reaches post_acquire
```

The sequence shows the lock was released properly (`lock_dropped` at
43µs), the next tick iteration reached `.write().await` (`pre_lock`), but
the waker was never fired — the future never completed.

## Why it's hard to spot

- **No `unsafe` code**: The waker leak is inside the tokio `batch_semaphore`
  implementation, which is safe Rust. No UBSan or Miri warning can catch it.
- **No crash or panic**: The task is simply parked forever. No error, no
  stack trace, no diagnostic — the system goes silent.
- **Non-deterministic**: No deterministic reproduction with `mock_shell()`
  in unit tests because the bug requires real wall-clock timing with sled
  flush latency (~1.7ms) under sustained contention.
- **No tokio bug report**: The specific contention pattern (many ~40µs
  writes + one ~1.7ms write) creates a timing window that may not have been
  exercised in existing tokio test coverage.

## The fix

Replaced `tokio::sync::RwLock` with `parking_lot::RwLock` in the
`ConsensusShell`. `parking_lot` uses OS-level `futex` synchronization —
there is no async waker machinery, no heap-allocated Waker list, and
therefore no waker leak to begin with.

The async `read()` and `write()` methods use a try-lock + yield spin loop
instead of blocking the OS thread:

```rust
pub async fn write(&self) -> MutationGuard<'_, S> {
    loop {
        if let Some(mut guard) = self.inner.try_write() {
            let before = guard.consensus_progress();
            return MutationGuard { shell: self, guard, before };
        }
        tokio::task::yield_now().await;
    }
}
```

The synchronous `blocking_read()` and `blocking_write()` methods (used only
within `spawn_blocking` contexts) call `parking_lot` directly.

This is safe because critical sections are extremely short (40µs typical,
1.7ms worst case), so the spin loop completes in microseconds. The critical
sections that scale with data size (FSM snapshot install, `read_entries`)
already run on `spawn_blocking` outside the consensus lock.

## Prevention

- **Prefer `parking_lot::RwLock` over `tokio::sync::RwLock`** for locks held
  across very short critical sections (<2ms) where the risk of waker leaks
  in the async semaphore outweighs the benefit of non-blocking acquisition.
- **Lock diagnostics**: When diagnosing a "task hung at `.await`" where no
  progress is made and no error is reported, suspect the waker mechanism
  itself. Add a pre-lock log and a post-lock log — if the pre-lock fires
  but the post-lock never does, the waker is lost.
- **Avoid yield-based contention on shared-nothing locks**: The try-lock +
  yield pattern works because contention is bounded. If contention were
  higher or critical sections longer, a proper async-aware lock (e.g.,
  `async-lock::RwLock`) would be needed.
