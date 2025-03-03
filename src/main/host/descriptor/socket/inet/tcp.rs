use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Weak};

use atomic_refcell::AtomicRefCell;
use bytes::BytesMut;
use linux_api::errno::Errno;
use linux_api::ioctls::IoctlRequest;
use nix::sys::socket::{AddressFamily, MsgFlags, Shutdown, SockaddrIn};
use shadow_shim_helper_rs::emulated_time::EmulatedTime;
use shadow_shim_helper_rs::simulation_time::SimulationTime;
use shadow_shim_helper_rs::syscall_types::ForeignPtr;

use crate::core::work::task::TaskRef;
use crate::core::worker::Worker;
use crate::cshadow as c;
use crate::host::descriptor::socket::inet;
use crate::host::descriptor::socket::{InetSocket, RecvmsgArgs, RecvmsgReturn, SendmsgArgs};
use crate::host::descriptor::{File, Socket};
use crate::host::descriptor::{
    FileMode, FileState, FileStatus, OpenFile, StateEventSource, StateListenerFilter, SyscallResult,
};
use crate::host::memory_manager::MemoryManager;
use crate::host::network::interface::FifoPacketPriority;
use crate::host::network::namespace::{AssociationHandle, NetworkNamespace};
use crate::host::syscall::io::{write_partial, IoVec, IoVecReader, IoVecWriter};
use crate::host::syscall_types::SyscallError;
use crate::network::packet::{PacketRc, PacketStatus};
use crate::utility::callback_queue::{CallbackQueue, Handle};
use crate::utility::sockaddr::SockaddrStorage;
use crate::utility::{HostTreePointer, ObjectCounter};

pub struct TcpSocket {
    tcp_state: tcp::TcpState<TcpDeps>,
    socket_weak: Weak<AtomicRefCell<Self>>,
    event_source: StateEventSource,
    status: FileStatus,
    file_state: FileState,
    association: Option<AssociationHandle>,
    connect_result_is_pending: bool,
    // should only be used by `OpenFile` to make sure there is only ever one `OpenFile` instance for
    // this file
    has_open_file: bool,
    _counter: ObjectCounter,
}

impl TcpSocket {
    pub fn new(status: FileStatus) -> Arc<AtomicRefCell<Self>> {
        let rv = Arc::new_cyclic(|weak: &Weak<AtomicRefCell<Self>>| {
            let tcp_dependencies = TcpDeps {
                timer_state: Arc::new(AtomicRefCell::new(TcpDepsTimerState {
                    socket: weak.clone(),
                    registered_by: tcp::TimerRegisteredBy::Parent,
                })),
            };

            AtomicRefCell::new(Self {
                tcp_state: tcp::TcpState::new(tcp_dependencies, tcp::TcpConfig::default()),
                socket_weak: weak.clone(),
                event_source: StateEventSource::new(),
                status,
                // the readable/writable file state shouldn't matter here since we run
                // `with_tcp_state` below to update it, but we need ACTIVE set so that epoll works
                file_state: FileState::ACTIVE,
                association: None,
                connect_result_is_pending: false,
                has_open_file: false,
                _counter: ObjectCounter::new("TcpSocket"),
            })
        });

        // run a no-op function on the state, which will force the socket to update its file state
        // to match the tcp state
        CallbackQueue::queue_and_run(|cb_queue| {
            rv.borrow_mut().with_tcp_state(cb_queue, |_state| ())
        });

        rv
    }

    pub fn get_status(&self) -> FileStatus {
        self.status
    }

    pub fn set_status(&mut self, status: FileStatus) {
        self.status = status;
    }

    pub fn mode(&self) -> FileMode {
        FileMode::READ | FileMode::WRITE
    }

    pub fn has_open_file(&self) -> bool {
        self.has_open_file
    }

    pub fn supports_sa_restart(&self) -> bool {
        true
    }

    pub fn set_has_open_file(&mut self, val: bool) {
        self.has_open_file = val;
    }

