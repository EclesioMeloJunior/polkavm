#![no_std]
#![no_main]
#![allow(clippy::missing_safety_doc)]

use core::ptr::addr_of_mut;
use core::sync::atomic::Ordering;
use core::sync::atomic::{AtomicBool, AtomicUsize};

#[rustfmt::skip]
use polkavm_common::{
    abi::VM_ADDR_USER_STACK_HIGH,
    utils::align_to_next_page_usize,
    zygote::{
        AddressTableRaw, VmCtx as VmCtxInner,
        SANDBOX_EMPTY_NATIVE_PROGRAM_COUNTER,
        SANDBOX_EMPTY_NTH_INSTRUCTION,
        VM_ADDR_JUMP_TABLE_RETURN_TO_HOST,
        VM_ADDR_JUMP_TABLE,
        VM_ADDR_NATIVE_CODE,
        VM_ADDR_SIGSTACK,
        VM_RPC_FLAG_RECONFIGURE,
        VM_SANDBOX_MAXIMUM_JUMP_TABLE_SIZE,
        VM_SANDBOX_MAXIMUM_JUMP_TABLE_VIRTUAL_SIZE,
        VM_SANDBOX_MAXIMUM_NATIVE_CODE_SIZE,
        VMCTX_FUTEX_BUSY,
        VMCTX_FUTEX_HOSTCALL,
        VMCTX_FUTEX_IDLE,
        VMCTX_FUTEX_INIT,
        VMCTX_FUTEX_TRAP,
    },
    VM_RPC_FLAG_CLEAR_PROGRAM_AFTER_EXECUTION,
    VM_RPC_FLAG_RESET_MEMORY_AFTER_EXECUTION,
    VM_RPC_FLAG_RESET_MEMORY_BEFORE_EXECUTION,
};
use polkavm_linux_raw as linux_raw;

#[cfg(debug_assertions)]
#[no_mangle]
extern "C" fn rust_eh_personality() {
    abort_with_message("rust_eh_personality called");
}

fn write_number_base10(value: u64, write_str: &mut dyn FnMut(&str)) {
    let n = if value >= 10 {
        write_number_base10(value / 10, write_str);
        value % 10
    } else {
        value
    };

    let s = [n as u8 + b'0'];
    let s = unsafe { core::str::from_utf8_unchecked(&s) };
    write_str(s);
}

fn write_number_base16(value: u64, write_str: &mut dyn FnMut(&str)) {
    let n = if value >= 16 {
        write_number_base16(value / 16, write_str);
        value % 16
    } else {
        value
    };

    let s = [if n < 10 { n as u8 + b'0' } else { (n - 10) as u8 + b'a' }];
    let s = unsafe { core::str::from_utf8_unchecked(&s) };
    write_str(s);
}

trait DisplayLite {
    fn fmt_lite(&self, write_str: impl FnMut(&str));
}

impl DisplayLite for &str {
    fn fmt_lite(&self, mut write_str: impl FnMut(&str)) {
        write_str(self)
    }
}

impl DisplayLite for usize {
    fn fmt_lite(&self, write_str: impl FnMut(&str)) {
        (*self as u64).fmt_lite(write_str)
    }
}

impl DisplayLite for u32 {
    fn fmt_lite(&self, write_str: impl FnMut(&str)) {
        u64::from(*self).fmt_lite(write_str)
    }
}

impl DisplayLite for u64 {
    fn fmt_lite(&self, mut write_str: impl FnMut(&str)) {
        write_number_base10(*self, &mut write_str)
    }
}

struct Hex<T>(T);

impl DisplayLite for Hex<usize> {
    fn fmt_lite(&self, write_str: impl FnMut(&str)) {
        Hex(self.0 as u64).fmt_lite(write_str)
    }
}

impl DisplayLite for Hex<u32> {
    fn fmt_lite(&self, write_str: impl FnMut(&str)) {
        Hex(u64::from(self.0)).fmt_lite(write_str)
    }
}

impl DisplayLite for Hex<u64> {
    fn fmt_lite(&self, mut write_str: impl FnMut(&str)) {
        write_str("0x");
        write_number_base16(self.0, &mut write_str)
    }
}

macro_rules! trace {
    ($arg:expr) => {{
        let fd = linux_raw::FdRef::from_raw_unchecked(linux_raw::STDERR_FILENO);
        let _ = linux_raw::sys_write(fd, $arg.as_bytes());
        let _ = linux_raw::sys_write(fd, b"\n");
    }};

    ($($arg:expr),+) => {{
        let fd = linux_raw::FdRef::from_raw_unchecked(linux_raw::STDERR_FILENO);
        $(
            DisplayLite::fmt_lite(&$arg, |s| {
                let _ = linux_raw::sys_write(fd, s.as_bytes());
            });
        )+
        let _ = linux_raw::sys_write(fd, b"\n");
    }};
}

#[repr(transparent)]
pub struct VmCtx(VmCtxInner);

unsafe impl Sync for VmCtx {}

impl core::ops::Deref for VmCtx {
    type Target = VmCtxInner;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[no_mangle]
#[link_section = ".vmctx"]
#[used]
// Use the `zeroed` constructor to make sure this doesn't take up any space in the executable.
pub static VMCTX: VmCtx = VmCtx(VmCtxInner::zeroed());

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    abort_with_message("panic triggered in zygote");
}

