use std::cell::{Cell, RefCell};
use std::ops::{Deref, DerefMut};

use linux_api::errno::Errno;
use nix::unistd::Pid;
use shadow_shim_helper_rs::explicit_drop::ExplicitDrop;
use shadow_shim_helper_rs::rootedcell::rc::RootedRc;
use shadow_shim_helper_rs::rootedcell::refcell::RootedRefCell;
use shadow_shim_helper_rs::shim_shmem::{HostShmemProtected, ThreadShmem};
use shadow_shim_helper_rs::syscall_types::{ForeignPtr, SysCallReg};
use shadow_shim_helper_rs::util::SendPointer;
use shadow_shim_helper_rs::HostId;
use shadow_shmem::allocator::{shmalloc, ShMemBlock};

use super::context::ProcessContext;
use super::descriptor::descriptor_table::DescriptorTable;
use super::host::Host;
use super::managed_thread::{self, ManagedThread};
use super::process::{Process, ProcessId};
use crate::cshadow as c;
use crate::host::syscall_condition::{SysCallConditionRef, SysCallConditionRefMut};
use crate::utility::{syscall, IsSend, ObjectCounter};

/// The thread's state after having been allowed to execute some code.
#[derive(Debug)]
#[must_use]
pub enum ResumeResult {
    /// Blocked on a syscall.
    Blocked,
    /// The thread has exited with the given code.
    ExitedThread(i32),
    /// The process has exited.
    ExitedProcess,
}

/// A virtual Thread in Shadow. Currently a thin wrapper around the C Thread,
/// which this object owns, and frees on Drop.
pub struct Thread {
    id: ThreadId,
    host_id: HostId,
    process_id: ProcessId,
    // If non-NULL, this address should be cleared and futex-awoken on thread exit.
    // See set_tid_address(2).
    tid_address: Cell<ForeignPtr<libc::pid_t>>,
    shim_shared_memory: ShMemBlock<'static, ThreadShmem>,
    syscallhandler: SendPointer<c::SysCallHandler>,
    /// Descriptor table; potentially shared with other threads and processes.
    // TODO: Consider using an Arc instead of RootedRc, particularly if this
    // continues to be the only RootedRc member. Cloning this object currently
    // only done when creating a child process or thread, and if we don't have
    // any RootedRc members we could get rid of the requirement to explicitly
    // drop Thread.
    desc_table: Option<RootedRc<RootedRefCell<DescriptorTable>>>,
    // TODO: convert to SysCallCondition (Rust wrapper for c::SysCallCondition).
    // Non-trivial because SysCallCondition is currently not `Send`.
    cond: Cell<SendPointer<c::SysCallCondition>>,
    /// The native, managed thread
    mthread: RefCell<ManagedThread>,
    _counter: ObjectCounter,
}

impl IsSend for Thread {}

