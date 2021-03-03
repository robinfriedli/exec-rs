use std::{
    hash::Hash,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crate::{Invoker, ModeWrapper};

/// Task executor that can synchronise tasks by value of a key provided when submitting a task.
///
/// For example, if i32 is used as key type, then tasks submitted with keys 3 and 5 may run concurrently
/// but several tasks submitted with key 7 are synchronised by a mutex mapped to the key.
///
/// Manages a concurrent hash map that maps [`ReferenceCountedMutex`](struct.ReferenceCountedMutex.html)
/// elements to the used keys. The [`ReferenceCountedMutex`](struct.ReferenceCountedMutex.html) struct
/// holds a mutex used for synchronisation and removes itself from the map automatically if not used by
/// any thread anymore by managing an atomic reference counter. If the counter is decremented from 1 to
/// 0 the element is removed from the map and the counter cannot be incremented back up again. If the counter
/// reached 0 future increments fail and a new `ReferenceCountedMutex` is created instead. When creating
/// a new `ReferenceCountedMutex` and inserting it to the map fails because another thread has already
/// created an element for the same key, the current thread tries to use the found existing element instead
/// as long as its reference counter is valid (greater than 0), else it retries creating the element.
///
/// The type of the key used for synchronisation must be able to be used as a key for the map and thus
/// must implement `Sync + Send + Clone + Hash + Ord` and have a static lifetime.
pub struct MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    mutex_map: flurry::HashMap<K, ReferenceCountedMutex<K>>,
}

impl<K> Default for MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    fn default() -> Self {
        MutexSync {
            mutex_map: flurry::HashMap::new(),
        }
    }
}

impl<K> MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Submits a task for execution using the provided key for synchronisation. Uses the mutex of the
    /// [`ReferenceCountedMutex`](struct.ReferenceCountedMutex.html) mapped to the key to synchronise
    /// execution of the task. The `ReferenceCountedMutex` removes itself from the map automatically
    /// as soon as no thread is using it anymore by managing an atomic reference counter.
    ///
    /// If the mutex map does not already contain an entry for the provided key, meaning no task is
    /// currently running with a mutex mapped to the same key, the current thread attempts to insert
    /// a new [`ReferenceCountedMutex`](struct.ReferenceCountedMutex.html), with an initial reference
    /// count of 1, and if it succeeds acquires the mutex and runs the task, else it tries to use the
    /// found existing `ReferenceCountedMutex` if its reference counter is valid (greater than 0) or
    /// else retries creating the `ReferenceCountedMutex`.
    ///
    /// If the mutex map already contains a `ReferenceCountedMutex` mapped to the same key, meaning there
    /// are threads running using the same key, the current thread attempts to increment the reference
    /// counter on the found `ReferenceCountedMutex` and if it succeeds it waits to acquire the mutex and
    /// then executes the task, if it fails, because the counter has been decremented to 0 because another
    /// thread is in the process of removing the mutex from the map, it tries to create a new
    /// `ReferenceCountedMutex`, same as above.
    pub fn evaluate<R, F: FnOnce() -> R>(&self, key: K, task: F) -> R {
        let mutex_map = self.mutex_map.pin();

        let rc_mutex = if let Some(mutex) = mutex_map.get(&key) {
            if mutex.increment_rc() > 0 {
                mutex
            } else {
                Self::create_mutex(key, &mutex_map)
            }
        } else {
            Self::create_mutex(key, &mutex_map)
        };

        let _guard = rc_mutex.mutex.lock();
        let mut sentinel = Sentinel {
            mutex_ref: &rc_mutex,
            map_ref: &mutex_map,
            canceled: false,
        };

        let result = task();

        sentinel.canceled = true;
        rc_mutex.decrement_rc(&mutex_map);

        result
    }

    #[inline]
    fn create_mutex<'a>(
        key: K,
        map_ref: &'a flurry::HashMapRef<'a, K, ReferenceCountedMutex<K>>,
    ) -> &'a ReferenceCountedMutex<K> {
        loop {
            let key_map = key.clone();
            let key_mutex = key.clone();
            match map_ref.try_insert(key_map, ReferenceCountedMutex::new(key_mutex)) {
                Ok(mutex_ref) => break mutex_ref,
                Err(insert_err) => {
                    let curr = insert_err.current;
                    if curr.increment_rc() > 0 {
                        break curr;
                    }
                }
            }
        }
    }
}

