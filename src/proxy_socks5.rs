use std::sync::Arc;
use futures_util::stream::{SplitSink, SplitStream};
use log::{debug, info};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::{client, Message};
type Error = Box<dyn std::error::Error>;

// proxy 5, 1, 0 request
// 1. send 5, 1 response
// 2. don't forward 5,1,0 reqeust to upstream
pub async fn handle_client_socks5_greeting_request(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut Arc<Mutex<OwnedReadHalf>>,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>
) -> Result<(), Box<dyn std::error::Error>> {
    let mut arr = [0u8; 1024];
    for i in 0..n {
        arr[i] = buf[i];
    }
    let data_arr = arr.split_at(n).0;
    debug!("buf {:?}", data_arr);
    if (n == 3 && data_arr == [5, 1, 0])
    || (n == 4 && data_arr == [5, 2, 0, 1]) {
        // Client to server [5, 1, 0]
        // read exactly 3 bytes: [5, 1, 0]
        // let mut client_greeting = [0 as u8; 3];
        // tcp_read.read_exact(&mut client_greeting).await?;
        // debug!("client_greeting: {:?}", client_greeting);
        // Server to client [5, 0]
        let server_greeting = [0x05, 0x00];
        debug!("server_greeting: {:?}", server_greeting);
        tcp_write.lock().await.write_all(&server_greeting).await?;
        info!("Proxy client greeting done");
        return Ok(());
    }
    Err(Error::from("ignored"))
}

// proxy connect request
// 1. send 5, 0, 0, 1, 0, 0, 0, 0, 0, 0 response
// 2. forward connect request to upstream
//
// example:
// connect request [5, 1, 0, 3, 10, 103, 111, 111, 103, 108, 101, 46, 99, 111, 109, 0, 80]
// response [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]
//
pub async fn handle_client_socks5_connect_request(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut Arc<Mutex<OwnedReadHalf>>,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>
) -> Result<(), Box<dyn std::error::Error>> {
    if buf.len() > 5 {
        if buf[0] == 0x05 &&
            buf[1] == 0x01 && // connect
            buf[2] == 0x00 { // reserved
            let mut valid_address = false;
            if buf[3] == 0x01 {
                // IPV4 address
                valid_address = true;
            } else if buf[3] == 0x03 {
                // domain name
                valid_address = true;
            } else if buf[3] == 0x04 {
                // IPV6 address
                valid_address = true;
            }
            if valid_address {
                info!("Handle client connect request. Response client directly");
                // return response directly
                tcp_write.lock().await.write_all([5, 0, 0, 1, 0, 0, 0, 0, 0, 0].as_ref()).await?;
            }
        }
    }
    return Ok(())
}

pub async fn handle_server_side_socks5_request(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut Arc<Mutex<OwnedReadHalf>>,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>
) -> Result<(), Box<dyn std::error::Error>> {
    // client to server [5, 1, 0]
    // send [5, 1, 0] to server like a socks5 client
    let client_greeting = [0x05, 0x01, 0x00];
    debug!("client_greeting: {:?}", client_greeting);
    info!("Send client greeting to server");
    tcp_write.lock().await.write_all(&client_greeting).await?;
    // server to client [5, 0]
    // read exactly 2 bytes: [5, 0]
    // let mut server_greeting = [0 as u8; 2];
    // tcp_read.lock().await.read_exact(&mut server_greeting).await?;
    // debug!("server_greeting: {:?}", server_greeting);

    Ok(())
}

pub async fn handle_server_side_socks5_greeting_response(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut Arc<Mutex<OwnedReadHalf>>,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("handle_server_side_socks5_greeting_response");
    if n == 2 && buf[0] == 5 && buf[1] == 0 {
        debug!("server_response: [5, 1]");
    } else {
        return Err(Error::from("socks5 server greeting response [5, 1] not received"));
    }

    // [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]
    // let mut seq = [0 as u8; 10];

    // tcp_read.lock().await.read_exact(&mut seq).await?;

    info!("Proxy server greeting done");
    Ok(())
}

pub async fn handle_server_side_socks5_connect_response(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut Arc<Mutex<OwnedReadHalf>>,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("handle_server_side_socks5_response");
    if n == 10 && buf[0] == 5 && buf[1] == 0 {
        debug!("server_response: {:?}", buf.split_at(n).0);
    } else {
        return Err(Error::from("socks5 server connect response [5, 0, 0, 1, 0, 0, 0, 0, 0, 0] not received"));
    }

    info!("Got server connect response");
    Ok(())
}