    /// Update the current tcp state. The tcp state should only ever be updated through this method.
    fn with_tcp_state<T>(
        &mut self,
        cb_queue: &mut CallbackQueue,
        f: impl FnOnce(&mut tcp::TcpState<TcpDeps>) -> T,
    ) -> T {
        let rv = f(&mut self.tcp_state);

        // we may have mutated the tcp state, so update the socket's file state and notify listeners

        // if there are packets to send, notify the host
        if self.tcp_state.wants_to_send() {
            // The upgrade could fail if this was run during a drop, or if some outer code decided
            // to take the `TcpSocket` out of the `Arc` for some reason. Might as well panic since
            // it might indicate a bug somewhere else.
            let socket = self.socket_weak.upgrade().unwrap();

            // First try getting our IP address from the tcp state (if it's connected), then try
            // from the association handle (if it's not connected but is bound). Assume that our IP
            // address will match an interface's IP address.
            let interface_ip = *self
                .tcp_state
                .local_remote_addrs()
                .map(|x| x.0)
                .or(self.association.as_ref().map(|x| x.local_addr()))
                .unwrap()
                .ip();

            cb_queue.add(move |_cb_queue| {
                Worker::with_active_host(|host| {
                    let inet_socket = InetSocket::Tcp(socket);
                    let compat_socket = unsafe { c::compatsocket_fromInetSocket(&inet_socket) };
                    host.notify_socket_has_packets(interface_ip, &compat_socket);
                })
                .unwrap();
            });
        }

        let mut read_write_flags = FileState::empty();
        let poll_state = self.tcp_state.poll();

        if poll_state.intersects(tcp::PollState::READABLE | tcp::PollState::RECV_CLOSED) {
            read_write_flags.insert(FileState::READABLE);
        }
        if poll_state.intersects(tcp::PollState::WRITABLE) {
            read_write_flags.insert(FileState::WRITABLE);
        }
        if poll_state.intersects(tcp::PollState::READY_TO_ACCEPT) {
            read_write_flags.insert(FileState::READABLE);
        }
        if poll_state.intersects(tcp::PollState::ERROR) {
            read_write_flags.insert(FileState::READABLE | FileState::WRITABLE);
        }

        // if the socket is closed, undo all of the flags set above (closed sockets aren't readable
        // or writable)
        if self.file_state.contains(FileState::CLOSED) {
            read_write_flags = FileState::empty();
        }

        // overwrite readable/writable flags
        self.copy_state(
            FileState::READABLE | FileState::WRITABLE,
            read_write_flags,
            cb_queue,
        );

        // if the tcp state is in the closed state
        if poll_state.contains(tcp::PollState::CLOSED) {
            // drop the association handle so that we're removed from the network interface
            self.association = None;
            // we do not change to `FileState::CLOSED` here since that flag represents that the file
            // has closed (with `close()`), not that the tcp state has closed
        }

        rv
    }

    pub fn push_in_packet(
        &mut self,
        mut packet: PacketRc,
        cb_queue: &mut CallbackQueue,
        _recv_time: EmulatedTime,
    ) {
        packet.add_status(PacketStatus::RcvSocketProcessed);

        // TODO: don't bother copying the bytes if we know the push will fail

        // TODO: we have no way of adding `PacketStatus::RcvSocketDropped` if the tcp state drops
        // the packet

        let header = packet
            .get_tcp()
            .expect("TCP socket received a non-tcp packet");

        // in the future, the packet could contain an array of `Bytes` objects and we could simply
        // transfer the `Bytes` objects directly from the payload to the tcp state without copying
        // the bytes themselves

        let mut payload = BytesMut::zeroed(packet.payload_size());
        let num_bytes_copied = packet.get_payload(&mut payload);
        assert_eq!(num_bytes_copied, packet.payload_size());
        let payload = tcp::Payload(vec![payload.freeze()]);

        self.with_tcp_state(cb_queue, |s| s.push_packet(&header, payload))
            .unwrap();

        packet.add_status(PacketStatus::RcvSocketBuffered);
    }

