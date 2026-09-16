//! Launch-token recovery from the memory of a running `dsh web` process.
//!
//! `dsh web` mints one random launch token per process and prints it exactly
//! once, as `http://127.0.0.1:<port>/?token=<43 base64url characters>`. The
//! token is held only in that process (`PROCESS_LAUNCH_TOKENS` /
//! `BrowserAuth.launchToken` in `packages/client/connection/src/browser-auth.ts`),
//! so once the printed line is gone — a detached desktop shell, a scrolled-away
//! terminal, a recycled log — an attached instance cannot be authenticated from
//! disk. This module reads the token back out of the owning process instead:
//!
//!   1. enumerate the committed, readable regions of the process listening on
//!      the port;
//!   2. harvest candidates — first the 43 characters behind the printed URL's
//!      `token=` anchor, then every maximal base64url run of exactly the token
//!      length, filtered by the 32-byte tail signature that
//!      `encodeBase64Url(randomBytes(32))` makes a mathematical certainty;
//!   3. exchange each candidate over HTTP and accept only the one that answers
//!      303 while minting the `dsh-auth-*` browser session cookie.
//!
//! Shape is never accepted as proof, and nothing is written to disk: this
//! reader only ever calls `ReadProcessMemory`.
//!
//! Windows only. The reader is raw `kernel32` FFI rather than a new dependency
//! — the same reason `lib.rs` declares `user32` by hand instead of pulling in a
//! bindings crate.

/// Result of one recovery attempt.
pub(crate) enum Outcome {
    /// A candidate exchanged for a browser session. `stats` carries the
    /// per-pass scan statistics for the log (never the token itself).
    Found { token: String, stats: String },
    /// Nothing verified. `detail` carries the per-pass scan statistics and the
    /// reason, so the fallback prompt can say what was already tried.
    NotFound { detail: String },
}

/// Recover the verified launch token of the process listening on `port`.
///
/// Blocking and CPU-bound (a second or two of memory reads); call it from a
/// background thread. Returns `NotFound` — never a shape-only guess — when the
/// process cannot be opened, holds no candidate, or no candidate verifies.
#[cfg(windows)]
pub(crate) fn recover(pid: u32, port: u16) -> Outcome {
    imp::recover(pid, port)
}

#[cfg(not(windows))]
pub(crate) fn recover(_pid: u32, _port: u16) -> Outcome {
    Outcome::NotFound {
        detail: "内存恢复仅 Windows 可用".to_string(),
    }
}

#[cfg(windows)]
mod imp {
    use super::Outcome;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::fmt;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const PROCESS_VM_READ: u32 = 0x0010;
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_IMAGE: u32 = 0x0100_0000;
    const PAGE_GUARD: u32 = 0x100;
    const PAGE_NOACCESS: u32 = 0x01;

    /// `encodeBase64Url(randomBytes(32))` — 32 bytes become 43 base64url
    /// characters.
    const TOKEN_LEN: usize = 43;
    /// Bytes per `ReadProcessMemory` call, plus a `TOKEN_LEN` overlap so a token
    /// straddling a chunk boundary is still seen whole.
    const CHUNK_BYTES: usize = 4 * 1024 * 1024;
    /// Candidates one worker gathers before an HTTP probe round. Batches keep
    /// the scan from reading all of memory before the first exchange, so a
    /// token found early stops the scan early.
    const BATCH_SIZE: usize = 16;
    const MAX_WORKERS: usize = 8;
    /// Candidates probed at once inside one batch. A whole-memory pass yields
    /// hundreds of candidates and the real token is often near the end of them,
    /// so probing one at a time would make the HTTP round-trips the dominant
    /// cost of the whole recovery.
    const PROBE_LANES: usize = 8;
    /// Per-candidate probe timeout. A live `dsh web` answers a loopback GET in
    /// single-digit milliseconds, so anything slower is a dead or wedged server.
    const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

    /// The `token=` of the printed launch URL. Passing it as the anchor narrows
    /// a pass to the one place the token is known to appear verbatim; measured
    /// on a live instance the anchored pass sees single-digit candidates where
    /// the unanchored pass sees hundreds.
    const TOKEN_ANCHOR: &[u8] = b"token=";

