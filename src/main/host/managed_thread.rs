use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::sync::{atomic, Arc};

use linux_api::sched::CloneFlags;
use log::{debug, error, log_enabled, trace, Level};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use shadow_shim_helper_rs::ipc::IPCData;
use shadow_shim_helper_rs::shim_event::{
    ShimEventAddThreadReq, ShimEventAddThreadRes, ShimEventSyscall, ShimEventSyscallComplete,
    ShimEventToShadow, ShimEventToShim,
};
use shadow_shim_helper_rs::syscall_types::{ForeignPtr, SysCallArgs, SysCallReg};
use shadow_shmem::allocator::ShMemBlock;
use vasi_sync::scchannel::SelfContainedChannelError;

use super::context::ThreadContext;
use super::host::Host;
use super::syscall_condition::SysCallCondition;
use crate::core::scheduler;
use crate::core::worker::{Worker, WORKER_SHARED};
use crate::cshadow;
use crate::host::syscall_types::SyscallReturn;
use crate::utility::syscall;

/// The ManagedThread's state after having been allowed to execute some code.
#[derive(Debug)]
#[must_use]
pub enum ResumeResult {
    /// Blocked on a SysCallCondition.
    Blocked(SysCallCondition),
    /// The native thread has exited with the given code.
    ExitedThread(i32),
    /// The thread's process has exited.
    ExitedProcess,
}

pub struct ManagedThread {
    ipc_shmem: Arc<ShMemBlock<'static, IPCData>>,
    is_running: Cell<bool>,
    return_code: Cell<Option<i32>>,

    /* holds the event for the most recent call from the plugin/shim */
    current_event: RefCell<ShimEventToShadow>,

    native_pid: nix::unistd::Pid,
    native_tid: nix::unistd::Pid,

    // Value storing the current CPU affinity of the thread (more precisely,
    // of the native thread backing this thread object). This value will be set
    // to AFFINITY_UNINIT if CPU pinning is not enabled or if the thread has
    // not yet been pinned to a CPU.
    affinity: Cell<i32>,
}

impl ManagedThread {
    pub fn native_pid(&self) -> nix::unistd::Pid {
        self.native_pid
    }

    pub fn native_tid(&self) -> nix::unistd::Pid {
        self.native_tid
    }

    /// Make the specified syscall on the native thread.
    ///
    /// Panics if the native thread is dead or dies during the syscall,
    /// including if the syscall itself is SYS_exit or SYS_exit_group.
    pub fn native_syscall(&self, ctx: &ThreadContext, n: i64, args: &[SysCallReg]) -> SysCallReg {
        let mut syscall_args = SysCallArgs {
            number: n,
            args: [SysCallReg::from(0u64); 6],
        };
        syscall_args.args[..args.len()].copy_from_slice(args);
        match self.continue_plugin(
            ctx.host,
            &ShimEventToShim::Syscall(ShimEventSyscall { syscall_args }),
        ) {
            ShimEventToShadow::SyscallComplete(res) => res.retval,
            other => panic!("Unexpected response from plugin: {other:?}"),
        }
    }

    pub fn spawn(
        plugin_path: &CStr,
        argv: Vec<CString>,
        mut envv: Vec<CString>,
        working_dir: &CStr,
        strace_fd: Option<RawFd>,
        log_path: &CStr,
    ) -> Self {
        let ipc_shmem = Arc::new(shadow_shmem::allocator::shmalloc(IPCData::new()));
        envv.push(CString::new(format!("SHADOW_IPC_BLK={}", ipc_shmem.serialize())).unwrap());
        debug!("spawning new mthread '{plugin_path:?}' with environment '{envv:?}', arguments '{argv:?}', and working directory '{working_dir:?}'");

        let shimlog_fd = nix::fcntl::open(
            log_path,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_CLOEXEC,
            Mode::S_IRUSR | Mode::S_IWUSR | Mode::S_IRGRP | Mode::S_IROTH | Mode::S_IWOTH,
        )
        .unwrap();

        let child_pid =
            Self::spawn_native(plugin_path, argv, envv, working_dir, strace_fd, shimlog_fd);

        // should be opened in the shim, so no need for it anymore
        nix::unistd::close(shimlog_fd).unwrap();

        // In Linux, the PID is equal to the TID of its first thread.
        let native_pid = child_pid;
        let native_tid = child_pid;

        // Configure the child_pid_watcher to close the IPC channel when the child dies.
        {
            let ipc = ipc_shmem.clone();
            WORKER_SHARED
                .borrow()
                .as_ref()
                .unwrap()
                .child_pid_watcher()
                .register_callback(child_pid, move |_pid| {
                    ipc.from_plugin().close_writer();
                })
        };

        trace!(
            "waiting for start event from shim with native pid {}",
            native_pid
        );
        let start_req = ipc_shmem.from_plugin().receive().unwrap();
        match &start_req {
            ShimEventToShadow::StartReq(_) => (),
            other => panic!("Unexpected result from shim: {other:?}"),
        };

        Self {
            ipc_shmem,
            is_running: Cell::new(true),
            return_code: Cell::new(None),
            current_event: RefCell::new(start_req),
            native_pid,
            native_tid,
            affinity: Cell::new(cshadow::AFFINITY_UNINIT),
        }
    }

