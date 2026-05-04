//! Types related to task management

use super::TaskContext;

/// Maximum syscall id we need to trace in ch3.
pub const MAX_SYSCALL_NUM: usize = 512;

/// The task control block (TCB) of a task.
#[derive(Copy, Clone)]
pub struct TaskControlBlock {
    /// The task status in it's lifecycle
    pub task_status: TaskStatus,
    /// The task context
    pub task_cx: TaskContext,
    /// The syscall counts of this task.
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
}

impl TaskControlBlock {
    /// Record one syscall for this task.
    pub fn record_syscall(&mut self, syscall_id: usize) {
        if syscall_id < MAX_SYSCALL_NUM {
            self.syscall_times[syscall_id] += 1;
        }
    }

    /// Query how many times this task has invoked a syscall.
    pub fn syscall_times(&self, syscall_id: usize) -> isize {
        self.syscall_times.get(syscall_id).copied().unwrap_or(0) as isize
    }
}

/// The status of a task
#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Exited,
}