    pub fn pull_out_packet(&mut self, cb_queue: &mut CallbackQueue) -> Option<PacketRc> {
        #[cfg(debug_assertions)]
        let wants_to_send = self.tcp_state.wants_to_send();

        // make sure that `self.has_data_to_send()` agrees with `tcp_state.wants_to_send()`
        #[cfg(debug_assertions)]
        debug_assert_eq!(self.has_data_to_send(), wants_to_send);

        // pop a packet from the socket
        let rv = self.with_tcp_state(cb_queue, |s| s.pop_packet());

        let (header, payload) = match rv {
            Ok(x) => x,
            Err(tcp::PopPacketError::NoPacket) => {
                #[cfg(debug_assertions)]
                debug_assert!(!wants_to_send);
                return None;
            }
            Err(tcp::PopPacketError::InvalidState) => {
                #[cfg(debug_assertions)]
                debug_assert!(!wants_to_send);
                return None;
            }
        };

        #[cfg(debug_assertions)]
        debug_assert!(wants_to_send);

        let mut packet = PacketRc::new();

        // TODO: This is expensive. Here we allocate a new buffer, copy all of the payload bytes to
        // this new buffer, and then copy the bytes in this new buffer to the packet's buffer. In
        // the future, the packet could contain an array of `Bytes` objects and we could simply
        // transfer the `Bytes` objects directly from the tcp state's `Payload` object to the packet
        // without copying the bytes themselves.
        let payload = payload.concat();

        packet.set_tcp(&header);
        // TODO: set packet priority?
        packet.set_payload(&payload, /* priority= */ 0);
        packet.add_status(PacketStatus::SndCreated);

        Some(packet)
    }

    pub fn peek_next_packet_priority(&self) -> Option<FifoPacketPriority> {
        // TODO: support packet priorities?
        self.has_data_to_send().then_some(0)
    }

    pub fn has_data_to_send(&self) -> bool {
        self.tcp_state.wants_to_send()
    }

    pub fn getsockname(&self) -> Result<Option<SockaddrIn>, SyscallError> {
        Ok(self.association.as_ref().map(|x| x.local_addr().into()))
    }

    pub fn getpeername(&self) -> Result<Option<SockaddrIn>, SyscallError> {
        Ok(self.association.as_ref().map(|x| x.remote_addr().into()))
    }

    pub fn address_family(&self) -> AddressFamily {
        AddressFamily::Inet
    }

    pub fn close(&mut self, cb_queue: &mut CallbackQueue) -> Result<(), SyscallError> {
        // we don't expect close() to ever have an error
        self.with_tcp_state(cb_queue, |state| state.close())
            .unwrap();

        // add the closed flag and remove all other flags
        self.copy_state(FileState::all(), FileState::CLOSED, cb_queue);

        Ok(())
    }

    pub fn bind(
        socket: &Arc<AtomicRefCell<Self>>,
        addr: Option<&SockaddrStorage>,
        net_ns: &NetworkNamespace,
        rng: impl rand::Rng,
    ) -> SyscallResult {
        // if the address pointer was NULL
        let Some(addr) = addr else {
            return Err(Errno::EFAULT.into());
        };

        // if not an inet socket address
        let Some(addr) = addr.as_inet() else {
            return Err(Errno::EINVAL.into());
        };

        let addr: SocketAddrV4 = (*addr).into();

        let mut socket_ref = socket.borrow_mut();

        // if the socket is already associated
        if socket_ref.association.is_some() {
            return Err(Errno::EINVAL.into());
        }

        // this will allow us to receive packets from any peer
        let peer_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);

        // associate the socket
        let (_addr, handle) = inet::associate_socket(
            InetSocket::Tcp(Arc::clone(socket)),
            addr,
            peer_addr,
            /* check_generic_peer= */ true,
            net_ns,
            rng,
        )?;

        socket_ref.association = Some(handle);