    pub fn resume(&self, ctx: &ThreadContext) -> ResumeResult {
        debug_assert!(self.is_running());

        self.sync_affinity_with_worker();

        // Flush any pending writes, e.g. from a previous mthread that exited
        // without flushing.
        ctx.process.free_unsafe_borrows_flush().unwrap();

        loop {
            let mut current_event = self.current_event.borrow_mut();
            let last_event = *current_event;
            *current_event = match last_event {
                ShimEventToShadow::StartReq(start_req) => {
                    // Write the serialized thread shmem handle directly to shim
                    // memory.
                    ctx.process
                        .memory_borrow_mut()
                        .write(
                            start_req.thread_shmem_block_to_init,
                            &ctx.thread.shmem().serialize(),
                        )
                        .unwrap();

                    if !start_req.process_shmem_block_to_init.is_null() {
                        // Write the serialized process shmem handle directly to
                        // shim memory.
                        ctx.process
                            .memory_borrow_mut()
                            .write(
                                start_req.process_shmem_block_to_init,
                                &ctx.process.shmem().serialize(),
                            )
                            .unwrap();
                    }

                    // send the message to the shim to call main().
                    trace!("sending start event code to shim");
                    self.continue_plugin(ctx.host, &ShimEventToShim::StartRes)
                }
                ShimEventToShadow::ProcessDeath => {
                    // The native threads are all dead or zombies. Nothing to do but
                    // clean up.
                    self.cleanup_after_exit_initiated();
                    return ResumeResult::ExitedProcess;
                }
                ShimEventToShadow::Syscall(syscall) => {
                    // Emulate the given syscall.

                    // `exit` is tricky since it only exits the *mthread*, and we don't have a way
                    // to be notified that the mthread has exited. We have to "fire and forget"
                    // the command to execute the syscall natively.
                    //
                    // TODO: We could use a tid futex in shared memory, as set by
                    // `set_tid_address`, to block here until the thread has
                    // actually exited.
                    if syscall.syscall_args.number == libc::SYS_exit {
                        let return_code = syscall.syscall_args.args[0].into();
                        debug!("Short-circuiting syscall exit({return_code})");
                        self.return_code.set(Some(return_code));
                        // Tell mthread to go ahead and make the exit syscall itself.
                        // We *don't* call `_managedthread_continuePlugin` here,
                        // since that'd release the ShimSharedMemHostLock, and we
                        // aren't going to get a message back to know when it'd be
                        // safe to take it again.
                        self.ipc_shmem
                            .to_plugin()
                            .send(ShimEventToShim::SyscallDoNative);
                        self.cleanup_after_exit_initiated();
                        return ResumeResult::ExitedThread(return_code);
                    }

                    let scr = unsafe {
                        cshadow::syscallhandler_make_syscall(
                            ctx.thread.csyscallhandler(),
                            &syscall.syscall_args,
                        )
                    };

                    // remove the mthread's old syscall condition since it's no longer needed
                    ctx.thread.cleanup_syscall_condition();

                    assert!(self.is_running());

                    // Flush any writes that legacy C syscallhandlers may have
                    // made.
                    ctx.process.free_unsafe_borrows_flush().unwrap();

                    match scr {
                        SyscallReturn::Block(b) => {
                            return ResumeResult::Blocked(unsafe {
                                SysCallCondition::consume_from_c(b.cond)
                            })
                        }
                        SyscallReturn::Done(d) => self.continue_plugin(
                            ctx.host,
                            &ShimEventToShim::SyscallComplete(ShimEventSyscallComplete {
                                retval: d.retval,
                                restartable: d.restartable,
                            }),
                        ),
                        SyscallReturn::Native => {
                            self.continue_plugin(ctx.host, &ShimEventToShim::SyscallDoNative)
                        }
                    }
                }
                ShimEventToShadow::AddThreadRes(res) => {
                    // We get here in the child process after forking.

                    // Child should have gotten 0 back from its native clone syscall.
                    assert_eq!(res.clone_res, 0);

                    // Complete the virtualized clone syscall.
                    self.continue_plugin(
                        ctx.host,
                        &ShimEventToShim::SyscallComplete(ShimEventSyscallComplete {
                            retval: 0.into(),
                            restartable: false,
                        }),
                    )
                }
                e @ ShimEventToShadow::SyscallComplete(_) => panic!("Unexpected event: {e:?}"),
            };
            assert!(self.is_running());
        }
    }

