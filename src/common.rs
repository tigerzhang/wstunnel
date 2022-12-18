use futures_util::{StreamExt};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

use tokio_tungstenite::tungstenite;
use tokio_tungstenite::{accept_async, connect_async_trust_certificate, tungstenite::protocol::Message};
type Error = Box<dyn std::error::Error>;
use crate::tokio::io::AsyncWriteExt;
use futures_util::SinkExt;
use log::{debug, error, info, trace, warn};
use std::{
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite};
use std::sync::{Arc};
use std::time::SystemTime;
use tokio::sync::Mutex;
use crate::{ConnectionStatus, ConnectionStatusCode};
use crate::ConnectionStatusCode::CONNECTED;

pub type ConStatus = Arc<Mutex<HashMap<u16, ConnectionStatus>>>;

/// Address type in socks5.
#[derive(Debug, Clone)]
pub enum Address {
    /// SocketAddr
    SocketAddr(SocketAddr),
    /// Domain
    Domain(String, u16),
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::SocketAddr(s) => fmt::Display::fmt(s, f),
            Address::Domain(domain, port) => write!(f, "{}:{}", domain, port),
        }
    }
}

impl Default for Address {
    fn default() -> Self {
        Address::SocketAddr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0))
    }
}

impl From<SocketAddr> for Address {
    fn from(addr: SocketAddr) -> Self {
        Address::SocketAddr(addr)
    }
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

fn host_to_address(host: &str, port: u16) -> Address {
    match strip_brackets(host).parse::<IpAddr>() {
        Ok(ip) => {
            let addr = SocketAddr::new(ip, port);
            addr.into()
        }
        Err(_) => Address::Domain(host.to_string(), port),
    }
}
fn no_addr() -> io::Error {
    io::ErrorKind::AddrNotAvailable.into()
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.rsplitn(2, ':');
        let port: u16 = parts
            .next()
            .ok_or_else(no_addr)?
            .parse()
            .map_err(|_| no_addr())?;
        let host = parts.next().ok_or_else(no_addr)?;
        Ok(host_to_address(host, port))
    }
}

