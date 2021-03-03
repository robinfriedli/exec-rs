use std::sync::Arc;

#[cfg(feature = "sync")]
pub mod sync;

/// Function that implements using a reference to a [`Mode`](struct.Mode.html) to invoke a task.
///
/// Uses the iterator returned by calling [`ModeCombiner::iter`](trait.ModeCombiner.html#method.iter)
/// on the [`ModeCombiner`](trait.ModeCombiner.html) created by the last [`Mode::with`](struct.Mode.html#method.with)
/// invocation to unwrap the [`ModeCombiner`](trait.ModeCombiner.html) inside out and wrap the submitted
/// task using [`ModeWrapper::wrap`](trait.ModeWrapper.html#method.wrap) at each step.
///
/// Then calls the produced task or simply calls the submitted task if no [`ModeWrapper`](trait.ModeWrapper.html)
/// has been supplied to the [`Mode`](struct.Mode.html).
///
/// [`Mode::with`](struct.Mode.html#method.with) consumes the supplied [`ModeWrapper`](trait.ModeWrapper.html)
/// to produce a [`ModeCombiner`](trait.ModeCombiner.html). The `ModeCombiner` is set on the `Mode` and,
/// if there already is a `ModeCombiner` present, combined with the existing `ModeCombiner` by calling
/// [`ModeCombiner::combine`](trait.ModeCombiner.html#method.combine). By default this produces a
/// [`DelegatingModeCombiner`](struct.DelegatingModeCombiner.html) that combines `ModeCombiners` by
/// setting the current `ModeCombiner` as the outer `ModeCombiner` of the newly added `ModeCombiner`
/// so that the iterator walks the `ModeCombiners` in the reverse order of which they were added, meaning
/// the `ModeCombiner` that was added first ends up wrapping the task last, meaning its task will be the
/// outermost task.
pub fn invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(mode: &Mode<'f, T>, task: F) -> T {
    let mut task: Box<dyn FnOnce() -> T + 'f> = Box::new(task);
    if let Some(ref mode_combiner) = mode.mode_combiner {
        for mode_wrapper in mode_combiner.iter() {
            task = mode_wrapper.wrapper_ref().wrap(task);
        }
    }

    task()
}

/// Trait that may be implemented for types that manage executing a task that do not care about
/// the return type of the task. Implementors may simply override [`pre_invoke`](trait.Invoker.html#method.pre_invoke)
/// and [`post_invoke`](trait.Invoker.html#method.post_invoke) to run code before or / and after
/// invoking a task or override [`do_invoke`](trait.Invoker.html#method.do_invoke) to control
/// exactly how a task is invoked, by default this simply calls the task if no mode was supplied
/// or calls [`crate::invoke`] if a mode was supplied.
///
/// Calling `pre_invoke` and `post_invoke` is managed by [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional)
/// which is the function used by both [`invoke_with_mode`](trait.Invoker.html#method.invoke_with_mode)
/// and [`invoke`](trait.Invoker.html#method.invoke) and internally calls [`do_invoke`](trait.Invoker.html#method.do_invoke).
/// So if implementors override [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional)
/// they either must manage calling `pre_invoke` and `post_invoke` or not use these functions.
pub trait Invoker {
    /// Called by [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional) before
    /// invoking each task. Can be used to run code before a task is invoked.
    fn pre_invoke(&self) {}