#[no_mangle]
unsafe fn memcpy(dst: *mut u8, src: *const u8, size: usize) -> *mut u8 {
    core::arch::asm!(
        "rep movsb", inout("rdi") dst => _, inout("rsi") src => _, inout("rcx") size => _,
        options(preserves_flags, nostack)
    );

    dst
}

fn reset_message() {
    unsafe {
        *VMCTX.message_length.get() = 0;
    }
}

#[inline]
fn append_to_message<'a, 'b>(mut input: &[u8])
where
    'a: 'b,
{
    let message_length = unsafe { &mut *VMCTX.message_length.get() };
    let message_buffer = &mut unsafe { &mut *VMCTX.message_buffer.get() }[..];

    while !input.is_empty() && (*message_length as usize) < message_buffer.len() {
        message_buffer[*message_length as usize] = input[0];
        *message_length += 1;
        input = &input[1..];
    }
}

fn graceful_abort() -> ! {
    let in_signal_handler = IN_SIGNAL_HANDLER.swap(true, Ordering::Relaxed);
    let errcode = if in_signal_handler { 101 } else { 100 };
    let _ = linux_raw::sys_exit(errcode);
    linux_raw::abort();
}

#[cold]
fn abort_with_message(error: &str) -> ! {
    let fd = linux_raw::FdRef::from_raw_unchecked(linux_raw::STDERR_FILENO);
    let _ = linux_raw::sys_write(fd, b"fatal error: ");
    let _ = linux_raw::sys_write(fd, error.as_bytes());
    let _ = linux_raw::sys_write(fd, b"\n");

    reset_message();
    append_to_message(error.as_bytes());

    core::sync::atomic::fence(Ordering::Release);
    graceful_abort();
}

#[cold]
fn abort_with_error(error: &str, err_obj: linux_raw::Error) -> ! {
    let fd = linux_raw::FdRef::from_raw_unchecked(linux_raw::STDERR_FILENO);
    let _ = linux_raw::sys_write(fd, b"fatal error: ");
    let _ = linux_raw::sys_write(fd, error.as_bytes());
    let _ = linux_raw::sys_write(fd, b": ");
    reset_message();
    append_to_message(error.as_bytes());
    append_to_message(b": ");
    err_obj.fmt_to_string(move |s| {
        let _ = linux_raw::sys_write(fd, s.as_bytes());
        append_to_message(s.as_bytes());
    });
    let _ = linux_raw::sys_write(fd, b"\n");

    core::sync::atomic::fence(Ordering::Release);
    graceful_abort();
}

const HOST_SOCKET_FILENO: linux_raw::c_int = linux_raw::STDIN_FILENO;

unsafe extern "C" fn entry_point(stack: *mut usize) -> ! {
    trace!("initializing...");
    let socket = initialize(stack);
    main_loop(socket);
}

static IN_SIGNAL_HANDLER: AtomicBool = AtomicBool::new(false);
static NATIVE_PAGE_SIZE: AtomicUsize = AtomicUsize::new(!0);
static IS_PROGRAM_DIRTY: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn signal_handler(signal: u32, _info: &linux_raw::siginfo_t, context: &linux_raw::ucontext) {
    if IN_SIGNAL_HANDLER.load(Ordering::Relaxed) || signal == linux_raw::SIGIO {
        graceful_abort();
    }

    IN_SIGNAL_HANDLER.store(true, Ordering::Relaxed);

    let rip = context.uc_mcontext.rip;
    *VMCTX.rip().get() = rip;

    trace!(
        "signal triggered from ",
        Hex(rip),
        " (signal = ",
        signal,
        ")",
        ", rax = ",
        Hex(context.uc_mcontext.rax),
        ", rcx = ",
        Hex(context.uc_mcontext.rcx),
        ", rdx = ",
        Hex(context.uc_mcontext.rdx),
        ", rbx = ",
        Hex(context.uc_mcontext.rbx),
        ", rsp = ",
        Hex(context.uc_mcontext.rsp),
        ", rbp = ",
        Hex(context.uc_mcontext.rbp),
        ", rsi = ",
        Hex(context.uc_mcontext.rsi),
        ", rdi = ",
        Hex(context.uc_mcontext.rdi),
        ", r8 = ",
        Hex(context.uc_mcontext.r8),
        ", r9 = ",
        Hex(context.uc_mcontext.r9),
        ", r10 = ",
        Hex(context.uc_mcontext.r10),
        ", r11 = ",
        Hex(context.uc_mcontext.r11),
        ", r12 = ",
        Hex(context.uc_mcontext.r12),
        ", r13 = ",
        Hex(context.uc_mcontext.r13),
        ", r14 = ",
        Hex(context.uc_mcontext.r14),
        ", r15 = ",
        Hex(context.uc_mcontext.r15)
    );

    let user_code = VM_ADDR_NATIVE_CODE;

    #[allow(clippy::needless_borrow)]
    if rip >= user_code && rip < user_code + (&*VMCTX.memory_config.get()).code_size as u64 {
        signal_host(VMCTX_FUTEX_TRAP, SignalHostKind::Normal)
            .unwrap_or_else(|error| abort_with_error("failed to wait for the host process (trap)", error));

        *VMCTX.rip().get() = SANDBOX_EMPTY_NATIVE_PROGRAM_COUNTER;
        longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1);
    } else {
        abort_with_message("segmentation fault")
    }
}

#[repr(C)]
struct JmpBuf {
    return_address: u64,
    rbx: u64,
    rsp: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    return_value: u64,
}

