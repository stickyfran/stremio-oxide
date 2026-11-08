# stremio-core patches

Vendored from `perpetus/stremio-core` at commit
`4562b37be8ea788801e12c608a6aa3e43c646123`.

Only two behavioural changes are applied, both documented inline:

1. `src/models/common/resource_loadable.rs` and
   `src/models/catalogs_with_extra.rs`: a cached `Loadable::Err` is no longer
   reused when the same resource is requested again. Before, one transient
   network failure poisoned a catalog or meta row for the rest of the session
   and only `Action::Unload` cleared it.
2. `src/models/common/resource_loadable.rs`: an addon returning an empty
   vector is now `Loadable::Ready(vec![])` instead of
   `Loadable::Err(EmptyContent)`. An empty catalog is a valid answer, not a
   network error, and must not become permanent.
3. `src/runtime/msg/action.rs` and `src/models/catalogs_with_extra.rs`: new
   `ActionCatalogsWithExtra::RetryStuck(Vec<usize>)` action. It re-emits the
   request for catalogue rows that are still `Loading`, without touching the
   rest of the board. The app uses it after a watchdog decides that a row has
   been stuck long enough.
4. `src/runtime/runtime.rs`: model lock diagnostics. `Runtime::model()` now
   returns a wrapper guard that registers the reading thread, and every
   read/write acquisition first polls with `try_read`/`try_write` for up to
   2 s. If the lock is not obtained, the waiter logs an error naming every
   thread that holds the lock (and whether the waiter already holds a read
   guard, which means a re-entrant read is deadlocking against a queued
   writer) and dumps the state of every thread. The lock is still a
   `std::sync::RwLock` and the behaviour is unchanged; only visibility was
   added.

Keep the diff minimal so it can be dropped once upstream accepts the change
or exposes a `force` flag for these models.
