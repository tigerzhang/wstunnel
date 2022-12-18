use std::sync::Arc;
use futures_util::stream::{SplitSink, SplitStream};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;

pub async fn handle_client_socks5_request(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut OwnedReadHalf,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>
) {

}

pub async fn handle_server_socks5_request(
    buf: &Vec<u8>,
    n: usize,
    tcp_read: &mut OwnedReadHalf,
    tcp_write: &mut Arc<Mutex<OwnedWriteHalf>>,
    ws_read: &mut Arc<Mutex<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    ws_write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>
) {

}