        Ok(0.into())
    }

    pub fn readv(
        &mut self,
        _iovs: &[IoVec],
        _offset: Option<libc::off_t>,
        _flags: libc::c_int,
        _mem: &mut MemoryManager,
        _cb_queue: &mut CallbackQueue,
    ) -> Result<libc::ssize_t, SyscallError> {
        // we could call TcpSocket::recvmsg() here, but for now we expect that there are no code
        // paths that would call TcpSocket::readv() since the readv() syscall handler should have
        // called TcpSocket::recvmsg() instead
        panic!("Called TcpSocket::readv() on a TCP socket");
    }

    pub fn writev(
        &mut self,
        _iovs: &[IoVec],
        _offset: Option<libc::off_t>,
        _flags: libc::c_int,
        _mem: &mut MemoryManager,
        _cb_queue: &mut CallbackQueue,
    ) -> Result<libc::ssize_t, SyscallError> {
        // we could call TcpSocket::sendmsg() here, but for now we expect that there are no code
        // paths that would call TcpSocket::writev() since the writev() syscall handler should have
        // called TcpSocket::sendmsg() instead
        panic!("Called TcpSocket::writev() on a TCP socket");
    }

    pub fn sendmsg(
        socket: &Arc<AtomicRefCell<Self>>,
        args: SendmsgArgs,
        mem: &mut MemoryManager,
        _net_ns: &NetworkNamespace,
        _rng: impl rand::Rng,
        cb_queue: &mut CallbackQueue,
    ) -> Result<libc::ssize_t, SyscallError> {
        let mut socket_ref = socket.borrow_mut();

        let Some(mut flags) = MsgFlags::from_bits(args.flags) else {
            log::debug!("Unrecognized send flags: {:#b}", args.flags);
            return Err(Errno::EINVAL.into());
        };

        if socket_ref.get_status().contains(FileStatus::NONBLOCK) {
            flags.insert(MsgFlags::MSG_DONTWAIT);
        }

        let len: libc::size_t = args.iovs.iter().map(|x| x.len).sum();

        // run in a closure so that an early return doesn't skip checking if we should block
        let result = (|| {
            let reader = IoVecReader::new(args.iovs, mem);

            let rv = socket_ref.with_tcp_state(cb_queue, |state| state.send(reader, len));

            let num_sent = match rv {
                Ok(x) => x,
                Err(tcp::SendError::Full) => return Err(Errno::EWOULDBLOCK),
                Err(tcp::SendError::NotConnected) => return Err(Errno::EPIPE),
                Err(tcp::SendError::StreamClosed) => return Err(Errno::EPIPE),
                Err(tcp::SendError::Io(e)) => return Err(Errno::try_from(e).unwrap()),
                Err(tcp::SendError::InvalidState) => return Err(Errno::EINVAL),
            };

            Ok(num_sent)
        })();

        // if the syscall would block and we don't have the MSG_DONTWAIT flag
        if result == Err(Errno::EWOULDBLOCK) && !flags.contains(MsgFlags::MSG_DONTWAIT) {
            return Err(SyscallError::new_blocked_on_file(
                File::Socket(Socket::Inet(InetSocket::Tcp(socket.clone()))),
                FileState::WRITABLE | FileState::CLOSED,
                socket_ref.supports_sa_restart(),
            ));
        }

        Ok(result?.try_into().unwrap())
    }

    pub fn recvmsg(
        socket: &Arc<AtomicRefCell<Self>>,
        args: RecvmsgArgs,
        mem: &mut MemoryManager,
        cb_queue: &mut CallbackQueue,
    ) -> Result<RecvmsgReturn, SyscallError> {
        let socket_ref = &mut *socket.borrow_mut();

        let Some(mut flags) = MsgFlags::from_bits(args.flags) else {
            log::debug!("Unrecognized recv flags: {:#b}", args.flags);
            return Err(Errno::EINVAL.into());
        };

        if socket_ref.get_status().contains(FileStatus::NONBLOCK) {
            flags.insert(MsgFlags::MSG_DONTWAIT);
        }

        let len: libc::size_t = args.iovs.iter().map(|x| x.len).sum();

        // run in a closure so that an early return doesn't skip checking if we should block
        let result = (|| {
            let writer = IoVecWriter::new(args.iovs, mem);

            let rv = socket_ref.with_tcp_state(cb_queue, |state| state.recv(writer, len));

            let num_recv = match rv {
                Ok(x) => x,
                Err(tcp::RecvError::Empty) => return Err(Errno::EWOULDBLOCK),
                Err(tcp::RecvError::NotConnected) => return Err(Errno::ENOTCONN),
                Err(tcp::RecvError::StreamClosed) => 0,
                Err(tcp::RecvError::Io(e)) => return Err(Errno::try_from(e).unwrap()),
                Err(tcp::RecvError::InvalidState) => return Err(Errno::EINVAL),
            };

            Ok(RecvmsgReturn {
                return_val: num_recv.try_into().unwrap(),
                addr: None,
                msg_flags: MsgFlags::empty().bits(),
                control_len: 0,
            })
        })();

        // if the syscall would block and we don't have the MSG_DONTWAIT flag
        if result.as_ref().err() == Some(&Errno::EWOULDBLOCK)
            && !flags.contains(MsgFlags::MSG_DONTWAIT)
        {
            return Err(SyscallError::new_blocked_on_file(
                File::Socket(Socket::Inet(InetSocket::Tcp(socket.clone()))),
                FileState::READABLE | FileState::CLOSED,
                socket_ref.supports_sa_restart(),
            ));
        }

        Ok(result?)
    }

    pub fn ioctl(
        &mut self,
        _request: IoctlRequest,
        _arg_ptr: ForeignPtr<()>,
        _mem: &mut MemoryManager,
    ) -> SyscallResult {
        todo!();
    }

    pub fn listen(
        socket: &Arc<AtomicRefCell<Self>>,
        backlog: i32,
        net_ns: &NetworkNamespace,
        rng: impl rand::Rng,
        cb_queue: &mut CallbackQueue,
    ) -> Result<(), SyscallError> {
        let socket_ref = &mut *socket.borrow_mut();

        // linux also makes this cast, so negative backlogs wrap around to large positive backlogs
        // https://elixir.free-electrons.com/linux/v5.11.22/source/net/ipv4/af_inet.c#L212
        let backlog = backlog as u32;

        let is_associated = socket_ref.association.is_some();

        let rv = if is_associated {
            // if already associated, do nothing
            let associate_fn = || Ok(None);
            socket_ref.with_tcp_state(cb_queue, |state| state.listen(backlog, associate_fn))
        } else {
            // if not associated, associate and return the handle
            let associate_fn = || {
                // implicitly bind to all interfaces
                let local_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);

                // want to receive packets from any address
                let peer_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
                let socket = Arc::clone(socket);

                // associate the socket
                let (_addr, handle) = inet::associate_socket(
                    InetSocket::Tcp(Arc::clone(&socket)),
                    local_addr,
                    peer_addr,
                    /* check_generic_peer= */ true,
                    net_ns,
                    rng,
                )?;

                Ok::<_, SyscallError>(Some(handle))
            };
            socket_ref.with_tcp_state(cb_queue, |state| state.listen(backlog, associate_fn))
        };

        let handle = match rv {
            Ok(x) => x,
            Err(tcp::ListenError::InvalidState) => return Err(Errno::EINVAL.into()),
            Err(tcp::ListenError::FailedAssociation(e)) => return Err(e),
        };

        // the `associate_fn` may or may not have run, so `handle` may or may not be set
        if let Some(handle) = handle {
            assert!(socket_ref.association.is_none());
            socket_ref.association = Some(handle);
        }

        Ok(())
    }

    pub fn connect(
        socket: &Arc<AtomicRefCell<Self>>,
        peer_addr: &SockaddrStorage,
        net_ns: &NetworkNamespace,
        rng: impl rand::Rng,
        cb_queue: &mut CallbackQueue,
    ) -> Result<(), SyscallError> {
        let socket_ref = &mut *socket.borrow_mut();

        // if there was an asynchronous error, return it
        if let Some(error) = socket_ref.with_tcp_state(cb_queue, |state| state.clear_error()) {
            // by returning this error, we're probably (but not necessarily) returning a previous
            // connect() result
            socket_ref.connect_result_is_pending = false;

            return Err(match error {
                // TODO: not sure what to return here
                tcp::TcpError::ResetSent => Errno::EINVAL,
                tcp::TcpError::ResetReceived => Errno::ECONNREFUSED,
                tcp::TcpError::TimedOut => Errno::ETIMEDOUT,
            }
            .into());
        }

        // if connect() had previously been called (either blocking or non-blocking), we need to
        // return the result
        if socket_ref.connect_result_is_pending {
            // ignore all connect arguments and just check if we've connected

            // if the ESTABLISHED flag isn't set, then it's still connecting
            if !socket_ref
                .tcp_state
                .poll()
                .contains(tcp::PollState::ESTABLISHED)
            {
                return Err(Errno::EALREADY.into());
            }

            // the ESTABLISHED flag is set and there were no socket errors (checked above)
            socket_ref.connect_result_is_pending = false;
            return Ok(());
        }

        // if not an inet socket address
        let Some(peer_addr) = peer_addr.as_inet() else {
            return Err(Errno::EINVAL.into());
        };

        let mut peer_addr: std::net::SocketAddrV4 = (*peer_addr).into();

        // On Linux a connection to 0.0.0.0 means a connection to localhost:
        // https://stackoverflow.com/a/22425796
        if peer_addr.ip().is_unspecified() {
            peer_addr.set_ip(std::net::Ipv4Addr::LOCALHOST);
        }

        let local_addr = socket_ref.association.as_ref().map(|x| x.local_addr());

        let rv = if let Some(mut local_addr) = local_addr {
            // the local address needs to be a specific address (this is normally what a routing
            // table would figure out for us)
            if local_addr.ip().is_unspecified() {
                if peer_addr.ip() == &std::net::Ipv4Addr::LOCALHOST {
                    local_addr.set_ip(Ipv4Addr::LOCALHOST)
                } else {
                    local_addr.set_ip(net_ns.default_ip)
                };
            }

            // it's already associated so use the existing address
            let associate_fn = || Ok((local_addr, None));
            socket_ref.with_tcp_state(cb_queue, |state| state.connect(peer_addr, associate_fn))
        } else {
            // if not associated, associate and return the handle
            let associate_fn = || {
                // the local address needs to be a specific address (this is normally what a routing
                // table would figure out for us)
                let local_addr = if peer_addr.ip() == &std::net::Ipv4Addr::LOCALHOST {
                    Ipv4Addr::LOCALHOST
                } else {
                    net_ns.default_ip
                };

                // add a wildcard port number
                let local_addr = SocketAddrV4::new(local_addr, 0);

                let (local_addr, handle) = inet::associate_socket(
                    InetSocket::Tcp(Arc::clone(socket)),
                    local_addr,
                    peer_addr,
                    /* check_generic_peer= */ true,
                    net_ns,
                    rng,
                )?;

                // use the actual local address that was assigned (will have port != 0)
                Ok((local_addr, Some(handle)))
            };
            socket_ref.with_tcp_state(cb_queue, |state| state.connect(peer_addr, associate_fn))
        };

        let handle = match rv {
            Ok(x) => x,
            Err(tcp::ConnectError::InProgress) => return Err(Errno::EALREADY.into()),
            Err(tcp::ConnectError::AlreadyConnected) => return Err(Errno::EISCONN.into()),
            Err(tcp::ConnectError::InvalidState) => return Err(Errno::EINVAL.into()),
            Err(tcp::ConnectError::FailedAssociation(e)) => return Err(e),
        };

        // the `associate_fn` may not have associated the socket, so `handle` may or may not be set
        if let Some(handle) = handle {
            assert!(socket_ref.association.is_none());
            socket_ref.association = Some(handle);
        }

        // we're attempting to connect, so set a flag so that we know a future connect() call should
        // return the result
        socket_ref.connect_result_is_pending = true;

        if socket_ref.status.contains(FileStatus::NONBLOCK) {
            Err(Errno::EINPROGRESS.into())
        } else {
            let err = SyscallError::new_blocked_on_file(
                File::Socket(Socket::Inet(InetSocket::Tcp(Arc::clone(socket)))),
                FileState::WRITABLE | FileState::CLOSED,
                socket_ref.supports_sa_restart(),
            );

            // block the current thread
            Err(err)
        }
    }

    pub fn accept(
        &mut self,
        net_ns: &NetworkNamespace,
        rng: impl rand::Rng,
        cb_queue: &mut CallbackQueue,
    ) -> Result<OpenFile, SyscallError> {
        let rv = self.with_tcp_state(cb_queue, |state| state.accept());

        let accepted_state = match rv {
            Ok(x) => x,
            Err(tcp::AcceptError::InvalidState) => return Err(Errno::EINVAL.into()),
            Err(tcp::AcceptError::NothingToAccept) => return Err(Errno::EAGAIN.into()),
        };

        let local_addr = accepted_state.local_addr();
        let remote_addr = accepted_state.remote_addr();

        // convert the accepted tcp state to a full tcp socket
        let new_socket = Arc::new_cyclic(|weak: &Weak<AtomicRefCell<Self>>| {
            let accepted_state = accepted_state.finalize(|deps| {
                // update the timer state for new and existing pending timers to use the new
                // accepted socket rather than the parent listening socket
                let timer_state = &mut *deps.timer_state.borrow_mut();
                timer_state.socket = weak.clone();
                timer_state.registered_by = tcp::TimerRegisteredBy::Parent;
            });

            AtomicRefCell::new(Self {
                tcp_state: accepted_state,
                socket_weak: weak.clone(),
                event_source: StateEventSource::new(),
                status: FileStatus::empty(),
                // the readable/writable file state shouldn't matter here since we run
                // `with_tcp_state` below to update it, but we need ACTIVE set so that epoll works
                file_state: FileState::ACTIVE,
                association: None,
                connect_result_is_pending: false,
                has_open_file: false,
                _counter: ObjectCounter::new("TcpSocket"),
            })
        });

        // run a no-op function on the state, which will force the socket to update its file state
        // to match the tcp state
        new_socket
            .borrow_mut()
            .with_tcp_state(cb_queue, |_state| ());

        // TODO: if the association fails, we lose the child socket

        // associate the socket
        let (_addr, handle) = inet::associate_socket(
            InetSocket::Tcp(Arc::clone(&new_socket)),
            local_addr,
            remote_addr,
            /* check_generic_peer= */ false,
            net_ns,
            rng,
        )?;

        new_socket.borrow_mut().association = Some(handle);

        Ok(OpenFile::new(File::Socket(Socket::Inet(InetSocket::Tcp(
            new_socket,
        )))))
    }

    pub fn shutdown(
        &mut self,
        _how: Shutdown,
        _cb_queue: &mut CallbackQueue,
    ) -> Result<(), SyscallError> {
        todo!();
    }

    pub fn getsockopt(
        &self,
        level: libc::c_int,
        optname: libc::c_int,
        optval_ptr: ForeignPtr<()>,
        optlen: libc::socklen_t,
        mem: &mut MemoryManager,
    ) -> Result<libc::socklen_t, SyscallError> {
        match (level, optname) {
            (libc::SOL_SOCKET, libc::SO_DOMAIN) => {
                let domain = libc::AF_INET;

                let optval_ptr = optval_ptr.cast::<libc::c_int>();
                let bytes_written = write_partial(mem, &domain, optval_ptr, optlen as usize)?;

                Ok(bytes_written as libc::socklen_t)
            }
            (libc::SOL_SOCKET, libc::SO_TYPE) => {
                let sock_type = libc::SOCK_STREAM;

                let optval_ptr = optval_ptr.cast::<libc::c_int>();
                let bytes_written = write_partial(mem, &sock_type, optval_ptr, optlen as usize)?;

                Ok(bytes_written as libc::socklen_t)
            }
            (libc::SOL_SOCKET, libc::SO_PROTOCOL) => {
                let protocol = libc::IPPROTO_TCP;

                let optval_ptr = optval_ptr.cast::<libc::c_int>();
                let bytes_written = write_partial(mem, &protocol, optval_ptr, optlen as usize)?;

                Ok(bytes_written as libc::socklen_t)
            }
            _ => {
                log::warn!("getsockopt called with unsupported level {level} and opt {optname}");
                Err(Errno::ENOPROTOOPT.into())
            }
        }
    }

    pub fn setsockopt(
        &mut self,
        level: libc::c_int,
        optname: libc::c_int,
        _optval_ptr: ForeignPtr<()>,
        _optlen: libc::socklen_t,
        _mem: &MemoryManager,
    ) -> Result<(), SyscallError> {
        match (level, optname) {
            (libc::SOL_SOCKET, libc::SO_REUSEADDR) => {
                // TODO: implement this, tor and tgen use it
                log::trace!("setsockopt SO_REUSEADDR not yet implemented");
            }
            (libc::SOL_SOCKET, libc::SO_REUSEPORT) => {
                // TODO: implement this, tgen uses it
                log::trace!("setsockopt SO_REUSEPORT not yet implemented");
            }
            (libc::SOL_SOCKET, libc::SO_KEEPALIVE) => {
                // TODO: implement this, libevent uses it in evconnlistener_new_bind()
                log::trace!("setsockopt SO_KEEPALIVE not yet implemented");
            }
            (libc::SOL_SOCKET, libc::SO_BROADCAST) => {
                // TODO: implement this, pkg.go.dev/net uses it
                log::trace!("setsockopt SO_BROADCAST not yet implemented");
            }
            _ => {
                log::warn!("setsockopt called with unsupported level {level} and opt {optname}");
                return Err(Errno::ENOPROTOOPT.into());
            }
        }

        Ok(())
    }

    pub fn add_listener(
        &mut self,
        monitoring: FileState,
        filter: StateListenerFilter,
        notify_fn: impl Fn(FileState, FileState, &mut CallbackQueue) + Send + Sync + 'static,
    ) -> Handle<(FileState, FileState)> {
        self.event_source
            .add_listener(monitoring, filter, notify_fn)
    }

    pub fn add_legacy_listener(&mut self, ptr: HostTreePointer<c::StatusListener>) {
        self.event_source.add_legacy_listener(ptr);
    }

    pub fn remove_legacy_listener(&mut self, ptr: *mut c::StatusListener) {
        self.event_source.remove_legacy_listener(ptr);
    }

    pub fn state(&self) -> FileState {
        self.file_state
    }

    fn copy_state(&mut self, mask: FileState, state: FileState, cb_queue: &mut CallbackQueue) {
        let old_state = self.file_state;

        // remove the masked flags, then copy the masked flags
        self.file_state.remove(mask);
        self.file_state.insert(state & mask);

        self.handle_state_change(old_state, cb_queue);
    }

    fn handle_state_change(&mut self, old_state: FileState, cb_queue: &mut CallbackQueue) {
        let states_changed = self.file_state ^ old_state;

        // if nothing changed
        if states_changed.is_empty() {
            return;
        }

        self.event_source
            .notify_listeners(self.file_state, states_changed, cb_queue);
    }
}

