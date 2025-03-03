use std::sync::Arc;

use atomic_refcell::AtomicRefCell;
use linux_api::errno::Errno;
use linux_api::fcntl::{DescriptorFlags, OFlag};
use linux_api::posix_types::{kernel_off_t, kernel_pid_t};
use log::*;
use shadow_shim_helper_rs::syscall_types::ForeignPtr;
use syscall_logger::log_syscall;

use crate::cshadow as c;
use crate::host::descriptor::pipe;
use crate::host::descriptor::shared_buf::SharedBuf;
use crate::host::descriptor::{CompatFile, Descriptor, File, FileMode, FileStatus, OpenFile};
use crate::host::process::{Process, ProcessId};
use crate::host::syscall::handler::{SyscallContext, SyscallHandler};
use crate::host::syscall::io::IoVec;
use crate::host::syscall::type_formatting::SyscallBufferArg;
use crate::host::syscall_types::{SyscallError, SyscallResult};
use crate::utility::callback_queue::CallbackQueue;

impl SyscallHandler {
    #[log_syscall(/* rv */ std::ffi::c_int, /* fd */ std::ffi::c_int)]
    pub fn close(ctx: &mut SyscallContext, fd: std::ffi::c_int) -> SyscallResult {
        trace!("Trying to close fd {}", fd);

        let fd = fd.try_into().or(Err(linux_api::errno::Errno::EBADF))?;

        // according to "man 2 close", in Linux any errors that may occur will happen after the fd is
        // released, so we should always deregister the descriptor even if there's an error while
        // closing
        let desc = ctx
            .objs
            .thread
            .descriptor_table_borrow_mut(ctx.objs.host)
            .deregister_descriptor(fd)
            .ok_or(linux_api::errno::Errno::EBADF)?;

        // if there are still valid descriptors to the open file, close() will do nothing
        // and return None
        crate::utility::legacy_callback_queue::with_global_cb_queue(|| {
            CallbackQueue::queue_and_run(|cb_queue| desc.close(ctx.objs.host, cb_queue))
                .unwrap_or(Ok(()))
                .map(|()| 0.into())
        })
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* oldfd */ std::ffi::c_int)]
    pub fn dup(ctx: &mut SyscallContext, fd: std::ffi::c_int) -> SyscallResult {
        // get the descriptor, or return early if it doesn't exist
        let mut desc_table = ctx.objs.thread.descriptor_table_borrow_mut(ctx.objs.host);
        let desc = Self::get_descriptor(&desc_table, fd)?;

        // duplicate the descriptor
        let new_desc = desc.dup(DescriptorFlags::empty());
        let new_fd = desc_table
            .register_descriptor(new_desc)
            .or(Err(Errno::ENFILE))?;

        // return the new fd
        Ok(std::ffi::c_int::try_from(new_fd).unwrap().into())
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* oldfd */ std::ffi::c_int, /* newfd */ std::ffi::c_int)]
    pub fn dup2(
        ctx: &mut SyscallContext,
        old_fd: std::ffi::c_int,
        new_fd: std::ffi::c_int,
    ) -> SyscallResult {
        // get the descriptor, or return early if it doesn't exist
        let mut desc_table = ctx.objs.thread.descriptor_table_borrow_mut(ctx.objs.host);
        let desc = Self::get_descriptor(&desc_table, old_fd)?;

        // from 'man 2 dup2': "If oldfd is a valid file descriptor, and newfd has the same
        // value as oldfd, then dup2() does nothing, and returns newfd"
        if old_fd == new_fd {
            return Ok(new_fd.into());
        }

        let new_fd = new_fd.try_into().or(Err(linux_api::errno::Errno::EBADF))?;

        // duplicate the descriptor
        let new_desc = desc.dup(DescriptorFlags::empty());
        let replaced_desc = desc_table.register_descriptor_with_fd(new_desc, new_fd);

        // close the replaced descriptor
        if let Some(replaced_desc) = replaced_desc {
            // from 'man 2 dup2': "If newfd was open, any errors that would have been reported at
            // close(2) time are lost"
            CallbackQueue::queue_and_run(|cb_queue| replaced_desc.close(ctx.objs.host, cb_queue));
        }

        // return the new fd
        Ok(std::ffi::c_int::from(new_fd).into())
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* oldfd */ std::ffi::c_int, /* newfd */ std::ffi::c_int,
                  /* flags */ linux_api::fcntl::OFlag)]
    pub fn dup3(
        ctx: &mut SyscallContext,
        old_fd: std::ffi::c_int,
        new_fd: std::ffi::c_int,
        flags: std::ffi::c_int,
    ) -> SyscallResult {
        // get the descriptor, or return early if it doesn't exist
        let mut desc_table = ctx.objs.thread.descriptor_table_borrow_mut(ctx.objs.host);
        let desc = Self::get_descriptor(&desc_table, old_fd)?;

        // from 'man 2 dup3': "If oldfd equals newfd, then dup3() fails with the error EINVAL"
        if old_fd == new_fd {
            return Err(linux_api::errno::Errno::EINVAL.into());
        }

        let new_fd = new_fd.try_into().or(Err(linux_api::errno::Errno::EBADF))?;

        let Some(flags) = OFlag::from_bits(flags) else {
            debug!("Invalid flags: {flags}");
            return Err(linux_api::errno::Errno::EINVAL.into());
        };

        let mut descriptor_flags = DescriptorFlags::empty();

        // dup3 only supports the O_CLOEXEC flag
        for flag in flags {
            match flag {
                OFlag::O_CLOEXEC => descriptor_flags.insert(DescriptorFlags::FD_CLOEXEC),
                x if x == OFlag::empty() => {
                    // The "empty" flag is always present. Ignore.
                }
                _ => {
                    debug!("Invalid flags for dup3: {flags:?}");
                    return Err(linux_api::errno::Errno::EINVAL.into());
                }
            }
        }

        // duplicate the descriptor
        let new_desc = desc.dup(descriptor_flags);
        let replaced_desc = desc_table.register_descriptor_with_fd(new_desc, new_fd);

        // close the replaced descriptor
        if let Some(replaced_desc) = replaced_desc {
            // from 'man 2 dup3': "If newfd was open, any errors that would have been reported at
            // close(2) time are lost"
            CallbackQueue::queue_and_run(|cb_queue| replaced_desc.close(ctx.objs.host, cb_queue));
        }

        // return the new fd
        Ok(std::ffi::c_int::try_from(new_fd).unwrap().into())
    }

    #[log_syscall(/* rv */ isize, /* fd */ std::ffi::c_int, /* buf */ *const std::ffi::c_void,
                  /* count */ usize)]
    pub fn read(
        ctx: &mut SyscallContext,
        fd: std::ffi::c_int,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
    ) -> Result<isize, SyscallError> {
        // if we were previously blocked, get the active file from the last syscall handler
        // invocation since it may no longer exist in the descriptor table
        let file = ctx
            .objs
            .thread
            .syscall_condition()
            // if this was for a C descriptor, then there won't be an active file object
            .and_then(|x| x.active_file().cloned());

        let file = match file {
            // we were previously blocked, so re-use the file from the previous syscall invocation
            Some(x) => x,
            // get the file from the descriptor table, or return early if it doesn't exist
            None => {
                let desc_table = ctx.objs.thread.descriptor_table_borrow(ctx.objs.host);
                match Self::get_descriptor(&desc_table, fd)?.file() {
                    CompatFile::New(file) => file.clone(),
                    // if it's a legacy file, use the C syscall handler instead
                    CompatFile::Legacy(_) => {
                        drop(desc_table);
                        return Self::legacy_syscall(c::syscallhandler_read, ctx).map(Into::into);
                    }
                }
            }
        };

        let mut result = Self::read_helper(ctx, file.inner_file(), buf_ptr, buf_size, None);

        // if the syscall will block, keep the file open until the syscall restarts
        if let Some(err) = result.as_mut().err() {
            if let Some(cond) = err.blocked_condition() {
                cond.set_active_file(file);
            }
        }

        let bytes_read = result?;
        Ok(bytes_read)
    }

    #[log_syscall(/* rv */ isize, /* fd */ std::ffi::c_int, /* buf */ *const std::ffi::c_void,
                  /* count */ usize, /* offset */ kernel_off_t)]
    pub fn pread64(
        ctx: &mut SyscallContext,
        fd: std::ffi::c_int,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
        offset: kernel_off_t,
    ) -> Result<isize, SyscallError> {
        // if we were previously blocked, get the active file from the last syscall handler
        // invocation since it may no longer exist in the descriptor table
        let file = ctx
            .objs
            .thread
            .syscall_condition()
            // if this was for a C descriptor, then there won't be an active file object
            .and_then(|x| x.active_file().cloned());

        let file = match file {
            // we were previously blocked, so re-use the file from the previous syscall invocation
            Some(x) => x,
            // get the file from the descriptor table, or return early if it doesn't exist
            None => {
                let desc_table = ctx.objs.thread.descriptor_table_borrow(ctx.objs.host);
                match Self::get_descriptor(&desc_table, fd)?.file() {
                    CompatFile::New(file) => file.clone(),
                    // if it's a legacy file, use the C syscall handler instead
                    CompatFile::Legacy(_) => {
                        drop(desc_table);
                        return Self::legacy_syscall(c::syscallhandler_pread64, ctx)
                            .map(Into::into);
                    }
                }
            }
        };

        let mut result = Self::read_helper(ctx, file.inner_file(), buf_ptr, buf_size, Some(offset));

        // if the syscall will block, keep the file open until the syscall restarts
        if let Some(err) = result.as_mut().err() {
            if let Some(cond) = err.blocked_condition() {
                cond.set_active_file(file);
            }
        }

        let bytes_read = result?;
        Ok(bytes_read)
    }

    fn read_helper(
        ctx: &mut SyscallContext,
        file: &File,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
        offset: Option<kernel_off_t>,
    ) -> Result<isize, SyscallError> {
        let iov = IoVec {
            base: buf_ptr,
            len: buf_size,
        };
        Self::readv_helper(ctx, file, &[iov], offset, 0)
    }

    #[log_syscall(/* rv */ isize, /* fd */ std::ffi::c_int,
                  /* buf */ SyscallBufferArg</* count */ 2>, /* count */ usize)]
    pub fn write(
        ctx: &mut SyscallContext,
        fd: std::ffi::c_int,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
    ) -> Result<isize, SyscallError> {
        // if we were previously blocked, get the active file from the last syscall handler
        // invocation since it may no longer exist in the descriptor table
        let file = ctx
            .objs
            .thread
            .syscall_condition()
            // if this was for a C descriptor, then there won't be an active file object
            .and_then(|x| x.active_file().cloned());

        let file = match file {
            // we were previously blocked, so re-use the file from the previous syscall invocation
            Some(x) => x,
            // get the file from the descriptor table, or return early if it doesn't exist
            None => {
                let desc_table = ctx.objs.thread.descriptor_table_borrow(ctx.objs.host);
                match Self::get_descriptor(&desc_table, fd)?.file() {
                    CompatFile::New(file) => file.clone(),
                    // if it's a legacy file, use the C syscall handler instead
                    CompatFile::Legacy(_) => {
                        drop(desc_table);
                        return Self::legacy_syscall(c::syscallhandler_write, ctx).map(Into::into);
                    }
                }
            }
        };

        let mut result = Self::write_helper(ctx, file.inner_file(), buf_ptr, buf_size, None);

        // if the syscall will block, keep the file open until the syscall restarts
        if let Some(err) = result.as_mut().err() {
            if let Some(cond) = err.blocked_condition() {
                cond.set_active_file(file);
            }
        }

        let bytes_written = result?;
        Ok(bytes_written)
    }

    #[log_syscall(/* rv */ isize, /* fd */ std::ffi::c_int,
                  /* buf */ SyscallBufferArg</* count */ 2>, /* count */ usize,
                  /* offset */ kernel_off_t)]
    pub fn pwrite64(
        ctx: &mut SyscallContext,
        fd: std::ffi::c_int,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
        offset: kernel_off_t,
    ) -> Result<isize, SyscallError> {
        // if we were previously blocked, get the active file from the last syscall handler
        // invocation since it may no longer exist in the descriptor table
        let file = ctx
            .objs
            .thread
            .syscall_condition()
            // if this was for a C descriptor, then there won't be an active file object
            .and_then(|x| x.active_file().cloned());

        let file = match file {
            // we were previously blocked, so re-use the file from the previous syscall invocation
            Some(x) => x,
            // get the file from the descriptor table, or return early if it doesn't exist
            None => {
                let desc_table = ctx.objs.thread.descriptor_table_borrow(ctx.objs.host);
                match Self::get_descriptor(&desc_table, fd)?.file() {
                    CompatFile::New(file) => file.clone(),
                    // if it's a legacy file, use the C syscall handler instead
                    CompatFile::Legacy(_) => {
                        drop(desc_table);
                        return Self::legacy_syscall(c::syscallhandler_pwrite64, ctx)
                            .map(Into::into);
                    }
                }
            }
        };

        let mut result =
            Self::write_helper(ctx, file.inner_file(), buf_ptr, buf_size, Some(offset));

        // if the syscall will block, keep the file open until the syscall restarts
        if let Some(err) = result.as_mut().err() {
            if let Some(cond) = err.blocked_condition() {
                cond.set_active_file(file);
            }
        }

        let bytes_written = result?;
        Ok(bytes_written)
    }

    fn write_helper(
        ctx: &mut SyscallContext,
        file: &File,
        buf_ptr: ForeignPtr<u8>,
        buf_size: usize,
        offset: Option<kernel_off_t>,
    ) -> Result<isize, SyscallError> {
        let iov = IoVec {
            base: buf_ptr,
            len: buf_size,
        };
        Self::writev_helper(ctx, file, &[iov], offset, 0)
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* pipefd */ [std::ffi::c_int; 2])]
    pub fn pipe(
        ctx: &mut SyscallContext,
        fd_ptr: ForeignPtr<[std::ffi::c_int; 2]>,
    ) -> SyscallResult {
        Self::pipe_helper(ctx, fd_ptr, 0)
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* pipefd */ [std::ffi::c_int; 2],
                  /* flags */ linux_api::fcntl::OFlag)]
    pub fn pipe2(
        ctx: &mut SyscallContext,
        fd_ptr: ForeignPtr<[std::ffi::c_int; 2]>,
        flags: std::ffi::c_int,
    ) -> SyscallResult {
        Self::pipe_helper(ctx, fd_ptr, flags)
    }

    fn pipe_helper(
        ctx: &mut SyscallContext,
        fd_ptr: ForeignPtr<[std::ffi::c_int; 2]>,
        flags: i32,
    ) -> SyscallResult {
        // make sure they didn't pass a NULL pointer
        if fd_ptr.is_null() {
            return Err(linux_api::errno::Errno::EFAULT.into());
        }

        let Some(flags) = OFlag::from_bits(flags) else {
            debug!("Invalid flags: {flags}");
            return Err(Errno::EINVAL.into());
        };

        let mut file_flags = FileStatus::empty();
        let mut descriptor_flags = DescriptorFlags::empty();

        for flag in flags.iter() {
            match flag {
                OFlag::O_NONBLOCK => file_flags.insert(FileStatus::NONBLOCK),
                OFlag::O_DIRECT => file_flags.insert(FileStatus::DIRECT),
                OFlag::O_CLOEXEC => descriptor_flags.insert(DescriptorFlags::FD_CLOEXEC),
                x if x == OFlag::empty() => {
                    // The "empty" flag is always present. Ignore.
                }
                unhandled => {
                    // TODO: return an error and change this to `warn_once_then_debug`?
                    warn!("Ignoring pipe flag {unhandled:?}");
                }
            }
        }

        // reference-counted buffer for the pipe
        let buffer = SharedBuf::new(c::CONFIG_PIPE_BUFFER_SIZE.try_into().unwrap());
        let buffer = Arc::new(AtomicRefCell::new(buffer));

        // reference-counted file object for read end of the pipe
        let reader = pipe::Pipe::new(FileMode::READ, file_flags);
        let reader = Arc::new(AtomicRefCell::new(reader));

        // reference-counted file object for write end of the pipe
        let writer = pipe::Pipe::new(FileMode::WRITE, file_flags);
        let writer = Arc::new(AtomicRefCell::new(writer));

        // set the file objects to listen for events on the buffer
        CallbackQueue::queue_and_run(|cb_queue| {
            pipe::Pipe::connect_to_buffer(&reader, Arc::clone(&buffer), cb_queue);
            pipe::Pipe::connect_to_buffer(&writer, Arc::clone(&buffer), cb_queue);
        });

        // file descriptors for the read and write file objects
        let mut reader_desc = Descriptor::new(CompatFile::New(OpenFile::new(File::Pipe(reader))));
        let mut writer_desc = Descriptor::new(CompatFile::New(OpenFile::new(File::Pipe(writer))));

        // set the file descriptor flags
        reader_desc.set_flags(descriptor_flags);
        writer_desc.set_flags(descriptor_flags);

        // register the file descriptors
        let mut dt = ctx.objs.thread.descriptor_table_borrow_mut(ctx.objs.host);
        // unwrap here since the error handling would be messy (need to deregister) and we shouldn't
        // ever need to worry about this in practice
        let read_fd = dt.register_descriptor(reader_desc).unwrap();
        let write_fd = dt.register_descriptor(writer_desc).unwrap();

        // try to write them to the caller
        let fds = [
            i32::try_from(read_fd).unwrap(),
            i32::try_from(write_fd).unwrap(),
        ];
        let write_res = ctx.objs.process.memory_borrow_mut().write(fd_ptr, &fds);

        // clean up in case of error
        match write_res {
            Ok(_) => Ok(0.into()),
            Err(e) => {
                CallbackQueue::queue_and_run(|cb_queue| {
                    // ignore any errors when closing
                    dt.deregister_descriptor(read_fd)
                        .unwrap()
                        .close(ctx.objs.host, cb_queue);
                    dt.deregister_descriptor(write_fd)
                        .unwrap()
                        .close(ctx.objs.host, cb_queue);
                });
                Err(e.into())
            }
        }
    }

    #[log_syscall(/* rv */ linux_api::posix_types::kernel_pid_t)]
    pub fn getppid(ctx: &mut SyscallContext) -> Result<kernel_pid_t, SyscallError> {
        Ok(ctx.objs.process.parent_id().into())
    }

    #[log_syscall(/* rv */ kernel_pid_t)]
    pub fn getpgrp(ctx: &mut SyscallContext) -> Result<kernel_pid_t, SyscallError> {
        Ok(ctx.objs.process.group_id().into())
    }

    #[log_syscall(/* rv */ kernel_pid_t, /* pid*/ kernel_pid_t)]
    pub fn getpgid(
        ctx: &mut SyscallContext,
        pid: kernel_pid_t,
    ) -> Result<kernel_pid_t, SyscallError> {
        if pid == 0 || pid == kernel_pid_t::from(ctx.objs.process.id()) {
            return Ok(ctx.objs.process.group_id().into());
        }
        let pid = ProcessId::try_from(pid).map_err(|_| Errno::EINVAL)?;
        let Some(process) = ctx.objs.host.process_borrow(pid) else {
            return Err(Errno::ESRCH.into());
        };
        let process = process.borrow(ctx.objs.host.root());
        Ok(process.group_id().into())
    }

    #[log_syscall(/* rv */ std::ffi::c_int, /* pid */ kernel_pid_t, /* pgid */kernel_pid_t)]
    pub fn setpgid(
        ctx: &mut SyscallContext,
        pid: kernel_pid_t,
        pgid: kernel_pid_t,
    ) -> Result<std::ffi::c_int, SyscallError> {
        let _processrc_borrow;
        let _process_borrow;
        let process: &Process;
        if pid == 0 || pid == kernel_pid_t::from(ctx.objs.process.id()) {
            _processrc_borrow = None;
            _process_borrow = None;
            process = ctx.objs.process;
        } else {
            let pid = ProcessId::try_from(pid).map_err(|_| Errno::EINVAL)?;
            let Some(pbrc) = ctx.objs.host.process_borrow(pid) else {
                return Err(Errno::ESRCH.into());
            };
            _processrc_borrow = Some(pbrc);
            _process_borrow = Some(
                _processrc_borrow
                    .as_ref()
                    .unwrap()
                    .borrow(ctx.objs.host.root()),
            );
            process = _process_borrow.as_ref().unwrap();
        }
        let pgid = if pgid == 0 {
            None
        } else {
            Some(ProcessId::try_from(pgid).map_err(|_| Errno::EINVAL)?)
        };
        if process.id() != ctx.objs.process.id() && process.parent_id() != ctx.objs.process.id() {
            // `setpgid(2)`: pid is not the calling process and not a child  of
            // the calling process.
            return Err(Errno::ESRCH.into());
        }
        if let Some(pgid) = pgid {
            if ctx.objs.host.process_session_id_of_group_id(pgid) != Some(process.session_id()) {
                // An attempt was made to move a process into a process group in
                // a different session
                return Err(Errno::EPERM.into());
            }
        }
        if process.session_id() != ctx.objs.process.session_id() {
            // `setpgid(2)`: ... or to change the process  group  ID of one of
            // the children of the calling process and the child was in a
            // different session
            return Err(Errno::EPERM.into());
        }
        if process.session_id() == process.id() {
            // `setpgid(2)`: ... or to change the process group ID of a session leader
            return Err(Errno::EPERM.into());
        }
        // TODO: Keep track of whether a process has performed an `execve`.
        // `setpgid(2): EACCES: An attempt was made to change the process group
        // ID of one of the children of the calling process and the child had
        // already performed an execve(2).
        if let Some(pgid) = pgid {
            if ctx.objs.host.process_session_id_of_group_id(pgid) != Some(process.session_id()) {
                // `setpgid(2)`: An attempt was made to move a process into a
                // process group in a different session
                return Err(Errno::EPERM.into());
            }
            process.set_group_id(pgid);
        } else {
            // `setpgid(2)`: If pgid is zero, then the PGID of the process
            // specified by pid is made the same as its process ID.
            process.set_group_id(process.id());
        }
        Ok(0)
    }

    #[log_syscall(/* rv */ kernel_pid_t, /* pid */ kernel_pid_t)]
    pub fn getsid(
        ctx: &mut SyscallContext,
        pid: kernel_pid_t,
    ) -> Result<kernel_pid_t, SyscallError> {
        if pid == 0 {
            return Ok(ctx.objs.process.session_id().into());
        }
        let Ok(pid) = ProcessId::try_from(pid) else {
            return Err(Errno::EINVAL.into());
        };
        let Some(processrc) = ctx.objs.host.process_borrow(pid) else {
            return Err(Errno::ESRCH.into());
        };
        let process = processrc.borrow(ctx.objs.host.root());
        // No need to check that process is in the same session:
        //
        // `getsid(2)`: A process with process ID pid exists, but it is not in
        // the same session as the calling process, and the implementation
        // considers this an error... **Linux does not return EPERM**.

        Ok(process.session_id().into())
    }

    #[log_syscall(/* rv */ kernel_pid_t)]
    pub fn setsid(ctx: &mut SyscallContext) -> Result<kernel_pid_t, SyscallError> {
        let pid = ctx.objs.process.id();
        if ctx.objs.host.process_session_id_of_group_id(pid).is_some() {
            // `setsid(2)`: The process group ID of any process equals the PID
            // of the calling process.  Thus, in particular, setsid() fails if
            // the calling process is already a process group leader.
            return Err(Errno::EPERM.into());
        }

        // `setsid(2)`: The calling process is the leader of the new session
        // (i.e., its session ID is made the same as its process ID).
        ctx.objs.process.set_session_id(pid);

        // `setsid(2)`: The calling  process  also  becomes  the  process group
        // leader of a new process group in the session (i.e., its process group
        // ID is made the same as its process ID).
        ctx.objs.process.set_group_id(pid);

        Ok(pid.into())
    }
}
