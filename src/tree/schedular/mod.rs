use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    mem::{offset_of, ManuallyDrop},
    pin::Pin,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    task::{Context, Poll},
};

mod queue;
mod waker;
use queue::Queue;

use crate::{defer::Defer, vtable::VTable};

use self::queue::NodeHeader;

pub struct CancelToken(NonNull<Task<u8>>);

impl CancelToken {
    pub fn detach(self) {
        // properly drop the pointer,
        // Safety: Below self is forgotten, so it won't be dropped twice.
        unsafe { Schedular::drop_task(self.0) };
        // forget so the drop impl wont run.
        std::mem::forget(self);
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        let rf = unsafe { self.0.as_ref() };
        // Set queued to prefend it from
        if matches!(rf.body.done.get(), TaskState::Cancelled | TaskState::Done) {
            // Safety: weak only dropped hear and when detached, in which case this won't run so this is sound.
            unsafe { Schedular::drop_task(self.0) };
            return;
        }

        rf.body.done.set(TaskState::Cancelled);

        if rf
            .body
            .queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Task wasn't scheduled yet, add it so it can be remove from the list.
            // If the queue was already dropped the task will be cancelled already.
            if let Some(queue) = rf.body.queue.upgrade() {
                unsafe {
                    Pin::new_unchecked(&*queue).push(self.0.cast());
                    queue.waker().wake();
                }
                // we transfered the ownership to the queue, so we dont need to run drop.
                return;
            }
        }
        // Safety: weak only dropped hear and when detached, in which case this won't run so this is sound.
        unsafe { Schedular::drop_task(self.0) };
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SchedularVTable {
    task_drive: unsafe fn(NonNull<Task<u8>>, cx: &mut Context) -> Poll<()>,
    task_drop: unsafe fn(NonNull<Task<u8>>),
}

impl SchedularVTable {
    pub const fn get<F: Future<Output = ()>>() -> SchedularVTable {
        SchedularVTable {
            task_drop: Self::drop::<F>,
            task_drive: Self::drive::<F>,
        }
    }

    unsafe fn drop<F: Future<Output = ()>>(ptr: NonNull<Task<u8>>) {
        Arc::decrement_strong_count(ptr.cast::<Task<F>>().as_ptr())
    }

