use std::{
    hash::Hash,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crate::{Invoker, ModeWrapper};

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
    fn new(key: K) -> Self {
        ReferenceCountedMutex {
            key,
            mutex: parking_lot::Mutex::new(()),
            rc: AtomicUsize::new(1),
        }
    }

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

    fn decrement_rc(&self, map_ref: &flurry::HashMapRef<K, ReferenceCountedMutex<K>>) {
        let curr = self.rc.fetch_sub(1, Ordering::Relaxed);

        if curr == 1 {
            map_ref.remove(&self.key);
        }
    }
}

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
        &self,
        mode: Option<&super::Mode<T>>,
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
                    let result = crate::invoke(&mode, || {
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
