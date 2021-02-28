use std::sync::Arc;

#[cfg(feature = "sync")]
pub mod sync;

pub fn invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(mode: &Mode<T>, task: F) -> T {
    let mut task: Box<dyn FnOnce() -> T + 'f> = Box::new(task);
    if let Some(ref mode_combiner) = mode.mode_combiner {
        for mode_wrapper in mode_combiner.iter() {
            task = mode_wrapper.wrapper_ref().wrap(task);
        }
    }

    task()
}

pub struct Sentinel<'a, I: Invoker + ?Sized> {
    invoker_ref: &'a I,
    cancelled: bool,
}

impl<I: Invoker + ?Sized> Drop for Sentinel<'_, I> {
    fn drop(&mut self) {
        if !self.cancelled {
            self.invoker_ref.post_invoke();
        }
    }
}

pub trait Invoker {
    fn pre_invoke(&self) {}

    fn invoke_with_mode<'f, T: 'f, F: FnOnce() -> T + 'f>(&self, mode: &Mode<T>, task: F) -> T {
        self.invoke_with_mode_optional(Some(mode), task)
    }

    fn invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(&self, task: F) -> T {
        self.invoke_with_mode_optional(None, task)
    }

    fn invoke_with_mode_optional<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &self,
        mode: Option<&Mode<T>>,
        task: F,
    ) -> T {
        self.pre_invoke();

        if self.invoke_post_invoke_on_panic() {
            let mut sentinel = Sentinel {
                invoker_ref: self,
                cancelled: false,
            };

            let result = self.do_invoke(mode, task);

            sentinel.cancelled = true;
            self.post_invoke();
            result
        } else {
            let result = self.do_invoke(mode, task);

            self.post_invoke();
            result
        }
    }

    fn do_invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(&self, mode: Option<&Mode<T>>, task: F) -> T {
        if let Some(mode) = mode {
            invoke(mode, task)
        } else {
            task()
        }
    }

    fn post_invoke(&self) {}

    fn invoke_post_invoke_on_panic(&self) -> bool {
        false
    }

    fn and_then<I: Invoker>(self, inner: I) -> CombinedInvoker<Self, I>
    where
        Self: Sized,
    {
        CombinedInvoker { outer: self, inner }
    }
}

pub struct BaseInvoker {}

impl Invoker for BaseInvoker {}

pub struct CombinedInvoker<O: Invoker, I: Invoker> {
    outer: O,
    inner: I,
}

impl<O: Invoker, I: Invoker> CombinedInvoker<O, I> {
    pub fn combine(outer: O, inner: I) -> CombinedInvoker<O, I> {
        CombinedInvoker { outer, inner }
    }
}

impl<O: Invoker, I: Invoker> Invoker for CombinedInvoker<O, I> {
    fn invoke_with_mode_optional<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &self,
        mode: Option<&Mode<T>>,
        task: F,
    ) -> T {
        self.outer
            .invoke_with_mode_optional(mode, || self.inner.invoke_with_mode_optional(mode, task))
    }
}

pub struct Mode<'m, T: 'm> {
    mode_combiner: Option<Box<dyn ModeCombiner<'m, T> + 'm>>,
}

impl<'m, T: 'm> Mode<'m, T> {
    pub fn new() -> Self {
        Self {
            mode_combiner: None,
        }
    }

    pub fn with<M: ModeWrapper<'m, T> + 'm>(mut self, mode_wrapper: M) -> Self {
        if let Some(curr_combiner) = self.mode_combiner {
            self.mode_combiner = Some(curr_combiner.combine(mode_wrapper.into_combiner()));
        } else {
            self.mode_combiner = Some(mode_wrapper.into_combiner());
        }

        self
    }
}

impl<'m, T: 'm> Default for Mode<'m, T> {
    fn default() -> Self {
        Mode::new()
    }
}

pub trait ModeWrapper<'m, T: 'm> {
    fn wrap<'f>(self: Arc<Self>, task: Box<dyn FnOnce() -> T + 'f>) -> Box<dyn FnOnce() -> T + 'f>;

    fn into_combiner(self) -> Box<dyn ModeCombiner<'m, T> + 'm>
    where
        Self: Sized + 'm,
    {
        Box::new(DelegatingModeCombiner {
            wrapper: Arc::new(self),
            outer: None,
        })
    }
}

pub trait ModeCombiner<'m, T: 'm> {
    fn combine(
        &self,
        other: Box<dyn ModeCombiner<'m, T> + 'm>,
    ) -> Box<dyn ModeCombiner<'m, T> + 'm>;

    fn get_outer(&self) -> Option<&dyn ModeCombiner<'m, T>>;

    fn set_outer(&mut self, outer: Arc<dyn ModeCombiner<'m, T> + 'm>);

    fn iter<'a>(&'a self) -> ModeCombinerIterator<'a, 'm, T>;

    fn wrapper_ref(&self) -> Arc<dyn ModeWrapper<'m, T> + 'm>;
}

