//! Runs `!Send` futures on the current thread.
use crate::task::{self, JoinHandle, Schedule, UnsendTask, Unsendable};

use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::task::{Context, Poll};

use pin_project::pin_project;

/// A set of tasks which are executed on the same thread.
///
/// In some cases, it is necessary to run one or more futures that do not
/// implement [`Send`] and thus are unsafe to send between threads. In these
/// cases, a [local task set] may be used to schedule one or more `!Send`
/// futures to run together on the same thread.
///
/// For example, the following code will not compile:
///
/// ```rust,compile_fail
/// # use tokio::runtime::Runtime;
/// use std::rc::Rc;
///
/// // `Rc` does not implement `Send`, and thus may not be sent between
/// // threads safely.
/// let unsend_data = Rc::new("my unsend data...");
///
/// let mut rt = Runtime::new().unwrap();
///
/// rt.block_on(async move {
///     let unsend_data = unsend_data.clone();
///     // Because the `async` block here moves `unsend_data`, the future is `!Send`.
///     // Since `tokio::spawn` requires the spawned future to implement `Send`, this
///     // will not compile.
///     tokio::spawn(async move {
///         println!("{}", unsend_data);
///         // ...
///     }).await.unwrap();
/// });
/// ```
/// In order to spawn `!Send` futures, we can use a local task set to
/// schedule them on the thread calling [`Runtime::block_on`]. When running
/// inside of the local task set, we can use [`task::spawn_local`], which can
/// spawn `!Send` futures. For example:
///
/// ```rust
/// # use tokio::runtime::Runtime;
/// use std::rc::Rc;
/// use tokio::task;
///
/// let unsend_data = Rc::new("my unsend data...");
///
/// let mut rt = Runtime::new().unwrap();
/// // Construct a local task set that can run `!Send` futures.
/// let local = task::LocalSet::new();
///
/// // Run the local task group.
/// local.block_on(&mut rt, async move {
///     let unsend_data = unsend_data.clone();
///     // `spawn_local` ensures that the future is spawned on the local
///     // task group.
///     task::spawn_local(async move {
///         println!("{}", unsend_data);
///         // ...
///     }).await.unwrap();
/// });
/// ```
///
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
/// [local task set]: struct.LocalSet.html
/// [`Runtime::block_on`]: ../struct.Runtime.html#method.block_on
/// [`task::spawn_local`]: fn.spawn.html
#[derive(Debug)]
pub struct LocalSet {
    scheduler: Rc<Scheduler>,
    _not_send_or_sync: PhantomData<*const ()>,
}

struct Scheduler {
    /// List of all active tasks spawned onto this executor.
    ///
    /// # Safety
    ///
    /// Must only be accessed from the primary thread
    tasks: UnsafeCell<task::OwnedList<Scheduler, Unsendable>>,

    /// Local run queue.
    ///
    /// Tasks notified from the current thread are pushed into this queue.
    ///
    /// # Safety
    ///
    /// References should not be handed out. Only call `push` / `pop` functions.
    /// Only call from the owning thread.
    queue: UnsafeCell<VecDeque<UnsendTask<Scheduler>>>,
}

#[pin_project]
struct LocalFuture<F> {
    scheduler: Rc<Scheduler>,
    #[pin]
    future: F,
}

thread_local! {
    static CURRENT_TASK_SET: Cell<Option<NonNull<Scheduler>>> = Cell::new(None);
}

/// Spawns a `!Send` future on the local task set.
///
/// The spawned future will be run on the same thread that called `spawn_local.`
/// This may only be called from the context of a local task set.
///
/// # Panics
///
/// - This function panics if called outside of a local task set.
///
/// # Examples
///
/// ```rust
/// # use tokio::runtime::Runtime;
/// use std::rc::Rc;
/// use tokio::task;
///
/// let unsend_data = Rc::new("my unsend data...");
///
/// let mut rt = Runtime::new().unwrap();
/// let local = task::LocalSet::new();
///
/// // Run the local task set.
/// local.block_on(&mut rt, async move {
///     let unsend_data = unsend_data.clone();
///     task::spawn_local(async move {
///         println!("{}", unsend_data);
///         // ...
///     }).await.unwrap();
/// });
/// ```
pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    CURRENT_TASK_SET.with(|current| {
        let current = current
            .get()
            .expect("`spawn_local` called from outside of a local::LocalSet!");
        unsafe {
            let (task, handle) = task::joinable_unsend(future);
            current.as_ref().schedule(task);
            handle
        }
    })
}

/// Max number of tasks to poll per tick.
const MAX_TASKS_PER_TICK: usize = 61;

