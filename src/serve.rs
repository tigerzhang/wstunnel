use std::collections::HashMap;
use std::mem::take;
use std::sync::{Arc};
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
use tokio::sync::Mutex;
use tokio::sync::oneshot::{Receiver, Sender};
use tokio::task::JoinHandle;

use crate::proxy_socks5;

pub async fn serve_server_side(
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
        let (wait_for_socks_init_sender, mut wait_for_socks_init_receiver) = tokio::sync::mpsc::channel(5);
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

        let tcp_write_arc = Arc::new(Mutex::new(tcp_write));
        let tcp_write_clone = tcp_write_arc.clone();
        let tcp_read_arc = Arc::new(Mutex::new(tcp_read));
        let tcp_read_arc_clone = tcp_read_arc.clone();
        let ws_read_arc = Arc::new(Mutex::new(ws_read));
        let ws_read_clone = ws_read_arc.clone();
        let ws_write_arc = Arc::new(Mutex::new(ws_write));
        let ws_write_arc_clone = ws_write_arc.clone();

        let task_ws_to_tcp = tokio::spawn(async move {
            handle_ws_incoming_task(&dir_clone_1, tcp_remote_port, tcp_read_arc_clone, tcp_write_clone, ws_read_clone, ws_write_arc_clone, shutdown_from_ws_tx, shutdown_from_tcp_rx, address1, con_status_map1, Some(wait_for_socks_init_sender)).await
        });
        let address2 = address.clone();
        let con_status_map2 = con_status_map_clone.clone();
        // Consume from the tcp socket and write on the websocket.
        let dir_clone_2 = dir_clone.clone();

        let tcp_write_clone_2 = tcp_write_arc.clone();
        let tcp_read_arc_clone = tcp_read_arc.clone();
        let ws_read_clone_2 = ws_read_arc.clone();
        let ws_write_arc_clone = ws_write_arc.clone();

        let task_tcp_to_ws = tokio::spawn(async move {
            // let _ = wait_for_socks_init_receiver.recv().await;
            // debug!("wait_for_socks_init_receiver recv");
            handle_tcp_incoming_task(&dir_clone_2, tcp_remote_port, tcp_read_arc_clone, tcp_write_clone_2, ws_read_clone_2, ws_write_arc_clone, shutdown_from_ws_rx, shutdown_from_tcp_tx, address, address2, con_status_map2).await
        });

        let dir_clone_3 = dir_clone.clone();
        // Finally, the cleanup task, all it does is close down the tcp connections.
        tokio::spawn(async move {
            clean_up_task(&dir_clone_3, con_status_map_clone, &tcp_remote_port, task_ws_to_tcp, task_tcp_to_ws).await;
        });
    }
    Ok(())
}

