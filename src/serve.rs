use std::sync::{Arc, Mutex};
use futures_util::SinkExt;
use log::{debug, error, info, trace, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite};
use tokio_tungstenite::tungstenite::Message;
use crate::common;
use crate::common::{ConStatus, Direction, Address};
use crate::ConnectionStatusCode::CONNECTED;
use std::time::SystemTime;
use futures_util::StreamExt;

pub async fn serve_ws_to_tcp(
    bind_location: &str,
    dest_location: &str,
    con_status_map: ConStatus
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(bind_location)
        .await
        .expect("Could not bind to port");
    info!("Successfully bound to {:?}", bind_location);

    loop {
        let con_status_map_clone = con_status_map.clone();
        let (socket, _) = listener
            .accept()
            .await
            .expect("Could not accept connection?");

        info!("Accepting ws connection from {:?}",socket.peer_addr().unwrap());
        info!("websocket accepting");
        let mut ws = accept_async(tokio_tungstenite::MaybeTlsStream::Plain(socket)).await?;
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
        let (mut dest_read, mut dest_write) = tcp.into_split();
        let (mut write, mut read) = ws.split();
        let (shutdown_from_ws_tx, mut shutdown_from_ws_rx) = tokio::sync::oneshot::channel::<bool>();
        let (shutdown_from_tcp_tx, mut shutdown_from_tcp_rx) = tokio::sync::oneshot::channel::<bool>();

        let address = Arc::new(Mutex::new(String::from("xxx")));

        let address1 = address.clone();
        let con_status_map1 = con_status_map_clone.clone();
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
                            debug!("Err reading data {:?} {}", p, addr.lock().unwrap());
                            // dest_write.shutdown().await?;
                            break;
                        }
                    };
                    match data {
                        Message::Binary(ref x) => {
                            let addr = Arc::clone(&address1);
                            debug!("ws got {} bytes from {}", x.len(), addr.lock().unwrap());
                            if x.len() < 11 {
                                debug!("ws got {:?}", x);
                            }
                            if dest_write.write(x).await.is_err() {
                                break;
                            };

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
            (dest_write, read)
        });
        let address2 = address.clone();
        let con_status_map2 = con_status_map_clone.clone();
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
                            warn!("tcp read 0 byte. Remote tcp socket has closed, sending close message on websocket. {}", addr.lock().unwrap());
                            break;
                            // debug!("tcp read 0 byte");
                        }
                        Ok(n) => {
                            if n < 11 {
                                debug!("tcp read {:?}", &buf[0..n]);
                            } else {
                                let addr = Arc::clone(&address2);
                                debug!("tcp read {} bytes {} ", n, addr.lock().unwrap());
                            }

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
            warn!("Reached end of consume from tcp. {}", address.lock().unwrap());
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

            let con_map = con_status_map_clone.clone();
            let mut con = con_map.lock().unwrap();
            con.remove(&tcp_remote_port);
        });
    }
    Ok(())
}

pub async fn serve_tcp_to_ws(
    bind_location: &str,
    dest_location: &str,
    con_status: ConStatus
) -> Result<(), Box<dyn std::error::Error>> {
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
            serve_ws_to_tcp(bind_location, dest_location, con_status_map.clone()).await
        }
        Direction::TcpToWs => {
            serve_tcp_to_ws(bind_location, dest_location, con_status_map).await
        }
    }
}