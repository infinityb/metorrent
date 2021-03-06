#![feature(raw, slice_bytes, iter_arith)]

#[macro_use]
extern crate log;

extern crate byteorder;
extern crate mio;
extern crate sha1;
extern crate bytes;
extern crate metorrent_common;
extern crate metorrent_middle;
extern crate metorrent_util;

use std::io;
use std::path::PathBuf;
use std::collections::HashMap;

use mio::{TryRead, TryWrite};
use mio::util::Slab;
use mio::{EventLoop, EventSet, Token};
use mio::tcp::{TcpListener, TcpSocket, TcpStream};
use mio::buf::{RingBuf, Buf, MutBuf};

use mt::common::message::Message;
use mt::common::{Bitfield, Storage};
use mt::middle::RingParser;

mod connection;
mod storage;

use mt::util::Sha1;
use connection::{Handshake, ConnectionState};

pub mod mt {
    pub use super::metorrent_common as common;
    pub use super::metorrent_middle as middle;
    pub use super::metorrent_util as util;
}

fn main() {
    println!("Hello, world!");
}

struct Torrent {
    info: mt::common::TorrentInfo,
    pieces: Bitfield,

    // Dynamic dispatch because disks are expensive anyway.
    storage: Box<Storage>,
}

fn do_read<R, B>(
    reader: &mut R,
    mutbuf: &mut B,
    cont: &mut bool,
) -> Result<Option<usize>, io::Error>
    where
        R: TryRead,
        B: MutBuf {
    
    let res = reader.try_read_buf(mutbuf);
    match &res {
        &Ok(None) => *cont = false,
        &Ok(Some(0)) => *cont = false,
        &Ok(Some(len)) => (),
        &Err(_) => (),
    };
    res
}

fn do_write<W, B>(
    writer: &mut W,
    buf: &mut B,
    cont: &mut bool,
) -> Result<Option<usize>, io::Error>
    where
        W: TryWrite,
        B: Buf {
    
    let res = writer.try_write_buf(buf);
    match &res {
        &Ok(None) => *cont = false,
        &Ok(Some(0)) => *cont = false,
        &Ok(Some(len)) => (),
        &Err(_) => (),
    };
    res
}

struct HandshakeConn {
    peer_addr: std::net::SocketAddr,
    conn: TcpStream,
    state: Handshake,
}

impl HandshakeConn {
    pub fn ready(
        &mut self,
        torrents: &HashMap<Sha1, Torrent>,
        peers: &mut Slab<PeerConn>,
        events: EventSet
    ) -> Result<(), ()> {
        let mut readable = events.is_readable();
        while readable {
            match do_read(&mut self.conn, &mut self.state.ingress_buf, &mut readable) {
                Ok(None) => (),
                Ok(Some(0)) => {
                    info!("peer({}): filled their read buffer.", self.peer_addr);
                },
                Ok(Some(_)) => (),
                Err(err) => {
                    warn!("peer({}) I/O error during read: {}", self.peer_addr, err);
                    return Err(());
                }
            }
            match self.state.try_read() {
                Ok(()) => (),
                Err(msg) => {
                    warn!("handshake({}): {}", self.peer_addr, msg);
                    return Err(());
                }
            }
        }

        if let Some(info_hash) = self.state.get_info_hash() {
            let torrent = try!(torrents.get(&info_hash).ok_or(()));
            self.state.update_torrent_info(torrent.info);
        }

        let mut writable = events.is_writable();
        while writable {
            match do_write(&mut self.conn, &mut self.state.ingress_buf, &mut writable) {
                Ok(None) => (),
                Ok(Some(0)) => {
                    info!("peer({}): emptied their write buffer.", self.peer_addr);
                },
                Ok(Some(_)) => (),
                Err(err) => {
                    warn!("peer({}) I/O error during write: {}", self.peer_addr, err);
                    return Err(());
                }
            }
        }

        Ok(())
    }
}

struct PeerConn {
    peer_addr: std::net::SocketAddr,
    conn: TcpStream,
    state: ConnectionState,
}

