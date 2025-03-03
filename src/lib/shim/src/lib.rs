#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]
// https://github.com/rust-lang/rfcs/blob/master/text/2585-unsafe-block-in-unsafe-fn.md
#![deny(unsafe_op_in_unsafe_fn)]

use core::cell::{Cell, RefCell};
use core::ffi::CStr;
use core::mem::MaybeUninit;

use crate::tls::ShimTlsVar;

use linux_api::signal::{rt_sigprocmask, SigProcMaskAction};
use shadow_shim_helper_rs::ipc::IPCData;
use shadow_shim_helper_rs::shim_event::{ShimEventStartReq, ShimEventToShadow, ShimEventToShim};
use shadow_shim_helper_rs::shim_shmem::{HostShmem, ManagerShmem, ProcessShmem, ThreadShmem};
use shadow_shim_helper_rs::simulation_time::SimulationTime;
use shadow_shim_helper_rs::syscall_types::ForeignPtr;
use shadow_shmem::allocator::{shdeserialize, ShMemBlockAlias, ShMemBlockSerialized};
use tls::ThreadLocalStorage;
use vasi_sync::lazy_lock::LazyLock;
use vasi_sync::scmutex::SelfContainedMutex;

/// cbindgen:ignore
mod bindings {
    #![allow(unused)]
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    // https://github.com/rust-lang/rust/issues/66220
    #![allow(improper_ctypes)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub mod clone;
pub mod mmap_box;
pub mod shimlogger;
pub mod syscall;
pub mod tls;

pub use shimlogger::export as shimlogger_export;

pub mod signals;

pub fn simtime() -> Option<SimulationTime> {
    SimulationTime::from_c_simtime(unsafe { bindings::shim_sys_get_simtime_nanos() })
}

mod tls_allow_native_syscalls {
    use super::*;

    static ALLOW_NATIVE_SYSCALLS: ShimTlsVar<Cell<bool>> =
        ShimTlsVar::new(&SHIM_TLS, || Cell::new(false));

    pub fn get() -> bool {
        ALLOW_NATIVE_SYSCALLS.get().get()
    }

    pub fn swap(new: bool) -> bool {
        ALLOW_NATIVE_SYSCALLS.get().replace(new)
    }
}

// We use a page for a stack guard, and up to another page to page-align the
// stack guard. We assume 4k pages here but detect at runtime if this is too small.
const SHIM_SIGNAL_STACK_GUARD_OVERHEAD: usize = 4096 * 2;

mod tls_thread_signal_stack {
    use super::*;

    static THREAD_SIGNAL_STACK: ShimTlsVar<Cell<*mut core::ffi::c_void>> =
        ShimTlsVar::new(&SHIM_TLS, || Cell::new(core::ptr::null_mut()));

    // Shouldn't need to make this very large, but needs to be big enough to run the
    // managed process's signal handlers as well - possibly recursively.
    //
    // Stack space that's *never* used shouldn't ever become resident, but an
    // occasional deep stack could force the pages to be resident ever after.  To
    // mitigate that, we could consider `madvise(MADV_DONTNEED)` after running
    // signal handlers, to let the OS reclaim the (now-popped) signal handler stack
    // frames.
    const SHIM_SIGNAL_STACK_MIN_USABLE_SIZE: usize = 1024 * 100;
    const SHIM_SIGNAL_STACK_SIZE: usize =
        SHIM_SIGNAL_STACK_GUARD_OVERHEAD + SHIM_SIGNAL_STACK_MIN_USABLE_SIZE;

