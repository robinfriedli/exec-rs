use std::{
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(feature = "mutex-async")]
use std::future::Future;

#[cfg(feature = "mutex-sync")]
use crate::{Invoker, ModeWrapper};

type MutexMap<K, M> = flurry::HashMap<K, Arc<ReferenceCountedMutex<M>>>;

/// Synchronizes task execution by a key provided when executing a task.
///
/// Tasks using different keys may run concurrently, while tasks using the same key are serialized.
///
/// Internally, mutexes are mapped to keys on demand and automatically removed when no task
/// is using or waiting for them anymore.
///
/// The key type must implement `Sync + Send + Clone + Hash + Ord` and have a static lifetime.
#[cfg(feature = "mutex-sync")]
pub struct MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    core: MutexCore<K, parking_lot::Mutex<()>>,
}

#[cfg(feature = "mutex-sync")]
impl<K> Default for MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    fn default() -> Self {
        Self {
            core: MutexCore::default(),
        }
    }
}

#[cfg(feature = "mutex-sync")]
impl<K> MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes a task while holding the mutex associated with the provided key.
    ///
    /// Tasks using the same key are serialized. Tasks using different keys may run concurrently.
    /// The mapped mutex is automatically removed when it is no longer in use.
    pub fn evaluate<R, F>(&self, key: K, task: F) -> R
    where
        F: FnOnce() -> R,
    {
        let mutex_ref = self.core.acquire(key, || parking_lot::Mutex::new(()));
        let _mutex_guard = mutex_ref.mutex.inner.lock();

        task()
    }
}

/// Asynchronously synchronizes task execution by key using [`tokio::sync::Mutex`].
///
/// Tasks using different keys may run concurrently, while tasks using the same key are serialized.
#[cfg(feature = "mutex-async")]
pub struct MutexAsync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    core: MutexCore<K, tokio::sync::Mutex<()>>,
}

#[cfg(feature = "mutex-async")]
impl<K> Default for MutexAsync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    fn default() -> Self {
        Self {
            core: MutexCore::default(),
        }
    }
}

#[cfg(feature = "mutex-async")]
impl<K> MutexAsync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes an asynchronous task while holding the mutex associated with the provided key.
    ///
    /// Tasks using the same key are serialized. Tasks using different keys may run concurrently.
    /// The mapped mutex is automatically removed when it is no longer in use.
    pub async fn evaluate<R, F, Fut>(&self, key: K, task: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = R>,
    {
        let mutex_ref = self.core.acquire(key, || tokio::sync::Mutex::new(()));
        let _mutex_guard = mutex_ref.mutex.inner.lock().await;

        task().await
    }
}

/// Holds a mutex together with the logical reference count for its mapped key.
///
/// Once the reference count reaches zero, it cannot be incremented again, preventing a mutex
/// that is being removed from the map from being reused.
struct ReferenceCountedMutex<M> {
    inner: M,
    rc: AtomicUsize,
}

impl<M> ReferenceCountedMutex<M> {
    /// Create a new ReferenceCountedMutex with an initial reference count of 1.
    fn new(mutex: M) -> Self {
        Self {
            inner: mutex,
            rc: AtomicUsize::new(1),
        }
    }

    /// Attempts to increment the reference counter, failing to do so if it has reached 0 already.
    /// Callers can check whether the increment succeeded by checking whether the witnessed value is 0.
    fn increment_rc(&self) -> usize {
        let curr = self.rc.load(Ordering::Relaxed);

        // disallow incrementing once it reached 0
        if curr == 0 {
            return curr;
        }

        let mut expected = curr;

        loop {
            match self.rc.compare_exchange_weak(
                expected,
                expected + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(witnessed) => break witnessed,
                Err(witnessed) if witnessed == 0 => break witnessed,
                Err(witnessed) => expected = witnessed,
            }
        }
    }

    /// Returns true if this was the final reference.
    fn decrement_rc(&self) -> bool {
        self.rc.fetch_sub(1, Ordering::Relaxed) == 1
    }
}

/// Owns one logical reference to a mapped mutex.
///
/// Dropping this reference decrements the logical reference count and removes the mutex from the
/// map when the final reference is released.
///
/// This is separate from the underlying mutex guard so that the async implementation can safely
/// account for cancellation while waiting for [`tokio::sync::Mutex::lock`].
struct ReferenceCountedMutexRef<'a, K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: 'static + Sync + Send,
{
    mutex_map: &'a MutexMap<K, M>,
    key: K,
    mutex: Arc<ReferenceCountedMutex<M>>,
}

impl<K, M> Drop for ReferenceCountedMutexRef<'_, K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: 'static + Sync + Send,
{
    fn drop(&mut self) {
        if self.mutex.decrement_rc() {
            let mutex_map = self.mutex_map.pin();
            mutex_map.remove(&self.key);
        }
    }
}