    /// Invoke a task with a [`Mode`](struct.Mode.html). The default implementation for [`do_invoke`](trait.Invoker.html#method.do_invoke)
    /// delegates to [`crate::invoke`] if a mode was supplied. The `Mode` is applied to
    /// the task invoked by this `Invoker`, meaning the task of modes will run inside the
    /// [`do_invoke`](trait.Invoker.html#method.do_invoke) invocation so that logic run by
    /// the invoker before the task runs executes before the modes and logic run by the
    /// invoker after the task executes after the modes.
    fn invoke_with_mode<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: &'f Mode<'f, T>,
        task: F,
    ) -> T {
        self.invoke_with_mode_optional(Some(mode), task)
    }

    /// Invoke a task using this invoker without a [`Mode`](struct.Mode.html).
    ///
    /// Calls [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional) with
    /// `None` as [`Mode`](struct.Mode.html), which in turn calls [`pre_invoke`](trait.Invoker.html#method.pre_invoke)
    /// and [`post_invoke`](trait.Invoker.html#method.post_invoke) and invokes the task by calling
    /// [`do_invoke`](trait.Invoker.html#method.do_invoke).
    fn invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(&'f self, task: F) -> T {
        self.invoke_with_mode_optional(None, task)
    }

    /// Invoke a task, optionally with a [`Mode`](struct.Mode.html) supplied.
    ///
    /// Note that this function is used by both [`invoke_with_mode`](trait.Invoker.html#method.invoke_with_mode)
    /// and [`invoke`](trait.Invoker.html#method.invoke) and responsible for invoking
    /// [`pre_invoke`](trait.Invoker.html#method.pre_invoke) and [`post_invoke`](trait.Invoker.html#method.post_invoke)
    /// and delegating invocation of the task to [`do_invoke`](trait.Invoker.html#method.do_invoke).
    fn invoke_with_mode_optional<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: Option<&'f Mode<'f, T>>,
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

    /// Core function responsible for actually invoking the task. This allows implementors to easily override
    /// how tasks are invoked without having to worry about doing any administrative tasks such as calling
    /// [`pre_invoke`](trait.Invoker.html#method.pre_invoke) and [`post_invoke`](trait.Invoker.html#method.post_invoke)
    /// as would be the case when overriding [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional).
    fn do_invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: Option<&'f Mode<'f, T>>,
        task: F,
    ) -> T {
        if let Some(mode) = mode {
            invoke(mode, task)
        } else {
            task()
        }
    }

    /// Called by [`invoke_with_mode_optional`](trait.Invoker.html#method.invoke_with_mode_optional) after
    /// invoking each task. Can be used to run code after a task is invoked.
    fn post_invoke(&self) {}

    /// Return true if [`post_invoke`](trait.Invoker.html#method.post_invoke) should be called even if
    /// the task panicked using a [`Sentinel`](struct.Sentinel.html), defaults to false.
    fn invoke_post_invoke_on_panic(&self) -> bool {
        false
    }

    /// Combines this `Invoker` with another `Invoker` by creating a [`CombinedInvoker`](struct.CombinedInvoker.html)
    /// that invokes tasks by first calling this `Invoker` with a task that submits the supplied task to the
    /// other `Invoker`, meaning the other `Invoker` will run inside this `Invoker` in such a way that the logic
    /// of this `Invoker` that runs before the task executes before the logic of the other `Invoker` but the logic
    /// of this `Invoker` that runs after the task executes after the logic of the other `Invoker`.
    fn and_then<I: Invoker>(self, inner: I) -> CombinedInvoker<Self, I>
    where
        Self: Sized,
    {
        CombinedInvoker { outer: self, inner }
    }
}

/// Type that manages calling [`Invoker::post_invoke`](trait.Invoker.html#method.post_invoke) when
/// dropped and [`Invoker::invoke_post_invoke_on_panic`](trait.Invoker.html#method.invoke_post_invoke_on_panic)
/// is true in case the task panicked.
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

/// Struct that provides an empty [`Invoker`](trait.Invoker.html) implementation.
pub struct BaseInvoker {}

impl Invoker for BaseInvoker {}

/// Struct that enables combining two [`Invokers`](trait.Invoker.html) by calling the second invoker
/// inside the first one.
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
        &'f self,
        mode: Option<&'f Mode<'f, T>>,
        task: F,
    ) -> T {
        self.outer.invoke_with_mode_optional(mode, move || {
            self.inner.invoke_with_mode_optional(mode, task)
        })
    }
}

