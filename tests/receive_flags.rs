#![cfg(unix)]
use std::{io::IoSliceMut, net::SocketAddr};
use udp_socket::{RecvMeta, UdpSocket};

#[tokio::test]
async fn exact_capacity_zero_length_and_oversize_are_distinct() {
    for ip in ["127.0.0.1:0", "[::1]:0"] {
        for mode in 0..3 {
            let raw = match std::net::UdpSocket::bind(ip) {
                Ok(raw) => raw,
                Err(e) if ip.starts_with('[') && e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                    eprintln!("IPv6 unavailable on test host: {e}; requires separate IPv6 live validation");
                    continue;
                }
                Err(e) => panic!("bind {}: {}", ip, e),
            };
            raw.set_nonblocking(true).unwrap();
            let socket = UdpSocket::from_socket(raw.into()).unwrap();
            let sender = tokio::net::UdpSocket::bind(ip).await.unwrap();
            let target: SocketAddr = socket.local_addr().unwrap();
            for size in [0, 127, 128, 129, 6000, 60000, 7] {
                sender.send_to(&vec![size as u8; size], target).await.unwrap();
                let mut buf = [0; 128];
                let meta = if mode == 2 {
                    loop {
                        socket.readable().await.unwrap();
                        match socket.try_read_datagram(&mut buf) {
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                            result => {
                                let m = result.unwrap();
                                break RecvMeta { source: m.source, len: m.len, truncated: m.truncated, ..Default::default() };
                            }
                        }
                    }
                } else if mode == 1 {
                    socket.recv_one(&mut buf).await.unwrap()
                } else {
                    let mut meta = [RecvMeta::default()];
                    let n = socket.recv(&mut [IoSliceMut::new(&mut buf)], &mut meta).await.unwrap();
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
        sender.send_to(&vec![size as u8; size], receiver.local_addr().unwrap()).unwrap();
    }
    let mut data = [[0; 128]; 5];
    let mut bufs: Vec<_> = data.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [RecvMeta::default(); 5];
    assert_eq!(receiver.recv(&mut bufs, &mut meta).await.unwrap(), 5);
    assert_eq!(meta.map(|m| (m.len, m.truncated)), [(128,true),(128,false),(0,false),(128,true),(7,false)]);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn metadata_capacity_bounds_the_kernel_batch() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for marker in [1,2,3] { sender.send_to(&[marker],receiver.local_addr().unwrap()).unwrap(); }
    let mut data = [[0;8];3];
    let mut bufs: Vec<_> = data.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [RecvMeta::default()];
    for _ in 0..3 { assert_eq!(receiver.recv(&mut bufs,&mut meta).await.unwrap(),1); }
    // No metadata slots must not consume a pending datagram or index out of bounds.
    sender.send_to(&[4],receiver.local_addr().unwrap()).unwrap();
    assert_eq!(receiver.recv(&mut bufs,&mut []).await.unwrap_err().kind(),std::io::ErrorKind::InvalidInput);
    assert_eq!(receiver.recv(&mut bufs,&mut meta).await.unwrap(),1);
    assert_eq!(data[0][0],4);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_pmtu_policy_is_applied_at_final_initialization() {
    use std::os::fd::AsRawFd;
    for ip in ["127.0.0.1:0","[::1]:0"] {
        for (policy,expected) in [(udp_socket::PmtuPolicy::Probe,libc::IP_PMTUDISC_PROBE),
                                  (udp_socket::PmtuPolicy::AllowFragmentation,libc::IP_PMTUDISC_DONT)] {
            let raw = match std::net::UdpSocket::bind(ip) {
                Err(e) if ip.starts_with('[') && e.kind()==std::io::ErrorKind::AddrNotAvailable => continue,
                result => result.unwrap(),
            };
            raw.set_nonblocking(true).unwrap();
            let receiver = UdpSocket::from_socket_with_pmtu(raw.into(),policy).unwrap();
            let (level,option)=if ip.starts_with('[') {(libc::IPPROTO_IPV6,libc::IPV6_MTU_DISCOVER)} else {(libc::IPPROTO_IP,libc::IP_MTU_DISCOVER)};
            let mut value: libc::c_int=0;
            let mut len=std::mem::size_of_val(&value) as libc::socklen_t;
            assert_eq!(unsafe{libc::getsockopt(receiver.as_raw_fd(),level,option,(&mut value as *mut libc::c_int).cast(),&mut len)},0);
            assert_eq!(value,expected);
        }
    }
}

#[tokio::test]
async fn zero_capacity_datagram_buffer_still_consumes_exactly_one_packet() {
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.set_nonblocking(true).unwrap();
    let receiver = UdpSocket::from_socket(raw.into()).unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for size in [0, 1, 60000, 0] {
        sender.send_to(&vec![7; size], receiver.local_addr().unwrap()).unwrap();
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
