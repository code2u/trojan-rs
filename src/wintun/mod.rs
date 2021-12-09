use std::{
    collections::BTreeMap,
    convert::TryInto,
    fs::File,
    io::{BufRead, BufReader},
    process::Command,
    sync::Arc,
};

use crossbeam::channel::Sender;
use mio::{Events, Poll, Token, Waker};
use rustls::{ClientConfig, OwnedTrustAnchor, RootCertStore};
use smoltcp::{
    iface::{Interface, InterfaceBuilder, Routes, SocketHandle},
    socket::{Socket, TcpSocket, TcpSocketBuffer, UdpPacketMetadata, UdpSocket, UdpSocketBuffer},
    time::{Duration, Instant},
    wire::{
        IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion, Ipv4Address, Ipv4Packet, Ipv6Packet,
        TcpPacket, UdpPacket,
    },
};
use wintun::{Adapter, Session};

use crate::{
    proxy::IdlePool,
    resolver::DnsResolver,
    types::Result,
    wintun::{
        ip::{is_private, TunInterface},
        tcp::TcpServer,
        udp::UdpServer,
    },
    OPTIONS,
};

mod ip;
mod tcp;
mod udp;

pub(crate) type SocketSet<'a> = Interface<'a, TunInterface>;

/// Token used for dns resolver
const RESOLVER: usize = 1;
const MIN_INDEX: usize = 2;
const MAX_INDEX: usize = usize::MAX / CHANNEL_CNT;
const CHANNEL_CNT: usize = 3;
/// channel index  for `IdlePool`
const CHANNEL_IDLE: usize = 0;
/// channel index for client `UdpConnection`
const CHANNEL_UDP: usize = 1;
/// channel index for remote tcp connection
const CHANNEL_TCP: usize = 2;

fn add_route_with_if(address: &str, netmask: &str, index: u32) {
    if let Err(err) = Command::new("route")
        .args([
            "add",
            address,
            "mask",
            netmask,
            "0.0.0.0",
            "METRIC",
            "1",
            "IF",
            index.to_string().as_str(),
        ])
        .output()
    {
        log::error!("route add {} failed:{}", address, err);
    }
}

fn add_route_with_gw(address: &str, netmask: &str, gateway: &str) {
    if let Err(err) = Command::new("route")
        .args(["add", address, "mask", netmask, gateway, "METRIC", "1"])
        .output()
    {
        log::error!("route add {} failed:{}", address, err);
    }
}

pub fn run() -> Result<()> {
    let wintun = unsafe { wintun::load_from_path(&OPTIONS.wintun_args().wintun)? };
    let adapter = match Adapter::open(&wintun, OPTIONS.wintun_args().name.as_str()) {
        Ok(a) => a,
        Err(_) => Adapter::create(&wintun, "trojan", OPTIONS.wintun_args().name.as_str(), None)?,
    };

    let hostname = OPTIONS.wintun_args().hostname.as_str().try_into()?;

    let mut root_store = RootCertStore::empty();
    root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));
    let config = ClientConfig::builder()
        .with_safe_default_cipher_suites()
        .with_safe_default_kx_groups()
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let config = Arc::new(config);

    let mut poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), Token(RESOLVER))?);
    let mut resolver = DnsResolver::new(waker, Token(RESOLVER));
    let mut pool = IdlePool::new(
        config,
        hostname,
        OPTIONS.wintun_args().pool_size + 1,
        OPTIONS.wintun_args().port,
        OPTIONS.wintun_args().hostname.clone(),
    );
    pool.init(&poll, &resolver);
    pool.init_index(CHANNEL_CNT, CHANNEL_IDLE, MIN_INDEX, MAX_INDEX);

    let (sender, receiver) = crossbeam::channel::bounded(OPTIONS.wintun_args().buffer_size);

    let session = Arc::new(adapter.start_session(wintun::MAX_RING_CAPACITY)?);

    let ip_addrs = [IpCidr::new(IpAddress::v4(0, 0, 0, 1), 0)];

    let mut routes = Routes::new(BTreeMap::new());
    routes
        .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 1))
        .unwrap();
    let mut interface = InterfaceBuilder::new(
        TunInterface::new(session.clone(), receiver, OPTIONS.wintun_args().mtu),
        [],
    )
    .any_ip(true)
    .ip_addrs(ip_addrs)
    .routes(routes)
    .finalize();

    let mut events = Events::with_capacity(1024);
    let timeout = Some(Duration::from_millis(1));
    let mut udp_server = UdpServer::new();
    let mut tcp_server = TcpServer::new();

    let mut last_udp_check_time = std::time::Instant::now();
    let mut last_tcp_check_time = std::time::Instant::now();
    let check_duration = std::time::Duration::new(10, 0);

    let index = adapter.get_adapter_index()?;
    add_route_with_if("0.0.0.0", "0.0.0.0", index);
    if OPTIONS.wintun_args().add_white_list {
        add_ipset(
            OPTIONS.wintun_args().white_ip_list.as_str(),
            OPTIONS.wintun_args().default_gateway.as_str(),
        )?;
        log::warn!("white list added");
    }

    let mut now = Instant::now();
    loop {
        let (udp_handles, tcp_handles) = do_tun_read(&session, &sender, &mut interface)?;
        if let Err(err) = interface.poll(now) {
            log::info!("interface error:{}", err);
        }
        udp_server.do_local(&mut pool, &poll, &resolver, udp_handles, &mut interface);
        tcp_server.do_local(&mut pool, &poll, &resolver, tcp_handles, &mut interface);

        now = Instant::now();
        let timeout = interface.poll_delay(now).or(timeout);
        poll.poll(
            &mut events,
            timeout.map(|d| std::time::Duration::from_millis(d.total_millis())),
        )?;
        for event in &events {
            match event.token().0 {
                RESOLVER => {
                    resolver.consume(|_, ip| {
                        pool.resolve(ip);
                    });
                }
                i if i % CHANNEL_CNT == CHANNEL_IDLE => {
                    pool.ready(event, &poll);
                }
                i if i % CHANNEL_CNT == CHANNEL_UDP => {
                    udp_server.do_remote(event, &poll, &mut interface);
                }
                _ => {
                    tcp_server.do_remote(event, &poll, &mut interface);
                }
            }
        }

        let now = std::time::Instant::now();
        if now - last_tcp_check_time > check_duration {
            tcp_server.check_timeout(&poll, now, &mut interface);
            last_tcp_check_time = now;
        }

        if now - last_udp_check_time > OPTIONS.udp_idle_duration {
            udp_server.check_timeout(now, &mut interface);
            last_udp_check_time = now;
        }
    }
}

