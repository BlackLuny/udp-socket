use crate::proto::{RecvMeta, SocketType, Transmit, UdpCapabilities};
use futures_lite::future::poll_fn;
use socket2::Socket;
use std::io::{IoSliceMut, Result};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
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
        let inner = TokioUdpSocket::from_std(socket)?;
        Ok(Self { inner, ty })
    }

    pub fn from_socket(socket: Socket) -> Result<Self> {
        let socket = std::net::UdpSocket::from(socket);
        let ty = platform::init(&socket)?;
        let inner = TokioUdpSocket::from_std(socket)?;
        Ok(Self { inner, ty })
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
    pub fn set_gro(&self, enabled: bool) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
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
                return Err(std::io::Error::last_os_error());
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
    pub fn try_read_mmsg(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize> {
        self.inner.try_io(Interest::READABLE, || {
            platform::recv(self.inner.as_raw_fd(), buffers, meta)
        })
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

    pub async fn recv(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Result<usize> {
        poll_fn(|cx| self.poll_recv(cx, buffers, meta)).await
    }
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
            ecn: None,
            dst_ip: None,
            stride: 0,
        };
        Ok(1)
    }
}