    unsafe fn drive<F: Future<Output = ()>>(ptr: NonNull<Task<u8>>, cx: &mut Context) -> Poll<()> {
        let ptr = ptr.cast::<Task<F>>();
        Pin::new_unchecked(&mut *(*ptr.as_ref().future.get())).poll(cx)
    }
}

#[repr(C)]
struct Task<F> {
    head: NodeHeader,
    body: TaskBody,
    future: UnsafeCell<ManuallyDrop<F>>,
}

impl<F> Task<F>
where
    F: Future<Output = ()>,
{
    fn new(queue: Weak<Queue>, future: F) -> Self {
        Task {
            head: NodeHeader::new(),
            body: TaskBody {
                queue,
                vtable: VTable::get::<F>(),
                next: Cell::new(None),
                prev: Cell::new(None),
                queued: AtomicBool::new(true),
                done: Cell::new(TaskState::Running),
            },
            future: UnsafeCell::new(ManuallyDrop::new(future)),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TaskState {
    // Task is still actively running,
    Running,
    // Task was cancelled but not yet freed from the list.
    Cancelled,
    // Task is done, and should be removed.
    Done,
}

// Seperate struct to not have everything be repr(C)
struct TaskBody {
    queue: Weak<Queue>,
    vtable: &'static VTable,
    // The double linked list of tasks.
    next: Cell<Option<NonNull<Task<u8>>>>,
    prev: Cell<Option<NonNull<Task<u8>>>>,
    done: Cell<TaskState>,
    // wether the task is currently in the queue to be re-polled.
    queued: AtomicBool,
}

pub struct Schedular {
    len: Cell<usize>,
    should_poll: Arc<Queue>,
    all_next: Cell<Option<NonNull<Task<u8>>>>,
    all_prev: Cell<Option<NonNull<Task<u8>>>>,
}

impl Schedular {
    pub fn new() -> Self {
        let queue = Arc::new(Queue::new());
        unsafe {
            Pin::new_unchecked(&*queue).init();
        }
        Schedular {
            len: Cell::new(0),
            should_poll: queue,
            all_prev: Cell::new(None),
            all_next: Cell::new(None),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.all_next.get().is_none()
    }

    /// # Safety
    /// This function erases any lifetime associated with the future.
    /// Caller must ensure that either the future completes or is dropped before the lifetime
    pub unsafe fn push<F>(&self, f: F)
    where
        F: Future<Output = ()>,
    {
        let queue = Arc::downgrade(&self.should_poll);

        debug_assert_eq!(offset_of!(Task<F>, future), offset_of!(Task<u8>, future));

        let task = Arc::new(Task::new(queue, f));
        // One count for the all list and one for the should_poll list.
        let task = NonNull::new_unchecked(Arc::into_raw(task) as *mut Task<F>).cast::<Task<u8>>();
        Arc::increment_strong_count(task.as_ptr());

        self.push_task_to_all(task);

        Pin::new_unchecked(&*self.should_poll).push(task.cast());
        self.len.set(self.len.get() + 1);
    }

    /// # Safety
    /// This function erases any lifetime associated with the future.
    /// Caller must ensure that either the future completes or is dropped before the lifetime
    pub unsafe fn push_cancellable<F>(&self, f: F) -> CancelToken
    where
        F: Future<Output = ()>,
    {
        let queue = Arc::downgrade(&self.should_poll);

        debug_assert_eq!(offset_of!(Task<F>, future), offset_of!(Task<u8>, future));

        let task = Arc::new(Task::new(queue, f));

        // One count for the all list and one for the should_poll list and one for the cancel
        // token.
        let task = NonNull::new_unchecked(Arc::into_raw(task) as *mut Task<F>).cast::<Task<u8>>();
        Arc::increment_strong_count(task.as_ptr());
        Arc::increment_strong_count(task.as_ptr());

        self.push_task_to_all(task);

        Pin::new_unchecked(&*self.should_poll).push(task.cast());
        self.len.set(self.len.get() + 1);

        CancelToken(task)
    }

    unsafe fn push_task_to_all(&self, task: NonNull<Task<u8>>) {
        task.as_ref().body.next.set(self.all_next.get());

        if let Some(x) = self.all_next.get() {
            x.as_ref().body.prev.set(Some(task));
        }
        self.all_next.set(Some(task));
        if self.all_prev.get().is_none() {
            self.all_prev.set(Some(task));
        }
    }

    unsafe fn pop_task_all(&self, task: NonNull<Task<u8>>) {
        task.as_ref().body.queued.store(true, Ordering::Release);
        task.as_ref().body.done.set(TaskState::Done);

        // detach the task from the all list
        if let Some(next) = task.as_ref().body.next.get() {
            next.as_ref().body.prev.set(task.as_ref().body.prev.get())
        } else {
            self.all_prev.set(task.as_ref().body.prev.get());
        }
        if let Some(prev) = task.as_ref().body.prev.get() {
            prev.as_ref().body.next.set(task.as_ref().body.next.get())
        } else {
            self.all_next.set(task.as_ref().body.next.get());
        }

        // drop the ownership of the all list,
        // Task is now dropped or only owned by wakers or
        Self::drop_task(task);
        self.len.set(self.len.get() - 1);
    }

    unsafe fn drop_task(ptr: NonNull<Task<u8>>) {
        (ptr.as_ref().body.vtable.tree.task_drop)(ptr)
    }

    unsafe fn drive_task(ptr: NonNull<Task<u8>>, ctx: &mut Context) -> Poll<()> {
        (ptr.as_ref().body.vtable.tree.task_drive)(ptr, ctx)
    }

    pub unsafe fn poll(&self, cx: &mut Context) -> Poll<()> {
        // During polling ownership is upheld by making sure arc counts are properly tranfered.
        // Both ques, should_poll and all, have ownership of the arc count.
        // Whenever a task is pushed onto the should_poll queue ownership is transfered.
        // During task pusing into the schedular ownership of the count was transfered into the all
        // list.

        if self.is_empty() {
            // No tasks, nothing to be done.
            return Poll::Ready(());
        }

        self.should_poll.waker().register(cx.waker());

        let mut iteration = 0;
        let mut yielded = 0;

        loop {
            // Popped a task, ownership taken from the que
            let cur = match Pin::new_unchecked(&*self.should_poll).pop() {
                queue::Pop::Empty => {
                    return if self.is_empty() {
                        // No more tasks left, nothing to be done.
                        Poll::Ready(())
                    } else {
                        // Tasks left but none ready, return ownership.
                        Poll::Pending
                    };
                }
                queue::Pop::Value(x) => x,
                queue::Pop::Inconsistant => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };

            let cur = cur.cast::<Task<u8>>();

            match cur.as_ref().body.done.get() {
                TaskState::Cancelled => {
                    self.pop_task_all(cur);
                    continue;
                }
                TaskState::Done => {
                    // Task was already done, we con drop the ownership we got from the que.
                    Self::drop_task(cur);
                    continue;
                }
                TaskState::Running => {}
            }

            let prev = cur.as_ref().body.queued.swap(false, Ordering::AcqRel);
            assert!(prev);

            // ownership transfered into the waker, which won't drop until the iteration completes.
            let waker = waker::get(cur);
            // if drive_task panics we still want to remove the task from the list.
            // So handle it with a drop
            let remove = Defer::new(self, |this| (*this).pop_task_all(cur));
            let mut ctx = Context::from_waker(&waker);

            iteration += 1;

            match Self::drive_task(cur, &mut ctx) {
                Poll::Ready(_) => {
                    // Nothing todo the defer will remove the task from the list.
                }
                Poll::Pending => {
                    // don't remove task from the list.
                    remove.take();
                    yielded += cur.as_ref().body.queued.load(Ordering::Relaxed) as usize;
                    if yielded > 2 || iteration > self.len.get() {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
            }
        }
    }

    pub fn clear(&self) {
        // Clear all pending futures from the all list
        let mut cur = self.all_next.get();
        while let Some(c) = cur {
            unsafe {
                cur = c.as_ref().body.next.get();
                self.pop_task_all(c)
            }
        }

        loop {
            let cur = match unsafe { Pin::new_unchecked(&*self.should_poll).pop() } {
                queue::Pop::Empty => break,
                queue::Pop::Value(x) => x,
                queue::Pop::Inconsistant => {
                    std::thread::yield_now();
                    continue;
                }
            };

            unsafe { Self::drop_task(cur.cast()) };
        }
    }
}

impl Drop for Schedular {
    fn drop(&mut self) {
        self.clear()
    }
}