impl LocalSet {
    /// Returns a new local task set.
    pub fn new() -> Self {
        Self {
            scheduler: Rc::new(Scheduler::new()),
            _not_send_or_sync: PhantomData,
        }
    }

    /// Spawns a `!Send` task onto the local task set.
    ///
    /// This task is guaranteed to be run on the current thread.
    ///
    /// Unlike the free function [`spawn_local`], this method may be used to
    /// spawn_local local tasks when the task set is _not_ running. For example:
    /// ```rust
    /// # use tokio::runtime::Runtime;
    /// use tokio::task;
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// let local = task::LocalSet::new();
    ///
    /// // Spawn a future on the local set. This future will be run when
    /// // we call `block_on` to drive the task set.
    /// local.spawn_local(async {
    ///    // ...
    /// });
    ///
    /// // Run the local task set.
    /// local.block_on(&mut rt, async move {
    ///     // ...
    /// });
    ///
    /// // When `block_on` finishes, we can spawn_local _more_ futures, which will
    /// // run in subsequent calls to `block_on`.
    /// local.spawn_local(async {
    ///    // ...
    /// });
    ///
    /// local.block_on(&mut rt, async move {
    ///     // ...
    /// });
    /// ```
    /// [`spawn_local`]: fn.spawn_local.html
    pub fn spawn_local<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (task, handle) = task::joinable_unsend(future);
        self.scheduler.schedule(task);
        handle
    }

    /// Run a future to completion on the provided runtime, driving any local
    /// futures spawned on this task set on the current thread.
    ///
    /// This runs the given future on the runtime, blocking until it is
    /// complete, and yielding its resolved result. Any tasks or timers which
    /// the future spawns internally will be executed on the runtime. The future
    /// may also call [`spawn_local`] to spawn_local additional local futures on the
    /// current thread.
    ///
    /// This method should not be called from an asynchronous context.
    ///
    /// # Panics
    ///
    /// This function panics if the executor is at capacity, if the provided
    /// future panics, or if called within an asynchronous execution context.
    ///
    /// # Notes
    ///
    /// Since this function internally calls [`Runtime::block_on`], and drives
    /// futures in the local task set inside that call to `block_on`, the local
    /// futures may not use [in-place blocking]. If a blocking call needs to be
    /// issued from a local task, the [`spawn_blocking`] API may be used instead.
    ///
    /// For example, this will panic:
    /// ```should_panic
    /// use tokio::runtime::Runtime;
    /// use tokio::{task, blocking};
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// let local = task::LocalSet::new();
    /// local.block_on(&mut rt, async {
    ///     let join = task::spawn_local(async {
    ///         let blocking_result = blocking::in_place(|| {
    ///             // ...
    ///         });
    ///         // ...
    ///     });
    ///     join.await.unwrap();
    /// })
    /// ```
    /// This, however, will not panic:
    /// ```
    /// use tokio::runtime::Runtime;
    /// use tokio::{task, blocking};
    ///
    /// let mut rt = Runtime::new().unwrap();
    /// let local = task::LocalSet::new();
    /// local.block_on(&mut rt, async {
    ///     let join = task::spawn_local(async {
    ///         let blocking_result = blocking::spawn_blocking(|| {
    ///             // ...
    ///         }).await;
    ///         // ...
    ///     });
    ///     join.await.unwrap();
    /// })
    /// ```
    ///
    /// [`spawn_local`]: fn.spawn_local.html
    /// [`Runtime::block_on`]: ../struct.Runtime.html#method.block_on
    /// [in-place blocking]: ../blocking/fn.in_place.html
    /// [`spawn_blocking`]: ../blocking/fn.spawn_blocking.html
    pub fn block_on<F>(&self, rt: &mut crate::runtime::Runtime, future: F) -> F::Output
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let scheduler = self.scheduler.clone();
        rt.block_on(LocalFuture { scheduler, future })
    }
}

impl Default for LocalSet {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Future> Future for LocalFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let scheduler = this.scheduler;
        let future = this.future;

        scheduler.with(|| {
            scheduler.tick();

            match future.poll(cx) {
                Poll::Ready(v) => Poll::Ready(v),
                Poll::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        })
    }
}

// === impl Scheduler ===

impl Schedule<Unsendable> for Scheduler {
    fn bind(&self, task: &UnsendTask<Self>) {
        unsafe {
            (*self.tasks.get()).insert(task);
        }
    }

    fn release(&self, _: UnsendTask<Self>) {
        unreachable!("tasks should only be completed locally")
    }