    /// Allocates and installs a signal stack. This is to ensure that our
    /// signal handlers have enough stack space; otherwise we can run out in managed
    /// processes that use small stacks.
    ///
    /// This should be called once per thread before any signal handlers run.
    /// Panics if already called on the current thread.
    pub fn init() {
        if THREAD_SIGNAL_STACK.get().get().is_null() {
            // Allocate
            let new_stack = unsafe {
                rustix::mm::mmap_anonymous(
                    core::ptr::null_mut(),
                    SHIM_SIGNAL_STACK_SIZE,
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::PRIVATE,
                )
            }
            .unwrap();

            // Save to thread-local, so that we can deallocate on thread exit
            assert!(
                THREAD_SIGNAL_STACK.get().replace(new_stack).is_null(),
                "Allocated signal stack twice for current thread"
            );

            // Set up guard page
            unsafe { rustix::mm::mprotect(new_stack, 4096, rustix::mm::MprotectFlags::empty()) }
                .unwrap();
        } else {
            // We get here after forking.
            //
            // We still have the signal stack allocated in the new process.
            // We still need to install it though, below.
        }

        // Install via `sigaltstack`. The kernel will switch to this stack when
        // invoking one of our signal handlers.
        let stack_descriptor = linux_api::signal::stack_t {
            ss_sp: THREAD_SIGNAL_STACK.get().get(),
            ss_size: SHIM_SIGNAL_STACK_SIZE.try_into().unwrap(),
            // Clear the alternate stack settings on entry to signal handler, and
            // restore it on exit.  Otherwise a signal handler invoked while another
            // is running on the same thread would clobber the first handler's stack.
            // Instead we want the second handler to push a new frame on the alt
            // stack that's already installed.
            ss_flags: linux_api::signal::SigAltStackFlags::SS_AUTODISARM.bits(),
        };
        unsafe {
            linux_api::signal::sigaltstack(Some(&stack_descriptor), None).unwrap();
        }
    }

    /// # Safety
    ///
    /// After calling this function, the current thread must ensure the following
    /// sequence can't happen before exiting:
    ///
    /// * Another thread runs.
    /// * That thread also frees its stack.
    /// * This thread runs again on the signal stack (e.g. by handling a new signal).
    ///
    /// Generally in the shim we rely on Shadow's scheduling model to ensure
    /// this, since we know Shadow won't permit another thread to run
    /// preemptively before the curent thread has a chance to finish exiting.
    pub unsafe fn free() {
        // A signal stack waiting to be freed.
        //
        // We can't free the current thread's signal stack, since we may be running on it.
        // Instead we save the pointer to this global, and free the pointer that was already
        // there, if any.
        //
        // We smuggle the pointer through as a `usize`, since pointers aren't `Sync`.
        static FREE_SIGNAL_STACK: SelfContainedMutex<usize> = SelfContainedMutex::const_new(0);

        let mut free_signal_stack = FREE_SIGNAL_STACK.lock();
        let this_thread_stack = THREAD_SIGNAL_STACK.get().replace(core::ptr::null_mut());
        let stack_to_free_now =
            core::mem::replace(&mut *free_signal_stack, this_thread_stack as usize);
        if stack_to_free_now != 0 {
            unsafe {
                rustix::mm::munmap(
                    stack_to_free_now as *mut core::ffi::c_void,
                    SHIM_SIGNAL_STACK_SIZE,
                )
            }
            .unwrap();
        }
    }
}

/// Per-thread IPC channel between the shim, running in a managed process, and
/// the shadow process.
mod tls_ipc {
    use super::*;
    static IPC_DATA_BLOCK: ShimTlsVar<RefCell<Option<ShMemBlockAlias<IPCData>>>> =
        ShimTlsVar::new(&SHIM_TLS, || RefCell::new(None));

    // Panics if this thread's IPC hasn't been initialized yet.
    pub fn with<O>(f: impl FnOnce(&IPCData) -> O) -> O {
        let ipc = IPC_DATA_BLOCK.get();
        let ipc = ipc.borrow();
        ipc.as_ref().map(|block| f(block)).unwrap()
    }

    /// The previous value, if any, is dropped.
    ///
    /// # Safety
    ///
    /// `blk` must contained a serialized block referencing a `ShMemBlock` of type `IPCData`.
    /// The `ShMemBlock` must outlive the current thread.
    pub unsafe fn set(blk: &ShMemBlockSerialized) {
        let blk: ShMemBlockAlias<IPCData> = unsafe { shdeserialize(blk) };
        IPC_DATA_BLOCK.get().replace(Some(blk));
    }
}

mod tls_thread_shmem {
    use super::*;

    static SHMEM: ShimTlsVar<RefCell<Option<ShMemBlockAlias<ThreadShmem>>>> =
        ShimTlsVar::new(&SHIM_TLS, || RefCell::new(None));

    /// Panics if `set` hasn't been called yet.
    pub fn with<O>(f: impl FnOnce(&ThreadShmem) -> O) -> O {
        f(SHMEM.get().borrow().as_ref().unwrap())
    }