fn do_tun_read(
    session: &Arc<Session>,
    sender: &Sender<Vec<u8>>,
    sockets: &mut SocketSet,
) -> Result<(Vec<SocketHandle>, Vec<SocketHandle>)> {
    let mut udp_handles = Vec::new();
    let mut tcp_handles = Vec::new();
    loop {
        let packet = session.try_receive()?;
        if packet.is_none() {
            break;
        }
        let packet = packet.unwrap();
        let (src_addr, dst_addr, payload, protocol) =
            match IpVersion::of_packet(packet.bytes()).unwrap() {
                IpVersion::Ipv4 => {
                    let packet = Ipv4Packet::new_checked(packet.bytes()).unwrap();
                    let src_addr = packet.src_addr();
                    let dst_addr = packet.dst_addr();
                    (
                        IpAddress::Ipv4(src_addr),
                        IpAddress::Ipv4(dst_addr),
                        packet.payload(),
                        packet.protocol(),
                    )
                }
                IpVersion::Ipv6 => {
                    let packet = Ipv6Packet::new_checked(packet.bytes()).unwrap();
                    let src_addr = packet.src_addr();
                    let dst_addr = packet.dst_addr();
                    (
                        IpAddress::Ipv6(src_addr),
                        IpAddress::Ipv6(dst_addr),
                        packet.payload(),
                        packet.next_header(),
                    )
                }
                _ => continue,
            };
        let (src_port, dst_port, notify, connect) = match protocol {
            IpProtocol::Udp => {
                let packet = UdpPacket::new_checked(payload).unwrap();
                (packet.src_port(), packet.dst_port(), true, None)
            }
            IpProtocol::Tcp => {
                let packet = TcpPacket::new_checked(payload).unwrap();
                (
                    packet.src_port(),
                    packet.dst_port(),
                    !packet.payload().is_empty() || packet.fin(),
                    Some(packet.syn() && !packet.ack()),
                )
            }
            _ => continue,
        };

        let src_endpoint = IpEndpoint::new(src_addr, src_port);
        let dst_endpoint = IpEndpoint::new(dst_addr, dst_port);
        if is_private(dst_endpoint) {
            continue;
        }

        if let Some(connect) = connect {
            if let Some(handle) = if connect {
                let mut socket = TcpSocket::new(
                    TcpSocketBuffer::new(vec![0; OPTIONS.wintun_args().tcp_rx_buffer_size]),
                    TcpSocketBuffer::new(vec![0; OPTIONS.wintun_args().tcp_tx_buffer_size]),
                );
                socket.listen(dst_endpoint).unwrap();
                Some(sockets.add_socket(socket))
            } else {
                sockets.sockets().find_map(|(handle, socket)| match socket {
                    Socket::Tcp(socket)
                        if socket.local_endpoint() == dst_endpoint
                            && socket.remote_endpoint() == src_endpoint =>
                    {
                        Some(handle)
                    }
                    _ => None,
                })
            } {
                if notify {
                    tcp_handles.push(handle);
                }
            }
        } else {
            let handle = sockets.sockets().find_map(|(handle, socket)| match socket {
                Socket::Udp(socket) if socket.endpoint() == dst_endpoint => Some(handle),
                _ => None,
            });
            let handle = match handle {
                None => {
                    let mut socket = UdpSocket::new(
                        UdpSocketBuffer::new(
                            vec![UdpPacketMetadata::EMPTY; OPTIONS.wintun_args().udp_rx_meta_size],
                            vec![0; OPTIONS.wintun_args().udp_rx_buffer_size],
                        ),
                        UdpSocketBuffer::new(
                            vec![UdpPacketMetadata::EMPTY; OPTIONS.wintun_args().udp_rx_meta_size],
                            vec![0; OPTIONS.wintun_args().udp_tx_buffer_size],
                        ),
                    );
                    socket.bind(dst_endpoint)?;
                    sockets.add_socket(socket)
                }
                Some(handle) => handle,
            };
            udp_handles.push(handle);
        }

        if let Err(err) = sender.try_send(packet.bytes().into()) {
            log::warn!("sender buffer is full:{}", err);
        }
    }

    Ok((udp_handles, tcp_handles))
}

fn add_ipset(config: &str, gw: &str) -> Result<()> {
    let file = File::open(config)?;
    let buffer = BufReader::new(file);
    buffer.lines().for_each(|line| {
        let line = line.unwrap();
        let line: Vec<_> = line.split('/').collect();
        log::warn!("route add {} mask {}", line[0], line[1]);
        add_route_with_gw(line[0], line[1], gw);
    });
    Ok(())
}