/// Shared state stored in timers. This allows us to update existing timers when a child `TcpState`
/// is accept()ed and becomes owned by a new `TcpSocket` object.
#[derive(Debug)]
struct TcpDepsTimerState {
    /// The socket that the timer callback will run on.
    socket: Weak<AtomicRefCell<TcpSocket>>,
    /// Whether the timer callback should modify the state of this socket
    /// ([`TimerRegisteredBy::Parent`]), or one of its child sockets ([`TimerRegisteredBy::Child`]).
    registered_by: tcp::TimerRegisteredBy,
}

/// The dependencies required by `TcpState::new()` so that the tcp code can interact with the
/// simulator.
#[derive(Debug)]
struct TcpDeps {
    /// State shared between all timers registered from this `TestEnvState`. This is needed since we
    /// may need to update existing pending timers when we accept() a `TcpState` from a listening
    /// state.
    timer_state: Arc<AtomicRefCell<TcpDepsTimerState>>,
}

impl tcp::Dependencies for TcpDeps {
    type Instant = EmulatedTime;
    type Duration = SimulationTime;

    fn register_timer(
        &self,
        time: Self::Instant,
        f: impl FnOnce(&mut tcp::TcpState<Self>, tcp::TimerRegisteredBy) + Send + Sync + 'static,
    ) {
        // make sure the socket is kept alive in the closure while the timer is waiting to be run
        // (don't store a weak reference), otherwise the socket may have already been dropped and
        // the timer won't run
        // TODO: is this the behaviour we want?
        let timer_state = self.timer_state.borrow();
        let socket = timer_state.socket.upgrade().unwrap();
        let registered_by = timer_state.registered_by;

        // This is needed because `TaskRef` takes a `Fn`, but we have a `FnOnce`. It would be nice
        // if we could schedule a task that is guaranteed to run only once so we could avoid this
        // extra allocation and atomic. Instead we'll panic if it does run more than once.
        let f = Arc::new(AtomicRefCell::new(Some(f)));

        // schedule a task with the host
        Worker::with_active_host(|host| {
            let task = TaskRef::new(move |_host| {
                // take ownership of the task; will panic if the task is run more than once
                let f = f.borrow_mut().take().unwrap();

                // run the original closure on the tcp state
                CallbackQueue::queue_and_run(|cb_queue| {
                    socket.borrow_mut().with_tcp_state(cb_queue, |state| {
                        f(state, registered_by);
                    })
                });
            });

            host.schedule_task_at_emulated_time(task, time);
        })
        .unwrap();
    }

    fn current_time(&self) -> Self::Instant {
        Worker::current_time().unwrap()
    }

    fn fork(&self) -> Self {
        let timer_state = self.timer_state.borrow();

        // if a child is trying to fork(), something has gone wrong
        assert_eq!(timer_state.registered_by, tcp::TimerRegisteredBy::Parent);

        Self {
            timer_state: Arc::new(AtomicRefCell::new(TcpDepsTimerState {
                socket: timer_state.socket.clone(),
                registered_by: tcp::TimerRegisteredBy::Child,
            })),
        }
    }
}