static mut RESUME_IDLE_LOOP_JMPBUF: JmpBuf = JmpBuf {
    return_address: 0,
    rbx: 0,
    rsp: 0,
    rbp: 0,
    r12: 0,
    r13: 0,
    r14: 0,
    r15: 0,
    return_value: 0,
};

extern "C" {
    fn zygote_longjmp(jmpbuf: *mut JmpBuf, return_value: u64) -> !;
    fn zygote_setjmp(jmpbuf: *mut JmpBuf) -> u64;
    fn zygote_signal_restorer();
}

use zygote_longjmp as longjmp;
use zygote_setjmp as setjmp;
use zygote_signal_restorer as signal_restorer;

core::arch::global_asm!(
    include_str!(concat!(env!("OUT_DIR"), "/global_asm.s")),
    entry_point = sym entry_point,
);

#[inline(never)]
unsafe fn initialize(mut stack: *mut usize) -> linux_raw::Fd {
    /*
        The initial stack contains the following:
            argc: usize,
            argv: [*const u8; argc],
            _: *const c_void, // NULL
            envp: [*const u8; _],
            _: *const c_void, // NULL
            auxv: [(usize, usize); _],
            _: (usize, usize), // (AT_NULL, _)
    */
    let argc = *stack;
    stack = stack.add(1);
    let argv = stack.cast::<*mut u8>();
    let envp = argv.add(argc + 1);
    let auxv = {
        let mut p = envp;
        while !(*p).is_null() {
            p = p.add(1);
        }
        p.add(1).cast::<(usize, usize)>()
    };

    let minsigstksz;
    let mut fsgsbase_supported = false;
    let page_size = {
        let mut page_size_opt = None;
        let mut minsigstksz_opt = None;
        let mut auxv = auxv;
        loop {
            let (kind, value) = *auxv;
            if kind == linux_raw::AT_NULL as usize {
                break;
            }

            if kind == linux_raw::AT_PAGESZ as usize {
                page_size_opt = Some(value);
            }

            if kind == linux_raw::AT_MINSIGSTKSZ as usize {
                minsigstksz_opt = Some(value);
            }

            if kind == linux_raw::AT_HWCAP2 as usize && value & linux_raw::HWCAP2_FSGSBASE != 0 {
                fsgsbase_supported = true;
            }

            auxv = auxv.add(1);
        }

        if let Some(size) = minsigstksz_opt {
            trace!("signal stack size: ", size);
        }

        let base_sigstack_size = core::cmp::max(linux_raw::MINSIGSTKSZ as usize, 4 * 4096);
        minsigstksz = core::cmp::max(base_sigstack_size, minsigstksz_opt.unwrap_or(base_sigstack_size));

        if let Some(page_size) = page_size_opt {
            NATIVE_PAGE_SIZE.store(page_size, Ordering::Relaxed);
            page_size
        } else {
            abort_with_message("AT_PAGESZ not found in auxv");
        }
    };

    let socket = linux_raw::Fd::from_raw_unchecked(HOST_SOCKET_FILENO);
    let vmctx_memfd = linux_raw::recvfd(socket.borrow()).unwrap_or_else(|error| abort_with_error("failed to read vmctx fd", error));

    linux_raw::sys_mmap(
        &VMCTX as *const VmCtx as *mut core::ffi::c_void,
        page_size,
        linux_raw::PROT_READ | linux_raw::PROT_WRITE,
        linux_raw::MAP_FIXED | linux_raw::MAP_SHARED,
        Some(vmctx_memfd.borrow()),
        0,
    )
    .unwrap_or_else(|error| abort_with_error("failed to mmap vmctx", error));

    vmctx_memfd
        .close()
        .unwrap_or_else(|error| abort_with_error("failed to close vmctx memfd", error));

    let lifetime_pipe = linux_raw::recvfd(socket.borrow()).unwrap_or_else(|error| abort_with_error("failed to read lifetime pipe", error));

    // Make sure we're killed when the parent process exits.
    let pid = linux_raw::sys_getpid().unwrap_or_else(|error| abort_with_error("failed to get process PID", error)) as u32;
    linux_raw::sys_fcntl(lifetime_pipe.borrow(), linux_raw::F_SETOWN, pid)
        .unwrap_or_else(|error| abort_with_error("failed to fcntl(F_SETOWN) on the lifetime pipe", error));

    linux_raw::sys_fcntl(
        lifetime_pipe.borrow(),
        linux_raw::F_SETFL,
        linux_raw::O_NONBLOCK | linux_raw::O_ASYNC,
    )
    .unwrap_or_else(|error| abort_with_error("failed to fcntl(F_SETFL) on the lifetime pipe", error));

    lifetime_pipe.leak();

    // Wait for the host to fill out vmctx.
    signal_host(VMCTX_FUTEX_INIT, SignalHostKind::Normal)
        .unwrap_or_else(|error| abort_with_error("failed to wait for the host process (init)", error));

    // Unmap the original stack.
    linux_raw::sys_munmap(
        VMCTX.init.stack_address.load(Ordering::Relaxed) as *mut core::ffi::c_void,
        VMCTX.init.stack_length.load(Ordering::Relaxed) as usize,
    )
    .unwrap_or_else(|error| abort_with_error("failed to unmap kernel-provided stack", error));

    // We don't need the VDSO, so just unmap it.
    if VMCTX.init.vdso_length.load(Ordering::Relaxed) != 0 {
        linux_raw::sys_munmap(
            VMCTX.init.vdso_address.load(Ordering::Relaxed) as *mut core::ffi::c_void,
            VMCTX.init.vdso_length.load(Ordering::Relaxed) as usize,
        )
        .unwrap_or_else(|error| abort_with_error("failed to unmap [vdso]", error));
    }

    if VMCTX.init.vvar_length.load(Ordering::Relaxed) != 0 {
        linux_raw::sys_munmap(
            VMCTX.init.vvar_address.load(Ordering::Relaxed) as *mut core::ffi::c_void,
            VMCTX.init.vvar_length.load(Ordering::Relaxed) as usize,
        )
        .unwrap_or_else(|error| abort_with_error("failed to unmap [vvar]", error));
    }

    // These are technically unnecessary, but let's do it anyway as a just-in-case
    // failsafe in case there's actually something in memory over there.
    linux_raw::sys_munmap(core::ptr::null_mut(), 0x200000000)
        .unwrap_or_else(|error| abort_with_error("failed to make sure the address space is unmapped", error));

    linux_raw::sys_munmap(
        VM_ADDR_JUMP_TABLE as *mut core::ffi::c_void,
        align_to_next_page_usize(page_size, VM_SANDBOX_MAXIMUM_JUMP_TABLE_VIRTUAL_SIZE as usize)
            .unwrap_or_else(|| abort_with_message("overflow")),
    )
    .unwrap_or_else(|error| abort_with_error("failed to make sure the jump table address space is unmapped", error));

    // Set up our signal handler.
    let minsigstksz = align_to_next_page_usize(page_size, minsigstksz).unwrap_or_else(|| abort_with_message("overflow"));

    linux_raw::sys_mmap(
        VM_ADDR_SIGSTACK as *mut core::ffi::c_void,
        minsigstksz,
        linux_raw::PROT_READ | linux_raw::PROT_WRITE,
        linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
        None,
        0,
    )
    .unwrap_or_else(|error| abort_with_error("failed to mmap signal stack", error));

    linux_raw::sys_sigaltstack(
        &linux_raw::stack_t {
            ss_sp: VM_ADDR_SIGSTACK as *mut core::ffi::c_void,
            ss_flags: 0,
            ss_size: minsigstksz,
        },
        None,
    )
    .unwrap_or_else(|error| abort_with_error("failed to set signal stack", error));

    let mut sa: linux_raw::kernel_sigaction = core::mem::zeroed();
    // TODO: Fill in `sa_mask`?
    sa.sa_handler = Some(core::mem::transmute(signal_handler as usize));
    sa.sa_flags |=
        linux_raw::SA_RESTORER as u64 | linux_raw::SA_SIGINFO as u64 | linux_raw::SA_NODEFER as u64 | linux_raw::SA_ONSTACK as u64;
    sa.sa_restorer = Some(signal_restorer);

    linux_raw::sys_rt_sigprocmask(linux_raw::SIG_SETMASK, &0, None)
        .unwrap_or_else(|error| abort_with_error("failed to set sigprocmask", error));

    linux_raw::sys_rt_sigaction(linux_raw::SIGSEGV, &sa, None)
        .unwrap_or_else(|error| abort_with_error("failed to set up a signal handler for SIGSEGV", error));

    linux_raw::sys_rt_sigaction(linux_raw::SIGILL, &sa, None)
        .unwrap_or_else(|error| abort_with_error("failed to set up a signal handler for SIGILL", error));

    linux_raw::sys_rt_sigaction(linux_raw::SIGFPE, &sa, None)
        .unwrap_or_else(|error| abort_with_error("failed to set up a signal handler for SIGFPE", error));

    linux_raw::sys_rt_sigaction(linux_raw::SIGIO, &sa, None)
        .unwrap_or_else(|error| abort_with_error("failed to set up a signal handler for SIGIO", error));

    // Set up the sysreturn jump table.
    linux_raw::sys_mmap(
        VM_ADDR_JUMP_TABLE_RETURN_TO_HOST as *mut core::ffi::c_void,
        page_size,
        linux_raw::PROT_READ | linux_raw::PROT_WRITE,
        linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
        None,
        0,
    )
    .unwrap_or_else(|error| abort_with_error("failed to map the sysreturn jump table", error));

    if fsgsbase_supported {
        trace!("fsgsbase is supported");
        unsafe {
            core::arch::asm!(
                "wrgsbase {addr}",
                addr = in(reg) VM_ADDR_JUMP_TABLE
            );
        }
    } else {
        trace!("fsgsbase is NOT supported; falling back to arch_prctl");
        linux_raw::sys_arch_prctl_set_gs(VM_ADDR_JUMP_TABLE as usize)
            .unwrap_or_else(|error| abort_with_error("failed to set the %gs register", error));
    }

    // Change the name of the process.
    linux_raw::sys_prctl_set_name(b"polkavm-sandbox\0").unwrap_or_else(|error| abort_with_error("failed to set the process name", error));

    // Unmount the filesystem.
    //
    // Previously we did this before `execveat`ing into the zygote but for some
    // ungodly unexplicable reason on *some* Linux distributions (but not all of them!)
    // the `pivot_root` makes the `execveat` fail with an ENOENT error, even if we
    // physically copy the zygote binary into the newly created filesystem and open
    // it immediately before `execveat`ing with an `open`, and even if we also have
    // /proc mounted in the new namespace.
    linux_raw::sys_pivot_root(linux_raw::cstr!("."), linux_raw::cstr!("."))
        .unwrap_or_else(|error| abort_with_error("failed to sandbox the filesystem", error));
    linux_raw::sys_umount2(linux_raw::cstr!("."), linux_raw::MNT_DETACH)
        .unwrap_or_else(|error| abort_with_error("failed to sandbox the filesystem", error));

    linux_raw::sys_prctl_set_securebits(
        // Make UID == 0 have no special privileges.
        linux_raw::SECBIT_NOROOT |
        linux_raw::SECBIT_NOROOT_LOCKED |
        // Calling 'setuid' from/to UID == 0 doesn't change any privileges.
        linux_raw::SECBIT_NO_SETUID_FIXUP |
        linux_raw::SECBIT_NO_SETUID_FIXUP_LOCKED |
        // The process cannot add capabilities to its ambient set.
        linux_raw::SECBIT_NO_CAP_AMBIENT_RAISE |
        linux_raw::SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED,
    )
    .unwrap_or_else(|error| abort_with_error("failed to sandbox the zygote", error));

    // Finally, drop all capabilities.
    linux_raw::sys_capset_drop_all().unwrap_or_else(|error| abort_with_error("failed to sandbox the zygote", error));

    const SECCOMP_FILTER: &[linux_raw::sock_filter] = &linux_raw::bpf! {
        (a = syscall_nr),
        (if a == linux_raw::SYS_futex => jump @1),
        (if a == linux_raw::SYS_mmap => jump @5),
        (if a == linux_raw::SYS_munmap => jump @1),
        (if a == linux_raw::SYS_madvise => jump @4),
        (if a == linux_raw::SYS_close => jump @1),
        (if a == linux_raw::SYS_write => jump @3),
        (if a == linux_raw::SYS_recvmsg => jump @2),
        (if a == linux_raw::SYS_rt_sigreturn => jump @1),
        (if a == linux_raw::SYS_sched_yield => jump @1),
        (if a == linux_raw::SYS_exit => jump @1),
        (seccomp_kill_thread),

        // SYS_recvmsg
        ([2]: a = syscall_arg[0]),
        (if a != HOST_SOCKET_FILENO => jump @0),
        (seccomp_allow),

        // SYS_write
        ([3]: a = syscall_arg[0]),
        (if a != linux_raw::STDERR_FILENO => jump @0),
        (seccomp_allow),

        // SYS_madvise
        ([4]: a = syscall_arg[2]),
        (if a != linux_raw::MADV_DONTNEED => jump @0),
        (seccomp_allow),

        // SYS_mmap
        ([5]: a = syscall_arg[2]),
        (a &= linux_raw::PROT_EXEC),
        (if a != 0 => jump @6),
        (seccomp_allow),

        // SYS_mmap + PROT_EXEC
        ([6]: a = syscall_arg[2]),
        (if a != linux_raw::PROT_EXEC => jump @0),
        (seccomp_allow),

        ([0]: seccomp_kill_thread),
        ([1]: seccomp_allow),
    };

    linux_raw::sys_seccomp_set_mode_filter(SECCOMP_FILTER).unwrap_or_else(|error| abort_with_error("failed to set seccomp filter", error));

    VMCTX.futex.store(VMCTX_FUTEX_IDLE, Ordering::Release);
    linux_raw::sys_futex_wake_one(&VMCTX.futex)
        .unwrap_or_else(|error| abort_with_error("failed to wake up the host process on initialization", error));

    socket
}