    /// The previous value, if any, is dropped.
    ///
    /// # Safety
    ///
    /// `blk` must contained a serialized block referencing a `ShMemBlock` of
    /// type `ThreadShmem`.  The `ShMemBlock` must outlive the current thread.
    pub unsafe fn set(blk: &ShMemBlockSerialized) {
        // SAFETY: Caller guarantees correct type.
        let blk = unsafe { shdeserialize(blk) };
        SHMEM.get().borrow_mut().replace(blk);
    }
}

mod global_manager_shmem {
    use super::*;

    // This is set explicitly, so needs a Mutex.
    static INITIALIZER: SelfContainedMutex<Option<ShMemBlockSerialized>> =
        SelfContainedMutex::const_new(None);

    // The actual block is in a `LazyLock`, which is much faster to access.
    // It uses `INITIALIZER` to do its one-time init.
    static SHMEM: LazyLock<ShMemBlockAlias<ManagerShmem>> = LazyLock::const_new(|| {
        let serialized = INITIALIZER.lock().take().unwrap();
        unsafe { shdeserialize(&serialized) }
    });

    /// # Safety
    ///
    /// `blk` must contained a serialized block referencing a `ShMemBlock` of type `ManagerShmem`.
    /// The `ShMemBlock` must outlive this process.
    pub unsafe fn set(blk: &ShMemBlockSerialized) {
        assert!(!SHMEM.initd());
        assert!(INITIALIZER.lock().replace(*blk).is_none());
        // Ensure that `try_get` returns true (without it having to take the
        // `INITIALIZER` lock to check), and that we fail early if `SHMEM` can't
        // actually be initialized.
        SHMEM.force();
    }

    /// Panics if `set` hasn't been called yet.
    pub fn get() -> impl core::ops::Deref<Target = ShMemBlockAlias<'static, ManagerShmem>> + 'static
    {
        SHMEM.force()
    }

    pub fn try_get(
    ) -> Option<impl core::ops::Deref<Target = ShMemBlockAlias<'static, ManagerShmem>> + 'static>
    {
        if !SHMEM.initd() {
            // No need to do the more-expensive `INITIALIZER` check; `set`
            // forces `SHMEM` to initialize.
            None
        } else {
            Some(get())
        }
    }
}

mod global_host_shmem {
    use super::*;

    // This is set explicitly, so needs a Mutex.
    static INITIALIZER: SelfContainedMutex<Option<ShMemBlockSerialized>> =
        SelfContainedMutex::const_new(None);

    // The actual block is in a `LazyLock`, which is much faster to access.
    // It uses `INITIALIZER` to do its one-time init.
    static SHMEM: LazyLock<ShMemBlockAlias<HostShmem>> = LazyLock::const_new(|| {
        let serialized = INITIALIZER.lock().take().unwrap();
        unsafe { shdeserialize(&serialized) }
    });

    /// # Safety
    ///
    /// `blk` must contained a serialized block referencing a `ShMemBlock` of type `HostShmem`.
    /// The `ShMemBlock` must outlive this process.
    pub unsafe fn set(blk: &ShMemBlockSerialized) {
        assert!(!SHMEM.initd());
        assert!(INITIALIZER.lock().replace(*blk).is_none());
        // Ensure that `try_get` returns true (without it having to take the
        // `INITIALIZER` lock to check), and that we fail early if `SHMEM` can't
        // actually be initialized.
        SHMEM.force();
    }

    /// Panics if `set` hasn't been called yet.
    pub fn get() -> impl core::ops::Deref<Target = ShMemBlockAlias<'static, HostShmem>> + 'static {
        SHMEM.force()
    }

    pub fn try_get(
    ) -> Option<impl core::ops::Deref<Target = ShMemBlockAlias<'static, HostShmem>> + 'static> {
        if !SHMEM.initd() {
            // No need to do the more-expensive `INITIALIZER` check; `set`
            // forces `SHMEM` to initialize.
            None
        } else {
            Some(get())
        }
    }
}

mod tls_process_shmem {
    use super::*;

    static SHMEM: ShimTlsVar<RefCell<Option<ShMemBlockAlias<ProcessShmem>>>> =
        ShimTlsVar::new(&SHIM_TLS, || RefCell::new(None));

    /// Panics if `set` hasn't been called yet.
    pub fn with<O>(f: impl FnOnce(&ProcessShmem) -> O) -> O {
        f(SHMEM.get().borrow().as_ref().unwrap())
    }

