//! Mutex (spin-like and blocking(sleep))

use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::block_current_and_run_next;
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

fn task_tid(task: &Arc<TaskControlBlock>) -> usize {
    task.inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

fn queue_contains_tid(queue: &VecDeque<Arc<TaskControlBlock>>, tid: usize) -> bool {
    queue.iter().any(|task| task_tid(task) == tid)
}

fn remove_waiter(queue: &mut VecDeque<Arc<TaskControlBlock>>, tid: usize) {
    if let Some(pos) = queue.iter().position(|task| task_tid(task) == tid) {
        let _ = queue.remove(pos);
    }
}

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex
    fn lock(&self);
    /// Unlock the mutex
    fn unlock(&self);
    /// Owner thread id if the mutex is held.
    fn owner_tid(&self) -> Option<usize>;
    /// Thread ids waiting on this mutex.
    fn waiting_tids(&self) -> Vec<usize>;
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    inner: UPSafeCell<MutexSpinInner>,
}

struct MutexSpinInner {
    locked: bool,
    owner: Option<usize>,
    granted: Option<usize>,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexSpinInner {
                    locked: false,
                    owner: None,
                    granted: None,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) {
        trace!("kernel: MutexSpin::lock");
        let tid = current_tid();
        loop {
            let mut inner = self.inner.exclusive_access();
            if inner.granted == Some(tid) {
                inner.granted = None;
                return;
            }
            if !inner.locked {
                inner.locked = true;
                inner.owner = Some(tid);
                remove_waiter(&mut inner.wait_queue, tid);
                return;
            }
            if !queue_contains_tid(&inner.wait_queue, tid) {
                inner.wait_queue.push_back(Arc::clone(&current_task().unwrap()));
            }
            drop(inner);
            block_current_and_run_next();
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut inner = self.inner.exclusive_access();
        assert!(inner.locked);
        if let Some(waking_task) = inner.wait_queue.pop_front() {
            let waking_tid = task_tid(&waking_task);
            inner.owner = Some(waking_tid);
            inner.granted = Some(waking_tid);
            wakeup_task(waking_task);
        } else {
            inner.locked = false;
            inner.owner = None;
            inner.granted = None;
        }
    }

    fn owner_tid(&self) -> Option<usize> {
        self.inner.exclusive_access().owner
    }

    fn waiting_tids(&self) -> Vec<usize> {
        self.inner
            .exclusive_access()
            .wait_queue
            .iter()
            .map(task_tid)
            .collect()
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
}

struct MutexBlockingInner {
    locked: bool,
    owner: Option<usize>,
    granted: Option<usize>,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new() -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    owner: None,
                    granted: None,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) {
        trace!("kernel: MutexBlocking::lock");
        let tid = current_tid();
        let mut mutex_inner = self.inner.exclusive_access();
        if mutex_inner.granted == Some(tid) {
            mutex_inner.granted = None;
            return;
        }
        if mutex_inner.locked {
            if !queue_contains_tid(&mutex_inner.wait_queue, tid) {
                mutex_inner.wait_queue.push_back(Arc::clone(&current_task().unwrap()));
            }
            drop(mutex_inner);
            block_current_and_run_next();
            let mut mutex_inner = self.inner.exclusive_access();
            if mutex_inner.granted == Some(tid) {
                mutex_inner.granted = None;
            } else {
                remove_waiter(&mut mutex_inner.wait_queue, tid);
            }
        } else {
            mutex_inner.locked = true;
            mutex_inner.owner = Some(tid);
        }
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        assert!(mutex_inner.locked);
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            let waking_tid = task_tid(&waking_task);
            mutex_inner.owner = Some(waking_tid);
            mutex_inner.granted = Some(waking_tid);
            wakeup_task(waking_task);
        } else {
            mutex_inner.locked = false;
            mutex_inner.owner = None;
            mutex_inner.granted = None;
        }
    }

    fn owner_tid(&self) -> Option<usize> {
        self.inner.exclusive_access().owner
    }

    fn waiting_tids(&self) -> Vec<usize> {
        self.inner
            .exclusive_access()
            .wait_queue
            .iter()
            .map(task_tid)
            .collect()
    }
}