#[link_section = ".text_hot"]
#[inline(never)]
unsafe fn main_loop(socket: linux_raw::Fd) -> ! {
    if setjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF)) != 0 {
        IN_SIGNAL_HANDLER.store(false, Ordering::Relaxed);

        trace!("returning to idle...");

        let rpc_flags = *VMCTX.rpc_flags.get();
        if rpc_flags & VM_RPC_FLAG_CLEAR_PROGRAM_AFTER_EXECUTION != 0 {
            clear_program();
        } else if rpc_flags & VM_RPC_FLAG_RESET_MEMORY_AFTER_EXECUTION != 0 {
            reset_memory();
        }

        VMCTX.futex.store(VMCTX_FUTEX_IDLE, Ordering::Release);
        linux_raw::sys_futex_wake_one(&VMCTX.futex).unwrap_or_else(|error| abort_with_error("failed to wake up the host process", error));
    }

    'wait_loop: while VMCTX.futex.load(Ordering::Relaxed) == VMCTX_FUTEX_IDLE {
        // Use a `black_box` to prevent loop unrolling.
        for _ in 0..core::hint::black_box(20) {
            let _ = linux_raw::sys_sched_yield();
            if VMCTX.futex.load(Ordering::Relaxed) != VMCTX_FUTEX_IDLE {
                break 'wait_loop;
            }
        }

        match linux_raw::sys_futex_wait(&VMCTX.futex, VMCTX_FUTEX_IDLE, None) {
            Ok(()) => continue,
            Err(error) if error.errno() == linux_raw::EAGAIN || error.errno() == linux_raw::EINTR => continue,
            Err(error) => {
                abort_with_error("failed to wait for the host process", error);
            }
        }
    }

    core::sync::atomic::fence(Ordering::Acquire);
    trace!("work received...");

    let rpc_flags = *VMCTX.rpc_flags.get();
    let rpc_address = *VMCTX.rpc_address.get().cast::<Option<extern "C" fn() -> !>>();
    let rpc_sbrk = *VMCTX.rpc_sbrk.get();

    if rpc_flags & VM_RPC_FLAG_RECONFIGURE != 0 {
        load_program(socket.borrow());
    } else if rpc_flags & VM_RPC_FLAG_RESET_MEMORY_BEFORE_EXECUTION != 0 {
        reset_memory();
    }

    if rpc_sbrk > 0 {
        let new_heap_top = *VMCTX.heap_info.heap_top.get() + rpc_sbrk as u64;
        if syscall_sbrk(new_heap_top) == 0 {
            abort_with_message("sbrk failed");
        }
    }

    if let Some(rpc_address) = rpc_address {
        trace!("jumping to: ", Hex(rpc_address as usize));

        VMCTX.is_memory_dirty.store(true, Ordering::Relaxed);
        rpc_address();
    } else {
        longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1);
    }
}

