//! Windows-native process-tree and TCP-listener primitives.
//!
//! Why this module exists: every DSH backend restart used to shell out to
//! `netstat -ano -p tcp` (to learn who listens on 3080) and to
//! `taskkill /PID … /T /F` (to kill the owned tree), then poll those foreign
//! processes every 300–400 ms while waiting for the port to come free. Each
//! `netstat` spawn costs ~50–300 ms on this machine (measured), the poll loop
//! runs up to 8 s, and a dying server can stretch each readiness HTTP probe
//! to its 3 s timeout — a restart that should take ~1 s routinely took
//! 6–20 s (see dsh.log: "[backend] 重启清理" → "===== 启动尝试" gaps).
//!
//! All three operations exist in-process in the Win32 API, so this module
//! replaces the spawn-and-parse round trips with direct FFI — the same
//! raw-`kernel32`/explicit-`#[link]` style as `memtok.rs` and the
//! MessageBox FFI in lib.rs (both learned the LNK2019 lesson: the `windows`
//! crates in the dependency tree mostly use raw-dylib, so the import libs
//! must be named explicitly).
//!
//!   * [`listener_pids`] — who is LISTENING on a TCP port
//!     (`GetExtendedTcpTable`, iphlpapi). Microseconds, no process spawn.
//!     Matches `netstat -ano -p tcp` semantics for the IPv4 listener case
//!     (the DSH web server binds 127.0.0.1) while ignoring the many
//!     ESTABLISHED rows that share the same local port.
//!   * [`kill_tree`] — kill a process and its descendants natively
//!     (Toolhelp32 snapshot + `TerminateProcess`), replacing `taskkill /T`.
//!   * [`wait_exit`] — block on a process handle until it exits
//!     (`OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject`), so the restart
//!     chain proceeds the instant the kernel reaps the old backend instead
//!     of polling the port table on a timer.

#[cfg(windows)]
use std::time::Duration;

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