    fn release_local(&self, task: &UnsendTask<Self>) {
        unsafe {
            (*self.tasks.get()).remove(task);
        }
    }

    fn schedule(&self, task: UnsendTask<Self>) {
        unsafe {
            (*self.queue.get()).push_front(task);
        }
    }
}

impl Scheduler {
    fn new() -> Self {
        Self {
            tasks: UnsafeCell::new(task::OwnedList::new()),
            queue: UnsafeCell::new(VecDeque::with_capacity(64)),
        }
    }

    fn with<F>(&self, f: impl FnOnce() -> F) -> F {
        struct Entered<'a> {
            current: &'a Cell<Option<NonNull<Scheduler>>>,
        }

        impl<'a> Drop for Entered<'a> {
            fn drop(&mut self) {
                self.current.set(None);
            }
        }

        CURRENT_TASK_SET.with(|current| {
            current.set(Some(NonNull::from(self)));
            let _entered = Entered { current };
            f()
        })
    }

    fn is_current(&self) -> bool {
        CURRENT_TASK_SET
            .try_with(|current| {
                current
                    .get()
                    .iter()
                    .any(|current| ptr::eq(current.as_ptr(), self as *const _))
            })
            .unwrap_or(false)
    }

    fn next_task(&self) -> Option<UnsendTask<Self>> {
        unsafe { (*self.queue.get()).pop_front() }
    }

    fn tick(&self) {
        assert!(self.is_current());
        for _ in 0..MAX_TASKS_PER_TICK {
            let task = match self.next_task() {
                Some(task) => task,
                None => return,
            };

            if let Some(task) = task.run(&mut || Some(self.into())) {
                self.schedule(task);
            }
        }
    }
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

impl fmt::Debug for Scheduler {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Scheduler { .. }").finish()
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        // Drain all local tasks
        while let Some(task) = self.next_task() {
            task.shutdown();
        }

        // Release owned tasks
        unsafe {
            (*self.tasks.get()).shutdown();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::runtime;

    #[test]
    fn local_current_thread() {
        let mut rt = runtime::Builder::new().current_thread().build().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            spawn_local(async {}).await.unwrap();
        });
    }

    #[test]
    fn local_threadpool() {
        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            spawn_local(async {
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn local_threadpool_timer() {
        // This test ensures that runtime services like the timer are properly
        // set for the local task set.
        use std::time::Duration;
        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            let join = spawn_local(async move {
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
                crate::time::delay_for(Duration::from_millis(10)).await;
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
            });
            join.await.unwrap();
        });
    }

    #[test]
    // This will panic, since the thread that calls `block_on` cannot use
    // in-place blocking inside of `block_on`.
    #[should_panic]
    fn local_threadpool_blocking_in_place() {
        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            let join = spawn_local(async move {
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
                runtime::blocking::in_place(|| {});
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
            });
            join.await.unwrap();
        });
    }

    #[test]
    fn local_threadpool_blocking_run() {
        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            let join = spawn_local(async move {
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
                runtime::blocking::run(|| {
                    assert!(
                        !ON_RT_THREAD.with(|cell| cell.get()),
                        "blocking must not run on the local task set's thread"
                    );
                })
                .await;
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
            });
            join.await.unwrap();
        });
    }

    #[test]
    fn all_spawns_are_local() {
        use futures_util::future;

        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            let handles = (0..128)
                .map(|_| {
                    spawn_local(async {
                        assert!(ON_RT_THREAD.with(|cell| cell.get()));
                    })
                })
                .collect::<Vec<_>>();
            for result in future::join_all(handles).await {
                result.unwrap();
            }
        })
    }

    #[test]
    fn nested_spawn_is_local() {
        thread_local! {
            static ON_RT_THREAD: Cell<bool> = Cell::new(false);
        }

        ON_RT_THREAD.with(|cell| cell.set(true));

        let mut rt = runtime::Runtime::new().unwrap();
        LocalSet::new().block_on(&mut rt, async {
            assert!(ON_RT_THREAD.with(|cell| cell.get()));
            spawn_local(async {
                assert!(ON_RT_THREAD.with(|cell| cell.get()));
                spawn_local(async {
                    assert!(ON_RT_THREAD.with(|cell| cell.get()));
                    spawn_local(async {
                        assert!(ON_RT_THREAD.with(|cell| cell.get()));
                        spawn_local(async {
                            assert!(ON_RT_THREAD.with(|cell| cell.get()));
                        })
                        .await
                        .unwrap();
                    })
                    .await
                    .unwrap();
                })
                .await
                .unwrap();
            })
            .await
            .unwrap();
        })
    }
}
