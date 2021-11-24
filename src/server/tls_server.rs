use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use mio::{event::Event, net::TcpListener, Poll, Token};
use rustls::{ServerConfig, ServerConnection};

use crate::{
    resolver::DnsResolver,
    server::{connection::Connection, CHANNEL_CNT, CHANNEL_PROXY, MAX_INDEX, MIN_INDEX},
    status::StatusProvider,
    tls_conn::TlsConn,
};
use std::net::IpAddr;

pub enum PollEvent<'a> {
    Network(&'a Event),
    Dns((Token, Option<IpAddr>)),
}

impl<'a> PollEvent<'a> {
    fn token(&self) -> Token {
        match self {
            PollEvent::Network(event) => event.token(),
            PollEvent::Dns((token, _)) => *token,
        }
    }
}

pub struct TlsServer {
    listener: TcpListener,
    config: Arc<ServerConfig>,
    next_id: usize,
    conns: HashMap<usize, Connection>,
}

pub trait Backend: StatusProvider {
    fn ready(&mut self, event: &Event, conn: &mut TlsConn);
    fn dispatch(&mut self, data: &[u8]);
    fn timeout(&self, t1: Instant, t2: Instant) -> bool {
        t2 - t1 > self.get_timeout()
    }
    fn get_timeout(&self) -> Duration;
}

impl TlsServer {
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> TlsServer {
        TlsServer {
            listener,
            config,
            next_id: MIN_INDEX,
            conns: HashMap::new(),
        }
    }

    pub fn accept(&mut self, poll: &Poll) {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    log::debug!(
                        "get new connection, token:{}, address:{}",
                        self.next_id,
                        addr
                    );
                    if let Err(err) = stream.set_nodelay(true) {
                        log::error!("set nodelay failed:{}", err);
                        continue;
                    }
                    let session = ServerConnection::new(self.config.clone()).unwrap();
                    let index = self.next_index();
                    let mut tls_conn = TlsConn::new(
                        index,
                        Token(index * CHANNEL_CNT + CHANNEL_PROXY),
                        rustls::Connection::Server(session),
                        stream,
                    );
                    if tls_conn.register(poll) {
                        let conn = Connection::new(index, tls_conn);
                        self.conns.insert(index, conn);
                    } else {
                        tls_conn.shutdown();
                        tls_conn.check_status(poll);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    log::debug!("no more connection to be accepted");
                    break;
                }
                Err(err) => {
                    log::error!("accept failed with error:{}, exit now", err);
                    std::panic::panic_any(err)
                }
            }
        }
    }

    fn next_index(&mut self) -> usize {
        let index = self.next_id;
        self.next_id += 1;
        if self.next_id > MAX_INDEX {
            self.next_id = MIN_INDEX;
        }
        index
    }

    fn token2index(&mut self, token: Token) -> usize {
        token.0 / CHANNEL_CNT
    }

    pub fn do_conn_event(
        &mut self,
        poll: &Poll,
        event: PollEvent,
        resolver: Option<&mut DnsResolver>,
    ) {
        let index = self.token2index(event.token());
        if self.conns.contains_key(&index) {
            let conn = self.conns.get_mut(&index).unwrap();
            conn.ready(poll, event, resolver);
            if conn.destroyed() {
                self.conns.remove(&index);
                log::debug!("connection:{} closed, remove from pool", index);
            }
        } else {
            log::error!("connection:{} not found to do event", index);
        }
    }

    pub fn check_timeout(&mut self, check_active_time: Instant, poll: &Poll) {
        let mut list = Vec::new();
        for (index, conn) in &mut self.conns {
            if conn.timeout(check_active_time) {
                list.push(*index);
                log::warn!("connection:{} timeout, close now", index);
                conn.destroy(poll)
            }
        }

        for index in list {
            self.conns.remove(&index);
        }
    }
}
