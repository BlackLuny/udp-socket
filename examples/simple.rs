use anyhow::Result;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Instant;
use std::{borrow::Cow, io::IoSliceMut};
use udp_socket::{EcnCodepoint, RecvMeta, Transmit, UdpSocket, BATCH_SIZE};

fn opt_socket() -> Result<UdpSocket> {
    // let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // socket.set_send_buffer_size(8192)?;
    // socket.set_recv_buffer_size(8192)?;
    // socket.set_nonblocking(true)?;
    // socket.bind(&SockAddr::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))))?;

    // Ok(UdpSocket::from_socket(socket)?)
    Ok(UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let socket1 = opt_socket()?;
    let socket2 = opt_socket()?;
    let addr2 = socket2.local_addr()?;

    let mut transmits = Vec::with_capacity(BATCH_SIZE);
    for i in 0..BATCH_SIZE {
        let contents = (i as u64).to_be_bytes().to_vec();
        transmits.push(Transmit {
            destination: addr2,
            ecn: None,
            segment_size: Some(1200),
            contents: Cow::Owned(contents),
            src_ip: None,
        });
    }

    let task1 = tokio::spawn(async move {
        log::debug!("before send");
        for i in 0..1000 {
            socket1.send(&transmits).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        log::debug!("after send");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });

    let task2 = tokio::spawn(async move {
        let mut cnt = 0;
        loop {
            let mut storage = [[0u8; 1200]; BATCH_SIZE];
            let mut buffers = Vec::with_capacity(BATCH_SIZE);
            let mut rest = &mut storage[..];
            for _ in 0..BATCH_SIZE {
                let (b, r) = rest.split_at_mut(1);
                rest = r;
                buffers.push(IoSliceMut::new(&mut b[0]));
            }

            let mut meta = [RecvMeta::default(); BATCH_SIZE];
            let n = socket2.recv(&mut buffers, &mut meta).await.unwrap();
            for i in 0..n {
                log::debug!(
                    "received {} {:?} {:?}",
                    i,
                    &buffers[i][..meta[i].len],
                    &meta[i]
                );
            }
            cnt += n;
            println!("total received {} packets", cnt);
        }
    });

    let start = Instant::now();
    task1.await;
    task2.await;
    println!(
        "sent {} packets in {}ms",
        BATCH_SIZE,
        start.elapsed().as_millis()
    );
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    Ok(())
}