impl Address {
    /// Convert `Address` to `SocketAddr`. If `Address` is a domain, return `std::io::ErrorKind::InvalidInput`
    #[allow(dead_code)]
    pub fn to_socket_addr(self) -> Result<SocketAddr, Error> {
        match self {
            Address::SocketAddr(s) => Ok(s),
            _ => Err(Error::from("invalid input")),
        }
    }
    #[allow(dead_code)]
    async fn read_port<R>(mut reader: R) -> Result<u16, Error>
        where
            R: AsyncRead + Unpin,
    {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).await?;
        let port = u16::from_be_bytes(buf);
        Ok(port)
    }
    #[allow(dead_code)]
    async fn write_port<W>(mut writer: W, port: u16) -> Result<(), Error>
        where
            W: AsyncWrite + Unpin,
    {
        writer.write_all(&port.to_be_bytes()).await?;
        Ok(())
    }
    /// Length of `Address` in bytes after serialized.
    #[allow(dead_code)]
    pub fn serialized_len(&self) -> Result<usize, Error> {
        Ok(match self {
            Address::SocketAddr(SocketAddr::V4(_)) => {
                // 1 byte for ATYP, 4 bytes for IPV4 address, 2 bytes for port
                1 + 4 + 2
            }
            Address::SocketAddr(SocketAddr::V6(_)) => {
                // 1 byte for ATYP, 16 bytes for IPV6 address, 2 bytes for port
                1 + 16 + 2
            }
            Address::Domain(domain, _) => {
                if domain.len() >= 256 {
                    return Err(Error::from("invalid domain"));
                }
                // 1 byte for ATYP, 1 byte for domain length, domain, 2 bytes for port
                1 + 1 + domain.len() + 2
            }
        })
    }
    /// Write `Address` to `AsyncWrite`.
    #[allow(dead_code)]
    pub async fn write<W>(&self, mut writer: W) -> Result<(), Error>
        where
            W: AsyncWrite + Unpin,
    {
        match self {
            Address::SocketAddr(SocketAddr::V4(addr)) => {
                writer.write_all(&[0x01]).await?;
                writer.write_all(&addr.ip().octets()).await?;
                Self::write_port(writer, addr.port()).await?;
            }
            Address::SocketAddr(SocketAddr::V6(addr)) => {
                writer.write_all(&[0x04]).await?;
                writer.write_all(&addr.ip().octets()).await?;
                Self::write_port(writer, addr.port()).await?;
            }
            Address::Domain(domain, port) => {
                if domain.len() >= 256 {
                    return Err(Error::from("domain name"));
                }
                let header = [0x03, domain.len() as u8];
                writer.write_all(&header).await?;
                writer.write_all(domain.as_bytes()).await?;
                Self::write_port(writer, *port).await?;
            }
        };
        Ok(())
    }

    pub async fn read_from_buf(buf: &Vec<u8>, offset: usize) -> Result<Self, Error> {
        let address_type = buf[offset];
        Ok(match address_type {
            1 => {
                let mut ip = [0u8; 4];
                ip.copy_from_slice(&buf[offset + 1..offset + 1 + 4]);
                let mut port_buf = [0u8; 2];
                port_buf.copy_from_slice(&buf[offset + 1 + 4..offset + 1 + 4 + 2]);
                let port = u16::from_be_bytes(port_buf);
                Address::SocketAddr(SocketAddr::new(ip.into(), port))
            }
            3 => {
                let len = buf[offset+1];
                let len_size = len as usize;
                let mut domain = vec![0u8; len_size];
                domain.copy_from_slice(&buf[offset + 1 + 1..offset + 1 + 1 + len_size]);

                let domain = String::from_utf8(domain).map_err(|_e| "invalid domain")?;

                let mut port_buf = [0u8; 2];
                port_buf.copy_from_slice(&buf[offset + 1 + 1 + len_size..offset + 1 + 1 + len_size + 2]);
                let port = u16::from_be_bytes(port_buf);
                Address::Domain(domain, port)
            }
            4 => {
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&buf[offset + 1..offset + 1 + 16]);
                let mut port_buf = [0u8; 2];
                port_buf.copy_from_slice(&buf[offset + 1 + 16..offset + 1 + 16 + 2]);
                let port = u16::from_be_bytes(port_buf);
                Address::SocketAddr(SocketAddr::new(ip.into(), port))
            }
            _ => return Err(Error::from("invalid address"))
        })
    }

    /// Read `Address` from `AsyncRead`.
    #[allow(dead_code)]
    pub async fn read<R>(mut reader: R) -> Result<Self, Error>
        where
            R: AsyncRead + Unpin,
    {
        let mut atyp = [0u8; 1];
        reader.read_exact(&mut atyp).await?;

        Ok(match atyp[0] {
            1 => {
                let mut ip = [0u8; 4];
                reader.read_exact(&mut ip).await?;
                Address::SocketAddr(SocketAddr::new(
                    ip.into(),
                    Self::read_port(&mut reader).await?,
                ))
            }
            3 => {
                let mut len = [0u8; 1];
                reader.read_exact(&mut len).await?;
                let len = len[0] as usize;
                let mut domain = vec![0u8; len];
                reader.read_exact(&mut domain).await?;

                let domain =
                    String::from_utf8(domain).map_err(|_e| Error::from("invalid domain"))?;

                Address::Domain(domain, Self::read_port(&mut reader).await?)
            }
            4 => {
                let mut ip = [0u8; 16];
                reader.read_exact(&mut ip).await?;
                Address::SocketAddr(SocketAddr::new(
                    ip.into(),
                    Self::read_port(&mut reader).await?,
                ))
            }
            _ => return Err(Error::from("invalid address")),
        })
    }
}

pub enum TcpOrDestination {
    Tcp(TcpStream),
    Dest(String),
}