#[link_section = ".text_hot"]
unsafe fn reset_memory() {
    if !VMCTX.is_memory_dirty.load(Ordering::Relaxed) {
        return;
    }

    trace!("resetting memory...");
    let cfg = &mut *VMCTX.memory_config.get();
    let rw_data_size = cfg.memory_map.rw_data_size();
    if rw_data_size > 0 {
        linux_raw::sys_madvise(
            cfg.memory_map.rw_data_address() as *mut core::ffi::c_void,
            rw_data_size as usize,
            linux_raw::MADV_DONTNEED,
        )
        .unwrap_or_else(|error| abort_with_error("failed to clear user read/write memory", error));
    }

    let initial_heap_threshold = u64::from(cfg.memory_map.rw_data_range().end);
    let heap_top = *VMCTX.heap_info.heap_top.get();
    if heap_top > initial_heap_threshold {
        linux_raw::sys_munmap(
            initial_heap_threshold as *mut core::ffi::c_void,
            heap_top as usize - initial_heap_threshold as usize,
        )
        .unwrap_or_else(|error| abort_with_error("failed to unmap the heap", error));
    }

    *VMCTX.heap_info.heap_top.get() = u64::from(cfg.memory_map.heap_base());
    *VMCTX.heap_info.heap_threshold.get() = initial_heap_threshold;

    let stack_size = cfg.memory_map.stack_size() as usize;
    if stack_size > 0 {
        linux_raw::sys_madvise(
            (VM_ADDR_USER_STACK_HIGH as usize - stack_size) as *mut core::ffi::c_void,
            stack_size,
            linux_raw::MADV_DONTNEED,
        )
        .unwrap_or_else(|error| abort_with_error("failed to clear user stack", error));
    }

    VMCTX.is_memory_dirty.store(false, Ordering::Relaxed);
}

