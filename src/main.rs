use std::collections::HashMap;
use clap::{App, Arg};

use tokio;

use env_logger::{Builder, WriteStyle};
use log::{LevelFilter};

type Error = Box<dyn std::error::Error>;

mod common;
mod serve;
mod proxy_socks5;

use std::sync::{Arc};
use std::time::SystemTime;
use tokio::sync::Mutex;
use warp::Filter;
use crate::common::Direction;
// use console_subscriber;

#[derive(Debug)]
pub enum ConnectionStatusCode {
    NEW,
    CONNECTED,
    ERROR,
    CLOSED
}

#[derive(Debug)]
pub struct ConnectionStatus {
    status: ConnectionStatusCode,
    address: String,
    last_active: SystemTime,
    bytes_got: u32,
    bytes_sent: u32
}

// type ConStatus = Arc<Mutex<HashMap<u16, ConnectionStatus>>>;

// helpful example; https://github.com/snapview/tokio-tungstenite/issues/137

#[tokio::main]
async fn main() -> Result<(), Error>{
    #[cfg(feature = "console")]
    {
        console_subscriber::init();

        tracing::info!("console_subscriber enabled");
    }
    let app = App::new("Websocket Bridge")
        .about("Allows bridging a TCP connection over a websocket.")
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Verbosity, add more for more verbose output."),
        )
        .arg(
            Arg::with_name("mode")
                .possible_value("ws_to_tcp")
                .possible_value("tcp_to_ws")
                .takes_value(true)
                .required(true)
                .help("The direction of transfer."),
        )
        .arg(
            Arg::with_name("bind")
                .takes_value(true)
                .required(true)
                .help("ip:port to bind to."),
        )
        .arg(
            Arg::with_name("dest")
                .takes_value(true)
                .required(true)
                .help(
                    "ip:port to send to (for websockets; ip:port, [ws[s]://]example.com/sub/path)",
                ),
        )
        .arg(
            Arg::with_name("stat")
                .takes_value(true)
                .required(true)
                .help(
                    "port to bind for stat"
                )
        )
        ;

    let matches = app.clone().get_matches();

    let verbosity = matches.occurrences_of("v");
    let level = match verbosity {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        3 => LevelFilter::Debug,
        4 => LevelFilter::Trace,
        _ => {
            LevelFilter::Info
        }
    };

    let _stylish_logger = Builder::new()
        .filter(None, level)
        .write_style(WriteStyle::Always)
        .init();

    let bind_value = matches
        .value_of("bind")
        .ok_or(clap::Error::with_description(
            "Couldn't find bind value.",
            clap::ErrorKind::EmptyValue,
        )).unwrap().to_string();

    let dest_value = matches
        .value_of("dest")
        .ok_or(clap::Error::with_description(
            "Couldn't find dest value.",
            clap::ErrorKind::EmptyValue,
        )).unwrap().to_string();

    // let rt = tokio::runtime::Runtime::new().unwrap();

    let direction = match matches
        .value_of("mode")
        .ok_or(clap::Error::with_description(
            "Couldn't find mode value.",
            clap::ErrorKind::EmptyValue,
        ))? {
        "ws_to_tcp" => common::Direction::ServerSide,
        "tcp_to_ws" => common::Direction::ClientSide,
        &_ => {
            panic!("Got unknown direction, shouldn't be possible.");
        }
    };

    let stat_value = matches
        .value_of("stat")
        .ok_or(clap::Error::with_description(
            "Couldn't find stat value.",
            clap::ErrorKind::EmptyValue,
        )).unwrap().to_string();

    let con_status_map = Arc::new(Mutex::new(HashMap::new()));
    let con_status_map2 = con_status_map.clone();

    let _http = tokio::spawn(async move {
        let hello = warp::path!("hello" / String)
            .map(|name| format!("Hello, {}!", name));

        // let con_map = con_status_map2.lock().await;

        // let stat = warp::path("stat")
        //     .map(move || {
                // let mut ret: String = String::new();
                // for key in con_map.keys() {
                //     ret += &*format!("{}: {:?}\n", key, con_map.get(key).unwrap());
                // }
                // ret
            // });

        // let routes = warp::get().and(hello.or(stat));
        let routes = warp::get().and(hello);

        let stat_port: u16 = stat_value.trim().parse().expect("stat port error");
        warp::serve(routes)
            .run(([127, 0, 0, 1], stat_port))
            .await;
    });

    let tunnel = tokio::spawn(async move {
        match direction {
            Direction::ServerSide => {
                let _ = serve::serve_server_side(&bind_value, &dest_value, &direction, con_status_map).await;
            }
            Direction::ClientSide => {
                // let res = common::serve(&bind_value, &dest_value, &direction, con_status_map.clone()).await;
                // panic!("Serve returned with {:?}", res);
                let _ = serve::serve_client_side(&bind_value, &dest_value, &direction, con_status_map).await;
            }
        }
    });

    // 如果 tunnel 退出，整个 app 应该退出
    let _ = tokio::join!(tunnel);

    // TODO: 建立连接时，延迟大的原因：socks 握手带来的额外延迟。考虑本地处理 socks 握手。

    Ok(())
}
