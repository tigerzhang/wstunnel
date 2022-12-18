use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use futures_util::SinkExt;
use log::{debug, error, info, trace, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{MaybeTlsStream, tungstenite, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;
use crate::{common, ConnectionStatus, ConnectionStatusCode};
use crate::common::{ConStatus, Direction, Address};
use crate::ConnectionStatusCode::CONNECTED;
use std::time::SystemTime;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::StreamExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot::{Receiver, Sender};
use tokio::task::JoinHandle;

pub async fn serve_ws_to_tcp(
    bind_location: &str,
    dest_location: &str,
    dir: &Direction,
    con_status_map: ConStatus
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(bind_location)
        .await
        .expect("Could not bind to port");
    info!("Successfully bound to {:?}", bind_location);

    loop {
        let dir_clone = (*dir).clone();
        let con_status_map_clone = con_status_map.clone();
        let (socket, _) = listener
            .accept()
            .await
            .expect("Could not accept connection?");

        info!("Accepting ws connection from {:?}",socket.peer_addr().unwrap());
        info!("websocket accepting");
        let mut ws = tokio_tungstenite::accept_async(tokio_tungstenite::MaybeTlsStream::Plain(socket)).await?;
        // Try to setup the tcp stream we'll be communicating to, if this fails, we close the websocket.
        let tcp = match TcpStream::connect(dest_location).await {
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
        };
        let tcp_remote_port = tcp.peer_addr().unwrap().port();

        // We got the tcp connection setup, split both streams in their read and write parts
        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let (mut ws_write, mut ws_read) = ws.split();
        let (shutdown_from_ws_tx, mut shutdown_from_ws_rx) = tokio::sync::oneshot::channel::<bool>();
        let (shutdown_from_tcp_tx, mut shutdown_from_tcp_rx) = tokio::sync::oneshot::channel::<bool>();

        let address = Arc::new(Mutex::new(String::from("xxx")));

        let address1 = address.clone();
        let con_status_map1 = con_status_map_clone.clone();
        let dir_clone_1 = dir_clone.clone();
        let task_ws_to_tcp = tokio::spawn(async move {
            ws_to_tcp_task(&dir_clone_1, tcp_remote_port, tcp_write, ws_read, shutdown_from_ws_tx, shutdown_from_tcp_rx, address1, con_status_map1).await
        });
        let address2 = address.clone();
        let con_status_map2 = con_status_map_clone.clone();
        // Consume from the tcp socket and write on the websocket.
        let dir_clone_2 = dir_clone.clone();
        let task_tcp_to_ws = tokio::spawn(async move {
            tcp_to_ws_task(&dir_clone_2, tcp_remote_port, tcp_read, ws_write, shutdown_from_ws_rx, shutdown_from_tcp_tx, address, address2, con_status_map2).await
        });

        let dir_clone_3 = dir_clone.clone();
        // Finally, the cleanup task, all it does is close down the tcp connections.
        tokio::spawn(async move {
            clean_up_task(&dir_clone_3, con_status_map_clone, &tcp_remote_port, task_ws_to_tcp, task_tcp_to_ws).await;
        });
    }
    Ok(())
}

async fn clean_up_task(dir: &Direction, con_status_map_clone: Arc<Mutex<HashMap<u16, ConnectionStatus>>>, tcp_remote_port: &u16, task_ws_to_tcp: JoinHandle<(OwnedWriteHalf, SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>)>, task_tcp_to_ws: JoinHandle<(OwnedReadHalf, SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>, bool)>) {
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
    match Arc::try_unwrap(dest_write) {
        Ok(u) => {
            // let u = Arc::try_unwrap(dest_write_clone).unwrap();
            // let dest_write_clone_2 = dest_write_clone.lock().await.into_inner();
            let o = u.into_inner();
            let mut tcp_stream = dest_read.reunite(o).unwrap();
            if let Err(ref v) = tcp_stream.shutdown().await {
                error!(
                "Error properly closing the tcp from {:?}: {:?}",
                tcp_stream.peer_addr(),
                v
            );
                return;
            }
        }
        Err(e) => {
            error!("Could not unwrap the Arc, this should never happen {:?}", e);
        }
    }

    if ws_need_close {
        let u = Arc::try_unwrap(read);
        match u {
            Ok(u) => {
                let o = u.into_inner();
                let mut ws_stream = o.reunite(write).unwrap();
                if let Err(ref v) = ws_stream.get_mut().shutdown().await {
                    error!(
                    "Error properly closing the ws from {:?}: {:?}",
                    "something", v
                );
                    return;
                }
            }
            Err(e) => {
                error!("Could not unwrap the Arc, this should never happen {:?}", e);
            }
        }
    }
    debug!("Properly closed connections.");

    let con_map = con_status_map_clone.clone();
    let mut con = con_map.lock().await;
    con.remove(&tcp_remote_port);
}

async fn tcp_to_ws_debug(buf: &Vec<u8>, n: usize, address2: Arc<Mutex<String>>) {
    if n < 11 {
        debug!("tcp read {:?}", &buf[0..n]);
    } else {
        let addr = Arc::clone(&address2);
        debug!("tcp read {} bytes {} ", n, addr.lock().unwrap());
    }
}

async fn tcp_to_ws_status(con_status_map2: Arc<Mutex<HashMap<u16, ConnectionStatus>>>, address2: Arc<Mutex<String>>, tcp_remote_port: u16, buf: &Vec<u8>, n: usize) {
    {
        let con_map = con_status_map2.clone();
        let mut status = con_map.lock().unwrap();
        if status.contains_key(&tcp_remote_port) {
            let status = status.get_mut(&tcp_remote_port).unwrap();
            status.bytes_sent += n as u32;
        }
    }

    if n > 3 && buf[0] == 5 && buf[1] == 1 {
        // buf[2] reserved
        let addr = Arc::clone(&address2);
        let addr_str = Address::read_from_buf(&buf, 3).await.unwrap().to_string();
        let mut value = addr.lock().unwrap();
        // *value = "abc".to_string();
        *value = addr_str.clone();

        info!("Connecting {}. remote port {}", addr_str.clone(), tcp_remote_port);

        {
            let con_map = con_status_map2.clone();
            let mut status = con_map.lock().unwrap();
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
}

async fn tcp_to_ws_task(dir: &Direction, tcp_remote_port: u16, mut tcp_read: OwnedReadHalf, mut ws_write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>, mut shutdown_from_ws_rx: Receiver<bool>, shutdown_from_tcp_tx: Sender<bool>, address: Arc<Mutex<String>>, address2: Arc<Mutex<String>>, con_status_map2: Arc<Mutex<HashMap<u16, ConnectionStatus>>>) -> (OwnedReadHalf, SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>, bool) {
    let mut need_close = true;
    let mut buf = vec![0; 1024 * 1024];
    loop {
        tokio::select! {
            res = tcp_read.read(&mut buf) => {
                // Return value of `Ok(0)` signifies that the remote has closed, if this happens
                // we want to initiate shutting down the websocket.
                match res {
                    Ok(0) => {
                        let addr = Arc::clone(&address2);
                        warn!("tcp read 0 byte. Remote tcp socket has closed, sending close message on websocket. {}", addr.lock().unwrap());
                        break;
                        // debug!("tcp read 0 byte");
                    }
                    Ok(n) => {
                        tcp_to_ws_debug(&buf, n, address2.clone()).await;

                        tcp_to_ws_status(con_status_map2.clone(), address2.clone(), tcp_remote_port, &buf, n).await;

                        let _addr = Arc::clone(&address2);
                        // debug!("ws send {} bytes", n);
                        let res = buf[..n].to_vec();
                        match ws_write.send(Message::Binary(res)).await {
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
        if let Err(v) = ws_write.send(Message::Close(None)).await {
            error!("Failed to send close message to websocket: {:?}", v);
        }
    }
    if let Err(_) = shutdown_from_tcp_tx.send(true) {
        // This happens if the shutdown happens from the other side.
        // error!("Could not send shutdown signal: {:?}", v);
    }
    warn!("Reached end of consume from tcp. {}", address.lock().unwrap());
    (tcp_read, ws_write, need_close)
}

async fn ws_to_tcp_status(con_status_map1: Arc<Mutex<HashMap<u16, ConnectionStatus>>>, tcp_remote_port: u16, x: &[u8]) {
    let con_map = con_status_map1.clone();
    let mut status = con_map.lock().unwrap();
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

async fn ws_to_tcp_debug(x: &[u8]) {
    if x.len() < 11 {
        debug!("ws got {:?}", x);
    }
}

async fn ws_to_tcp_task(dir: &Direction, tcp_remote_port: u16, mut tcp_write: OwnedWriteHalf, mut ws_read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>, shutdown_from_ws_tx: Sender<bool>, mut shutdown_from_tcp_rx: Receiver<bool>, address1: Arc<Mutex<String>>, con_status_map1: Arc<Mutex<HashMap<u16, ConnectionStatus>>>) -> (OwnedWriteHalf, SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) {
    loop {
        tokio::select! {
            message = ws_read.next() => {
                if message.is_none() {
                    debug!("Got none, end of websocket stream.");
                    break;
                }
                let message = message.unwrap();
                let data = match message {
                    Ok(v) => v,
                    Err(p) => {
                        let addr = Arc::clone(&address1);
                        debug!("Err reading data {:?} {}", p, addr.lock().unwrap());
                        // dest_write.shutdown().await?;
                        break;
                    }
                };
                match data {
                    Message::Binary(ref x) => {
                        let addr = Arc::clone(&address1);
                        debug!("ws got {} bytes from {}", x.len(), addr.lock().unwrap());

                        ws_to_tcp_debug(x).await;

                        if tcp_write.write(x).await.is_err() {
                            break;
                        };

                        ws_to_tcp_status(con_status_map1.clone(), tcp_remote_port, x).await;

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
    debug!("Reached end of consume from websocket. {}", address1.lock().unwrap());
    if let Err(_) = shutdown_from_ws_tx.send(true) {
        // This happens if the shutdown happens from the other side.
        // error!("Could not send shutdown signal: {:?}", v);
    }
    (tcp_write, ws_read)
}

pub async fn serve_tcp_to_ws(
    bind_location: &str,
    dest_location: &str,
    dir: &Direction,
    con_status_map: ConStatus
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(bind_location)
        .await
        .expect("Could not bind to port");
    info!("Successfully bound to {:?}", bind_location);
    loop {
        let dir_clone = (*dir).clone();
        let con_status_map_clone = con_status_map.clone();
        let proto_addition = if &dest_location[..2] != "ws" {
            "ws://"
        } else {
            ""
        };
       let dest_location = proto_addition.to_owned() + dest_location;

        let (mut tcp, _) = listener
            .accept()
            .await
            .expect("Could not accept connection?");
        let peer_addr = match tcp.peer_addr() {
            Ok(addr)=> addr,
            Err(error)=> {
                error!("{}", error);
                return Err(Box::new(error));
            }
        };

        info!("Accepting tcp connection from {:?}",peer_addr);

        let connection_status = ConnectionStatus {
            status: ConnectionStatusCode::NEW,
            address: String::new(),
            last_active: SystemTime::now(),
            bytes_got: 0,
            bytes_sent: 0
        };
        let con_status_map_clone = con_status_map.clone();
        // let mut status = con_status_map_clone_2.lock().unwrap();
        con_status_map_clone.lock().unwrap().insert(tcp.peer_addr().unwrap().port(), connection_status);

        info!("connecting {}", dest_location);
        let (ws, _) = match tokio_tungstenite::connect_async_trust_certificate(dest_location).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Something went wrong connecting {:?}", e);
                tcp.shutdown().await?;
                return Err(Box::new(e));
            }
        };

        let tcp_remote_port = tcp.peer_addr().unwrap().port();

        // We got the tcp connection setup, split both streams in their read and write parts
        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let (mut ws_write, mut ws_read) = ws.split();
        let (shutdown_from_ws_tx, mut shutdown_from_ws_rx) = tokio::sync::oneshot::channel::<bool>();
        let (shutdown_from_tcp_tx, mut shutdown_from_tcp_rx) = tokio::sync::oneshot::channel::<bool>();

        let address = Arc::new(Mutex::new(String::from("xxx")));

        let address1 = address.clone();
        let con_status_map1 = con_status_map_clone.clone();
        let dir_clone_1 = dir_clone.clone();
        let task_ws_to_tcp = tokio::spawn(async move {
            ws_to_tcp_task(&dir_clone_1, tcp_remote_port, tcp_write, ws_read, shutdown_from_ws_tx, shutdown_from_tcp_rx, address1, con_status_map1).await
        });
        let address2 = address.clone();
        let con_status_map2 = con_status_map_clone.clone();
        // Consume from the tcp socket and write on the websocket.
        let dir_clone_2 = dir_clone.clone();
        let task_tcp_to_ws = tokio::spawn(async move {
            tcp_to_ws_task(&dir_clone_2, tcp_remote_port, tcp_read, ws_write, shutdown_from_ws_rx, shutdown_from_tcp_tx, address, address2, con_status_map2).await
        });

        let dir_clone_3 = dir_clone.clone();
        // Finally, the cleanup task, all it does is close down the tcp connections.
        tokio::spawn(async move {
            clean_up_task(&dir_clone_3, con_status_map_clone, &tcp_remote_port, task_ws_to_tcp, task_tcp_to_ws).await;
        });
    }
    Ok(())
}

pub async fn serve_separate(
    bind_location: &str,
    dest_location: &str,
    dir: &common::Direction,
    con_status_map: ConStatus
) -> Result<(), Box<dyn std::error::Error>> {
    match dir {
        Direction::WsToTcp => {
            serve_ws_to_tcp(bind_location, dest_location, dir, con_status_map.clone()).await
        }
        Direction::TcpToWs => {
            serve_tcp_to_ws(bind_location, dest_location, dir, con_status_map.clone()).await
        }
    }
}