/// Type that manages decrementing the reference counter of the [`ReferenceCountedMutex`](struct.ReferenceCountedMutex.html)
/// if execution of the task panics.
struct Sentinel<'a, K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    mutex_ref: &'a ReferenceCountedMutex<K>,
    map_ref: &'a flurry::HashMapRef<'a, K, ReferenceCountedMutex<K>>,
    canceled: bool,
}

impl<K> Drop for Sentinel<'_, K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    fn drop(&mut self) {
        if !self.canceled {
            self.mutex_ref.decrement_rc(self.map_ref);
        }
    }
}

/// Struct that holds the mutex used for synchronisation and manages removing itself from the
/// containing map once no longer referenced by any threads. Removes itself from the map when
/// decrementing the counter from 1 to 0 and makes sure that the counter cannot be incremented
/// back up once reaching 0 in case a thread finds a ReferenceCountedMutex that is in the
/// process of being removed from the map.
pub struct ReferenceCountedMutex<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    key: K,
    mutex: parking_lot::Mutex<()>,
    rc: AtomicUsize,
}

impl<K> ReferenceCountedMutex<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    /// Create a new ReferenceCountedMutex with an initial reference count of 1.
    fn new(key: K) -> Self {
        ReferenceCountedMutex {
            key,
            mutex: parking_lot::Mutex::new(()),
            rc: AtomicUsize::new(1),
        }
    }

    /// Attempts to increment the reference counter, failing to do so if it has reached 0 already.
    /// Callers can check whether the increment succeed by checking whether the witnessed value is 0.
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

    /// Decrements the reference counter, removing the entry from the map if the previous value was 0.
    /// This is protected against race conditions since ReferenceCountedMutex elements cannot be used
    /// anymore once the reference counter has reached 0, so even if some other thread might still
    /// find this ReferenceCountedMutex in the map after this thread has decremented the rc to 0 but
    /// before this thread removed the element from the map, the other thread will fail to increment
    /// the reference counter and thus has to create a new ReferenceCountedMutex element.
    fn decrement_rc(&self, map_ref: &flurry::HashMapRef<K, ReferenceCountedMutex<K>>) {
        let curr = self.rc.fetch_sub(1, Ordering::Relaxed);

        if curr == 1 {
            map_ref.remove(&self.key);
        }
    }
}

/// Struct that implements the [`ModeWrapper`](trait.ModeWrapper.html) and [`Invoker`](trait.Invoker.html)
/// traits for any type that borrows [`MutexSync`](struct.MutexSync.html) and a specific key. Enables using
/// [`MutexSync`](struct.MutexSync.html) as a `ModeWrapper` or `Invoker`.
pub struct MutexSyncExecutor<K, M>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: std::borrow::Borrow<MutexSync<K>> + 'static,
{
    key: K,
    mutex_sync: M,
}

impl<T, K, M> ModeWrapper<'static, T> for MutexSyncExecutor<K, M>
where
    T: 'static,
    K: 'static + Sync + Send + Clone + Hash + Ord,
    M: std::borrow::Borrow<MutexSync<K>> + 'static,
{
    fn wrap<'f>(
        self: Arc<Self>,
        task: Box<(dyn FnOnce() -> T + 'f)>,
    ) -> Box<(dyn FnOnce() -> T + 'f)> {
        Box::new(move || self.mutex_sync.borrow().evaluate(self.key.clone(), task))
    }
}

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

#[cfg(test)]
mod tests {

    use crate::Invoker;

    use super::{MutexSync, MutexSyncExecutor};
    use std::sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc,
    };

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

        assert_eq!(failed.load(Ordering::Relaxed), false);
    }

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

        assert_eq!(failed.load(Ordering::Relaxed), false);
    }

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

        assert_eq!(failed.load(Ordering::Relaxed), false);
    }
}