impl Thread {
    /// Minimal wrapper around the native managed thread.
    pub fn mthread(&self) -> impl Deref<Target = ManagedThread> + '_ {
        self.mthread.borrow()
    }

    /// Have the plugin thread natively execute the given syscall.
    fn native_syscall_raw(
        &self,
        ctx: &ProcessContext,
        n: i64,
        args: &[SysCallReg],
    ) -> libc::c_long {
        self.mthread
            .borrow()
            .native_syscall(&ctx.with_thread(self), n, args)
            .into()
    }

    /// Have the plugin thread natively execute the given syscall.
    fn native_syscall(
        &self,
        ctx: &ProcessContext,
        n: i64,
        args: &[SysCallReg],
    ) -> Result<SysCallReg, Errno> {
        syscall::raw_return_value_to_result(self.native_syscall_raw(ctx, n, args))
    }

    pub fn process_id(&self) -> ProcessId {
        self.process_id
    }

    pub fn host_id(&self) -> HostId {
        self.host_id
    }

    pub fn native_pid(&self) -> Pid {
        self.mthread.borrow().native_pid()
    }

    pub fn native_tid(&self) -> Pid {
        self.mthread.borrow().native_tid()
    }

    pub fn csyscallhandler(&self) -> *mut c::SysCallHandler {
        self.syscallhandler.ptr()
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    /// Returns whether the given thread is its thread group (aka process) leader.
    /// Typically this is true for the first thread created in a process.
    pub fn is_leader(&self) -> bool {
        self.id == self.process_id.into()
    }

    pub fn syscall_condition(&self) -> Option<SysCallConditionRef> {
        // We check the for null explicitly here instead of using `as_mut` to
        // construct and match an `Option<&mut c::SysCallCondition>`, since it's
        // difficult to ensure we're not breaking any Rust aliasing rules when
        // constructing a mutable reference.
        let c = self.cond.get().ptr();
        if c.is_null() {
            None
        } else {
            Some(unsafe { SysCallConditionRef::borrow_from_c(c) })
        }
    }

    pub fn syscall_condition_mut(&self) -> Option<SysCallConditionRefMut> {
        // We can't safely use `as_mut` here, since that would construct a mutable reference,
        // and we can't prove no other reference exists.
        let c = self.cond.get().ptr();
        if c.is_null() {
            None
        } else {
            Some(unsafe { SysCallConditionRefMut::borrow_from_c(c) })
        }
    }

    pub fn cleanup_syscall_condition(&self) {
        if let Some(c) = unsafe {
            self.cond
                .replace(SendPointer::new(std::ptr::null_mut()))
                .ptr()
                .as_mut()
        } {
            unsafe { c::syscallcondition_cancel(c) };
            unsafe { c::syscallcondition_unref(c) };
        }
    }

    pub fn descriptor_table(&self) -> &RootedRc<RootedRefCell<DescriptorTable>> {
        self.desc_table.as_ref().unwrap()
    }

    #[track_caller]
    pub fn descriptor_table_borrow<'a>(
        &'a self,
        host: &'a Host,
    ) -> impl Deref<Target = DescriptorTable> + 'a {
        self.desc_table.as_ref().unwrap().borrow(host.root())
    }

    #[track_caller]
    pub fn descriptor_table_borrow_mut<'a>(
        &'a self,
        host: &'a Host,
    ) -> impl Deref<Target = DescriptorTable> + DerefMut + 'a {
        self.desc_table.as_ref().unwrap().borrow_mut(host.root())
    }

    /// Natively execute munmap(2) on the given thread.
    pub fn native_munmap(
        &self,
        ctx: &ProcessContext,
        ptr: ForeignPtr<u8>,
        size: usize,
    ) -> Result<(), Errno> {
        self.native_syscall(ctx, libc::SYS_munmap, &[ptr.into(), size.into()])?;
        Ok(())
    }

    /// Natively execute mmap(2) on the given thread.
    pub fn native_mmap(
        &self,
        ctx: &ProcessContext,
        addr: ForeignPtr<u8>,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<ForeignPtr<u8>, Errno> {
        Ok(self
            .native_syscall(
                ctx,
                libc::SYS_mmap,
                &[
                    SysCallReg::from(addr),
                    SysCallReg::from(len),
                    SysCallReg::from(prot),
                    SysCallReg::from(flags),
                    SysCallReg::from(fd),
                    SysCallReg::from(offset),
                ],
            )?
            .into())
    }

    /// Natively execute mremap(2) on the given thread.
    pub fn native_mremap(
        &self,
        ctx: &ProcessContext,
        old_addr: ForeignPtr<u8>,
        old_len: usize,
        new_len: usize,
        flags: i32,
        new_addr: ForeignPtr<u8>,
    ) -> Result<ForeignPtr<u8>, Errno> {
        Ok(self
            .native_syscall(
                ctx,
                libc::SYS_mremap,
                &[
                    SysCallReg::from(old_addr),
                    SysCallReg::from(old_len),
                    SysCallReg::from(new_len),
                    SysCallReg::from(flags),
                    SysCallReg::from(new_addr),
                ],
            )?
            .into())
    }

    /// Natively execute mmap(2) on the given thread.
    pub fn native_mprotect(
        &self,
        ctx: &ProcessContext,
        addr: ForeignPtr<u8>,
        len: usize,
        prot: i32,
    ) -> Result<(), Errno> {
        self.native_syscall(
            ctx,
            libc::SYS_mprotect,
            &[
                SysCallReg::from(addr),
                SysCallReg::from(len),
                SysCallReg::from(prot),
            ],
        )?;
        Ok(())
    }

    /// Natively execute open(2) on the given thread.
    pub fn native_open(
        &self,
        ctx: &ProcessContext,
        pathname: ForeignPtr<u8>,
        flags: i32,
        mode: i32,
    ) -> Result<i32, Errno> {
        let res = self.native_syscall(
            ctx,
            libc::SYS_open,
            &[
                SysCallReg::from(pathname),
                SysCallReg::from(flags),
                SysCallReg::from(mode),
            ],
        );
        Ok(i32::from(res?))
    }

    /// Natively execute close(2) on the given thread.
    pub fn native_close(&self, ctx: &ProcessContext, fd: i32) -> Result<(), Errno> {
        self.native_syscall(ctx, libc::SYS_close, &[SysCallReg::from(fd)])?;
        Ok(())
    }

    /// Natively execute brk(2) on the given thread.
    pub fn native_brk(
        &self,
        ctx: &ProcessContext,
        addr: ForeignPtr<u8>,
    ) -> Result<ForeignPtr<u8>, Errno> {
        let res = self.native_syscall(ctx, libc::SYS_brk, &[SysCallReg::from(addr)])?;
        Ok(ForeignPtr::from(res))
    }

    /// Allocates some space in the plugin's memory. Use `get_writeable_ptr` to write to it, and
    /// `flush` to ensure that the write is flushed to the plugin's memory.
    pub fn malloc_foreign_ptr(
        &self,
        ctx: &ProcessContext,
        size: usize,
    ) -> Result<ForeignPtr<u8>, Errno> {
        // SAFETY: No pointer specified; can't pass a bad one.
        self.native_mmap(
            ctx,
            ForeignPtr::null(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    }

    /// Frees a pointer previously returned by `malloc_foreign_ptr`
    pub fn free_foreign_ptr(
        &self,
        ctx: &ProcessContext,
        ptr: ForeignPtr<u8>,
        size: usize,
    ) -> Result<(), Errno> {
        self.native_munmap(ctx, ptr, size)?;
        Ok(())
    }

    /// Create a new `Thread`, wrapping `mthread`. Intended for use by
    /// syscall handlers such as `clone`.
    pub fn wrap_mthread(
        host: &Host,
        mthread: ManagedThread,
        desc_table: RootedRc<RootedRefCell<DescriptorTable>>,
        pid: ProcessId,
        tid: ThreadId,
    ) -> Result<Thread, Errno> {
        let child = Self {
            mthread: RefCell::new(mthread),
            syscallhandler: unsafe {
                SendPointer::new(c::syscallhandler_new(host.id(), pid.into(), tid.into()))
            },
            cond: Cell::new(unsafe { SendPointer::new(std::ptr::null_mut()) }),
            id: tid,
            host_id: host.id(),
            process_id: pid,
            tid_address: Cell::new(ForeignPtr::null()),
            shim_shared_memory: shmalloc(ThreadShmem::new(
                &host.shim_shmem_lock_borrow().unwrap(),
                tid.into(),
            )),
            desc_table: Some(desc_table),
            _counter: ObjectCounter::new("Thread"),
        };
        Ok(child)
    }

    /// Shared memory for this thread.
    pub fn shmem(&self) -> &ShMemBlock<ThreadShmem> {
        &self.shim_shared_memory
    }

    pub fn resume(&self, ctx: &ProcessContext) -> ResumeResult {
        // Ensure the condition isn't triggered again, but don't clear it yet.
        // Syscall handler can still access.
        if let Some(c) = unsafe { self.cond.get().ptr().as_mut() } {
            unsafe { c::syscallcondition_cancel(c) };
        }

        let res = self.mthread.borrow().resume(&ctx.with_thread(self));

        // Now we're done with old condition.
        if let Some(c) = unsafe {
            self.cond
                .replace(SendPointer::new(std::ptr::null_mut()))
                .ptr()
                .as_mut()
        } {
            unsafe { c::syscallcondition_unref(c) };
        }

        match res {
            managed_thread::ResumeResult::Blocked(cond) => {
                // Wait on new condition.
                let cond = cond.into_inner();
                self.cond.set(unsafe { SendPointer::new(cond) });
                if let Some(cond) = unsafe { cond.as_mut() } {
                    unsafe { c::syscallcondition_waitNonblock(cond, ctx.host, ctx.process, self) }
                }
                ResumeResult::Blocked
            }
            managed_thread::ResumeResult::ExitedThread(c) => ResumeResult::ExitedThread(c),
            managed_thread::ResumeResult::ExitedProcess => ResumeResult::ExitedProcess,
        }
    }

    pub fn handle_process_exit(&self) {
        self.cleanup_syscall_condition();
        self.mthread.borrow().handle_process_exit();
    }

    pub fn return_code(&self) -> Option<i32> {
        self.mthread.borrow().return_code()
    }

    pub fn is_running(&self) -> bool {
        self.mthread.borrow().is_running()
    }

    pub fn get_tid_address(&self) -> ForeignPtr<libc::pid_t> {
        self.tid_address.get()
    }

    pub fn set_tid_address(&self, ptr: ForeignPtr<libc::pid_t>) {
        self.tid_address.set(ptr)
    }

    pub fn unblocked_signal_pending(
        &self,
        process: &Process,
        host_shmem: &HostShmemProtected,
    ) -> bool {
        debug_assert_eq!(process.id(), self.process_id);

        let thread_shmem_protected = self.shmem().protected.borrow(&host_shmem.root);

        let unblocked_signals = !thread_shmem_protected.blocked_signals;
        let pending_signals = self
            .shmem()
            .protected
            .borrow(&host_shmem.root)
            .pending_signals
            | process
                .shmem()
                .protected
                .borrow(&host_shmem.root)
                .pending_signals;

        !(pending_signals & unblocked_signals).is_empty()
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        if let Some(c) = unsafe { self.cond.get_mut().ptr().as_mut() } {
            unsafe { c::syscallcondition_cancel(c) };
            unsafe { c::syscallcondition_unref(c) };
        }
        unsafe { c::syscallhandler_free(self.syscallhandler.ptr()) };
    }
}

impl ExplicitDrop for Thread {
    type ExplicitDropParam = Host;
    type ExplicitDropResult = ();

    fn explicit_drop(mut self, host: &Host) {
        if let Some(table) = self.desc_table.take() {
            table.explicit_drop_recursive(host.root(), host);
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone, Ord, PartialOrd)]
pub struct ThreadId(u32);

impl TryFrom<libc::pid_t> for ThreadId {
    type Error = <u32 as TryFrom<libc::pid_t>>::Error;

    fn try_from(value: libc::pid_t) -> Result<Self, Self::Error> {
        Ok(Self(u32::try_from(value)?))
    }
}

impl From<ProcessId> for ThreadId {
    fn from(value: ProcessId) -> Self {
        // A process ID is also a valid thread ID
        ThreadId(value.into())
    }
}

impl From<ThreadId> for libc::pid_t {
    fn from(val: ThreadId) -> Self {
        val.0.try_into().unwrap()
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

mod export {
    use shadow_shim_helper_rs::shim_shmem::export::{ShimShmemHostLock, ShimShmemThread};
    use shadow_shim_helper_rs::syscall_types::UntypedForeignPtr;

    use super::*;
    use crate::core::worker::Worker;
    use crate::host::descriptor::socket::inet::InetSocket;
    use crate::host::descriptor::socket::Socket;
    use crate::host::descriptor::{CompatFile, Descriptor, File};
    use crate::host::host::Host;
    use crate::host::process::Process;

    /// Make the requested syscall from within the plugin.
    ///
    /// Does *not* flush or invalidate MemoryManager pointers, such as those
    /// obtained through `process_getReadablePtr` etc.
    ///
    /// Arguments are treated opaquely. e.g. no pointer-marshalling is done.
    ///
    /// The return value is the value returned by the syscall *instruction*.
    /// You can map to a corresponding errno value with syscall_rawReturnValueToErrno.
    //
    // Rust doesn't support declaring a function with varargs (...), but this
    // declaration is ABI compatible with a caller who sees this function declared
    // with arguments `Thread* thread, long n, ...`. We manually generate that declartion
    // in our bindings.
    #[no_mangle]
    unsafe extern "C" fn thread_nativeSyscall(
        thread: *const Thread,
        n: libc::c_long,
        arg1: SysCallReg,
        arg2: SysCallReg,
        arg3: SysCallReg,
        arg4: SysCallReg,
        arg5: SysCallReg,
        arg6: SysCallReg,
    ) -> libc::c_long {
        let thread = unsafe { thread.as_ref().unwrap() };
        Worker::with_active_host(|host| {
            Worker::with_active_process(|process| {
                thread.native_syscall_raw(
                    &ProcessContext::new(host, process),
                    n,
                    &[arg1, arg2, arg3, arg4, arg5, arg6],
                )
            })
            .unwrap()
        })
        .unwrap()
    }

    #[no_mangle]
    pub unsafe extern "C" fn thread_getID(thread: *const Thread) -> libc::pid_t {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.id().into()
    }

    /// Sets the `clear_child_tid` attribute as for `set_tid_address(2)`. The thread
    /// will perform a futex-wake operation on the given address on termination.
    #[no_mangle]
    pub unsafe extern "C" fn thread_setTidAddress(thread: *const Thread, addr: UntypedForeignPtr) {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.set_tid_address(addr.cast::<libc::pid_t>());
    }

    /// Gets the `clear_child_tid` attribute, as set by `thread_setTidAddress`.
    #[no_mangle]
    pub unsafe extern "C" fn thread_getTidAddress(thread: *const Thread) -> UntypedForeignPtr {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.get_tid_address().cast::<()>()
    }

    /// Returns a typed pointer to memory shared with the shim (which is backed by
    /// the block returned by thread_getShMBlock).
    #[no_mangle]
    pub unsafe extern "C" fn thread_sharedMem(thread: *const Thread) -> *const ShimShmemThread {
        let thread = unsafe { thread.as_ref().unwrap() };
        &*thread.shim_shared_memory
    }

    #[no_mangle]
    pub unsafe extern "C" fn thread_getProcess(thread: *const Thread) -> *const Process {
        let thread = unsafe { thread.as_ref().unwrap() };
        Worker::with_active_host(|host| {
            let process = host.process_borrow(thread.process_id).unwrap();
            let p: &Process = &process.borrow(host.root());
            p as *const _
        })
        .unwrap()
    }

    #[no_mangle]
    pub unsafe extern "C" fn thread_getHost(thread: *const Thread) -> *const Host {
        let thread = unsafe { thread.as_ref().unwrap() };
        Worker::with_active_host(|host| {
            assert_eq!(host.id(), thread.host_id());
            host as *const _
        })
        .unwrap()
    }

    /// Get the syscallhandler for this thread.
    #[no_mangle]
    pub unsafe extern "C" fn thread_getSysCallHandler(
        thread: *const Thread,
    ) -> *mut c::SysCallHandler {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.syscallhandler.ptr()
    }

    #[no_mangle]
    pub unsafe extern "C" fn thread_getSysCallCondition(
        thread: *const Thread,
    ) -> *mut c::SysCallCondition {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.cond.get().ptr()
    }

    #[no_mangle]
    pub unsafe extern "C" fn thread_clearSysCallCondition(thread: *const Thread) {
        let thread = unsafe { thread.as_ref().unwrap() };
        thread.cleanup_syscall_condition();
    }

    /// Returns true iff there is an unblocked, unignored signal pending for this
    /// thread (or its process).
    #[no_mangle]
    pub unsafe extern "C" fn thread_unblockedSignalPending(
        thread: *const Thread,
        host_lock: *const ShimShmemHostLock,
    ) -> bool {
        let thread = unsafe { thread.as_ref().unwrap() };
        let host_lock = unsafe { host_lock.as_ref().unwrap() };

        Worker::with_active_host(|host| {
            let process = host.process_borrow(thread.process_id()).unwrap();
            let process = process.borrow(host.root());
            thread.unblocked_signal_pending(&process, host_lock)
        })
        .unwrap()
    }

    /// Register a `Descriptor`. This takes ownership of the descriptor and you must not access it
    /// after.
    #[no_mangle]
    pub extern "C" fn thread_registerDescriptor(
        thread: *const Thread,
        desc: *mut Descriptor,
    ) -> libc::c_int {
        let thread = unsafe { thread.as_ref().unwrap() };
        let desc = Descriptor::from_raw(desc).unwrap();

        Worker::with_active_host(|host| {
            thread
                .descriptor_table_borrow_mut(host)
                .register_descriptor(*desc)
                .unwrap()
                .into()
        })
        .unwrap()
    }

    /// Get a temporary reference to a descriptor.
    #[no_mangle]
    pub extern "C" fn thread_getRegisteredDescriptor(
        thread: *const Thread,
        handle: libc::c_int,
    ) -> *const Descriptor {
        let thread = unsafe { thread.as_ref().unwrap() };

        let handle = match handle.try_into() {
            Ok(i) => i,
            Err(_) => {
                log::debug!("Attempted to get a descriptor with handle {}", handle);
                return std::ptr::null();
            }
        };

        Worker::with_active_host(
            |host| match thread.descriptor_table_borrow(host).get(handle) {
                Some(d) => d as *const Descriptor,
                None => std::ptr::null(),
            },
        )
        .unwrap()
    }

    /// Get a temporary mutable reference to a descriptor.
    #[no_mangle]
    pub extern "C" fn thread_getRegisteredDescriptorMut(
        thread: *const Thread,
        handle: libc::c_int,
    ) -> *mut Descriptor {
        let thread = unsafe { thread.as_ref().unwrap() };

        let handle = match handle.try_into() {
            Ok(i) => i,
            Err(_) => {
                log::debug!("Attempted to get a descriptor with handle {}", handle);
                return std::ptr::null_mut();
            }
        };

        Worker::with_active_host(|host| {
            match thread.descriptor_table_borrow_mut(host).get_mut(handle) {
                Some(d) => d as *mut Descriptor,
                None => std::ptr::null_mut(),
            }
        })
        .unwrap()
    }

    /// Get a temporary reference to a legacy file.
    #[no_mangle]
    pub unsafe extern "C" fn thread_getRegisteredLegacyFile(
        thread: *const Thread,
        handle: libc::c_int,
    ) -> *mut c::LegacyFile {
        let thread = unsafe { thread.as_ref().unwrap() };

        let handle = match handle.try_into() {
            Ok(i) => i,
            Err(_) => {
                log::debug!("Attempted to get a descriptor with handle {}", handle);
                return std::ptr::null_mut();
            }
        };

        Worker::with_active_host(|host| {
        match thread.descriptor_table_borrow(host).get(handle).map(|x| x.file()) {
            Some(CompatFile::Legacy(file)) => file.ptr(),
            Some(CompatFile::New(file)) => {
                // we have a special case for the legacy C TCP objects
                if let File::Socket(Socket::Inet(InetSocket::LegacyTcp(tcp))) = file.inner_file() {
                    tcp.borrow().as_legacy_file()
                } else {
                    log::warn!(
                        "A descriptor exists for fd={}, but it is not a legacy file. Returning NULL.",
                        handle
                    );
                    std::ptr::null_mut()
                }
            }
            None => std::ptr::null_mut(),
        }
        }).unwrap()
    }
}
