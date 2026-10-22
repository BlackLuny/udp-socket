#![cfg(unix)]
use std::{io::IoSliceMut, net::SocketAddr};
use udp_socket::{RecvMeta, UdpSocket};

/// Largest oversize datagram to send: Darwin caps UDP sends at
/// `net.inet.udp.maxdgram` (9216 by default).
const BIG: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    60000
} else {
    9000
};

#[tokio::test]
async fn exact_capacity_zero_length_and_oversize_are_distinct() {
    for ip in ["127.0.0.1:0", "[::1]:0"] {
        for mode in 0..3 {
            let raw = match std::net::UdpSocket::bind(ip) {
                Ok(raw) => raw,
                Err(e)
                    if ip.starts_with('[') && e.kind() == std::io::ErrorKind::AddrNotAvailable =>
                {
                    eprintln!("IPv6 unavailable on test host: {e}; requires separate IPv6 live validation");
                    continue;
                }
                Err(e) => panic!("bind {}: {}", ip, e),
            };
            raw.set_nonblocking(true).unwrap();
            let socket = UdpSocket::from_socket(raw.into()).unwrap();
            let sender = tokio::net::UdpSocket::bind(ip).await.unwrap();
            let target: SocketAddr = socket.local_addr().unwrap();
            for size in [0, 127, 128, 129, 6000, BIG, 7] {
                sender
                    .send_to(&vec![size as u8; size], target)
                    .await
                    .unwrap();
                let mut buf = [0; 128];
                let meta = if mode == 2 {
                    loop {
                        socket.readable().await.unwrap();
                        match socket.try_read_datagram(&mut buf) {
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                            result => {
                                let m = result.unwrap();
                                break RecvMeta {
                                    source: m.source,
                                    len: m.len,
                                    truncated: m.truncated,
                                    ..Default::default()
                                };
                            }
                        }
                    }
                } else if mode == 1 {
                    socket.recv_one(&mut buf).await.unwrap()
                } else {
                    let mut meta = [RecvMeta::default()];
                    let n = socket
                        .recv(&mut [IoSliceMut::new(&mut buf)], &mut meta)
                        .await
                        .unwrap();
                    assert_eq!(n, 1);
                    meta[0]
                };
                assert_eq!(meta.truncated, size > 128, "{ip} mode={mode} size={size}");
                assert_eq!(meta.len, size.min(128));
                assert_eq!(&buf[..meta.len], &vec![size as u8; meta.len]);
                assert_eq!(meta.source, sender.local_addr().unwrap());
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn mixed_batch_preserves_flags_and_metadata_per_slot() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for size in [129, 128, 0, 6000, 7] {
        sender
            .send_to(&vec![size as u8; size], receiver.local_addr().unwrap())
            .unwrap();
    }
    let mut data = [[0; 128]; 5];
    let mut bufs: Vec<_> = data.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [RecvMeta::default(); 5];
    assert_eq!(receiver.recv(&mut bufs, &mut meta).await.unwrap(), 5);
    assert_eq!(
        meta.map(|m| (m.len, m.truncated)),
        [
            (128, true),
            (128, false),
            (0, false),
            (128, true),
            (7, false)
        ]
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn metadata_capacity_bounds_the_kernel_batch() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for marker in [1, 2, 3] {
        sender
            .send_to(&[marker], receiver.local_addr().unwrap())
            .unwrap();
    }
    let mut data = [[0; 8]; 3];
    let mut bufs: Vec<_> = data.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [RecvMeta::default()];
    for _ in 0..3 {
        assert_eq!(receiver.recv(&mut bufs, &mut meta).await.unwrap(), 1);
    }
    // No metadata slots must not consume a pending datagram or index out of bounds.
    sender
        .send_to(&[4], receiver.local_addr().unwrap())
        .unwrap();
    assert_eq!(
        receiver.recv(&mut bufs, &mut []).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(receiver.recv(&mut bufs, &mut meta).await.unwrap(), 1);
    assert_eq!(data[0][0], 4);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_pmtu_policy_is_applied_at_final_initialization() {
    use std::os::fd::AsRawFd;
    for ip in ["127.0.0.1:0", "[::1]:0"] {
        for (policy, expected) in [
            (udp_socket::PmtuPolicy::Probe, libc::IP_PMTUDISC_PROBE),
            (
                udp_socket::PmtuPolicy::AllowFragmentation,
                libc::IP_PMTUDISC_DONT,
            ),
        ] {
            let raw = match std::net::UdpSocket::bind(ip) {
                Err(e)
                    if ip.starts_with('[') && e.kind() == std::io::ErrorKind::AddrNotAvailable =>
                {
                    continue
                }
                result => result.unwrap(),
            };
            raw.set_nonblocking(true).unwrap();
            let dual_stack =
                ip.starts_with('[') && !socket2::SockRef::from(&raw).only_v6().unwrap();
            let receiver = UdpSocket::from_socket_with_pmtu(raw.into(), policy).unwrap();
            // A dual-stack IPv6 socket also carries IPv4-mapped traffic, so both levels must be set.
            let mut options = vec![];
            if ip.starts_with('[') {
                options.push((libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER));
            }
            if !ip.starts_with('[') || dual_stack {
                options.push((libc::IPPROTO_IP, libc::IP_MTU_DISCOVER));
            }
            for (level, option) in options {
                let mut value: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
                assert_eq!(
                    unsafe {
                        libc::getsockopt(
                            receiver.as_raw_fd(),
                            level,
                            option,
                            (&mut value as *mut libc::c_int).cast(),
                            &mut len,
                        )
                    },
                    0
                );
                assert_eq!(value, expected, "{ip} level={level}");
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn zero_capacity_datagram_buffer_still_consumes_exactly_one_packet() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for size in [0, 1, 60000, 0] {
        sender
            .send_to(&vec![7; size], receiver.local_addr().unwrap())
            .unwrap();
        let meta = loop {
            receiver.readable().await.unwrap();
            match receiver.try_read_datagram(&mut []) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                result => break result.unwrap(),
            }
        };
        assert_eq!((meta.len, meta.truncated), (0, size != 0));
        assert_eq!(meta.source, sender.local_addr().unwrap());
    }
}

/// Darwin/BSD cannot tell an empty datagram from a truncated one in an empty
/// buffer, so the read is refused up front and the datagram stays queued.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[tokio::test]
async fn zero_capacity_buffer_is_rejected_without_consuming() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for size in [0, 5] {
        sender
            .send_to(&vec![9; size], receiver.local_addr().unwrap())
            .unwrap();
        receiver.readable().await.unwrap();
        let err = receiver.try_read_datagram(&mut []).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let mut buf = [0; 16];
        let meta = receiver.recv_one(&mut buf).await.unwrap();
        assert_eq!((meta.len, meta.truncated), (size, false));
        assert_eq!(meta.source, sender.local_addr().unwrap());
    }
}

/// A coalesced UDP_GRO read cannot be represented as one datagram, so the
/// lightweight path refuses it without consuming anything.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn datagram_read_refuses_gro_sockets_without_consuming() {
    let raw = socket2::Socket::from(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    raw.set_nonblocking(true).unwrap();
    let on: libc::c_int = 1;
    use std::os::fd::AsRawFd;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                raw.as_raw_fd(),
                libc::SOL_UDP,
                libc::UDP_GRO,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of_val(&on) as _,
            )
        },
        0
    );
    // GRO enabled before the hand-over must be detected, not only via set_gro.
    let receiver = UdpSocket::from_socket(raw).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sender
        .send_to(&[1, 2, 3], receiver.local_addr().unwrap())
        .unwrap();
    receiver.readable().await.unwrap();
    let mut buf = [0; 64];
    assert_eq!(
        receiver.try_read_datagram(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    if cfg!(debug_assertions) {
        assert_eq!(
            UdpSocket::try_read_datagram_from(&receiver, &mut buf)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
    }
    let meta = receiver.recv_one(&mut buf).await.unwrap();
    assert_eq!(
        (meta.len, meta.truncated, &buf[..3]),
        (3, false, &[1, 2, 3][..])
    );

    // Disabling GRO keeps the refusal: see the queued-coalesced test below.
    receiver.set_gro(false).unwrap();
    sender
        .send_to(&[4], receiver.local_addr().unwrap())
        .unwrap();
    receiver.readable().await.unwrap();
    assert_eq!(
        receiver.try_read_datagram(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let meta = receiver.recv_one(&mut buf).await.unwrap();
    assert_eq!((meta.len, meta.truncated, buf[0]), (1, false, 4));
}

/// Datagrams coalesced while GRO was on stay coalesced in the receive queue
/// after it is disabled, so the refusal must outlive `set_gro(false)`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn coalesced_datagrams_queued_before_gro_disable_stay_refused() {
    let receiver = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    receiver.set_gro(true).unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    if UdpSocket::capabilities().unwrap().max_gso_segments < 4 {
        eprintln!("UDP GSO unavailable on test host; skipped");
        return;
    }
    let transmit = udp_socket::Transmit {
        destination: receiver.local_addr().unwrap(),
        ecn: None,
        contents: vec![7; 4000].into(),
        segment_size: Some(1000),
        src_ip: None,
    };
    sender.send(std::slice::from_ref(&transmit)).await.unwrap();
    receiver.set_gro(false).unwrap();

    receiver.readable().await.unwrap();
    let mut buf = vec![0; 65536];
    assert_eq!(
        receiver.try_read_datagram(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    // The queued receive is still the coalesced 4 x 1000 bytes, and with GRO
    // off the kernel no longer reports its stride, so not even the ancillary
    // path can split it.
    let meta = receiver.recv_one(&mut buf).await.unwrap();
    assert_eq!((meta.len, meta.stride, meta.truncated), (4000, 0, false));
}

/// Tokio requires non-blocking sockets; every constructor must ensure it.
#[tokio::test]
async fn constructors_make_the_socket_non_blocking() {
    use std::os::fd::AsRawFd;
    let blocking = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for socket in [
        UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap(),
        UdpSocket::from_socket(blocking.into()).unwrap(),
        UdpSocket::from_socket_with_pmtu(
            std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into(),
            udp_socket::PmtuPolicy::Probe,
        )
        .unwrap(),
    ] {
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0 && flags & libc::O_NONBLOCK != 0);
    }
}

/// First IPv6 link-local address with its interface scope, if the host has one.
#[cfg(target_os = "linux")]
fn link_local_v6() -> Option<std::net::SocketAddrV6> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = head;
    while !cur.is_null() && found.is_none() {
        let ifa = unsafe { &*cur };
        if !ifa.ifa_addr.is_null() && unsafe { (*ifa.ifa_addr).sa_family } as i32 == libc::AF_INET6
        {
            let sin6 = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            if ip.segments()[0] & 0xffc0 == 0xfe80 && sin6.sin6_scope_id != 0 {
                found = Some(std::net::SocketAddrV6::new(ip, 0, 0, sin6.sin6_scope_id));
            }
        }
        cur = ifa.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    found
}

/// Replies to a link-local peer need its scope; every receive path must keep it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn link_local_source_keeps_interface_scope() {
    let Some(local) = link_local_v6() else {
        eprintln!("no IPv6 link-local address on test host; skipped");
        return;
    };
    let raw = match std::net::UdpSocket::bind(local) {
        Ok(raw) => raw,
        // DAD may still be running on a freshly configured interface.
        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
            eprintln!("link-local {local} not usable yet: {e}; skipped");
            return;
        }
        Err(e) => panic!("bind {local}: {e}"),
    };
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind(local).unwrap();
    let expected = sender.local_addr().unwrap();
    assert!(matches!(expected, SocketAddr::V6(a) if a.scope_id() != 0));
    let mut buf = [0; 16];
    for mode in 0..3 {
        sender
            .send_to(&[mode], receiver.local_addr().unwrap())
            .unwrap();
        let source = match mode {
            0 => {
                let mut meta = [RecvMeta::default()];
                receiver
                    .recv(&mut [IoSliceMut::new(&mut buf)], &mut meta)
                    .await
                    .unwrap();
                meta[0].source
            }
            1 => receiver.recv_one(&mut buf).await.unwrap().source,
            _ => loop {
                receiver.readable().await.unwrap();
                match receiver.try_read_datagram(&mut buf) {
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    result => break result.unwrap().source,
                }
            },
        };
        assert_eq!(source, expected, "mode={mode}");
    }
}

/// Concurrent GRO toggles never clear the sticky refusal once an enable won.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn concurrent_gro_toggles_keep_refusal_sticky() {
    let socket = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let socket = socket.clone();
            std::thread::spawn(move || {
                for round in 0..200 {
                    socket.set_gro((i + round) % 2 == 0).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let mut buf = [0; 8];
    assert_eq!(
        socket.try_read_datagram(&mut buf).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
}
