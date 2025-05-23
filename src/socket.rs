use crate::proto::{RecvMeta, SocketType, Transmit, UdpCapabilities};
use futures_lite::future::poll_fn;
use std::io::{IoSliceMut, Result};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, RawFd};
use std::task::{Context, Poll};
use tokio::net::UdpSocket as TokioUdpSocket;

#[cfg(unix)]
use crate::unix as platform;
#[cfg(not(unix))]
use fallback as platform;

#[derive(Debug)]
pub struct UdpSocket {
    inner: TokioUdpSocket,
    fd: RawFd,
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
        let fd = socket.as_raw_fd();
        Ok(Self {
            inner: TokioUdpSocket::from_std(socket)?,
            fd,
            ty,
        })
    }

    pub fn socket_type(&self) -> SocketType {
        self.ty
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub fn ttl(&self) -> Result<u8> {
        let ttl = self.inner.ttl()?;
        Ok(ttl as u8)
    }

    pub fn set_ttl(&self, ttl: u8) -> Result<()> {
        self.inner.set_ttl(ttl as u32)
    }

    pub fn poll_send(&self, cx: &mut Context, transmits: &[Transmit<'_>]) -> Poll<Result<usize>> {
        match self.inner.poll_send_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
        }
        match platform::send(self.fd, transmits) {
            Ok(len) => Poll::Ready(Ok(len)),
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    pub fn poll_recv(
        &self,
        cx: &mut Context,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<Result<usize>> {
        match self.inner.poll_recv_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
        }
        Poll::Ready(platform::recv(self.fd, buffers, meta))
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
        };
        Ok(1)
    }
}
