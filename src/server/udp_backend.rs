use std::{net::SocketAddr, time::Duration};

use bytes::BytesMut;
use mio::{event::Event, net::UdpSocket, Interest, Poll, Token};

use crate::{
    config::OPTIONS,
    proto::{UdpAssociate, UdpParseResult, MAX_BUFFER_SIZE, MAX_PACKET_SIZE},
    server::tls_server::Backend,
    tls_conn::{ConnStatus, TlsConn},
};

pub struct UdpBackend {
    socket: UdpSocket,
    send_buffer: BytesMut,
    recv_body: Vec<u8>,
    recv_head: BytesMut,
    index: usize,
    token: Token,
    status: ConnStatus,
    interest: Interest,
    timeout: Duration,
    bytes_read: usize,
    bytes_sent: usize,
    remote_addr: SocketAddr,
}

impl UdpBackend {
    pub fn new(socket: UdpSocket, index: usize, token: Token) -> UdpBackend {
        let remote_addr = socket.local_addr().unwrap();
        UdpBackend {
            socket,
            send_buffer: Default::default(),
            recv_body: vec![0u8; MAX_PACKET_SIZE],
            recv_head: Default::default(),
            index,
            token,
            status: ConnStatus::Established,
            interest: Interest::READABLE,
            timeout: OPTIONS.udp_idle_duration,
            bytes_read: 0,
            bytes_sent: 0,
            remote_addr,
        }
    }

    fn do_send(&mut self, mut buffer: &[u8]) {
        loop {
            match UdpAssociate::parse(buffer) {
                UdpParseResult::Packet(packet) => {
                    match self
                        .socket
                        .send_to(&packet.payload[..packet.length], packet.address)
                    {
                        Ok(size) => {
                            self.bytes_sent += size;
                            if size != packet.length {
                                log::error!(
                                    "connection:{} udp packet is truncated, {}：{}",
                                    self.index,
                                    packet.length,
                                    size
                                );
                                self.status = ConnStatus::Closing;
                                return;
                            }
                            log::debug!(
                                "connection:{} write {} bytes to udp target:{}",
                                self.index,
                                size,
                                packet.address
                            );
                            buffer = &packet.payload[packet.length..];
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            log::debug!("connection:{} write to udp target blocked", self.index);
                            self.send_buffer.extend_from_slice(buffer);
                            break;
                        }
                        Err(err) => {
                            log::warn!(
                                "connection:{} send_to {} failed:{}",
                                self.index,
                                packet.address,
                                err
                            );
                            self.status = ConnStatus::Closing;
                            return;
                        }
                    }
                }
                UdpParseResult::InvalidProtocol => {
                    log::error!("connection:{} got invalid udp protocol", self.index);
                    self.status = ConnStatus::Closing;
                    return;
                }
                UdpParseResult::Continued => {
                    log::trace!("connection:{} got partial request", self.index);
                    self.send_buffer.extend_from_slice(buffer);
                    break;
                }
            }
        }
        if let ConnStatus::Shutdown = self.status {
            if self.send_buffer.is_empty() {
                log::debug!("connection:{} is closing for no data to send", self.index);
                self.status = ConnStatus::Closing;
            }
        }
    }

    fn do_read(&mut self, conn: &mut TlsConn) {
        loop {
            match self.socket.recv_from(self.recv_body.as_mut_slice()) {
                Ok((size, addr)) => {
                    self.remote_addr = addr;
                    self.bytes_read += size;
                    log::debug!(
                        "connection:{} got {} bytes udp data from:{}",
                        self.index,
                        size,
                        addr
                    );
                    self.recv_head.clear();
                    UdpAssociate::generate(&mut self.recv_head, &addr, size as u16);
                    if !conn.write_session(self.recv_head.as_ref()) {
                        self.status = ConnStatus::Closing;
                        break;
                    }
                    if !conn.write_session(&self.recv_body.as_slice()[..size]) {
                        self.status = ConnStatus::Closing;
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    log::debug!("connection:{} write to session blocked", self.index);
                    break;
                }
                Err(err) => {
                    log::warn!("connection:{} got udp read err:{}", self.index, err);
                    self.status = ConnStatus::Closing;
                    break;
                }
            }
        }
        conn.do_send();
    }

    fn setup(&mut self, poll: &Poll) {
        if let Err(err) = poll
            .registry()
            .reregister(&mut self.socket, self.token, self.interest)
        {
            log::error!(
                "connection:{} reregister udp target failed:{}",
                self.index,
                err
            );
            self.status = ConnStatus::Closing;
        }
    }
}

impl Backend for UdpBackend {
    fn ready(&mut self, event: &Event, conn: &mut TlsConn) {
        if event.is_readable() {
            self.do_read(conn);
        }
        if event.is_writable() {
            self.dispatch(&[]);
        }
    }

    fn dispatch(&mut self, buffer: &[u8]) {
        if self.send_buffer.is_empty() {
            self.do_send(buffer);
        } else {
            self.send_buffer.extend_from_slice(buffer);
            let buffer = self.send_buffer.split();
            self.do_send(buffer.as_ref());
        }
    }

    fn reregister(&mut self, poll: &Poll, readable: bool) {
        match self.status {
            ConnStatus::Closing => {
                let _ = poll.registry().deregister(&mut self.socket);
            }
            ConnStatus::Closed => {}
            _ => {
                let mut changed = false;
                if !self.send_buffer.is_empty() && !self.interest.is_writable() {
                    self.interest |= Interest::WRITABLE;
                    changed = true;
                    log::debug!("connection:{} add writable to udp target", self.index);
                }
                if self.send_buffer.is_empty() && self.interest.is_writable() {
                    self.interest = self
                        .interest
                        .remove(Interest::WRITABLE)
                        .unwrap_or(Interest::READABLE);
                    changed = true;
                    log::debug!("connection:{} remove writable from udp target", self.index);
                }
                if readable && !self.interest.is_readable() {
                    self.interest |= Interest::READABLE;
                    log::debug!("connection:{} add readable to udp target", self.index);
                    changed = true;
                }
                if !readable && self.interest.is_readable() {
                    self.interest = self
                        .interest
                        .remove(Interest::READABLE)
                        .unwrap_or(Interest::WRITABLE);
                    log::debug!("connection:{} remove readable to udp target", self.index);
                    changed = true;
                }

                if changed {
                    self.setup(poll);
                }
            }
        }
    }

    fn check_close(&mut self, poll: &Poll) {
        if let ConnStatus::Closing = self.status {
            let _ = poll.registry().deregister(&mut self.socket);
            self.status = ConnStatus::Closed;
            log::info!(
                "connection:{} address:{} closed, read {} bytes, sent {} bytes",
                self.index,
                self.remote_addr,
                self.bytes_read,
                self.bytes_sent
            );
        }
    }

    fn get_timeout(&self) -> Duration {
        self.timeout
    }

    fn status(&self) -> ConnStatus {
        self.status
    }

    fn shutdown(&mut self, poll: &Poll) {
        if self.send_buffer.is_empty() {
            self.status = ConnStatus::Closing;
            self.check_close(poll);
            return;
        }
        self.interest = Interest::WRITABLE;
        self.status = ConnStatus::Shutdown;
        self.setup(poll);
        self.check_close(poll);
    }

    fn writable(&self) -> bool {
        self.send_buffer.len() < MAX_BUFFER_SIZE
    }
}