/// Struct that manages collecting and combining [`ModeWrapper`](trait.ModeWrapper.html).
/// This is the type supplied when submitting tasks in order to apply `ModeWrappers`.
///
/// Modes may be used by implementing the [`ModeWrapper`](trait.ModeWrapper.html) trait to be able to
/// wrap tasks in an enclosing function. Unlike [`Invokers`](trait.Invoker.html), Modes and
/// `ModeWrappers` are generic over the return type of the tasks they may wrap and thus can
/// directly interact with the return value of the task. This also means that the lifetime of the
/// Mode is tied to the lifetime of the type they are generic over.
pub struct Mode<'m, T: 'm> {
    mode_combiner: Option<Box<dyn ModeCombiner<'m, T> + 'm>>,
}

impl<'m, T: 'm> Mode<'m, T> {
    /// Construct a new empty mode with no [`ModeCombiner`](trait.ModeCombiner.html).
    ///
    /// This Mode may already be used to invoke tasks with, in which case the Mode will simply
    /// be ignored.
    pub fn new() -> Self {
        Self {
            mode_combiner: None,
        }
    }

    /// Add a new [`ModeWrapper`](trait.ModeWrapper.html) implementation to this Mode.
    ///
    /// This calls the [`ModeWrapper::into_combiner`](trait.ModeWrapper.html#method.into_combiner) function
    /// of the `ModeWrapper` and sets the resulting [`ModeCombiner`](trait.ModeCombiner.html) on
    /// this Mode and, if there already is `ModeCombiner` present, combines the two by calling
    /// [`ModeCombiner::combine`](trait.ModeCombiner.html#method.combine) on the existing `ModeCombiner`.
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

/// Trait to implement in order to apply a mode to a task. ModeWrappers are supplied to a [`Mode`](struct.Mode.html)
/// using [`Mode::with`](trait.Mode.html#method.with) where they might be combined with other ModeWrappers
/// using the [`ModeCombiner`](trait.ModeCombiner.html) supplied by [`ModeWrapper::into_combiner`](trait.ModeWrapper.html#method.into_combiner).
/// Unlike [`Invokers`](trait.Invoker.html), `Modes` and `ModeWrappers` are generic over the return type
/// of the tasks they may wrap and thus can directly interact with the return value of the task.
/// This also means that the lifetime of the Mode is tied to the lifetime of the type they are generic over.
pub trait ModeWrapper<'m, T: 'm> {
    /// Applies this ModeWrapper to a task by wrapping the supplied boxed function into a new boxed function
    /// with the same return type and lifetime, both of which this ModeWrapper is generic over.
    fn wrap(self: Arc<Self>, task: Box<dyn FnOnce() -> T + 'm>) -> Box<dyn FnOnce() -> T + 'm>;

    /// Consume this ModeWrapper and produce a [`ModeCombiner`](trait.ModeCombiner.html). This is used by
    /// [`Mode::with`](trait.Mode.html#method.with) to be able to combine several ModeWrappers and can
    /// reference back to this ModeWrapper to wrap tasks when applying a Mode to a task. Combining
    /// ModeWrappers is implemented by a separate trait to be able to provide a default implementation
    /// in [`DelegatingModeCombiner`](struct.DelegatingModeCombiner.html) that combines `ModeCombiners` by
    /// setting the current `ModeCombiner` as the outer `ModeCombiner` of the newly added `ModeCombiner`
    /// so that the iterator walks the `ModeCombiners` in the reverse order of which they were added, meaning
    /// the `ModeCombiner` that was added first ends up wrapping the task last, meaning its task will be the
    /// outermost task.
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

/// Trait used to combine [`ModeWrappers`](trait.ModeWrapper.html) by allowing one `ModeWrapper` to
/// delegate to another `ModeWrapper` and providing an iterator that can unwrap combined ModeWrappers.
/// An implementation of this trait is returned by [`ModeWrapper::into_combiner`](trait.ModeWrapper.html#method.into_combiner)
/// which returns a [`DelegatingModeCombiner`](struct.DelegatingModeCombiner.html) by default.
pub trait ModeCombiner<'m, T: 'm> {
    /// Combine this ModeCombiner with the supplied boxed ModeCombiner.
    ///
    /// The default implementation in [`DelegatingModeCombiner`](struct.DelegatingModeCombiner.html)
    /// sets this ModeCombiner as the outer ModeCombiner of the supplied ModeCombiner so that the
    /// iterator walks the `ModeCombiners` in the reverse order of which they were added, meaning
    /// the `ModeCombiner` that was added first ends up wrapping the task last, meaning its task will be the
    /// outermost task.
    fn combine(
        &self,
        other: Box<dyn ModeCombiner<'m, T> + 'm>,
    ) -> Box<dyn ModeCombiner<'m, T> + 'm>;

    /// Return the outer ModeCombiner this ModeCombiner delegates to, this is the next ModeCombiner
    /// the iterator returned by [`ModeCombiner::iter`](trait.ModeCombiner.html#method.iter) steps to.
    fn get_outer(&self) -> Option<&dyn ModeCombiner<'m, T>>;

    /// Set the outer ModeCombiner this ModeCombiner delegates to, this is the next ModeCombiner
    /// the iterator returned by [`ModeCombiner::iter`](trait.ModeCombiner.html#method.iter) steps to.
    fn set_outer(&mut self, outer: Arc<dyn ModeCombiner<'m, T> + 'm>);

    /// Return an iterator that can unwrap combined ModeCombiners by stepping into the outer
    /// ModeCombiner recursively.
    fn iter<'a>(&'a self) -> ModeCombinerIterator<'a, 'm, T>;

    /// Reference the source [`ModeWrapper`](trait.ModeWrapper.html). Used to wrap the task when
    /// applying a [`Mode`](struct.Mode.html).
    fn wrapper_ref(&self) -> Arc<dyn ModeWrapper<'m, T> + 'm>;
}

/// Default implementation for the [`ModeWrapper`](trait.ModeWrapper.html) trait that combines `ModeCombiners` by
/// setting the current `ModeCombiner` as the outer `ModeCombiner` of the newly added `ModeCombiner`
/// so that the iterator walks the `ModeCombiners` in the reverse order of which they were added, meaning
/// the `ModeCombiner` that was added first ends up wrapping the task last, meaning its task will be the
/// outermost task.
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

/// Iterator that can unwrap combined ModeCombiners by stepping into the outer
/// ModeCombiner recursively.
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
    use crate::{invoke, BaseInvoker, Invoker, Mode, ModeCombinerIterator, ModeWrapper};
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