    pub fn handle_process_exit(&self) {
        // TODO: Only do this once per process; maybe by moving into `Process`.
        WORKER_SHARED
            .borrow()
            .as_ref()
            .unwrap()
            .child_pid_watcher()
            .unregister_pid(self.native_pid());

        self.cleanup_after_exit_initiated();
    }

    pub fn return_code(&self) -> Option<i32> {
        self.return_code.get()
    }

    pub fn is_running(&self) -> bool {
        self.is_running.get()
    }

    /// Execute the specified `clone` syscall in `self`, and use create a new
    /// `ManagedThread` object to manage it. The new thread will be managed
    /// by Shadow, and suitable for use with `Thread::wrap_mthread`.
    ///
    /// If the `clone` syscall fails, the native error is returned.
    pub fn native_clone(
        &self,
        ctx: &ThreadContext,
        flags: CloneFlags,
        child_stack: ForeignPtr<()>,
        ptid: ForeignPtr<libc::pid_t>,
        ctid: ForeignPtr<libc::pid_t>,
        newtls: libc::c_ulong,
    ) -> Result<ManagedThread, linux_api::errno::Errno> {
        let child_ipc_shmem = Arc::new(shadow_shmem::allocator::shmalloc(IPCData::new()));

        // Send the IPC block for the new mthread to use.
        let clone_res: i64 = match self.continue_plugin(
            ctx.host,
            &ShimEventToShim::AddThreadReq(ShimEventAddThreadReq {
                ipc_block: child_ipc_shmem.serialize(),
                flags: flags.bits(),
                child_stack,
                ptid: ptid.cast::<()>(),
                ctid: ctid.cast::<()>(),
                newtls,
            }),
        ) {
            ShimEventToShadow::AddThreadRes(ShimEventAddThreadRes { clone_res }) => clone_res,
            r => panic!("Unexpected result: {r:?}"),
        };
        let clone_res: SysCallReg = syscall::raw_return_value_to_result(clone_res)?;
        let child_native_tid = libc::pid_t::from(clone_res);
        trace!("native clone treated tid {child_native_tid}");

        trace!(
            "waiting for start event from shim with native tid {}",
            child_native_tid
        );
        let start_req = child_ipc_shmem.from_plugin().receive().unwrap();
        match &start_req {
            ShimEventToShadow::StartReq(_) => (),
            other => panic!("Unexpected result from shim: {other:?}"),
        };

        let native_pid = if flags.contains(CloneFlags::CLONE_THREAD) {
            self.native_pid
        } else {
            nix::unistd::Pid::from_raw(child_native_tid)
        };

        if !flags.contains(CloneFlags::CLONE_THREAD) {
            // Child is a new process; register it.
            WORKER_SHARED
                .borrow()
                .as_ref()
                .unwrap()
                .child_pid_watcher()
                .register_pid(native_pid);
        }

        // Register the child thread's IPC block with the ChildPidWatcher.
        {
            let child_ipc_shmem = child_ipc_shmem.clone();
            WORKER_SHARED
                .borrow()
                .as_ref()
                .unwrap()
                .child_pid_watcher()
                .register_callback(native_pid, move |_pid| {
                    child_ipc_shmem.from_plugin().close_writer();
                })
        };

        Ok(Self {
            ipc_shmem: child_ipc_shmem,
            is_running: Cell::new(true),
            return_code: Cell::new(None),
            current_event: RefCell::new(start_req),
            native_pid,
            native_tid: nix::unistd::Pid::from_raw(child_native_tid),
            // TODO: can we assume it's inherited from the current thread affinity?
            affinity: Cell::new(cshadow::AFFINITY_UNINIT),
        })
    }