#[inline(never)]
#[no_mangle]
pub unsafe extern "C" fn syscall_hostcall(hostcall: u32) {
    trace!("syscall: hostcall triggered");

    *VMCTX.hostcall().get() = hostcall;

    loop {
        signal_host(VMCTX_FUTEX_HOSTCALL, SignalHostKind::Normal)
            .unwrap_or_else(|error| abort_with_error("failed to wait for the host process (hostcall)", error));

        match *VMCTX.hostcall().get() {
            polkavm_common::zygote::HOSTCALL_ABORT_EXECUTION => longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1),
            polkavm_common::zygote::HOSTCALL_SBRK => {
                let new_heap_top = *VMCTX.heap_info.heap_top.get() + *VMCTX.rpc_sbrk.get() as u64;
                *VMCTX.rpc_sbrk.get() = syscall_sbrk(new_heap_top);
                *VMCTX.hostcall().get() = hostcall;
            }
            _ => break,
        }
    }
}

#[inline(never)]
#[no_mangle]
pub unsafe extern "C" fn syscall_trap() -> ! {
    trace!("syscall: trap triggered");
    signal_host(VMCTX_FUTEX_TRAP, SignalHostKind::Normal)
        .unwrap_or_else(|error| abort_with_error("failed to wait for the host process (trap)", error));

    longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1);
}

#[inline(never)]
#[no_mangle]
pub unsafe extern "C" fn syscall_return() -> ! {
    trace!("syscall: return triggered");
    longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1);
}

// Just for debugging. Normally should never be used.
#[inline(never)]
#[no_mangle]
pub unsafe extern "C" fn syscall_trace(nth_instruction: u32, rip: u64) {
    *VMCTX.hostcall().get() = polkavm_common::HOSTCALL_TRACE;
    *VMCTX.nth_instruction().get() = nth_instruction;
    *VMCTX.rip().get() = rip;

    signal_host(VMCTX_FUTEX_HOSTCALL, SignalHostKind::Trace)
        .unwrap_or_else(|error| abort_with_error("failed to wait for the host process (trace)", error));

    *VMCTX.nth_instruction().get() = SANDBOX_EMPTY_NTH_INSTRUCTION;
    *VMCTX.rip().get() = SANDBOX_EMPTY_NATIVE_PROGRAM_COUNTER;

    if *VMCTX.hostcall().get() == polkavm_common::zygote::HOSTCALL_ABORT_EXECUTION {
        longjmp(addr_of_mut!(RESUME_IDLE_LOOP_JMPBUF), 1);
    }
}