    /// The previous value, if any, is dropped.
    ///
    /// # Safety
    ///
    /// `blk` must contained a serialized block referencing a `ShMemBlock` of
    /// type `ProcessShmem`.  The `ShMemBlock` must outlive the current thread.
    pub unsafe fn set(blk: &ShMemBlockSerialized) {
        // SAFETY: Caller guarantees correct type.
        let blk = unsafe { shdeserialize(blk) };
        SHMEM.get().borrow_mut().replace(blk);
    }
}

// Force cargo to link against crates that aren't (yet) referenced from Rust
// code (but are referenced from this crate's C code).
// https://github.com/rust-lang/cargo/issues/9391
extern crate log_c2rust;
extern crate logger;
extern crate shadow_shim_helper_rs;
extern crate shadow_shmem;
extern crate shadow_tsc;

/// Global instance of thread local storage for use in the shim.
///
/// SAFETY: We ensure that every thread unregisters itself before exiting,
/// via [`release_and_exit_current_thread`].
static SHIM_TLS: ThreadLocalStorage = unsafe { ThreadLocalStorage::new(tls::Mode::Native) };

/// Release this thread's shim thread local storage and exit the thread.
///
/// Should be called by every thread that accesses thread local storage.
///
/// Panics if there are still any live references to this thread's [`ShimTlsVar`]s.
///
/// # Safety
///
/// In the case that this function somehow panics, caller must not
/// access thread local storage again from the current thread, e.g.
/// using `std::panic::catch_unwind` or a custom panic hook.
pub unsafe fn release_and_exit_current_thread(exit_status: i32) -> ! {
    // Block all signals, to ensure a signal handler can't run and attempt to
    // access thread local storage.
    rt_sigprocmask(
        SigProcMaskAction::SIG_BLOCK,
        &linux_api::signal::sigset_t::FULL,
        None,
    )
    .unwrap();

    // SAFETY: No code can access thread local storage in between deregistration
    // and exit, unless `unregister_curren_thread` itself panics.
    unsafe { SHIM_TLS.unregister_current_thread() }

    linux_api::exit::exit_raw(exit_status).unwrap();
    unreachable!()
}

/// Perform once-per-thread initialization for the shim.
///
/// Unlike `init_process` this must only be called once - we do so explicitly
/// when creating a new managed thread.
///
/// Uses C ABI so that we can call from `asm`.
extern "C" fn init_thread() {
    unsafe { bindings::_shim_child_thread_init_preload() };
    log::trace!("Finished shim thread init");
}

/// Ensure once-per-process init for the shim is done.
///
/// Safe and cheap to call repeatedly; e.g. from API entry points.
fn init_process() {
    static STARTED_INIT: LazyLock<()> = LazyLock::const_new(|| ());
    if STARTED_INIT.initd() {
        // Avoid recursion in initialization.
        //
        // TODO: This shouldn't be necessary once we've gotten rid of all
        // calls to libc from the shim's initialization.
        return;
    }
    STARTED_INIT.force();

    unsafe { bindings::_shim_parent_init_preload() };
    log::trace!("Finished shim global init");
}

/// Wait for "start" event from Shadow, use it to initialize the thread shared
/// memory block, and optionally to initialize the process shared memory block.
fn wait_for_start_event() {
    log::trace!("waiting for start event");

    let mut thread_blk_serialized = MaybeUninit::<ShMemBlockSerialized>::uninit();
    let mut process_blk_serialized = MaybeUninit::<ShMemBlockSerialized>::uninit();
    let start_req = ShimEventToShadow::StartReq(ShimEventStartReq {
        thread_shmem_block_to_init: ForeignPtr::from_raw_ptr(thread_blk_serialized.as_mut_ptr()),
        process_shmem_block_to_init: ForeignPtr::from_raw_ptr(process_blk_serialized.as_mut_ptr()),
    });
    let res = tls_ipc::with(|ipc| {
        ipc.to_shadow().send(start_req);
        ipc.from_shadow().receive().unwrap()
    });
    assert!(matches!(res, ShimEventToShim::StartRes));

    // SAFETY: shadow should have initialized
    let thread_blk_serialized = unsafe { thread_blk_serialized.assume_init() };
    // SAFETY: blk should be of the correct type and outlive this thread.
    unsafe { tls_thread_shmem::set(&thread_blk_serialized) };

    // SAFETY: shadow should have initialized
    let process_blk_serialized = unsafe { process_blk_serialized.assume_init() };
    // SAFETY: blk should be of the correct type and outlive this process.
    unsafe { tls_process_shmem::set(&process_blk_serialized) };
}

// Rust's linking of a `cdylib` only considers Rust `pub extern "C"` entry
// points, and the symbols those recursively used, to be used. i.e. any function
// called from outside of the shim needs to be exported from the Rust code. We
// wrap some C implementations here.
pub mod export {
    use core::ops::Deref;

