# exec-rs

Rust port of the corresponding [kotlin lib](https://github.com/robinfriedli/exec).

Library that provides utility traits for task execution and, if the sync feature is enabled, the ability to synchronise
tasks based on the value of a key.

## Installation

To add exec-rs to your project simply add the following Cargo dependency:
```toml
[dependencies]
exec-rs = "0.1.0"
```

Or to exclude the "sync" feature:
```toml
[dependencies.exec-rs]
version = "0.1.0"
default-features = false
```

## Executors

Provides two different types that can manage task execution depending on the use case.

### Invoker

```rust
pub trait Invoker {

    fn pre_invoke(&self);

    fn invoke_with_mode<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: &'f Mode<'f, T>,
        task: F,
    ) -> T;

    fn invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(&'f self, task: F) -> T;

    fn invoke_with_mode_optional<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: Option<&'f Mode<'f, T>>,
        task: F,
    ) -> T;

    fn do_invoke<'f, T: 'f, F: FnOnce() -> T + 'f>(
        &'f self,
        mode: Option<&'f Mode<'f, T>>,
        task: F,
    ) -> T;

    fn post_invoke(&self);

    fn invoke_post_invoke_on_panic(&self) -> bool;

    fn and_then<I: Invoker>(self, inner: I) -> CombinedInvoker<Self, I>;
}
```

Trait that may be implemented for types that manage executing a task that do not care about
the return type of the task. Implementors may simply override `pre_invoke` and `post_invoke` to run code before or / and after
invoking a task or override `do_invoke` to control exactly how a task is invoked, by default this simply calls the task
if no mode was supplied or calls `crate::invoke` if a mode was supplied.

Calling `pre_invoke` and `post_invoke` is managed by `invoke_with_mode_optional` which is the function used by both `invoke_with_mode`
and `invoke` and internally calls `do_invoke`. So if implementors override `invoke_with_mode_optional`
they either must manage calling `pre_invoke` and `post_invoke` or not use these functions.

### Mode

The Mode API consists of 3 parts: Mode, ModeWrapper and ModeCombiner.

#### Mode
```rust
pub struct Mode<'m, T: 'm> {
    mode_combiner: Option<Box<dyn ModeCombiner<'m, T> + 'm>>,
}
```
Struct that manages collecting and combining `ModeWrapper`. This is the type supplied when submitting tasks in order to apply `ModeWrappers`.

Modes may be used by implementing the `ModeWrapper` trait to be able to wrap tasks in an enclosing function. Unlike 
`Invokers`, `Modes` and `ModeWrappers` are generic over the return type of the tasks they may wrap and thus can
directly interact with the return value of the task. This also means that the lifetime of the
Mode is tied to the lifetime of the type they are generic over.

#### ModeWrapper
```rust
pub trait ModeWrapper<'m, T: 'm> {

    fn wrap(self: Arc<Self>, task: Box<dyn FnOnce() -> T + 'm>) -> Box<dyn FnOnce() -> T + 'm>;

    fn into_combiner(self) -> Box<dyn ModeCombiner<'m, T> + 'm>;
}
```
Trait to implement in order to apply a mode to a task. ModeWrappers are supplied to a `Mode` using `Mode::with` where
they might be combined with other ModeWrappers using the `ModeCombiner` supplied by `ModeWrapper::into_combiner`.
Unlike `Invokers`, `Modes` and `ModeWrappers` are generic over the return type
of the tasks they may wrap and thus can directly interact with the return value of the task.
This also means that the lifetime of the Mode is tied to the lifetime of the type they are generic over.

#### ModeCombiner
```rust
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
```
Trait used to combine `ModeWrappers` by allowing one `ModeWrapper` to delegate to another `ModeWrapper` and providing an
iterator that can unwrap combined ModeWrappers. An implementation of this trait is returned by `ModeWrapper::into_combiner`
which returns a `DelegatingModeCombiner` by default.

## Sync

The sync module provides the `MutexSync` struct which can be used to execute and synchronise tasks by the value of the
key provided when submitting a task.

```rust
pub struct MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    mutex_map: flurry::HashMap<K, ReferenceCountedMutex<K>>,
}

impl<K> MutexSync<K>
where
    K: 'static + Sync + Send + Clone + Hash + Ord,
{
    pub fn new() -> Self;

    pub fn evaluate<R, F: FnOnce() -> R>(&self, key: K, task: F) -> R;
}
```
Task executor that can synchronise tasks by value of a key provided when submitting a task.

For example, if i32 is used as key type, then tasks submitted with keys 3 and 5 may run concurrently
but several tasks submitted with key 7 are synchronised by a mutex mapped to the key.

Manages a concurrent hash map that maps `ReferenceCountedMutex` elements to the used keys. The `ReferenceCountedMutex` struct
holds a mutex used for synchronisation and removes itself from the map automatically if not used by
any thread anymore by managing an atomic reference counter. If the counter is decremented from 1 to
0 the element is removed from the map and the counter cannot be incremented back up again. If the counter
reached 0 future increments fail and a new `ReferenceCountedMutex` is created instead. When creating
a new `ReferenceCountedMutex` and inserting it to the map fails because another thread has already
created an element for the same key, the current thread tries to use the found existing element instead
as long as its reference counter is valid (greater than 0), else it retries creating the element.

The type of the key used for synchronisation must be able to be used as a key for the map and thus
must implement `Sync + Send + Clone + Hash + Ord` and have a static lifetime.