pub async fn communicate(tcp_in: TcpOrDestination, ws_in: TcpOrDestination, con_status_map: Arc<Mutex<HashMap<u16, ConnectionStatus>>>) -> Result<(), Error> {
    let mut ws;
    let tcp;

    match tcp_in {
        TcpOrDestination::Tcp(src_stream) => {
            // Convert the source stream into a websocket connection.
            info!("websocket accepting");
            ws = accept_async(tokio_tungstenite::MaybeTlsStream::Plain(src_stream)).await?;
        }
        TcpOrDestination::Dest(dest_location) => {
            info!("connecting {}", dest_location);
            let (wsz, _) = match connect_async_trust_certificate(dest_location).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("Something went wrong connecting {:?}", e);
                    if let TcpOrDestination::Tcp(mut v) = ws_in {
                        v.shutdown().await?;
                    }
                    return Err(Box::new(e));
                }
            };
            ws = wsz;
        }
    }

    match ws_in {
        TcpOrDestination::Tcp(v) => {
            tcp = v;
        }
        TcpOrDestination::Dest(dest_location) => {
            // Try to setup the tcp stream we'll be communicating to, if this fails, we close the websocket.
            tcp = match TcpStream::connect(dest_location).await {
                Ok(e) => e,
                Err(v) => {
                    let msg = tungstenite::protocol::frame::CloseFrame {
                        reason: std::borrow::Cow::Borrowed("Could not connect to destination."),
                        code: tungstenite::protocol::frame::coding::CloseCode::Error,
                    };

                    // Send the websocket close message.
                    if let Err(e) = ws.send(Message::Close(Some(msg))).await {
                        warn!("Tried to send close message, but this failed {:?}", e);
                    }

                    // Ensure we collect messages until the shutdown is actually performed.
                    let (mut _write, read) = ws.split();
                    read.for_each(|_message| async {}).await;
                    return Err(Box::new(v));
                }
            }
        }
    }

    let _ = match tcp.peer_addr() {
        Ok(addr) => addr,
        Err(e) => {
            return Err(Box::new(e));
        },
    };

    let tcp_remote_port = tcp.peer_addr().unwrap().port();

    // We got the tcp connection setup, split both streams in their read and write parts
    let (mut dest_read, mut dest_write) = tcp.into_split();
    let (mut write, mut read) = ws.split();
    let (shutdown_from_ws_tx, mut shutdown_from_ws_rx) = tokio::sync::oneshot::channel::<bool>();
    let (shutdown_from_tcp_tx, mut shutdown_from_tcp_rx) = tokio::sync::oneshot::channel::<bool>();

    let address = Arc::new(Mutex::new(String::from("xxx")));

    let address1 = address.clone();
    let con_status_map1 = con_status_map.clone();
    // Consume from the websocket, if this loop quits, we return both things we took ownership of.
    let task_ws_to_tcp = tokio::spawn(async move {
        loop {
            tokio::select! {
                message = read.next() => {
                    if message.is_none() {
                        debug!("Got none, end of websocket stream.");
                        break;
                    }
                    let message = message.unwrap();
                    let data = match message {
                        Ok(v) => v,
                        Err(p) => {
                            let addr = Arc::clone(&address1);
                            debug!("Err reading data {:?} {}", p, addr.lock().await);
                            // dest_write.shutdown().await?;
                            break;
                        }
                    };
                    match data {
                        Message::Binary(ref x) => {
                            let addr = Arc::clone(&address1);
                            debug!("ws got {} bytes from {}", x.len(), addr.lock().await);
                            if x.len() < 11 {
                                debug!("ws got {:?}", x);
                            }
                            if dest_write.write(x).await.is_err() {
                                break;
                            };

                            let con_map = con_status_map1.clone();
                            let mut status = con_map.lock().await;
                            if status.contains_key(&tcp_remote_port) {
                                let status = status.get_mut(&tcp_remote_port).unwrap();
                                status.bytes_got += x.len() as u32;
                                if x.len() == 2 && x[0] == 5 && x[1] == 0 {
                                    // handshake ack
                                    status.status = CONNECTED;
                                    status.last_active = SystemTime::now();
                                }
                            }
                        }
                        Message::Close(m) => {
                            trace!("Encountered close message {:?}", m);
                            // dest_write.shutdown().await?;
                            // need to somehow shut down the tcp socket here as well.
                        }
                        other => {
                            error!("Something unhandled on the websocket: {:?}", other);
                            // dest_write.shutdown().await?;
                        }
                    }
                },
                _shutdown_received = (&mut shutdown_from_tcp_rx ) =>
                {
                    break;
                }
            }
        }
        debug!("Reached end of consume from websocket. {}", address1.lock().await);
        if let Err(_) = shutdown_from_ws_tx.send(true) {
            // This happens if the shutdown happens from the other side.
            // error!("Could not send shutdown signal: {:?}", v);
        }
        (dest_write, read)
    });

    let address2 = address.clone();
    let con_status_map2 = con_status_map.clone();
    // Consume from the tcp socket and write on the websocket.
    let task_tcp_to_ws = tokio::spawn(async move {
        let mut need_close = true;
        let mut buf = vec![0; 1024 * 1024];
        loop {
            tokio::select! {
                res = dest_read.read(&mut buf) => {
                    // Return value of `Ok(0)` signifies that the remote has closed, if this happens
                    // we want to initiate shutting down the websocket.
                    match res {
                        Ok(0) => {
                            let addr = Arc::clone(&address2);
                            warn!("tcp read 0 byte. Remote tcp socket has closed, sending close message on websocket. {}", addr.lock().await);
                            break;
                            // debug!("tcp read 0 byte");
                        }
                        Ok(n) => {
                            if n < 11 {
                                debug!("tcp read {:?}", &buf[0..n]);
                            } else {
                                let addr = Arc::clone(&address2);
                                debug!("tcp read {} bytes {} ", n, addr.lock().await);
                            }

                            {
                                let con_map = con_status_map2.clone();
                                let mut status = con_map.lock().await;
                                if status.contains_key(&tcp_remote_port) {
                                    let status = status.get_mut(&tcp_remote_port).unwrap();
                                    status.bytes_sent += n as u32;
                                }
                            }

                            if n > 3 && buf[0] == 5 && buf[1] == 1 {
                                // buf[2] reserved
                                let addr = Arc::clone(&address2);
                                let addr_str = Address::read_from_buf(&buf, 3).await.unwrap().to_string();
                                let mut value = addr.lock().await;
                                // *value = "abc".to_string();
                                *value = addr_str.clone();

                                info!("Connecting {}. remote port {}", addr_str.clone(), tcp_remote_port);

                                {
                                    let con_map = con_status_map2.clone();
                                    let mut status = con_map.lock().await;
                                    if status.contains_key(&tcp_remote_port) {
                                        let status = status.get_mut(&tcp_remote_port).unwrap();
                                        status.address = addr_str.clone();
                                    }
                                }

                                // debug!("{}", addr);

                                // if addr_str.contains("canhazip.com") {
                                //     // info!("{:?}", con_status_map2.lock().unwrap());
                                //     let con_map = con_status_map2.lock().unwrap();
                                //     for key in con_map.keys() {
                                //         info!("{}: {:?}", key, con_map.get(key).unwrap());
                                //     }
                                // }
                            }
                            let _addr = Arc::clone(&address2);
                            // debug!("ws send {} bytes", n);
                            let res = buf[..n].to_vec();
                            match write.send(Message::Binary(res)).await {
                                Ok(_) => {
                                    continue;
                                }
                                Err(v) => {
                                    debug!("Failed to send binary data on ws: {:?}", v);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            // Unexpected socket error. There isn't much we can do here so just stop
                            // processing, and close the tcp socket.
                            error!(
                                "Something unexpected happened reading from the tcp socket: {:?}",
                                e
                            );
                            break;
                        }
                    }
                },
                _shutdown_received = (&mut shutdown_from_ws_rx ) =>
                {
                    need_close = false;
                    break;
                }
            }
        }

        // Send the websocket close message.
        if need_close {
            if let Err(v) = write.send(Message::Close(None)).await {
                error!("Failed to send close message to websocket: {:?}", v);
            }
        }
        if let Err(_) = shutdown_from_tcp_tx.send(true) {
            // This happens if the shutdown happens from the other side.
            // error!("Could not send shutdown signal: {:?}", v);
        }
        warn!("Reached end of consume from tcp. {}", address.lock().await);
        (dest_read, write, need_close)
    });

    // Finally, the cleanup task, all it does is close down the tcp connections.
    tokio::spawn(async move {
        // Wait for both tasks to complete.
        let (r_ws_to_tcp, r_tcp_to_ws) = tokio::join!(task_ws_to_tcp, task_tcp_to_ws);
        if let Err(ref v) = r_ws_to_tcp {
            error!(
                "Error joining: {:?}, dropping connection without proper close.",
                v
            );
            return;
        }
        let (dest_write, read) = r_ws_to_tcp.unwrap();

        if let Err(ref v) = r_tcp_to_ws {
            error!(
                "Error joining: {:?}, dropping connection without proper close.",
                v
            );
            return;
        }
        let (dest_read, write, ws_need_close) = r_tcp_to_ws.unwrap();

        // Reunite the streams, this is guaranteed to succeed as we always use the correct parts.
        let mut tcp_stream = dest_write.reunite(dest_read).unwrap();
        if let Err(ref v) = tcp_stream.shutdown().await {
            error!(
                "Error properly closing the tcp from {:?}: {:?}",
                tcp_stream.peer_addr(),
                v
            );
            return;
        }

        if ws_need_close {
            let mut ws_stream = write.reunite(read).unwrap();
            if let Err(ref v) = ws_stream.get_mut().shutdown().await {
                error!(
                    "Error properly closing the ws from {:?}: {:?}",
                    "something", v
                );
                return;
            }
        }
        debug!("Properly closed connections.");

        let con_map = con_status_map.clone();
        let mut con = con_map.lock().await;
        con.remove(&tcp_remote_port);
    });

    Ok(())
}

#[derive(Clone)]
pub enum Direction {
    ServerSide,
    ClientSide,
}

pub async fn serve(bind_location: &str, dest_location: &str, dir: &Direction, con_status_map: ConStatus) -> Result<(), Error> {
    let listener = TcpListener::bind(bind_location)
        .await
        .expect("Could not bind to port");
    info!("Successfully bound to {:?}", bind_location);

    loop {
        let in1 = match dir {
            Direction::ServerSide => {
                let (socket, _) = listener
                    .accept()
                    .await
                    .expect("Could not accept connection?");

                info!(
                    "Accepting ws connection from {:?}",
                    socket.peer_addr().unwrap()
                );
                TcpOrDestination::Tcp(socket)
            }
            Direction::ClientSide => {
                let proto_addition = if &dest_location[..2] != "ws" {
                    "ws://"
                } else {
                    ""
                };
                TcpOrDestination::Dest(proto_addition.to_owned() + dest_location)
            }
        };
        let in2 = match dir {
            Direction::ServerSide => TcpOrDestination::Dest(dest_location.to_owned()),
            Direction::ClientSide => {
                let (socket, _) = listener
                    .accept()
                    .await
                    .expect("Could not accept connection?");
                let peer_addr = match socket.peer_addr() {
                    Ok(addr)=> addr,
                    Err(error)=> {
                        error!("{}", error);
                        return Err(Box::new(error));
                    }
                };

                info!(
                    "Accepting tcp connection from {:?}",
                    peer_addr
                );
                let connection_status = ConnectionStatus {
                    status: ConnectionStatusCode::NEW,
                    address: String::new(),
                    last_active: SystemTime::now(),
                    bytes_got: 0,
                    bytes_sent: 0
                };
                let con_status_map_ = con_status_map.clone();
                let mut status = con_status_map_.lock().await;
                status.insert(socket.peer_addr().unwrap().port(), connection_status);
                TcpOrDestination::Tcp(socket)
            }
        };
        match communicate(in1, in2, con_status_map.clone()).await {
            Ok(_v) => {
                info!("Succesfully setup communication.");
            }
            Err(e) => {
                error!(
                    "Failed to connect to server {:?} (dest: {})",
                    e, dest_location
                );
            }
        };
    }
}