/// PIDs currently LISTENING on `port` (IPv4, TCP). Empty when nothing
/// listens. Never spawns a helper process.
#[cfg(windows)]
pub(crate) fn listener_pids(port: u16) -> Vec<u32> {
    // AF_INET = 2, TCP_TABLE_OWNER_PID_LISTENER = 5.
    const AF_INET: u32 = 2;
    const TCP_TABLE_OWNER_PID_LISTENER: u32 = 5;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const NO_ERROR: u32 = 0;
    const MIB_LISTENING: u32 = 2;

    #[repr(C)]
    struct MibTcpRowOwnerPid {
        dw_state: u32,
        dw_local_addr: u32,
        // 16-bit port stored in network byte order.
        dw_local_port: u32,
        dw_remote_addr: u32,
        dw_remote_port: u32,
        dw_owning_pid: u32,
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetExtendedTcpTable(
            p_tcp_table: *mut core::ffi::c_void,
            pdw_size: *mut u32,
            b_order: i32,
            ul_af: u32,
            table_class: u32,
            reserved: u32,
        ) -> u32;
    }

    unsafe {
        // Sizing call: the required size comes back in pdw_size.
        let mut size: u32 = 0;
        let rc = GetExtendedTcpTable(
            core::ptr::null_mut(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if size == 0 || (rc != NO_ERROR && rc != ERROR_INSUFFICIENT_BUFFER) {
            // Empty table, or an unexpected failure — degrade to "no
            // listeners"; the caller's readiness probe stays authoritative.
            return Vec::new();
        }

        let mut buffer: Vec<u8> = vec![0u8; size as usize];
        let rc = GetExtendedTcpTable(
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if rc != NO_ERROR {
            return Vec::new();
        }

        // MIB_TCPTABLE_OWNER_PID = { DWORD dwNumEntries; MIB_TCPROW_OWNER_PID table[]; }
        // The rows come from a byte buffer (1-byte aligned), so every read is
        // unaligned-safe; the trailing-state filter also copes with table
        // classes that return non-listener rows.
        let base = buffer.as_ptr();
        let num_entries = (base as *const u32).read_unaligned();
        if num_entries == 0 {
            return Vec::new();
        }
        let rows = base.add(4) as *const MibTcpRowOwnerPid;
        let row_len = std::mem::size_of::<MibTcpRowOwnerPid>();
        // Guard against a truncated buffer: never read past what the API wrote.
        let available = (size as usize).saturating_sub(4) / row_len;
        let count = (num_entries as usize).min(available);
        let wanted = port.to_be() as u32;

        let mut pids = Vec::new();
        for i in 0..count {
            let row = core::ptr::read_unaligned(rows.add(i));
            if row.dw_state == MIB_LISTENING && row.dw_local_port == wanted {
                if !pids.contains(&row.dw_owning_pid) {
                    pids.push(row.dw_owning_pid);
                }
            }
        }
        pids
    }
}

/// Kill `root_pid` and its whole descendant tree, deepest first, natively.
/// Safety guards: never the current process, never the idle/System pids.
/// Failures (access denied, already gone) are silent — the caller's port
/// poll is the final authority on whether the port actually came free.
#[cfg(windows)]
pub(crate) fn kill_tree(root_pid: u32) {
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: isize = -1;

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut core::ffi::c_void;
        fn Process32FirstW(h_snapshot: *mut core::ffi::c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(h_snapshot: *mut core::ffi::c_void, lppe: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
    }

    if root_pid == 0 || root_pid == 4 || root_pid == std::process::id() {
        return;
    }

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || (snapshot as isize) == INVALID_HANDLE_VALUE {
            // Fall back to terminating the root alone: better than nothing
            // and matches the old taskkill-without-/T behavior on failure.
            terminate_one(root_pid, PROCESS_TERMINATE);
            return;
        }

        // One consistent (pid -> ppid) table from the snapshot.
        let mut table: Vec<(u32, u32)> = Vec::with_capacity(256);
        let mut entry = std::mem::zeroed::<ProcessEntry32W>();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                table.push((entry.th32_process_id, entry.th32_parent_process_id));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);

        // Children map for a BFS from the root.
        let mut ordered: Vec<u32> = vec![root_pid];
        let mut i = 0;
        while i < ordered.len() {
            let parent = ordered[i];
            for (pid, ppid) in &table {
                if *ppid == parent && *pid != std::process::id() && !ordered.contains(pid) {
                    ordered.push(*pid);
                }
            }
            i += 1;
        }

        // Terminate deepest first so children die before the (potentially
        // respawning) parent; the snapshot list is a fixed point.
        for pid in ordered.iter().rev() {
            if *pid == std::process::id() || *pid <= 4 {
                continue;
            }
            terminate_one(*pid, PROCESS_TERMINATE);
        }
    }
}

/// Open + terminate one process; silent on any failure.
#[cfg(windows)]
unsafe fn terminate_one(pid: u32, access: u32) {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut core::ffi::c_void;
        fn TerminateProcess(h_process: *mut core::ffi::c_void, u_exit_code: u32) -> i32;
        fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
    }
    if pid == 0 || pid == 4 || pid == std::process::id() {
        return;
    }
    let handle = OpenProcess(access, 0, pid);
    if handle.is_null() {
        return; // already gone, or access denied
    }
    let _ = TerminateProcess(handle, 1);
    CloseHandle(handle);
}

/// Block until `pid` exits or `budget` elapses. Returns true when the
/// process is gone. An `OpenProcess` failure means it is already reaped.
#[cfg(windows)]
pub(crate) fn wait_exit(pid: u32, budget: Duration) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(
            dw_desired_access: u32,
            b_inherit_handle: i32,
            dw_process_id: u32,
        ) -> *mut core::ffi::c_void;
        fn WaitForSingleObject(h_handle: *mut core::ffi::c_void, dw_milliseconds: u32) -> u32;
        fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
    }

    if pid == 0 || pid == 4 || pid == std::process::id() {
        return true;
    }

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Already exited and reaped — that is the state we wanted.
            return true;
        }
        let remaining = budget.as_millis().min(u32::MAX as u128) as u32;
        let rc = WaitForSingleObject(handle, remaining);
        CloseHandle(handle);
        rc == WAIT_OBJECT_0
    }
}

// ---------------------------------------------------------------------------
// Non-Windows fallbacks: the previous spawn-and-parse implementations.
// The shipped shell is Windows-only; these keep the crate compiling
// cross-platform (and CI honest about cfg-gating).
// ---------------------------------------------------------------------------

/// Listener PID on `port` via `netstat` (non-Windows fallback).
#[cfg(not(windows))]
pub(crate) fn listener_pids(port: u16) -> Vec<u32> {
    let mut command = std::process::Command::new("netstat");
    command.args(["-ano", "-p", "tcp"]);
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let suffix = format!(":{port}");
    let mut pids = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 5
            && fields[0] == "TCP"
            && fields[1].ends_with(&suffix)
            && fields[4] != "0"
        {
            if let Ok(pid) = fields[4].parse::<u32>() {
                if !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

/// Tree kill via `taskkill /T /F` (non-Windows fallback).
#[cfg(not(windows))]
pub(crate) fn kill_tree(pid: u32) {
    let mut command = std::process::Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    let _ = command.status();
}

/// Best-effort exit wait via a short poll (non-Windows fallback).
#[cfg(not(windows))]
pub(crate) fn wait_exit(pid: u32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        let alive = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}