#[inline(never)]
#[no_mangle]
pub unsafe extern "C" fn syscall_sbrk(pending_heap_top: u64) -> u32 {
    trace!(
        "syscall: sbrk triggered: ",
        Hex(*VMCTX.heap_info.heap_top.get()),
        " -> ",
        Hex(pending_heap_top),
        " (",
        Hex(pending_heap_top - *VMCTX.heap_info.heap_top.get()),
        ")"
    );

    let memory_map = &(*VMCTX.memory_config.get()).memory_map;
    if pending_heap_top > u64::from(memory_map.heap_base() + memory_map.max_heap_size()) {
        return 0;
    }

    let Some(start) = align_to_next_page_usize(memory_map.page_size() as usize, *VMCTX.heap_info.heap_top.get() as usize) else {
        abort_with_message("unreachable")
    };

    let Some(end) = align_to_next_page_usize(memory_map.page_size() as usize, pending_heap_top as usize) else {
        abort_with_message("unreachable")
    };

    let size = end - start;
    if size > 0 {
        linux_raw::sys_mmap(
            start as *mut core::ffi::c_void,
            end - start,
            linux_raw::PROT_READ | linux_raw::PROT_WRITE,
            linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
            None,
            0,
        )
        .unwrap_or_else(|error| abort_with_error("failed to mmap sbrk increase", error));
    }

    trace!(
        "new rwdata range: ",
        Hex(memory_map.rw_data_address()),
        "-",
        Hex(end),
        " (",
        Hex(end - memory_map.rw_data_address() as usize),
        ")"
    );

    *VMCTX.heap_info.heap_top.get() = pending_heap_top;
    *VMCTX.heap_info.heap_threshold.get() = end as u64;

    pending_heap_top as u32
}

#[link_section = ".address_table"]
#[no_mangle]
pub static ADDRESS_TABLE: AddressTableRaw = AddressTableRaw {
    syscall_hostcall,
    syscall_trap,
    syscall_return,
    syscall_trace,
    syscall_sbrk,
};

enum SignalHostKind {
    Normal,
    Trace,
}

fn signal_host(futex_value_to_set: u32, kind: SignalHostKind) -> Result<(), linux_raw::Error> {
    VMCTX.futex.store(futex_value_to_set, Ordering::Release);
    linux_raw::sys_futex_wake_one(&VMCTX.futex).unwrap_or_else(|error| abort_with_error("failed to wake up the host process", error));

    let spin_target = match kind {
        SignalHostKind::Normal => 64,
        SignalHostKind::Trace => 512,
    };

    'outer: loop {
        unsafe {
            *VMCTX.counters.syscall_wait_loop_start.get() += 1;
        }

        let new_futex_value = VMCTX.futex.load(Ordering::Relaxed);
        if new_futex_value == VMCTX_FUTEX_BUSY {
            break;
        }

        if new_futex_value != futex_value_to_set {
            abort_with_message("unexpected futex value while waiting for the host");
        }

        for _ in 0..spin_target {
            core::hint::spin_loop();
            if VMCTX.futex.load(Ordering::Relaxed) == VMCTX_FUTEX_BUSY {
                break 'outer;
            }
        }

        unsafe {
            *VMCTX.counters.syscall_futex_wait.get() += 1;
        }

        let result = linux_raw::sys_futex_wait(&VMCTX.futex, futex_value_to_set, None);
        match result {
            Ok(()) => {
                continue;
            }
            Err(error) if error.errno() == linux_raw::EAGAIN || error.errno() == linux_raw::EINTR => {
                continue;
            }
            Err(error) => {
                return Err(error);
            }
        }
    }

    core::sync::atomic::fence(Ordering::Acquire);
    Ok(())
}