    /// Characters a 43-character base64url encoding of exactly 32 bytes can end
    /// with. The final character carries only the last 4 input bits, so its
    /// 6-bit value is a multiple of four. This is a consequence of the token's
    /// length, not an assumption about its content.
    const TAIL_ALPHABET: &[u8] = b"AEIMQUYcgkosw048";

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn VirtualQueryEx(
            process: *mut c_void,
            address: *const c_void,
            info: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
        fn ReadProcessMemory(
            process: *mut c_void,
            address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    /// `MEMORY_BASIC_INFORMATION`. On x64 Windows inserts `PartitionId` (a WORD)
    /// between `AllocationProtect` and `RegionSize`; declaring `region_size` as
    /// `usize` makes the compiler insert exactly that padding, so every field
    /// lands at its real offset on both x64 and x86.
    ///
    /// The two leading-pad fields are written by `VirtualQueryEx` and never read
    /// here, but `#[repr(C)]` needs them present to keep the offsets right.
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        _allocation_base: *mut c_void,
        _allocation_protect: u32,
        region_size: usize,
        state: u32,
        protect: u32,
        ty: u32,
    }

    impl MemoryBasicInformation {
        fn empty() -> Self {
            Self {
                base_address: std::ptr::null_mut(),
                _allocation_base: std::ptr::null_mut(),
                _allocation_protect: 0,
                region_size: 0,
                state: 0,
                protect: 0,
                ty: 0,
            }
        }
    }

    /// Owns the process handle so every early return still closes it.
    ///
    /// `Send`/`Sync` are asserted by hand because the handle is a bare pointer:
    /// a Win32 process handle is just a kernel object reference, and reading it
    /// from several threads at once with `ReadProcessMemory` is a read-only
    /// operation the kernel serializes.
    struct ProcessHandle(*mut c_void);

    unsafe impl Send for ProcessHandle {}
    unsafe impl Sync for ProcessHandle {}

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Per-pass scan statistics, for the shell log. Contains no token.
    struct PassStats {
        regions: usize,
        images_skipped: usize,
        image_bytes: u64,
        scanned: u64,
        candidates: usize,
        probed: usize,
        solved: bool,
        elapsed: Duration,
    }

    impl fmt::Display for PassStats {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "regions={} imagesSkipped={}({:.1}MB) scanned={:.1}MB candidates={} probed={} earlyStop={} {:.1}s",
                self.regions,
                self.images_skipped,
                self.image_bytes as f64 / 1_048_576.0,
                self.scanned as f64 / 1_048_576.0,
                self.candidates,
                self.probed,
                if self.solved { "yes" } else { "no" },
                self.elapsed.as_secs_f64(),
            )
        }
    }

    const fn base64url_table() -> [bool; 256] {
        let mut table = [false; 256];
        let mut c = b'A';
        while c <= b'Z' {
            table[c as usize] = true;
            c += 1;
        }
        let mut c = b'a';
        while c <= b'z' {
            table[c as usize] = true;
            c += 1;
        }
        let mut c = b'0';
        while c <= b'9' {
            table[c as usize] = true;
            c += 1;
        }
        table[b'-' as usize] = true;
        table[b'_' as usize] = true;
        table
    }

    const fn tail_table() -> [bool; 256] {
        let mut table = [false; 256];
        let mut i = 0;
        while i < TAIL_ALPHABET.len() {
            table[TAIL_ALPHABET[i] as usize] = true;
            i += 1;
        }
        table
    }

    static BASE64URL: [bool; 256] = base64url_table();
    static TAIL: [bool; 256] = tail_table();

    fn is_readable(info: &MemoryBasicInformation) -> bool {
        let protect = info.protect & 0xFF;
        info.state == MEM_COMMIT
            && (info.protect & PAGE_GUARD) == 0
            && protect != 0
            && protect != PAGE_NOACCESS
    }

    /// Append every maximal base64url run of exactly `TOKEN_LEN` bytes.
    ///
    /// Maximality is what keeps ordinary hashes and ids out: a run longer than
    /// the token length is some other encoding, and a run shorter than it is
    /// truncated. `tail_filter` additionally requires the final character to be
    /// reachable by a 32-byte payload; the last pass drops it to stay correct if
    /// the upstream token encoding ever changes.
    fn collect_runs(buffer: &[u8], tail_filter: bool, out: &mut Vec<String>) {
        let length = buffer.len();
        for i in 0..length {
            if !BASE64URL[buffer[i] as usize] {
                continue;
            }
            // Only a run's first byte can start the token.
            if i > 0 && BASE64URL[buffer[i - 1] as usize] {
                continue;
            }
            let end = i + TOKEN_LEN;
            if end > length {
                break;
            }
            if !shaped(buffer, i, end) {
                continue;
            }
            if end < length && BASE64URL[buffer[end] as usize] {
                continue;
            }
            accepted(buffer, i, end, tail_filter, out);
        }
    }

    /// Append every token that starts immediately after `marker`.
    ///
    /// The marker itself is not base64url, so a match is by construction the
    /// start of its run — no predecessor check is needed.
    fn collect_anchored(buffer: &[u8], marker: &[u8], tail_filter: bool, out: &mut Vec<String>) {
        let span = marker.len() + TOKEN_LEN;
        if buffer.len() < span {
            return;
        }
        for i in 0..=buffer.len() - span {
            if !buffer[i..].starts_with(marker) {
                continue;
            }
            let start = i + marker.len();
            let end = start + TOKEN_LEN;
            if !shaped(buffer, start, end) {
                continue;
            }
            if end < buffer.len() && BASE64URL[buffer[end] as usize] {
                continue;
            }
            accepted(buffer, start, end, tail_filter, out);
        }
    }

    /// True when every byte of `buffer[start..end]` is base64url.
    fn shaped(buffer: &[u8], start: usize, end: usize) -> bool {
        (start..end).all(|at| BASE64URL[buffer[at] as usize])
    }

    /// Apply the tail filter and push the token when it passes.
    fn accepted(
        buffer: &[u8],
        start: usize,
        end: usize,
        tail_filter: bool,
        out: &mut Vec<String>,
    ) {
        if tail_filter && !TAIL[buffer[end - 1] as usize] {
            return;
        }
        // Every byte passed the base64url table, so the slice is ASCII.
        if let Ok(token) = std::str::from_utf8(&buffer[start..end]) {
            out.push(token.to_string());
        }
    }

    /// Exchange one candidate for a browser session over the loopback HTTP
    /// server. A valid launch token answers `303` with a `Set-Cookie` for the
    /// `dsh-auth-*` session; every other candidate answers `401`.
    ///
    /// The request is hand-written rather than built with the crate's HTTP
    /// client for two reasons: an explicit `Host` header is mandatory (the
    /// cookie name and its signed audience both derive from the request
    /// authority, so a request without `Host` is always 401), and the probe
    /// must see the bare `303` instead of following it.
    fn probe(token: &str, port: u16) -> bool {
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
        let request = format!(
            "GET /?token={token} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        // Headers only: the body is irrelevant, and `Connection: close` ends the
        // read at EOF if the server keeps the socket open.
        let mut head: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    head.extend_from_slice(&chunk[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") || head.len() > 16 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&head);
        let mut lines = text.split("\r\n");
        let status = lines.next().unwrap_or_default();
        if !status.starts_with("HTTP/1.1 303") && !status.starts_with("HTTP/1.0 303") {
            return false;
        }
        text.split("\r\n").any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("set-cookie:") && lower.contains("dsh-auth-")
        })
    }

    /// Probe the candidates of one batch on several lanes at once, skipping any
    /// already probed by another worker. Returns the first candidate that
    /// verifies; that hit stops the remaining lanes.
    fn probe_batch(
        batch: &[String],
        port: u16,
        seen: &Mutex<HashSet<String>>,
        probed: &AtomicUsize,
    ) -> Option<String> {
        let fresh: Vec<String> = {
            let mut guard = match seen.lock() {
                Ok(guard) => guard,
                Err(_) => return None,
            };
            batch
                .iter()
                .filter(|candidate| guard.insert((*candidate).clone()))
                .cloned()
                .collect()
        };
        if fresh.is_empty() {
            return None;
        }

        let winner: Mutex<Option<String>> = Mutex::new(None);
        let stop = AtomicBool::new(false);
        let next = AtomicUsize::new(0);
        let lanes = fresh.len().min(PROBE_LANES);

        std::thread::scope(|scope| {
            for _ in 0..lanes {
                scope.spawn(|| loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= fresh.len() {
                        break;
                    }
                    probed.fetch_add(1, Ordering::Relaxed);
                    if probe(&fresh[index], port) {
                        stop.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = winner.lock() {
                            if slot.is_none() {
                                *slot = Some(fresh[index].clone());
                            }
                        }
                        break;
                    }
                });
            }
        });

        winner.lock().ok().and_then(|mut slot| slot.take())
    }

    /// One full pass: enumerate, then read the regions on several threads,
    /// probing candidates as they arrive and stopping the moment one verifies.
    fn scan(
        process: &ProcessHandle,
        port: u16,
        anchor: Option<&[u8]>,
        skip_images: bool,
        tail_filter: bool,
    ) -> (Option<String>, PassStats) {
        let started = Instant::now();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut images_skipped = 0usize;
        let mut image_bytes = 0u64;

        // Enumerate first, then read: enumeration is cheap and the resulting
        // list is what the workers divide between them.
        let mut address = 0usize;
        let mut info = MemoryBasicInformation::empty();
        loop {
            let written = unsafe {
                VirtualQueryEx(
                    process.0,
                    address as *const c_void,
                    &mut info,
                    std::mem::size_of::<MemoryBasicInformation>(),
                )
            };
            if written == 0 {
                break;
            }
            let base = info.base_address as usize;
            let size = info.region_size;
            let next = base.saturating_add(size);
            if is_readable(&info) && size > 0 {
                if skip_images && (info.ty & MEM_IMAGE) != 0 {
                    // A mapped image is executable code; the token is a JS string
                    // on the V8 heap in private pages.
                    images_skipped += 1;
                    image_bytes += size as u64;
                } else {
                    ranges.push((base, size));
                }
            }
            if next <= address {
                break;
            }
            address = next;
        }

        let next_range = AtomicUsize::new(0);
        let scanned = AtomicU64::new(0);
        let probed = AtomicUsize::new(0);
        let candidates = AtomicUsize::new(0);
        let solved = AtomicBool::new(false);
        let winner: Mutex<Option<String>> = Mutex::new(None);
        let seen: Mutex<HashSet<String>> = Mutex::new(HashSet::new());

        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(MAX_WORKERS);

        let record = |hit: String| {
            solved.store(true, Ordering::Relaxed);
            if let Ok(mut slot) = winner.lock() {
                if slot.is_none() {
                    *slot = Some(hit);
                }
            }
        };

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let mut buffer = vec![0u8; CHUNK_BYTES + TOKEN_LEN];
                    let mut batch: Vec<String> = Vec::with_capacity(BATCH_SIZE);
                    while !solved.load(Ordering::Relaxed) {
                        let index = next_range.fetch_add(1, Ordering::Relaxed);
                        if index >= ranges.len() {
                            break;
                        }
                        let (base, size) = ranges[index];
                        let mut offset = 0usize;
                        while offset < size && !solved.load(Ordering::Relaxed) {
                            let want = buffer.len().min(size - offset);
                            let mut read = 0usize;
                            let ok = unsafe {
                                ReadProcessMemory(
                                    process.0,
                                    (base + offset) as *const c_void,
                                    buffer.as_mut_ptr() as *mut c_void,
                                    want,
                                    &mut read,
                                )
                            };
                            if ok != 0 && read > 0 {
                                scanned.fetch_add(read as u64, Ordering::Relaxed);
                                match anchor {
                                    Some(marker) => collect_anchored(
                                        &buffer[..read],
                                        marker,
                                        tail_filter,
                                        &mut batch,
                                    ),
                                    None => collect_runs(&buffer[..read], tail_filter, &mut batch),
                                }
                            }
                            if batch.len() >= BATCH_SIZE {
                                candidates.fetch_add(batch.len(), Ordering::Relaxed);
                                if let Some(hit) = probe_batch(&batch, port, &seen, &probed) {
                                    record(hit);
                                    break;
                                }
                                batch.clear();
                            }
                            offset += CHUNK_BYTES;
                        }
                    }
                    // Candidates this worker read but never got to probe.
                    if !batch.is_empty() && !solved.load(Ordering::Relaxed) {
                        candidates.fetch_add(batch.len(), Ordering::Relaxed);
                        if let Some(hit) = probe_batch(&batch, port, &seen, &probed) {
                            record(hit);
                        }
                    }
                });
            }
        });

        let hit = winner.lock().ok().and_then(|mut slot| slot.take());
        let stats = PassStats {
            regions: ranges.len(),
            images_skipped,
            image_bytes,
            scanned: scanned.load(Ordering::Relaxed),
            candidates: candidates.load(Ordering::Relaxed),
            probed: probed.load(Ordering::Relaxed),
            solved: hit.is_some(),
            elapsed: started.elapsed(),
        };
        (hit, stats)
    }

    /// Open the process and run the passes, cheapest and most specific first:
    /// the printed launch URL's `token=` anchor, then every maximal base64url
    /// run over private memory (mapped images cannot hold a JS string, so that
    /// pass reads roughly a fifth less), then the same over all of memory, and
    /// finally without the tail filter.
    pub(super) fn recover(pid: u32, port: u16) -> Outcome {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if raw.is_null() {
            let error = unsafe { GetLastError() };
            return Outcome::NotFound {
                detail: format!("OpenProcess 失败(Win32 错误 {error}), 目标进程可能属于其他用户或已退出"),
            };
        }
        let handle = ProcessHandle(raw);

        let mut report: Vec<String> = Vec::new();
        for (index, anchor, skip_images, tail_filter) in [
            (0usize, Some(TOKEN_ANCHOR), true, true),
            (1, None, true, true),
            (2, None, false, true),
            (3, None, false, false),
        ] {
            let (hit, stats) = scan(&handle, port, anchor, skip_images, tail_filter);
            let kind = if anchor.is_some() { "anchor" } else { "runs" };
            report.push(format!("pass{}[{kind}] {stats}", index + 1));
            if let Some(token) = hit {
                return Outcome::Found {
                    token,
                    stats: report.join("; "),
                };
            }
        }
        Outcome::NotFound {
            detail: report.join("; "),
        }
    }
}