impl PeerConn {
    // eloop: &mut EventLoop<Server>, 
    pub fn ready(
        &mut self,
        client: &TorrentClient,
        events: EventSet
    ) -> Result<(), ()> {
        use mio::{TryWrite, TryRead};
        
        let mut readable = events.is_readable();
        while readable {
            match self.conn.try_read_buf(&mut self.state.ingress_buf) {
                Ok(Some(0)) => {
                    readable = false;
                    info!("peer({}): filled their read buffer.", self.peer_addr);
                },
                Ok(Some(_)) => (),
                Ok(None) => readable = false,
                Err(err) => {
                    warn!("peer({}) I/O error: {}", self.peer_addr, err);
                    return Err(());
                }
            }
            let info_hash = self.state.torrent_info.info_hash;
            for msg in self.state.ingress_buf.parse_msg() {
                try!(client.handle(&info_hash, &msg));
                try!(self.state.handle(&msg));
            }
        }

        let mut writable = events.is_writable();
        while writable {
            match self.conn.try_write_buf(&mut self.state.egress_buf) {
                // Finished writing: break.
                Ok(Some(0)) => {
                    writable = false;
                    info!("peer({}): finished writeout", self.peer_addr);
                }
                // Wrote some stuff: keep trying.
                Ok(Some(_)) => (),
                // EWOULDBLOCK: break; try again next time.
                Ok(None) => writable = false, 
                // I/O Error: kill the connection.
                Err(err) => {
                    warn!("peer({}) I/O error: {}", self.peer_addr, err);
                    return Err(());
                }
            }
        }
        
        Ok(())
    }
}

struct TrackerConn {
    conn: TcpStream,
    info_hash: Sha1,
}

impl TrackerConn {
    // eloop: &mut EventLoop<Server>, 
    pub fn ready(
        &mut self,
        torrent: &mut Torrent,
        events: EventSet
    ) -> Result<(), ()> {
        Ok(())
    }
}

struct TorrentClient {
    bind_socket: TcpListener,
    connections: Slab<PeerConn>,
    handshakes: Slab<HandshakeConn>,
    trackers: Slab<TrackerConn>,
    torrents: HashMap<Sha1, Torrent>,
}

impl TorrentClient {
    pub fn new(bind: TcpListener) -> TorrentClient {
        TorrentClient {
            bind_socket: bind,
            connections: Slab::new_starting_at(Token(4096), 4096),
            handshakes: Slab::new_starting_at(Token(8192), 128),
            trackers: Slab::new_starting_at(Token(8320), 128),
            torrents: HashMap::new(),
        }
    }

    pub fn handle(&mut self, ih: &Sha1, msg: &Message) -> Result<(), ()> {
        println!("[{}]: {:?}", ih, msg);
        Ok(())
    }
}

enum Command {
    // Allocate token and insert into slab
    AddHandshake(HandshakeConn),
    // Allocate token and insert into slab
    AddPeer(PeerConn),
}

impl ::mio::Handler for TorrentClient {
    type Timeout = ();
    type Message = ();

    fn ready(&mut self, eloop: &mut EventLoop<TorrentClient>, token: Token, events: EventSet) {
        let TorrentClient {
            connections: ref mut connections,
            handshakes: ref mut handshakes,
            trackers: ref mut trackers,
            torrents: ref mut torrents,
            ..
        } = *self;
        if 4096 <= token.as_usize() && token.as_usize() < 8192 {
            match connections[token].ready(torrents, events) {
                Ok(()) => println!("torrent_client.connections[token].ready(...) => OK"),
                Err(()) => println!("torrent_client.connections[token].ready(...) => ERR"),
            }
        }
        if 8192 <= token.as_usize() && token.as_usize() < 8320 {
            match handshakes[token].ready(torrents, connections, events) {
                Ok(()) => println!("torrent_client.handshakes[token].ready(...) => OK"),
                Err(()) => println!("torrent_client.handshakes[token].ready(...) => ERR"),
            }
        }
        if 8320 <= token.as_usize() && token.as_usize() < 8448 {
            match trackers[token].ready(torrents, events) {
                Ok(()) => println!("torrent_client.trackers[token].ready(...) => OK"),
                Err(()) => println!("torrent_client.trackers[token].ready(...) => ERR"),
            }
        }
    }
}