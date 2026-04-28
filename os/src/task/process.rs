//! Implementation of  [`ProcessControlBlock`]

use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::TaskControlBlock;
use super::{add_task, SignalFlags};
use super::{pid_alloc, PidHandle};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{translated_byte_buffer, translated_refmut, MemorySet, KERNEL_SPACE};
use crate::sync::{Condvar, Mutex, Semaphore, UPSafeCell};
use crate::trap::{trap_handler, TrapContext};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

fn write_user_value<T>(token: usize, dst: *mut T, value: &T) {
    let size = core::mem::size_of::<T>();
    let src = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size) };
    let mut user_buf = translated_byte_buffer(token, dst as *const u8, size);
    let mut offset = 0usize;
    for slice in user_buf.iter_mut() {
        let len = slice.len();
        slice.copy_from_slice(&src[offset..offset + len]);
        offset += len;
    }
    assert_eq!(offset, size);
}

/// Process Control Block
pub struct ProcessControlBlock {
    /// immutable
    pub pid: PidHandle,
    /// mutable
    inner: UPSafeCell<ProcessControlBlockInner>,
}

/// Inner of Process Control Block
pub struct ProcessControlBlockInner {
    /// is zombie?
    pub is_zombie: bool,
    /// memory set(address space)
    pub memory_set: MemorySet,
    /// parent process
    pub parent: Option<Weak<ProcessControlBlock>>,
    /// children process
    pub children: Vec<Arc<ProcessControlBlock>>,
    /// exit code
    pub exit_code: i32,
    /// file descriptor table
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    /// signal flags
    pub signals: SignalFlags,
    /// deadlock detection enabled?
    deadlock_detect: bool,
    /// tasks(also known as threads)
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    /// task resource allocator
    pub task_res_allocator: RecycleAllocator,
    /// mutex list
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    /// semaphore list
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    /// condvar list
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    /// get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    /// allocate a new file descriptor
    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }
    /// allocate a new task id
    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }
    /// deallocate a task id
    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }
    /// the count of tasks(threads) in this process
    pub fn thread_count(&self) -> usize {
        self.tasks.len()
    }
    /// get a task with tid in this process
    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }

    fn waiting_mutex_of(&self, tid: usize) -> Option<usize> {
        self.mutex_list.iter().enumerate().find_map(|(id, mutex)| {
            mutex.as_ref().and_then(|mutex| {
                if mutex.waiting_tids().iter().any(|waiting_tid| *waiting_tid == tid) {
                    Some(id)
                } else {
                    None
                }
            })
        })
    }

    fn waiting_semaphore_of(&self, tid: usize) -> Option<usize> {
        self.semaphore_list.iter().enumerate().find_map(|(id, sem)| {
            sem.as_ref().and_then(|sem| {
                if sem.waiting_tids().iter().any(|waiting_tid| *waiting_tid == tid) {
                    Some(id)
                } else {
                    None
                }
            })
        })
    }

    fn mutex_chain_reaches(&self, start_tid: usize, target_tid: usize) -> bool {
        let mut current_tid = start_tid;
        let mut visited: Vec<usize> = Vec::new();
        loop {
            if current_tid == target_tid {
                return true;
            }
            if visited.iter().any(|visited_tid| *visited_tid == current_tid) {
                return false;
            }
            visited.push(current_tid);
            let Some(waiting_mutex_id) = self.waiting_mutex_of(current_tid) else {
                return false;
            };
            let Some(owner_tid) = self.mutex_list[waiting_mutex_id]
                .as_ref()
                .and_then(|mutex| mutex.owner_tid())
            else {
                return false;
            };
            current_tid = owner_tid;
        }
    }

    fn semaphore_chain_reaches(&self, start_tid: usize, target_tid: usize) -> bool {
        let mut stack = vec![start_tid];
        let mut visited: Vec<usize> = Vec::new();
        while let Some(tid) = stack.pop() {
            if tid == target_tid {
                return true;
            }
            if visited.iter().any(|visited_tid| *visited_tid == tid) {
                continue;
            }
            visited.push(tid);
            let Some(waiting_sem_id) = self.waiting_semaphore_of(tid) else {
                continue;
            };
            if let Some(sem) = &self.semaphore_list[waiting_sem_id] {
                for holder_tid in sem.holder_tids() {
                    stack.push(holder_tid);
                }
            }
        }
        false
    }

    fn mutex_deadlock_on_request(&self, current_tid: usize, mutex_id: usize) -> bool {
        let Some(mutex) = self.mutex_list.get(mutex_id).and_then(|mutex| mutex.as_ref()) else {
            return false;
        };
        let Some(owner_tid) = mutex.owner_tid() else {
            return false;
        };
        if owner_tid == current_tid {
            return true;
        }
        self.mutex_chain_reaches(owner_tid, current_tid)
    }

    fn semaphore_deadlock_on_request(&self, current_tid: usize, sem_id: usize) -> bool {
        let Some(sem) = self.semaphore_list.get(sem_id).and_then(|sem| sem.as_ref()) else {
            return false;
        };
        if sem.available_count() > 0 {
            return false;
        }
        let holders = sem.holder_tids();
        if holders.iter().any(|holder_tid| *holder_tid == current_tid) {
            return true;
        }
        for holder_tid in holders {
            if self.semaphore_chain_reaches(holder_tid, current_tid) {
                return true;
            }
        }
        false
    }

    fn has_mutex_deadlock(&self) -> bool {
        for mutex in self.mutex_list.iter().flatten() {
            let Some(owner_tid) = mutex.owner_tid() else {
                continue;
            };
            for waiter_tid in mutex.waiting_tids() {
                if owner_tid == waiter_tid || self.mutex_chain_reaches(owner_tid, waiter_tid) {
                    return true;
                }
            }
        }
        false
    }

    fn has_semaphore_deadlock(&self) -> bool {
        for sem in self.semaphore_list.iter().flatten() {
            let holders = sem.holder_tids();
            if holders.is_empty() {
                continue;
            }
            for waiter_tid in sem.waiting_tids() {
                if holders.iter().any(|holder_tid| *holder_tid == waiter_tid) {
                    return true;
                }
                for holder_tid in holders.iter().copied() {
                    if self.semaphore_chain_reaches(holder_tid, waiter_tid) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl ProcessControlBlock {
    /// inner_exclusive_access
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// new process from elf file
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    signals: SignalFlags::empty(),
                    deadlock_detect: false,
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>) {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        // memory_set with elf program headers/trampoline/trap context/user stack
        trace!("kernel: exec .. MemorySet::from_elf");
        let (memory_set, ustack_base, entry_point) = MemorySet::from_elf(elf_data);
        let new_token = memory_set.token();
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        self.inner_exclusive_access().memory_set = memory_set;
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        trace!("kernel: exec .. alloc user resource for main thread again");
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        // push arguments on user stack
        trace!("kernel: exec .. push arguments on user stack");
        let mut user_sp = task_inner.res.as_mut().unwrap().ustack_top();
        user_sp -= (args.len() + 1) * core::mem::size_of::<usize>();
        let argv_base = user_sp;
        write_user_value(
            new_token,
            (argv_base + args.len() * core::mem::size_of::<usize>()) as *mut usize,
            &0usize,
        );
        for i in 0..args.len() {
            user_sp -= args[i].len() + 1;
            write_user_value(
                new_token,
                (argv_base + i * core::mem::size_of::<usize>()) as *mut usize,
                &user_sp,
            );
            let mut p = user_sp;
            for c in args[i].as_bytes() {
                *translated_refmut(new_token, p as *mut u8) = *c;
                p += 1;
            }
            *translated_refmut(new_token, p as *mut u8) = 0;
        }
        // make the user_sp aligned to 8B for k210 platform
        user_sp -= user_sp % core::mem::size_of::<usize>();
        // initialize trap_cx
        trace!("kernel: exec .. initialize trap_cx");
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        *task_inner.get_trap_cx() = trap_cx;
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        trace!("kernel: fork");
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    deadlock_detect: parent.deadlock_detect,
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }
    /// get pid
    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    /// Check whether deadlock detection is enabled for this process.
    pub fn deadlock_detect_enabled(&self) -> bool {
        self.inner_exclusive_access().deadlock_detect
    }

    /// Enable or disable deadlock detection for this process.
    pub fn set_deadlock_detect(&self, enabled: bool) {
        self.inner_exclusive_access().deadlock_detect = enabled;
    }

    /// Check whether the requested mutex lock would deadlock.
    pub fn mutex_deadlock_on_request(&self, current_tid: usize, mutex_id: usize) -> bool {
        self.inner_exclusive_access()
            .mutex_deadlock_on_request(current_tid, mutex_id)
    }

    /// Check whether the requested semaphore down would deadlock.
    pub fn semaphore_deadlock_on_request(&self, current_tid: usize, sem_id: usize) -> bool {
        self.inner_exclusive_access()
            .semaphore_deadlock_on_request(current_tid, sem_id)
    }

    /// Check whether the current mutex/semaphore state already contains a deadlock.
    pub fn has_deadlock(&self) -> bool {
        let inner = self.inner_exclusive_access();
        inner.has_mutex_deadlock() || inner.has_semaphore_deadlock()
    }
}
