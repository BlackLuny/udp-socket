use crate::proto::{RecvMeta, SocketType, Transmit, UdpCapabilities};
use futures_lite::future::poll_fn;
use socket2::Socket;
use std::io::{IoSliceMut, Result};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::io::Interest;
use tokio::net::UdpSocket as TokioUdpSocket;

#[cfg(unix)]
use crate::unix as platform;
#[cfg(not(unix))]
use fallback as platform;

#[derive(Debug)]
pub struct UdpSocket {
    inner: TokioUdpSocket,
    ty: SocketType,
    /// Linux `UDP_GRO` has been enabled at some point. Sticky: coalesced
    /// datagrams queued before GRO is disabled stay coalesced, and
    /// `try_read_datagram` cannot split them.
    gro_seen: AtomicBool,
}

impl Deref for UdpSocket {
    type Target = TokioUdpSocket;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for UdpSocket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl UdpSocket {
    pub fn capabilities() -> Result<UdpCapabilities> {
        Ok(UdpCapabilities {
            max_gso_segments: platform::max_gso_segments()?,
        })
    }

    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        let ty = platform::init(&socket)?;
        Self::from_initialized(socket, ty)
    }

    pub fn from_socket(socket: Socket) -> Result<Self> {
        let socket = std::net::UdpSocket::from(socket);
        let ty = platform::init(&socket)?;
        Self::from_initialized(socket, ty)
    }

    /// Like `from_socket`, with an explicit PMTU policy (Linux/Android only;
    /// ignored elsewhere).
    #[cfg(unix)]
    pub fn from_socket_with_pmtu(socket: Socket, policy: crate::PmtuPolicy) -> Result<Self> {
        let socket = std::net::UdpSocket::from(socket);
        let ty = platform::init_with_pmtu(&socket, policy)?;
        Self::from_initialized(socket, ty)
    }

    fn from_initialized(socket: std::net::UdpSocket, ty: SocketType) -> Result<Self> {
        // Tokio requires non-blocking mode; a blocking socket stalls the
        // runtime thread on a stale readiness event.
        socket.set_nonblocking(true)?;
        // A caller may have enabled UDP_GRO before handing the socket over.
        #[cfg(unix)]
        let gro_seen = platform::gro_enabled(socket.as_raw_fd());
        #[cfg(not(unix))]
        let gro_seen = false;
        let inner = TokioUdpSocket::from_std(socket)?;
        Ok(Self {
            inner,
            ty,
            gro_seen: AtomicBool::new(gro_seen),
        })
    }