/// Struct that implements the [`ModeWrapper`] and [`Invoker`] traits for any type that borrows [`MutexSync`]
/// and a specific key. Enables using [`MutexSync`] as a `ModeWrapper` or `Invoker`.
#[cfg(feature = "mutex-sync")]
pub struct MutexSyncExecutor<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: std::borrow::Borrow<MutexSync<K>> + 'static,
{
    key: K,
    mutex_sync: M,
}

#[cfg(feature = "mutex-sync")]
impl<T, K, M> ModeWrapper<'static, T> for MutexSyncExecutor<K, M>
where
    T: 'static,
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: std::borrow::Borrow<MutexSync<K>> + 'static,
{
    fn wrap<'f>(self: Arc<Self>, task: Box<dyn FnOnce() -> T + 'f>) -> Box<dyn FnOnce() -> T + 'f> {
        Box::new(move || self.mutex_sync.borrow().evaluate(self.key.clone(), task))
    }
}

#[cfg(feature = "mutex-sync")]
impl<K, M> Invoker for MutexSyncExecutor<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: std::borrow::Borrow<MutexSync<K>> + 'static,
{
    fn do_invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: Option<&'f super::Mode<'f, T>>,
        task: F,
    ) -> T {
        self.mutex_sync.borrow().evaluate(self.key.clone(), || {
            if let Some(mode) = mode {
                super::invoke(mode, task)
            } else {
                task()
            }
        })
    }
}

/// Shared implementation for keyed mutex lookup, reference counting and cleanup.
struct MutexCore<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: 'static + Sync + Send,
{
    mutex_map: MutexMap<K, M>,
}

impl<K, M> Default for MutexCore<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: 'static + Sync + Send,
{
    fn default() -> Self {
        Self {
            mutex_map: flurry::HashMap::new(),
        }
    }
}

impl<K, M> MutexCore<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: 'static + Sync + Send,
{
    fn acquire<F>(&self, key: K, create_mutex: F) -> ReferenceCountedMutexRef<'_, K, M>
    where
        F: FnOnce() -> M,
    {
        let mutex_map = self.mutex_map.pin();

        let mutex = if let Some(mutex) = mutex_map.get(&key) {
            if mutex.increment_rc() > 0 {
                Arc::clone(mutex)
            } else {
                Self::create_mutex(&key, &mutex_map, create_mutex())
            }
        } else {
            Self::create_mutex(&key, &mutex_map, create_mutex())
        };

        // The returned reference owns the mapped mutex through an Arc, so the Flurry guard does not
        // have to stay pinned while waiting for or holding the mutex. This is required by the async
        // implementation because the pin must not be held across an await point.
        drop(mutex_map);

        ReferenceCountedMutexRef {
            mutex_map: &self.mutex_map,
            key,
            mutex,
        }
    }

