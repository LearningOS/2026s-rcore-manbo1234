//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{translated_byte_buffer, MapPermission, PageTable, PTEFlags, VirtAddr};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, suspend_current_and_run_next,
    with_current_task_mut,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

fn user_range_is_accessible(token: usize, start: usize, len: usize, writable: bool) -> bool {
    if len == 0 {
        return true;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return false,
    };
    let page_table = PageTable::from_token(token);
    let mut cur = start;
    while cur < end {
        let vpn = VirtAddr::from(cur).floor();
        let pte = match page_table.translate(vpn) {
            Some(pte) if pte.is_valid() => pte,
            _ => return false,
        };
        if !pte.flags().contains(PTEFlags::U) {
            return false;
        }
        if writable {
            if !pte.writable() {
                return false;
            }
        } else if !pte.readable() {
            return false;
        }
        let next_page = (cur / PAGE_SIZE + 1) * PAGE_SIZE;
        cur = next_page.min(end);
    }
    true
}

fn mmap_permission(port: usize) -> Option<MapPermission> {
    if port & !0x7 != 0 || port == 0 {
        return None;
    }
    let mut permission = MapPermission::U;
    if port & 0x1 != 0 {
        permission |= MapPermission::R;
    }
    if port & 0x2 != 0 {
        permission |= MapPermission::W;
    }
    if port & 0x4 != 0 {
        permission |= MapPermission::X;
    }
    Some(permission)
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// Write the current time into a user `TimeVal`.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let token = current_user_token();
    let size = core::mem::size_of::<TimeVal>();
    if !user_range_is_accessible(token, ts as usize, size, true) {
        return -1;
    }
    let now_us = get_time_us();
    let time_val = TimeVal {
        sec: now_us / 1_000_000,
        usec: now_us % 1_000_000,
    };
    let src = unsafe {
        core::slice::from_raw_parts((&time_val as *const TimeVal) as *const u8, size)
    };
    let mut offset = 0;
    for dst in translated_byte_buffer(token, ts as *const u8, size) {
        let len = dst.len();
        dst.copy_from_slice(&src[offset..offset + len]);
        offset += len;
    }
    0
}

/// Read/write a user byte or query syscall invocation counts.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        0 => {
            let token = current_user_token();
            if !user_range_is_accessible(token, id, 1, false) {
                return -1;
            }
            let buffer = translated_byte_buffer(token, id as *const u8, 1);
            buffer.into_iter().next().unwrap()[0] as isize
        }
        1 => {
            let token = current_user_token();
            if !user_range_is_accessible(token, id, 1, true) {
                return -1;
            }
            let buffer = translated_byte_buffer(token, id as *const u8, 1);
            buffer.into_iter().next().unwrap()[0] = data as u8;
            0
        }
        2 => super::syscall_count(id).map(|count| count as isize).unwrap_or(-1),
        _ => -1,
    }
}

/// Map anonymous user pages.
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    let permission = match mmap_permission(port) {
        Some(permission) => permission,
        None => return -1,
    };
    if len == 0 {
        return 0;
    }
    with_current_task_mut(|task| {
        if task.memory_set.mmap(VirtAddr::from(start), len, permission) {
            0
        } else {
            -1
        }
    })
}

/// Unmap anonymous user pages.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    with_current_task_mut(|task| {
        if task.memory_set.munmap(VirtAddr::from(start), len) {
            0
        } else {
            -1
        }
    })
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
