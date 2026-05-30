# Async Temporary Lifetime Deadlock

## What happened

A unit test for `replicate_snapshot_to_peer` deadlocked on a `current_thread` tokio
runtime. The test constructed a `PeerManager` inline within the function call
arguments, reading from the shared `Arc<RwLock<...>>` state:

```rust
// DEADLOCK
let res = replicate_snapshot_to_peer(
    state.clone(),
    Arc::new(
        PeerManager::try_new(state.read().await.identity(), &HashMap::new()).unwrap(),
    ),
    peer_id,
    params,
    last_included_index,
    last_included_term,
    Duration::from_secs(1),
)
.await;
```

Extracting the `PeerManager` into a separate `let` binding resolved it:

```rust
// WORKS
let peer_manager = Arc::new(
    PeerManager::try_new(state.read().await.identity(), &HashMap::new()).unwrap(),
);
let res = replicate_snapshot_to_peer(
    state.clone(),
    peer_manager,
    // ...
)
.await;
```

## Root cause

Rust's **temporary scope rules** ([Reference][1]) keep temporaries created in a
function-call argument expression alive until the **end of the enclosing
statement** (the semicolon).

[1]: https://doc.rust-lang.org/reference/destructors.html#temporary-scopes

In the deadlocking version, the `RwLockReadGuard` returned by
`state.read().await` is a temporary. It lives until the `;`, but the
`.await` on `replicate_snapshot_to_peer` is **part of the same statement**.
The timeline:

```
let res = foo(state.read().await.identity(), ...).await;
             ^-- read lock acquired --^                   |
             |                                            |
             |                  temporary guard still     |
             |                  alive until here ---------+
             |                                            |
             foo tries state.write().await                |
             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^           |
             DEADLOCK: write can't acquire while          |
                      read guard is alive                 |
                                                    semicolon
```

Inside `replicate_snapshot_to_peer` (consensus.rs:1013) the write lock
`state.write().await` can never succeed because the read guard from the
argument expression is still live — a classic `RwLock` writer starvation on
the same thread.

## Why it's hard to spot

- **No compiler warning**: The guard isn't stored in a named variable; the
  borrow checker sees it as a temporary and does not flag the cross-await
  hold.
- **No runtime diagnostic**: `tokio::sync::RwLock` will simply park the
  writer task forever on a `current_thread` runtime since no other task can
  run to release the reader.
- **Looks innocuous**: `state.read().await.identity()` reads one field and
  "obviously" releases the guard — except the temporary lifetime rules
  extend it past the `.await` on the outer call.

## Prevention

When passing values derived from a lock guard into an async function call,
always **extract the value into a named variable first**:

```rust
// GOOD: guard dropped at semicolon before outer .await
let id = state.read().await.identity();
let peer_manager = Arc::new(PeerManager::try_new(id, &HashMap::new()).unwrap());
foo(state.clone(), peer_manager, ...).await;

// BAD: guard lives across the outer .await
foo(state.clone(), Arc::new(
    PeerManager::try_new(state.read().await.identity(), &HashMap::new()).unwrap(),
), ...).await;
```

This rule applies to any type that holds a lock guard: `RwLockReadGuard`,
`MutexGuard`, and any RAII wrapper that dereferences to guarded state.