    use super::*;

    /// # Safety
    ///
    /// The syscall itself must be safe to make.
    #[no_mangle]
    pub unsafe extern "C" fn shim_api_syscall(
        n: core::ffi::c_long,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
        arg6: u64,
    ) -> i64 {
        unsafe { bindings::shimc_api_syscall(n, arg1, arg2, arg3, arg4, arg5, arg6) }
    }

    /// # Safety
    ///
    /// Pointers must be dereferenceable.
    #[no_mangle]
    pub unsafe extern "C" fn shim_api_getaddrinfo(
        node: *const core::ffi::c_char,
        service: *const core::ffi::c_char,
        hints: *const libc::addrinfo,
        res: *mut *mut libc::addrinfo,
    ) -> i32 {
        unsafe { bindings::shimc_api_getaddrinfo(node, service, hints, res) }
    }

    /// # Safety
    ///
    /// * Pointers must be dereferenceable.
    /// * `res` is invalidated afterwards.
    #[no_mangle]
    pub unsafe extern "C" fn shim_api_freeaddrinfo(res: *mut libc::addrinfo) {
        unsafe { bindings::shimc_api_freeaddrinfo(res) }
    }

    /// # Safety
    ///
    /// Pointers must be dereferenceable
    #[no_mangle]
    pub unsafe extern "C" fn shim_api_getifaddrs(ifap: *mut *mut libc::ifaddrs) -> i32 {
        unsafe { bindings::shimc_api_getifaddrs(ifap) }
    }

    /// # Safety
    ///
    /// * Pointers must be dereferenceable.
    /// * `ifa` is invalidated afterwards.
    #[no_mangle]
    pub unsafe extern "C" fn shim_api_freeifaddrs(ifa: *mut libc::ifaddrs) {
        unsafe { bindings::shimc_api_freeifaddrs(ifa) }
    }

    /// Sets the flag determining whether syscalls are passed through natively, and
    /// returns the old value. Typical usage is to set this to the desired value at
    /// the beginning of an operation, and restore the old value afterwards.
    #[no_mangle]
    pub extern "C" fn shim_swapAllowNativeSyscalls(new: bool) -> bool {
        tls_allow_native_syscalls::swap(new)
    }

    /// Whether syscall interposition is currently enabled.
    #[no_mangle]
    pub extern "C" fn shim_interpositionEnabled() -> bool {
        !tls_allow_native_syscalls::get()
    }

    /// Allocates and installs a signal stack. This is to ensure that our
    /// signal handlers have enough stack space; otherwise we can run out in managed
    /// processes that use small stacks.
    ///
    /// This should be called once per thread before any signal handlers run.
    /// Panics if already called on the current thread.
    #[no_mangle]
    pub extern "C" fn _shim_init_signal_stack() {
        tls_thread_signal_stack::init();
    }

    /// # Safety
    ///
    /// The current thread must exit before:
    /// * Another thread runs.
    /// * That thread also frees its stack.
    /// * This thread runs again on the signal stack (e.g. by handling a new signal).
    ///
    /// Generally in the shim we rely on Shadow's scheduling model to ensure
    /// this, since we know Shadow won't permit another thread to run
    /// preemptively before the curent thread has a chance to finish exiting.
    #[no_mangle]
    pub unsafe extern "C" fn shim_freeSignalStack() {
        unsafe { tls_thread_signal_stack::free() };
    }

