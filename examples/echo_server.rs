extern crate smoltcp;

use smoltcp::iface::{EthernetInterfaceBuilder, NeighborCache};
use smoltcp::phy::wait as phy_wait;
use smoltcp::phy::XdpSocket;
//use smoltcp::phy::RawSocket;
use smoltcp::socket::SocketSet;
use smoltcp::socket::{TcpSocket, TcpSocketBuffer};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};
use std::collections::BTreeMap;
use std::env;
use std::os::unix::io::AsRawFd;

fn main() {
    let ifname = env::args().nth(1).unwrap();
    let device = XdpSocket::new(ifname.as_ref()).unwrap();
    let fd = device.as_raw_fd();
    let ip_addrs = [IpCidr::new(IpAddress::v4(10, 0, 0, 1), 24)];
    let neighbor_cache = NeighborCache::new(BTreeMap::new());
    let ethernet_addr = EthernetAddress([0xea, 0x3f, 0x86, 0x07, 0x3d, 0xc1]);

    let mut iface = EthernetInterfaceBuilder::new(device)
        .ip_addrs(ip_addrs)
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .finalize();

    let mut sockets = SocketSet::new(vec![]);
    let _ = sockets.add(TcpSocket::new(TcpSocketBuffer::new(vec![0; 65535]), TcpSocketBuffer::new(vec![0; 65535])));
    loop {
        let timestamp = Instant::now();
        match iface.poll(&mut sockets, timestamp) {
            Ok(_) => {}
            Err(e) => {
                println!("poll error: {}", e);
            }
        }
        let mut new_socket = 0;
        for s in sockets.iter_mut() {
            if let smoltcp::socket::Socket::Tcp(socket) = smoltcp::socket::SocketRef::into_inner(s)
            {
                if !socket.is_open() {
                    socket.listen(12345).unwrap();
                    new_socket += 1;
                }

                if socket.may_recv() {
                    let data = socket
                        .recv(|buffer| {
                            let recvd_len = buffer.len();
                            let data = buffer.to_owned();
                            (recvd_len, data)
                        })
                        .unwrap();
                    if socket.can_send() && data.len() > 0 {
                        socket.send_slice(&data[..]).unwrap();
                    }
                } else if socket.may_send() {
                    println!("close");
                    socket.close();
                }
            }
        }
        for _ in 0..new_socket {
            let _ = sockets.add(TcpSocket::new(TcpSocketBuffer::new(vec![0; 65535]), TcpSocketBuffer::new(vec![0; 65535])));
        }

        phy_wait(fd, iface.poll_delay(&sockets, timestamp)).expect("wait error");
    }
}
