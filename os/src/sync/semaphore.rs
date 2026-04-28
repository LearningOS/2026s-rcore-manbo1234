//! Semaphore

use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::{BTreeMap, VecDeque}, sync::Arc, vec::Vec};

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

/// semaphore structure
pub struct Semaphore {
    /// semaphore inner
    inner: UPSafeCell<SemaphoreInner>,
}

struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    holders: BTreeMap<usize, usize>,
    granted: VecDeque<usize>,
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(res_count: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                    holders: BTreeMap::new(),
                    granted: VecDeque::new(),
                })
            },
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let tid = current_tid();
        let mut inner = self.inner.exclusive_access();
        let mut remove_holder = false;
        if let Some(count) = inner.holders.get_mut(&tid) {
            if *count > 0 {
                *count -= 1;
                if *count == 0 {
                    remove_holder = true;
                }
            }
        }
        if remove_holder {
            let _ = inner.holders.remove(&tid);
        }
        inner.count += 1;
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                let waking_tid = task_tid(&task);
                *inner.holders.entry(waking_tid).or_insert(0) += 1;
                inner.granted.push_back(waking_tid);
                wakeup_task(task);
            }
        }
    }

    /// down operation of semaphore
    pub fn down(&self) {
        trace!("kernel: Semaphore::down");
        let tid = current_tid();
        let mut inner = self.inner.exclusive_access();
        inner.count -= 1;
        if inner.count < 0 {
            if !queue_contains_tid(&inner.wait_queue, tid) {
                inner.wait_queue.push_back(Arc::clone(&current_task().unwrap()));
            }
            drop(inner);
            block_current_and_run_next();
            let mut inner = self.inner.exclusive_access();
            if let Some(pos) = inner.granted.iter().position(|grant_tid| *grant_tid == tid) {
                let _ = inner.granted.remove(pos);
            } else {
                *inner.holders.entry(tid).or_insert(0) += 1;
            }
            remove_waiter(&mut inner.wait_queue, tid);
        } else {
            *inner.holders.entry(tid).or_insert(0) += 1;
        }
    }

    /// The number of available semaphore units.
    pub fn available_count(&self) -> isize {
        self.inner.exclusive_access().count
    }

    /// Get the tids currently waiting on this semaphore.
    pub fn waiting_tids(&self) -> Vec<usize> {
        self.inner
            .exclusive_access()
            .wait_queue
            .iter()
            .map(task_tid)
            .collect()
    }

    /// Get the tids currently holding this semaphore.
    pub fn holder_tids(&self) -> Vec<usize> {
        self.inner
            .exclusive_access()
            .holders
            .iter()
            .filter_map(|(tid, count)| if *count > 0 { Some(*tid) } else { None })
            .collect()
    }
}