async fn clean_up_task(
    dir: &Direction,
    con_status_map_clone: Arc<Mutex<HashMap<u16, ConnectionStatus>>>,
    tcp_remote_port: &u16,
    task_ws_to_tcp: JoinHandle<(Arc<Mutex<OwnedWriteHalf>>, Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>)>,
    task_tcp_to_ws: JoinHandle<(Arc<Mutex<OwnedReadHalf>>, Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>, bool)>
) {
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
        Ok(dest_write_takeout) => {
            // let u = Arc::try_unwrap(dest_write_clone).unwrap();
            // let dest_write_clone_2 = dest_write_clone.lock().await.into_inner();
            let dest_write_inner = dest_write_takeout.into_inner();
            match Arc::try_unwrap(dest_read) {
                Ok(dest_read_takeout) => {
                    let dest_read_inner = dest_read_takeout.into_inner();
                    let mut tcp_stream = dest_read_inner.reunite(dest_write_inner).unwrap();
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
                    error!("Error taking out dest_read: {:?}", e);
                }
            }
        }
        Err(e) => {
            error!("Could not unwrap the Arc, this should never happen {:?}", e);
        }
    }

    if ws_need_close {
        match Arc::try_unwrap(read) {
            Ok(read_takeout) => {
                let read_inner = read_takeout.into_inner();
                match Arc::try_unwrap(write) {
                    Ok(write_takeout) => {
                        let write_inner = write_takeout.into_inner();
                        let mut ws_stream = read_inner.reunite(write_inner).unwrap();
                        let r = ws_stream.get_ref();
                        if let Err(ref v) = ws_stream.close(None).await {
                            let r = ws_stream.get_ref();
                            match r {
                                MaybeTlsStream::Plain(t) => {
                                    error!(
                                        "Error properly closing the websocket from {:?}: {:?}",
                                        t.peer_addr(),
                                        v
                                    );
                                }
                                MaybeTlsStream::NativeTls(_) => {}
                                _ => {}
                            }
                            return;
                        }
                    }
                    Err(e) => {
                        error!("Error taking out write: {:?}", e);
                    }
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
        debug!("tcp read {} bytes {} ", n, addr.lock().await);
    }
}

async fn tcp_to_ws_status(con_status_map2: Arc<Mutex<HashMap<u16, ConnectionStatus>>>, address2: Arc<Mutex<String>>, tcp_remote_port: u16, buf: &Vec<u8>, n: usize) {
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
}

async fn handle_tcp_incoming_task(
    dir: &Direction,
    tcp_remote_port: u16,
    mut tcp_read: Arc<Mutex<OwnedReadHalf>>,
    mut tcp_write: Arc<Mutex<OwnedWriteHalf>>,
    mut ws_read: Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    mut ws_write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
    mut shutdown_from_ws_rx: Receiver<bool>,
    shutdown_from_tcp_tx: Sender<bool>,
    address: Arc<Mutex<String>>,
    address2: Arc<Mutex<String>>,
    con_status_map2: Arc<Mutex<HashMap<u16, ConnectionStatus>>>
) -> (Arc<tokio::sync::Mutex<OwnedReadHalf>>, Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>, bool) {
    let mut need_close = true;
    let mut buf = vec![0; 1024 * 1024];
    let mut need_init_state = true;
    loop {
        debug!("start tcp_to_ws_task loop");
        let tcp_read_clone = tcp_read.clone();
        let mut tcp_read_lock = tcp_read_clone.lock().await;
        tokio::select! {
            res = tcp_read_lock.read(&mut buf) => {
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
                        debug!("tcp read {} bytes", n);
                        let mut ignore_to_next_packet = false;
                        match dir {
                            Direction::ClientSide => {
                                if need_init_state {
                                    need_init_state = false;
                                    // handle client socks5 requests like a socks5 proxy
                                    let r = proxy_socks5::handle_client_socks5_greeting(&buf, n, &mut tcp_read, &mut tcp_write, &mut ws_read, &mut ws_write).await;
                                    if r.is_ok() {
                                        ignore_to_next_packet = true;
                                    }
                                }
                            }
                            Direction::ServerSide => {
                                // sink the socks5 server greeting message [5, 0]
                                // proxy_socks5::handle_server_socks5_request(&buf, n, &mut tcp_read, &mut tcp_write, &mut ws_read, &mut ws_write).await;
                                if need_init_state {
                                    need_init_state = false;
                                    match proxy_socks5::handle_server_side_socks5_response(&buf, n, &mut tcp_read, &mut tcp_write, &mut ws_read, &mut ws_write).await {
                                        Ok(_) => {
                                            ignore_to_next_packet = true;
                                        }
                                        Err(e) => {
                                            error!("Error handling server side socks5 response: {:?}", e);
                                        }
                                    }
                                }
                            }
                        }
                        if ignore_to_next_packet == false && need_init_state == false {
                            tcp_to_ws_debug(&buf, n, address2.clone()).await;

                            tcp_to_ws_status(con_status_map2.clone(), address2.clone(), tcp_remote_port, &buf, n).await;

                            let _addr = Arc::clone(&address2);
                            let res = buf[..n].to_vec();
                            match ws_write.lock().await.send(Message::Binary(res)).await {
                                Ok(_) => {
                                    debug!("ws send {} bytes", n);
                                    continue;
                                }
                                Err(v) => {
                                    debug!("Failed to send binary data on ws: {:?}", v);
                                    break;
                                }
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
        if let Err(v) = ws_write.lock().await.send(Message::Close(None)).await {
            error!("Failed to send close message to websocket: {:?}", v);
        }
    }
    if let Err(_) = shutdown_from_tcp_tx.send(true) {
        // This happens if the shutdown happens from the other side.
        // error!("Could not send shutdown signal: {:?}", v);
    }
    warn!("Reached end of consume from tcp. {}", address.lock().await);
    (tcp_read, ws_write, need_close)
}

async fn ws_to_tcp_status(con_status_map1: Arc<Mutex<HashMap<u16, ConnectionStatus>>>, tcp_remote_port: u16, x: &[u8]) {
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

async fn ws_to_tcp_debug(x: &[u8]) {
    if x.len() < 11 {
        debug!("ws got {:?}", x);
    }
}

async fn handle_ws_incoming_task(
    dir: &Direction,
    tcp_remote_port: u16,
    mut tcp_read: Arc<Mutex<OwnedReadHalf>>,
    mut tcp_write: Arc<Mutex<OwnedWriteHalf>>,
    mut ws_read: Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    mut ws_write: Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
    shutdown_from_ws_tx: Sender<bool>,
    mut shutdown_from_tcp_rx: Receiver<bool>,
    address1: Arc<Mutex<String>>,
    con_status_map1: Arc<Mutex<HashMap<u16, ConnectionStatus>>>,
    wait_for_socks_init_sender: Option<tokio::sync::mpsc::Sender<bool>>
) -> (Arc<Mutex<OwnedWriteHalf>>, Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>) {
    let wait_for_socks_init_sender_clone = wait_for_socks_init_sender.clone();
    let mut init_state = true;
    loop {
        let ws_read_clone = ws_read.clone();
        let ws_read_lock = ws_read_clone.lock().await;
        let mut ws_read_lock_unwrap = ws_read_lock;
        tokio::select! {
            message = ws_read_lock_unwrap.next() => {
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
                let mut ignore_to_next_packet = false;
                if init_state == false {
                    ignore_to_next_packet = false;
                }
                match data {
                    Message::Binary(ref x) => {
                        debug!("ws got {}", x.len());
                        match dir {
                            Direction::ClientSide => {
                                // handle client socks5 requests like a socks5 proxy
                                // proxy_socks5::handle_client_socks5_request(&buf, n, &mut tcp_read, &mut tcp_write, &mut ws_read, &mut ws_write).await;
                                init_state = false;
                            }
                            Direction::ServerSide => {
                                // handle server socks5 requests like a socks5 client
                                if init_state {
                                    match proxy_socks5::handle_server_side_socks5_request(&x, x.len(), &mut tcp_read, &mut tcp_write, &mut ws_read, &mut ws_write).await {
                                       Ok(_) => {
                                            // ignore_to_next_packet = true;
                                        }
                                        Err(_) => {
                                            error!("Failed to handle server socks5 request");
                                        }
                                    }
                                    init_state = false;
                                    if let Some(ref sender) = wait_for_socks_init_sender_clone {
                                        debug!("send socks5 init done");
                                        let _ = sender.send(true).await;
                                    }
                                }
                            }
                        }
                        debug!("ignore_to_next_packet {} init_state {}", ignore_to_next_packet, init_state);
                        if ignore_to_next_packet == false && init_state == false {
                            let addr = Arc::clone(&address1);
                            debug!("ws got {} bytes from {}", x.len(), addr.lock().await);

                            ws_to_tcp_debug(x).await;

                            if tcp_write.lock().await.write(x).await.is_err() {
                                break;
                            };

                            ws_to_tcp_status(con_status_map1.clone(), tcp_remote_port, x).await;
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
    (tcp_write, ws_read)
}

pub async fn serve_client_side(
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
        con_status_map_clone.lock().await.insert(tcp.peer_addr().unwrap().port(), connection_status);

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

        let tcp_read_arc = Arc::new(Mutex::new(tcp_read));
        let tcp_read_arc_clone = tcp_read_arc.clone();
        let tcp_write_arc = Arc::new(Mutex::new(tcp_write));
        let tcp_write_arc_clone = tcp_write_arc.clone();
        let ws_read_arc = Arc::new(Mutex::new(ws_read));
        let ws_read_arc_clone = ws_read_arc.clone();
        let ws_write_arc = Arc::new(Mutex::new(ws_write));
        let ws_write_arc_clone = ws_write_arc.clone();

        let task_ws_to_tcp = tokio::spawn(async move {
            handle_ws_incoming_task(&dir_clone_1, tcp_remote_port, tcp_read_arc_clone, tcp_write_arc_clone, ws_read_arc_clone, ws_write_arc_clone, shutdown_from_ws_tx, shutdown_from_tcp_rx, address1, con_status_map1, None).await
        });

        let address2 = address.clone();
        let con_status_map2 = con_status_map_clone.clone();
        // Consume from the tcp socket and write on the websocket.
        let dir_clone_2 = dir_clone.clone();

        let tcp_read_arc_clone = tcp_read_arc.clone();
        let tcp_write_arc_clone = tcp_write_arc.clone();
        let ws_read_arc_clone = ws_read_arc.clone();
        let ws_write_arc_clone = ws_write_arc.clone();

        let task_tcp_to_ws = tokio::spawn(async move {
            handle_tcp_incoming_task(&dir_clone_2, tcp_remote_port, tcp_read_arc_clone, tcp_write_arc_clone, ws_read_arc_clone, ws_write_arc_clone, shutdown_from_ws_rx, shutdown_from_tcp_tx, address, address2, con_status_map2).await
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
        Direction::ServerSide => {
            serve_server_side(bind_location, dest_location, dir, con_status_map.clone()).await
        }
        Direction::ClientSide => {
            serve_client_side(bind_location, dest_location, dir, con_status_map.clone()).await
        }
    }
}