pub struct DelegatingModeCombiner<'m, T> {
    wrapper: Arc<dyn ModeWrapper<'m, T> + 'm>,
    outer: Option<Arc<dyn ModeCombiner<'m, T> + 'm>>,
}

impl<T> Clone for DelegatingModeCombiner<'_, T> {
    fn clone(&self) -> Self {
        DelegatingModeCombiner {
            wrapper: self.wrapper.clone(),
            outer: self.outer.clone(),
        }
    }
}

impl<'m, T> ModeCombiner<'m, T> for DelegatingModeCombiner<'m, T> {
    fn combine(
        &self,
        mut other: Box<dyn ModeCombiner<'m, T> + 'm>,
    ) -> Box<dyn ModeCombiner<'m, T> + 'm> {
        let clone = self.clone();
        other.set_outer(Arc::new(clone));
        other
    }

    fn get_outer(&self) -> Option<&dyn ModeCombiner<'m, T>> {
        if let Some(ref outer) = self.outer {
            Some(outer.as_ref())
        } else {
            None
        }
    }

    fn set_outer(&mut self, outer: Arc<dyn ModeCombiner<'m, T> + 'm>) {
        self.outer = Some(outer);
    }

    fn iter<'a>(&'a self) -> ModeCombinerIterator<'a, 'm, T> {
        ModeCombinerIterator {
            mode_combiner: self,
            curr_combiner: None,
        }
    }

    fn wrapper_ref(&self) -> Arc<dyn ModeWrapper<'m, T> + 'm> {
        self.wrapper.clone()
    }
}

pub struct ModeCombinerIterator<'a, 'm, T: 'm> {
    mode_combiner: &'a dyn ModeCombiner<'m, T>,
    curr_combiner: Option<&'a dyn ModeCombiner<'m, T>>,
}

impl<'a, 'm, T: 'm> Iterator for ModeCombinerIterator<'a, 'm, T> {
    type Item = &'a dyn ModeCombiner<'m, T>;

    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        if let Some(curr_wrapper) = self.curr_combiner {
            let curr_outer = curr_wrapper.get_outer();

            if let Some(curr_outer) = curr_outer {
                self.curr_combiner = Some(curr_outer);
            } else {
                return None;
            }
        } else {
            self.curr_combiner = Some(self.mode_combiner);
        }

        self.curr_combiner
    }
}

#[cfg(test)]
mod tests {
    use crate::{invoke, Invoker, Mode, ModeWrapper};
    use std::sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    };

    static PRE_COUNTER: AtomicU16 = AtomicU16::new(1);
    static POST_COUNTER: AtomicU16 = AtomicU16::new(1);

    struct MultiplyTwoMode {}
    impl ModeWrapper<'static, i32> for MultiplyTwoMode {
        fn wrap<'f>(
            self: Arc<Self>,
            task: Box<(dyn FnOnce() -> i32 + 'f)>,
        ) -> Box<(dyn FnOnce() -> i32 + 'f)> {
            Box::new(move || {
                return task() * 2;
            })
        }
    }

    struct AddTwoMode {}
    impl ModeWrapper<'static, i32> for AddTwoMode {
        fn wrap<'f>(
            self: Arc<Self>,
            task: Box<(dyn FnOnce() -> i32 + 'f)>,
        ) -> Box<(dyn FnOnce() -> i32 + 'f)> {
            Box::new(move || {
                return task() + 2;
            })
        }
    }

    struct CounterInvoker {}
    impl Invoker for CounterInvoker {
        fn pre_invoke(&self) {
            PRE_COUNTER.fetch_add(1, Ordering::Relaxed);
        }
        fn post_invoke(&self) {
            POST_COUNTER.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct MultInvoker {}
    impl Invoker for MultInvoker {
        fn pre_invoke(&self) {
            PRE_COUNTER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| Some(x * 2))
                .unwrap();
        }
        fn post_invoke(&self) {
            POST_COUNTER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| Some(x * 2))
                .unwrap();
        }
    }

    #[test]
    fn it_works() {
        let mode = Mode::new().with(MultiplyTwoMode {}).with(AddTwoMode {});
        assert_eq!(invoke(&mode, || 2 + 2), 12);
    }

    #[test]
    fn test_combined_invoker() {
        let invoker = CounterInvoker {}
            .and_then(MultInvoker {})
            .and_then(MultInvoker {})
            .and_then(CounterInvoker {});

        invoker.invoke(|| {
            PRE_COUNTER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| Some(x * 3))
                .unwrap();

            POST_COUNTER
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| Some(x * 3))
                .unwrap();
        });

        assert_eq!(PRE_COUNTER.load(Ordering::Relaxed), 27);
        assert_eq!(POST_COUNTER.load(Ordering::Relaxed), 17);
    }
}