    struct StringRefMode<'a> {
        str_ref: &'a str,
    }
    impl<'a> ModeWrapper<'a, &'a str> for StringRefMode<'a> {
        fn wrap(
            self: Arc<Self>,
            task: Box<(dyn FnOnce() -> &'a str + 'a)>,
        ) -> Box<(dyn FnOnce() -> &'a str + 'a)> {
            Box::new(move || {
                task();
                self.str_ref
            })
        }
    }

    struct ModeCombinerIteratorMode {}
    impl<'a, 'm> ModeWrapper<'a, ModeCombinerIterator<'a, 'm, &'m str>> for ModeCombinerIteratorMode {
        fn wrap(
            self: Arc<Self>,
            task: Box<dyn FnOnce() -> ModeCombinerIterator<'a, 'm, &'m str> + 'a>,
        ) -> Box<dyn FnOnce() -> ModeCombinerIterator<'a, 'm, &'m str> + 'a> {
            Box::new(move || task())
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

    #[test]
    fn test_lifetime_iterator() {
        let s = String::from("test");
        let m = StringRefMode { str_ref: &s };
        let combiner = m.into_combiner();
        let iter = combiner.iter();

        let mode = Mode::new().with(ModeCombinerIteratorMode {});
        let _iter = invoke(&mode, move || iter);
    }

    #[test]
    fn test_lifetime_str_ref() {
        let s = String::from("test");
        let m = StringRefMode { str_ref: &s };

        let mode = Mode::new().with(m);

        assert_eq!("test", invoke(&mode, || { "fail" }));
    }

    #[test]
    fn test_shorter_lifetime() {
        let invoker = BaseInvoker {};

        {
            let shorter_lived_string = String::from("test");
            let str_ref = invoker.invoke(|| &shorter_lived_string);
            assert_eq!(str_ref, "test");
        }
    }
}
