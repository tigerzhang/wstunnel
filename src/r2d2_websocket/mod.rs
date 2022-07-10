use tokio::net::{TcpStream};
use tokio::runtime::Runtime;

use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::{connect_async};
use tokio_tungstenite::tungstenite::error::Error;
use tokio_tungstenite::tungstenite::handshake::client::Response;

pub struct WebsocketConnectionManager {
    url: String,
}

impl WebsocketConnectionManager {
    pub fn new(url: String) -> WebsocketConnectionManager {
        WebsocketConnectionManager {
            url,
        }
    }
}

impl r2d2::ManageConnection for WebsocketConnectionManager {
    type Connection = (WebSocketStream<MaybeTlsStream<TcpStream>>, Response);
    type Error = Error;

    fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let mut runtime = Runtime::new().unwrap();
        runtime.block_on(connect_async(self.url.clone()))
    }

    fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        Ok(())
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        false
    }
}