#[cold]
#[inline(never)]
unsafe fn load_program(socket: linux_raw::FdRef) {
    trace!("reconfiguring...");
    if NATIVE_PAGE_SIZE.load(Ordering::Relaxed) == 0 {
        abort_with_message("assertion failed: native page size is zero");
    }

    let fd = linux_raw::recvfd(socket).unwrap_or_else(|_| abort_with_message("failed to receive program memory fd"));

    clear_program();
    IS_PROGRAM_DIRTY.store(true, Ordering::Relaxed);

    let config = &mut *VMCTX.memory_config.get();
    if config.memory_map.ro_data_size() > 0 {
        if config.ro_data_fd_size > 0 {
            linux_raw::sys_mmap(
                config.memory_map.ro_data_address() as *mut core::ffi::c_void,
                config.ro_data_fd_size as usize,
                linux_raw::PROT_READ,
                linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE,
                Some(fd.borrow()),
                0,
            )
            .unwrap_or_else(|error| abort_with_error("failed to mmap read-only data", error));
        }

        if config.memory_map.ro_data_size() > config.ro_data_fd_size {
            linux_raw::sys_mmap(
                (config.memory_map.ro_data_address() + config.ro_data_fd_size) as *mut core::ffi::c_void,
                (config.memory_map.ro_data_size() - config.ro_data_fd_size) as usize,
                linux_raw::PROT_READ,
                linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
                None,
                0,
            )
            .unwrap_or_else(|error| abort_with_error("failed to mmap read-only data (trailing zeros)", error));
        }

        trace!(
            "new rodata range: ",
            Hex(config.memory_map.ro_data_address()),
            "-",
            Hex(config.memory_map.ro_data_address() + config.memory_map.ro_data_size()),
            " (",
            Hex(config.memory_map.ro_data_size()),
            ")"
        );
    }

    if config.memory_map.rw_data_size() > 0 {
        if config.rw_data_fd_size > 0 {
            linux_raw::sys_mmap(
                config.memory_map.rw_data_address() as *mut core::ffi::c_void,
                config.rw_data_fd_size as usize,
                linux_raw::PROT_READ | linux_raw::PROT_WRITE,
                linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE,
                Some(fd.borrow()),
                u64::from(config.ro_data_fd_size),
            )
            .unwrap_or_else(|error| abort_with_error("failed to mmap read-write data", error));
        }

        if config.memory_map.rw_data_size() > config.rw_data_fd_size {
            linux_raw::sys_mmap(
                (config.memory_map.rw_data_address() + config.rw_data_fd_size) as *mut core::ffi::c_void,
                (config.memory_map.rw_data_size() - config.rw_data_fd_size) as usize,
                linux_raw::PROT_READ | linux_raw::PROT_WRITE,
                linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
                None,
                0,
            )
            .unwrap_or_else(|error| abort_with_error("failed to mmap read-write data (trailing zeros)", error));
        }

        trace!(
            "new rwdata range: ",
            Hex(config.memory_map.rw_data_address()),
            "-",
            Hex(config.memory_map.rw_data_address() + config.memory_map.rw_data_size()),
            " (",
            Hex(config.memory_map.rw_data_size()),
            ")"
        );
    }

    if config.code_size > 0 {
        linux_raw::sys_mmap(
            VM_ADDR_NATIVE_CODE as *mut core::ffi::c_void,
            config.code_size as usize,
            linux_raw::PROT_EXEC,
            linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE,
            Some(fd.borrow()),
            (config.ro_data_fd_size + config.rw_data_fd_size).into(),
        )
        .unwrap_or_else(|error| abort_with_error("failed to mmap code", error));

        trace!(
            "new code range: ",
            Hex(VM_ADDR_NATIVE_CODE),
            "-",
            Hex(VM_ADDR_NATIVE_CODE + config.code_size as u64),
            " (",
            Hex(config.code_size),
            ")"
        );
    }

    if config.jump_table_size > 0 {
        linux_raw::sys_mmap(
            VM_ADDR_JUMP_TABLE as *mut core::ffi::c_void,
            config.jump_table_size as usize,
            linux_raw::PROT_READ,
            linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE,
            Some(fd.borrow()),
            (config.ro_data_fd_size + config.rw_data_fd_size + config.code_size) as linux_raw::c_ulong,
        )
        .unwrap_or_else(|error| abort_with_error("failed to mmap jump table", error));

        trace!(
            "new jump table range: ",
            Hex(VM_ADDR_JUMP_TABLE),
            "-",
            Hex(VM_ADDR_JUMP_TABLE + config.jump_table_size as u64),
            " (",
            Hex(config.jump_table_size),
            ")"
        );
    }

    fd.close()
        .unwrap_or_else(|error| abort_with_error("failed to close program memory fd", error));

    if config.memory_map.stack_size() > 0 {
        linux_raw::sys_mmap(
            config.memory_map.stack_address_low() as *mut core::ffi::c_void,
            config.memory_map.stack_size() as usize,
            linux_raw::PROT_READ | linux_raw::PROT_WRITE,
            linux_raw::MAP_FIXED | linux_raw::MAP_PRIVATE | linux_raw::MAP_ANONYMOUS,
            None,
            0,
        )
        .unwrap_or_else(|error| abort_with_error("failed to mmap stack", error));

        trace!(
            "new stack range: ",
            Hex(config.memory_map.stack_address_low()),
            "-",
            Hex(config.memory_map.stack_address_low() + config.memory_map.stack_size()),
            " (",
            Hex(config.memory_map.stack_size()),
            ")"
        );
    }

    trace!(
        "new sysreturn address: ",
        Hex(config.sysreturn_address),
        " (set at ",
        Hex(VM_ADDR_JUMP_TABLE_RETURN_TO_HOST),
        ")"
    );
    *(VM_ADDR_JUMP_TABLE_RETURN_TO_HOST as *mut u64) = config.sysreturn_address;
    VMCTX.is_memory_dirty.store(false, Ordering::Relaxed);
}

#[inline(never)]
unsafe fn clear_program() {
    if !IS_PROGRAM_DIRTY.load(Ordering::Relaxed) {
        return;
    }

    polkavm_common::static_assert!(VM_ADDR_NATIVE_CODE + (VM_SANDBOX_MAXIMUM_NATIVE_CODE_SIZE as u64) < 0x200000000);

    linux_raw::sys_munmap(core::ptr::null_mut(), 0x200000000)
        .unwrap_or_else(|error| abort_with_error("failed to unmap user accessible memory", error));

    linux_raw::sys_munmap(
        VM_ADDR_JUMP_TABLE as *mut core::ffi::c_void,
        VM_SANDBOX_MAXIMUM_JUMP_TABLE_SIZE as usize,
    )
    .unwrap_or_else(|error| abort_with_error("failed to unmap jump table", error));

    *(VM_ADDR_JUMP_TABLE_RETURN_TO_HOST as *mut u64) = 0;
    VMCTX.is_memory_dirty.store(false, Ordering::Relaxed);
    IS_PROGRAM_DIRTY.store(false, Ordering::Relaxed);
}