    /// # Safety
    ///
    /// Environment variable SHADOW_IPC_BLK must contained a serialized block of
    /// type `IPCData`, which outlives the current thread.
    #[no_mangle]
    pub unsafe extern "C" fn _shim_parent_init_ipc() {
        let envname = CStr::from_bytes_with_nul(b"SHADOW_IPC_BLK\0").unwrap();
        // SAFETY: Kind of not. We should pass this some other way than an environment
        // variable. Ok in practice as long as we do our initialization after libc's early
        // initialization, and nothing else mutates this environment variable.
        // https://github.com/shadow/shadow/issues/2848
        let ipc_blk = unsafe { libc::getenv(envname.as_ptr()) };
        assert!(!ipc_blk.is_null());
        let ipc_blk = unsafe { CStr::from_ptr(ipc_blk) };
        let ipc_blk = core::str::from_utf8(ipc_blk.to_bytes()).unwrap();

        use core::str::FromStr;
        let ipc_blk = ShMemBlockSerialized::from_str(ipc_blk).unwrap();
        // SAFETY: caller is responsible for `set`'s preconditions.
        unsafe { tls_ipc::set(&ipc_blk) };
    }

    /// This thread's IPC channel. Panics if it hasn't been initialized yet.
    ///
    /// # Safety
    ///
    /// The returned pointer must not outlive the current thread.
    #[no_mangle]
    pub unsafe extern "C" fn shim_thisThreadEventIPC() -> *const IPCData {
        tls_ipc::with(|ipc| ipc as *const _)
    }

    /// This thread's IPC channel. Panics if it hasn't been initialized yet.
    ///
    /// # Safety
    ///
    /// The returned pointer must not outlive the current thread.
    #[no_mangle]
    pub unsafe extern "C" fn shim_threadSharedMem(
    ) -> *const shadow_shim_helper_rs::shim_shmem::export::ShimShmemThread {
        tls_thread_shmem::with(|thread| thread as *const _)
    }

    #[no_mangle]
    pub extern "C" fn _shim_load() {
        init_process();
    }

    /// Should be used to exit every thread in the shim.
    ///
    /// # Safety
    ///
    /// In the case that this function somehow panics, caller must not
    /// access thread local storage again from the current thread, e.g.
    /// using `std::panic::catch_unwind` or a custom panic hook.
    #[no_mangle]
    pub unsafe extern "C" fn shim_release_and_exit_current_thread(status: i32) {
        unsafe { release_and_exit_current_thread(status) }
    }

    #[no_mangle]
    pub extern "C" fn shim_managerSharedMem(
    ) -> *const shadow_shim_helper_rs::shim_shmem::export::ShimShmemManager {
        let rv = global_manager_shmem::try_get();
        rv.map(|x| {
            let rv: &shadow_shim_helper_rs::shim_shmem::export::ShimShmemManager = x.deref();
            // We know this pointer will be live for the lifetime of the
            // process, and that we never construct a mutable reference to the
            // underlying data.
            rv as *const _
        })
        .unwrap_or(core::ptr::null())
    }

    #[no_mangle]
    pub extern "C" fn shim_hostSharedMem(
    ) -> *const shadow_shim_helper_rs::shim_shmem::export::ShimShmemHost {
        let rv = global_host_shmem::try_get();
        rv.map(|x| {
            let rv: &shadow_shim_helper_rs::shim_shmem::export::ShimShmemHost = x.deref();
            // We know this pointer will be live for the lifetime of the
            // process, and that we never construct a mutable reference to the
            // underlying data.
            rv as *const _
        })
        .unwrap_or(core::ptr::null())
    }

    #[no_mangle]
    pub extern "C" fn shim_processSharedMem(
    ) -> *const shadow_shim_helper_rs::shim_shmem::export::ShimShmemProcess {
        tls_process_shmem::with(|process| {
            // We know this pointer will be live for the lifetime of the
            // process, and that we never construct a mutable reference to the
            // underlying data.
            process as *const _
        })
    }

    /// Wait for start event from shadow, from a newly spawned thread.
    #[no_mangle]
    pub extern "C" fn _shim_preload_only_child_ipc_wait_for_start_event() {
        wait_for_start_event();
    }

    #[no_mangle]
    pub extern "C" fn _shim_ipc_wait_for_start_event() {
        wait_for_start_event();
    }

    #[no_mangle]
    pub extern "C" fn _shim_parent_init_manager_shm() {
        unsafe { global_manager_shmem::set(&global_host_shmem::get().manager_shmem) }
    }

    #[no_mangle]
    pub extern "C" fn _shim_parent_init_host_shm() {
        tls_process_shmem::with(|process| unsafe { global_host_shmem::set(&process.host_shmem) });
    }
}