    #[must_use]
    fn continue_plugin(&self, host: &Host, event: &ShimEventToShim) -> ShimEventToShadow {
        // Update shared state before transferring control.
        host.shim_shmem_lock_borrow_mut().unwrap().max_runahead_time =
            Worker::max_event_runahead_time(host);
        host.shim_shmem()
            .sim_time
            .store(Worker::current_time().unwrap(), atomic::Ordering::Relaxed);

        // Release lock so that plugin can take it. Reacquired in `wait_for_next_event`.
        host.unlock_shmem();

        self.ipc_shmem.to_plugin().send(*event);

        let event = match self.ipc_shmem.from_plugin().receive() {
            Ok(e) => e,
            Err(SelfContainedChannelError::WriterIsClosed) => ShimEventToShadow::ProcessDeath,
        };

        // Reacquire the shared memory lock, now that the shim has yielded control
        // back to us.
        host.lock_shmem();

        // Update time, which may have been incremented in the shim.
        let shim_time = host.shim_shmem().sim_time.load(atomic::Ordering::Relaxed);
        if log_enabled!(Level::Trace) {
            let worker_time = Worker::current_time().unwrap();
            if shim_time != worker_time {
                trace!(
                    "Updating time from {worker_time:?} to {shim_time:?} (+{:?})",
                    shim_time - worker_time
                );
            }
        }
        Worker::set_current_time(shim_time);

        event
    }

    /// To be called after we expect the native thread to have exited, or to
    /// exit imminently.
    fn cleanup_after_exit_initiated(&self) {
        if !self.is_running.get() {
            return;
        }
        self.wait_for_native_exit();
        trace!("child {:?} exited", self.native_tid());
        self.is_running.set(false);
    }

    /// Wait until the managed thread is no longer running.
    fn wait_for_native_exit(&self) {
        let native_pid = self.native_pid();
        let native_tid = self.native_tid();

        // We use `tgkill` and `/proc/x/stat` to detect whether the thread is still running,
        // looping until it doesn't.
        //
        // Alternatively we could use `set_tid_address` or `set_robust_list` to
        // be notified on a futex. Those are a bit underdocumented and fragile,
        // though. In practice this shouldn't have to loop significantly.
        trace!("Waiting for native thread {native_pid}.{native_tid} to exit");
        loop {
            if self.ipc_shmem.from_plugin().writer_is_closed() {
                // This indicates that the whole process has stopped executing;
                // no need to poll the individual thread.
                break;
            }
            match tgkill(native_pid, native_tid, None) {
                Err(Errno::ESRCH) => {
                    trace!("Thread is done exiting; proceeding with cleanup");
                    break;
                }
                Err(e) => {
                    error!("Unexpected tgkill error: {:?}", e);
                    break;
                }
                Ok(()) if native_pid == native_tid => {
                    // Thread leader could be in a zombie state waiting for
                    // the other threads to exit.
                    let filename = format!("/proc/{native_pid}/stat");
                    let stat = match std::fs::read_to_string(filename) {
                        Err(e) => {
                            assert!(e.kind() == std::io::ErrorKind::NotFound);
                            trace!("tgl {native_pid} is fully dead");
                            break;
                        }
                        Ok(s) => s,
                    };
                    if stat.contains(") Z") {
                        trace!("tgl {native_pid} is a zombie");
                        break;
                    }
                    // Still alive and in a non-zombie state; continue
                }
                Ok(()) => {
                    // Thread is still alive; continue.
                }
            };
            std::thread::yield_now();
        }
    }

    fn sync_affinity_with_worker(&self) {
        let current_affinity = scheduler::core_affinity()
            .map(|x| i32::try_from(x).unwrap())
            .unwrap_or(cshadow::AFFINITY_UNINIT);
        self.affinity.set(unsafe {
            cshadow::affinity_setProcessAffinity(
                self.native_tid().into(),
                current_affinity,
                self.affinity.get(),
            )
        });
    }