    #[inline]
    fn create_mutex(
        key: &K,
        map_ref: &flurry::HashMapRef<'_, K, Arc<ReferenceCountedMutex<M>>>,
        mutex: M,
    ) -> Arc<ReferenceCountedMutex<M>> {
        let mut mutex = Arc::new(ReferenceCountedMutex::new(mutex));

        loop {
            match map_ref.try_insert(key.clone(), mutex) {
                Ok(mutex_ref) => break Arc::clone(mutex_ref),
                Err(insert_err) => {
                    let current = insert_err.current;

                    if current.increment_rc() > 0 {
                        break Arc::clone(current);
                    }

                    // The existing mutex has reached rc == 0 and is in the process of being removed.
                    // Retry inserting the mutex that lost this insertion race.
                    mutex = insert_err.not_inserted;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[cfg(feature = "mutex-sync")]
    use crate::Invoker;

    #[cfg(feature = "mutex-sync")]
    use super::{MutexSync, MutexSyncExecutor};

    #[cfg(feature = "mutex-async")]
    use super::MutexAsync;

    #[cfg(any(feature = "mutex-sync", feature = "mutex-async"))]
    use std::sync::Arc;

    #[cfg(feature = "mutex-sync")]
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    #[cfg(feature = "mutex-async")]
    use std::{
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::Duration,
    };

    #[cfg(feature = "mutex-sync")]
    #[test]
    fn it_works() {
        let mutex_sync = Arc::new(MutexSync::<i32>::new());
        let failed = Arc::new(AtomicBool::new(false));
        let running_set = Arc::new(flurry::HashSet::<i32>::new());

        let mut handles = Vec::with_capacity(5);

        for _ in 0..5 {
            let mutex_sync = mutex_sync.clone();
            let failed = failed.clone();
            let running_set = running_set.clone();

            let handle = std::thread::spawn(move || {
                for i in 0..15 {
                    let mutex_sync = mutex_sync.clone();
                    let failed = failed.clone();
                    let running_set = running_set.clone();

                    let mut handles = Vec::with_capacity(5);

                    let handle = std::thread::spawn(move || {
                        let running_set = running_set.pin();
                        mutex_sync.evaluate(i, || {
                            if running_set.contains(&i) {
                                failed.store(true, Ordering::Relaxed);
                            }

                            running_set.insert(i);

                            std::thread::sleep(std::time::Duration::from_secs(1));

                            if !running_set.contains(&i) {
                                failed.store(true, Ordering::Relaxed);
                            }

                            std::thread::sleep(std::time::Duration::from_secs(1));
                            running_set.remove(&i);

                            if running_set.contains(&i) {
                                failed.store(true, Ordering::Relaxed);
                            }
                        })
                    });

                    handles.push(handle);

                    for handle in handles {
                        handle.join().unwrap();
                    }
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(!failed.load(Ordering::Relaxed));
    }

    #[cfg(feature = "mutex-sync")]
    #[test]
    fn test_concurrent_different_key() {
        let running = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));

        let mutex_sync = Arc::new(MutexSync::<i32>::new());

        let mut handles = Vec::with_capacity(2);

        let mutex_sync1 = mutex_sync.clone();
        let running1 = running.clone();
        let handle1 = std::thread::spawn(move || {
            mutex_sync1.evaluate(1, move || {
                running1.store(true, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_secs(5));
                running1.store(false, Ordering::Relaxed);
            });
        });
        handles.push(handle1);

        let mutex_sync2 = mutex_sync.clone();
        let running2 = running.clone();
        let failed2 = failed.clone();
        let handle2 = std::thread::spawn(move || {
            mutex_sync2.evaluate(2, move || {
                std::thread::sleep(std::time::Duration::from_secs(3));

                if !running2.load(Ordering::Relaxed) {
                    failed2.store(true, Ordering::Relaxed);
                }
            });
        });
        handles.push(handle2);

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(!failed.load(Ordering::Relaxed));
    }

    #[cfg(feature = "mutex-sync")]
    #[test]
    fn test_mutex_sync_executor() {
        let mutex_sync = Arc::new(MutexSync::<i32>::new());
        let failed = Arc::new(AtomicBool::new(false));
        let running_set = Arc::new(flurry::HashSet::<i32>::new());
        let multiplier_map = Arc::new(flurry::HashMap::<i32, AtomicI32>::new());

        {
            let map = multiplier_map.pin();
            for i in 0..5 {
                map.insert(i, AtomicI32::new(0));
            }
        }

        let mutex_sync_executor = MutexSyncExecutor {
            key: 1,
            mutex_sync: MutexSync::<i32>::new(),
        };

        assert_eq!(mutex_sync_executor.invoke(|| 4), 4);

        let mut handles = Vec::with_capacity(25);

        for _ in 0..5 {
            for i in 0..5 {
                let failed = failed.clone();
                let failed2 = failed.clone();
                let running_set = running_set.clone();
                let multiplier_map = multiplier_map.clone();

                let executor = MutexSyncExecutor {
                    key: i,
                    mutex_sync: mutex_sync.clone(),
                };

                let handle = std::thread::spawn(move || {
                    let running_set = running_set.pin();
                    executor.invoke(move || {
                        if running_set.contains(&i) {
                            failed.store(true, Ordering::Relaxed);
                        }

                        running_set.insert(i);

                        std::thread::sleep(std::time::Duration::from_secs(1));

                        if !running_set.contains(&i) {
                            failed.store(true, Ordering::Relaxed);
                        }

                        std::thread::sleep(std::time::Duration::from_secs(1));
                        running_set.remove(&i);

                        if running_set.contains(&i) {
                            failed.store(true, Ordering::Relaxed);
                        }
                    });

                    let mode = crate::Mode::<i32>::new().with(executor);
                    let result = crate::invoke(&mode, move || {
                        let multiplier_map = multiplier_map.pin();
                        let multiplier = multiplier_map.get(&i).unwrap();
                        multiplier.store(2, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let result = multiplier.load(Ordering::Relaxed) * 4;
                        multiplier.store(0, Ordering::Relaxed);
                        result
                    });

                    if result != 8 {
                        failed2.store(true, Ordering::Relaxed);
                    }
                });

                handles.push(handle);
            }
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(!failed.load(Ordering::Relaxed));
    }

    #[cfg(feature = "mutex-sync")]
    #[test]
    fn test_remove_mutex_on_panic() {
        let mutex_sync = Arc::new(MutexSync::<i32>::new());

        let m = mutex_sync.clone();
        let handle = std::thread::spawn(move || {
            m.evaluate(1, || {
                panic!("test panic");
            });
        });

        let _ = handle.join();
        assert!(mutex_sync.core.mutex_map.is_empty());
    }

    #[cfg(feature = "mutex-async")]
    async fn wait_for_rc(mutex: &MutexAsync<i32>, key: i32, expected: usize) {
        for _ in 0..1000 {
            let current = {
                let map = mutex.core.mutex_map.pin();

                map.get(&key)
                    .map(|mutex| mutex.rc.load(AtomicOrdering::Relaxed))
            };

            if current == Some(expected) {
                return;
            }

            tokio::task::yield_now().await;
        }

        panic!("Timed out waiting for mutex with key {key} to reach reference count {expected}");
    }

    #[cfg(feature = "mutex-async")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_mutex_async_same_key() {
        let mutex = Arc::new(MutexAsync::<i32>::new());
        let running = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(32);

        for _ in 0..32 {
            let mutex = mutex.clone();
            let running = running.clone();
            let failed = failed.clone();

            handles.push(tokio::spawn(async move {
                mutex
                    .evaluate(1, || async move {
                        if running.fetch_add(1, AtomicOrdering::SeqCst) != 0 {
                            failed.store(1, AtomicOrdering::SeqCst);
                        }

                        // Give other tasks a chance to run while the mutex is held.
                        tokio::task::yield_now().await;

                        if running.fetch_sub(1, AtomicOrdering::SeqCst) != 1 {
                            failed.store(1, AtomicOrdering::SeqCst);
                        }
                    })
                    .await;
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(failed.load(AtomicOrdering::SeqCst), 0);
        assert!(mutex.core.mutex_map.is_empty());
    }

    #[cfg(feature = "mutex-async")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mutex_async_concurrent_different_key() {
        let mutex = Arc::new(MutexAsync::<i32>::new());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let mut handles = Vec::with_capacity(2);

        for key in [1, 2] {
            let mutex = mutex.clone();
            let barrier = barrier.clone();

            handles.push(tokio::spawn(async move {
                mutex
                    .evaluate(key, || async move {
                        // Both tasks must be able to reach this point simultaneously.
                        // If different keys are accidentally synchronised with each other,
                        // the first task waits here forever.
                        barrier.wait().await;
                    })
                    .await;
            }));
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await
        .expect("Tasks using different keys did not execute concurrently");

        assert!(mutex.core.mutex_map.is_empty());
    }

    #[cfg(feature = "mutex-async")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_async_mutex_on_panic() {
        let mutex = Arc::new(MutexAsync::<i32>::new());

        let m = mutex.clone();
        let handle = tokio::spawn(async move {
            let _: () = m
                .evaluate(1, || async {
                    panic!("test panic");
                })
                .await;
        });

        let err = handle.await.unwrap_err();

        assert!(err.is_panic());
        assert!(mutex.core.mutex_map.is_empty());
    }

    #[cfg(feature = "mutex-async")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_async_mutex_on_holder_cancellation() {
        let mutex = Arc::new(MutexAsync::<i32>::new());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let m = mutex.clone();
        let handle = tokio::spawn(async move {
            m.evaluate(1, || async move {
                started_tx.send(()).unwrap();

                std::future::pending::<()>().await;
            })
            .await;
        });

        // At this point the task is inside evaluate() and holds the actual mutex.
        started_rx.await.unwrap();

        wait_for_rc(&mutex, 1, 1).await;

        handle.abort();

        let err = handle.await.unwrap_err();
        assert!(err.is_cancelled());

        assert!(mutex.core.mutex_map.is_empty());
    }

    #[cfg(feature = "mutex-async")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_remove_async_mutex_reference_on_waiter_cancellation() {
        let mutex = Arc::new(MutexAsync::<i32>::new());

        let (holder_started_tx, holder_started_rx) = tokio::sync::oneshot::channel();
        let (release_holder_tx, release_holder_rx) = tokio::sync::oneshot::channel();

        let m = mutex.clone();
        let holder = tokio::spawn(async move {
            m.evaluate(1, || async move {
                holder_started_tx.send(()).unwrap();

                let _ = release_holder_rx.await;
            })
            .await;
        });

        // Ensure the first task actually owns the mutex.
        holder_started_rx.await.unwrap();

        let m = mutex.clone();
        let waiter = tokio::spawn(async move {
            m.evaluate(1, || async {}).await;
        });

        // Holder + waiter must both have logical references to the mapped mutex.
        wait_for_rc(&mutex, 1, 2).await;

        // Cancel while waiter is suspended in tokio::sync::Mutex::lock().await.
        waiter.abort();

        let err = waiter.await.unwrap_err();
        assert!(err.is_cancelled());

        // The waiter's logical reference must have been cleaned up even though it
        // never acquired the actual mutex.
        wait_for_rc(&mutex, 1, 1).await;

        release_holder_tx.send(()).unwrap();
        holder.await.unwrap();

        assert!(mutex.core.mutex_map.is_empty());
    }
}