    pub fn socket_type(&self) -> SocketType {
        self.ty
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Enable / disable Linux `UDP_GRO`. When enabled, the kernel may coalesce
    /// multiple datagrams from the same flow into a single recv. Each
    /// resulting `RecvMeta::stride` reports the original segment size so the
    /// caller can split the buffer back into per-datagram views.
    ///
    /// On non-Linux platforms this is a no-op and returns `Ok(())`.
    /// Returns the underlying `setsockopt` error on Linux when the kernel
    /// does not support `UDP_GRO` (kernels older than 5.0).
    ///
    /// Choose GRO before reading. Disabling it does not split receives already
    /// coalesced in the queue, and the kernel then stops reporting their
    /// `stride`; drain or recreate the socket instead of toggling. Once enabled,
    /// `try_read_datagram` stays refused for the socket's lifetime; calling
    /// this concurrently with `try_read_datagram` is unsupported.
    pub fn set_gro(&self, enabled: bool) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Publish before the kernel can coalesce anything.
            let was_seen = enabled && self.gro_seen.swap(true, Ordering::SeqCst);
            let on: libc::c_int = if enabled { 1 } else { 0 };
            let rc = unsafe {
                libc::setsockopt(
                    self.inner.as_raw_fd(),
                    libc::SOL_UDP,
                    libc::UDP_GRO,
                    &on as *const _ as _,
                    std::mem::size_of_val(&on) as _,
                )
            };
            if rc == -1 {
                let err = std::io::Error::last_os_error();
                if enabled && !was_seen {
                    // GRO was never on; keep the lightweight path available.
                    self.gro_seen.store(false, Ordering::SeqCst);
                }
                return Err(err);
            }
        }
        let _ = enabled; // silence unused on non-linux
        Ok(())
    }

    pub fn ttl(&self) -> Result<u8> {
        let ttl = self.inner.ttl()?;
        Ok(ttl as u8)
    }

    pub fn set_ttl(&self, ttl: u8) -> Result<()> {
        self.inner.set_ttl(ttl as u32)
    }

    pub fn poll_send(&self, cx: &mut Context, transmits: &[Transmit<'_>]) -> Poll<Result<usize>> {
        loop {
            match self.inner.poll_send_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            }
            match self.inner.try_io(Interest::WRITABLE, || {
                platform::send(self.inner.as_raw_fd(), transmits)
            }) {
                Ok(count) => return Poll::Ready(Ok(count)),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(err));
                }
            }
        }
    }
    /// Truncated datagrams are reported with `RecvMeta::truncated` set rather
    /// than skipped; callers must check the flag.
    pub fn try_read_mmsg(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize> {
        self.inner.try_io(Interest::READABLE, || {
            platform::recv(self.inner.as_raw_fd(), buffers, meta)
        })
    }

    /// Receive a datagram with truncation detection, without ancillary metadata.
    /// Use `try_read_one` for ECN, destination IP or GRO.
    ///
    /// Fails with `Unsupported`, without consuming a datagram, once UDP_GRO has
    /// been enabled on this socket (see `set_gro`): a coalesced read would
    /// otherwise look like one datagram.
    #[cfg(unix)]
    pub fn try_read_datagram(&self, buffer: &mut [u8]) -> Result<crate::DatagramMeta> {
        if self.gro_seen.load(Ordering::SeqCst) {
            return Err(gro_unsupported());
        }
        Self::try_read_datagram_from(&self.inner, buffer)
    }

    /// Same contract as `try_read_datagram`, for an existing Tokio socket.
    /// Does not change socket options. The caller must guarantee UDP_GRO has
    /// never been enabled on the socket: datagrams coalesced while it was on
    /// stay queued after disabling it. Debug builds check only the current
    /// option, with an extra `getsockopt` per call.
    #[cfg(unix)]
    pub fn try_read_datagram_from(
        socket: &TokioUdpSocket,
        buffer: &mut [u8],
    ) -> Result<crate::DatagramMeta> {
        if cfg!(debug_assertions) && platform::gro_enabled(socket.as_raw_fd()) {
            return Err(gro_unsupported());
        }
        socket.try_io(Interest::READABLE, || {
            platform::recv_datagram(socket.as_raw_fd(), buffer)
        })
    }

    /// Receive one datagram without losing the kernel's truncation flag.
    #[cfg(unix)]
    pub fn try_read_one(&self, buffer: &mut [u8]) -> Result<RecvMeta> {
        Self::try_read_one_from(&self.inner, buffer)
    }

    /// Read flags from an existing Tokio socket without changing its options.
    #[cfg(unix)]
    pub fn try_read_one_from(socket: &TokioUdpSocket, buffer: &mut [u8]) -> Result<RecvMeta> {
        let mut meta = [RecvMeta::default()];
        socket.try_io(Interest::READABLE, || {
            platform::recv_one(
                socket.as_raw_fd(),
                &mut [IoSliceMut::new(buffer)],
                &mut meta,
            )
        })?;
        Ok(meta[0])
    }

    #[cfg(unix)]
    pub async fn recv_one(&self, buffer: &mut [u8]) -> Result<RecvMeta> {
        loop {
            self.inner.readable().await?;
            match self.try_read_one(buffer) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                result => return result,
            }
        }
    }
    pub fn try_write_mmsg(&self, transmits: &[Transmit<'_>]) -> Result<usize> {
        self.inner.try_io(Interest::WRITABLE, || {
            platform::send(self.inner.as_raw_fd(), transmits)
        })
    }
    pub fn poll_recv(
        &self,
        cx: &mut Context,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<Result<usize>> {
        loop {
            match self.inner.poll_recv_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            }
            match self.inner.try_io(Interest::READABLE, || {
                platform::recv(self.inner.as_raw_fd(), buffers, meta)
            }) {
                Ok(count) => return Poll::Ready(Ok(count)),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Poll::Ready(Err(err));
                }
            }
        }
    }

    pub async fn send(&self, transmits: &[Transmit<'_>]) -> Result<usize> {
        let mut i = 0;
        while i < transmits.len() {
            i += poll_fn(|cx| self.poll_send(cx, &transmits[i..])).await?;
        }
        Ok(i)
    }

    /// See `try_read_mmsg`: truncated datagrams are reported, not skipped.
    pub async fn recv(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize> {
        poll_fn(|cx| self.poll_recv(cx, buffers, meta)).await
    }
}

#[cfg(unix)]
fn gro_unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "try_read_datagram cannot split UDP_GRO reads; use try_read_one",
    )
}

#[cfg(not(unix))]
mod fallback {
    use super::*;

    pub fn max_gso_segments() -> Result<usize> {
        Ok(1)
    }

    pub fn init(socket: &std::net::UdpSocket) -> Result<SocketType> {
        Ok(if socket.local_addr()?.is_ipv4() {
            SocketType::Ipv4
        } else {
            SocketType::Ipv6Only
        })
    }

    pub fn send(socket: &TokioUdpSocket, transmits: &[Transmit<'_>]) -> Result<usize> {
        let mut sent = 0;
        for transmit in transmits {
            match socket.send_to(&transmit.contents, &transmit.destination) {
                Ok(_) => {
                    sent += 1;
                }
                Err(_) if sent != 0 => {
                    // We need to report that some packets were sent in this case, so we rely on
                    // errors being either harmlessly transient (in the case of WouldBlock) or
                    // recurring on the next call.
                    return Ok(sent);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(sent)
    }

    pub fn recv(
        socket: &TokioUdpSocket,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize> {
        let (len, source) = socket.recv_from(&mut buffers[0])?;
        meta[0] = RecvMeta {
            source,
            len,
            truncated: false,
            ecn: None,
            dst_ip: None,
            stride: 0,
        };
        Ok(1)
    }
}