    fn spawn_native(
        plugin_path: &CStr,
        argv: Vec<CString>,
        mut envv: Vec<CString>,
        working_dir: &CStr,
        strace_fd: Option<RawFd>,
        shimlog_fd: RawFd,
    ) -> nix::unistd::Pid {
        // Tell the shim to change the working dir.
        //
        // TODO: Instead use posix_spawn_file_actions_addchdir_np, which was added
        // in glibc 2.29. We should be able to do so once we've dropped support
        // for some platforms, as planned for the shadow 3.0 release.
        // https://github.com/shadow/shadow/discussions/2496
        envv.push(
            CString::new(format!(
                "SHADOW_WORKING_DIR={}",
                working_dir.to_str().unwrap()
            ))
            .unwrap(),
        );

        // posix_spawn is documented as taking pointers to *mutable* char for argv and
        // envv. It *probably* doesn't actually mutate them, but we
        // conservatively give it what it asks for. We have to "reconstitute"
        // the CString's after the fork + exec to deallocate them.
        let argv_ptrs: Vec<*mut i8> = argv
            .into_iter()
            .map(CString::into_raw)
            // the last element of argv must be NULL
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect();
        let envv_ptrs: Vec<*mut i8> = envv
            .into_iter()
            .map(CString::into_raw)
            // the last element of argv must be NULL
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect();

        let mut file_actions: libc::posix_spawn_file_actions_t = shadow_pod::zeroed();
        Errno::result(unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) }).unwrap();

        // Dup straceFd; the dup'd descriptor won't have O_CLOEXEC set.
        //
        // Since dup2 is a no-op when the new and old file descriptors are equal, we have
        // to arrange to call dup2 twice - first to a temporary descriptor, and then back
        // to the original descriptor number.
        //
        // Here we use STDOUT_FILENO as the temporary descriptor, since we later
        // replace that below.
        //
        // Once we drop support for platforms with glibc older than 2.29, we *could*
        // consider taking advantage of a new feature that would let us just use a
        // single `posix_spawn_file_actions_adddup2` call with equal descriptors.
        // OTOH it's a non-standard extension, and I think ultimately uses the same
        // number of syscalls, so it might be better to continue using this slightly
        // more awkward method anyway.
        // https://github.com/bminor/glibc/commit/805334b26c7e6e83557234f2008497c72176a6cd
        // https://austingroupbugs.net/view.php?id=411
        if let Some(strace_fd) = strace_fd {
            Errno::result(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &mut file_actions,
                    strace_fd,
                    libc::STDOUT_FILENO,
                )
            })
            .unwrap();
            Errno::result(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &mut file_actions,
                    libc::STDOUT_FILENO,
                    strace_fd,
                )
            })
            .unwrap();
        }

        // set stdout/stderr as the shim log. This also clears the FD_CLOEXEC flag.
        Errno::result(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                shimlog_fd,
                libc::STDOUT_FILENO,
            )
        })
        .unwrap();
        Errno::result(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                shimlog_fd,
                libc::STDERR_FILENO,
            )
        })
        .unwrap();

        let mut spawn_attr: libc::posix_spawnattr_t = shadow_pod::zeroed();
        Errno::result(unsafe { libc::posix_spawnattr_init(&mut spawn_attr) }).unwrap();

        // In versions of glibc before 2.24, we need this to tell posix_spawn
        // to use vfork instead of fork. In later versions it's a no-op.
        Errno::result(unsafe {
            libc::posix_spawnattr_setflags(
                &mut spawn_attr,
                libc::POSIX_SPAWN_USEVFORK.try_into().unwrap(),
            )
        })
        .unwrap();

        let child_pid = {
            let mut child_pid = -1;
            Errno::result(unsafe {
                libc::posix_spawn(
                    &mut child_pid,
                    plugin_path.as_ptr(),
                    &file_actions,
                    &spawn_attr,
                    argv_ptrs.as_ptr(),
                    envv_ptrs.as_ptr(),
                )
            })
            .unwrap();
            nix::unistd::Pid::from_raw(child_pid)
        };

        Errno::result(unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) })
            .unwrap();
        Errno::result(unsafe { libc::posix_spawnattr_destroy(&mut spawn_attr) }).unwrap();

        // register the read-end of the pipe, so that we'll be notified of the
        // child's death when the write-end is closed.
        WORKER_SHARED
            .borrow()
            .as_ref()
            .unwrap()
            .child_pid_watcher()
            .register_pid(child_pid);

        // Drop the cloned argv and env.
        drop(
            argv_ptrs
                .into_iter()
                .filter(|p| !p.is_null())
                .map(|p| unsafe { CString::from_raw(p) }),
        );
        drop(
            envv_ptrs
                .into_iter()
                .filter(|p| !p.is_null())
                .map(|p| unsafe { CString::from_raw(p) }),
        );

        debug!(
            "started process {} with PID {child_pid:?}",
            plugin_path.to_str().unwrap()
        );

        child_pid
    }
}

impl Drop for ManagedThread {
    fn drop(&mut self) {
        // Dropping while the thread is running is unsound because the running
        // thread still has access to shared memory regions that will be
        // deallocated, and potentially reallocated for another purpose. The
        // running thread accessing a deallocated or repurposed memory region
        // can cause numerous problems.
        assert!(!self.is_running());
    }
}

fn tgkill(
    pid: nix::unistd::Pid,
    tid: nix::unistd::Pid,
    signo: Option<nix::sys::signal::Signal>,
) -> nix::Result<()> {
    let pid = pid.as_raw();
    let tid = tid.as_raw();
    let signo = match signo {
        Some(s) => s as i32,
        None => 0,
    };
    let res = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, signo) };
    Errno::result(res).map(|i: i64| {
        assert_eq!(i, 0);
    })
}
