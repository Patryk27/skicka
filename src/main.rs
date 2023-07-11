use async_stream::stream;
use clap::Parser;
use futures::{FutureExt, Stream, StreamExt};
use hyper::body::Bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{header, Body, Error, Method, Request, Response, Server, StatusCode};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use tokio::{pin, select, time};
use ubyte::ByteUnit;

#[derive(Debug, Parser)]
struct Args {
    #[clap(long)]
    #[clap(default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    #[clap(long)]
    remote: Option<String>,

    #[clap(long)]
    motto: Option<String>,

    #[clap(long)]
    #[clap(default_value = "120s")]
    #[arg(value_parser = parse_duration)]
    initial_timeout: Duration,

    #[clap(long)]
    #[clap(default_value = "60s")]
    #[arg(value_parser = parse_duration)]
    chunk_timeout: Duration,

    #[clap(long)]
    #[clap(default_value = "8GB")]
    #[arg(value_parser = parse_storage)]
    max_transfer_size: u64,

    #[clap(long)]
    #[clap(default_value = "1KB")]
    #[arg(value_parser = parse_storage)]
    max_uri_length: u64,

    #[clap(long)]
    #[clap(default_value = "512")]
    max_active_connections: usize,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let listen = args.listen.clone();

    let state = Arc::new(State {
        args,
        connections: Default::default(),
        next_connection_idx: Default::default(),
    });

    let service = make_service_fn(move |_| {
        let state = state.clone();

        async move { Ok::<_, Error>(service_fn(move |request| handle(state.clone(), request))) }
    });

    println!(r#"   _____ _    _      _         "#);
    println!(r#"  / ____| |  (_)    | |        "#);
    println!(r#" | (___ | | ___  ___| | ____ _ "#);
    println!(r#"  \___ \| |/ / |/ __| |/ / _` |"#);
    println!(r#"  ____) |   <| | (__|   < (_| |"#);
    println!(r#" |_____/|_|\_\_|\___|_|\_\__,_|"#);
    println!();
    println!("Started; listening at: {}", listen);
    println!();

    Server::bind(&listen).serve(service).await.unwrap();
}

struct State {
    args: Args,
    connections: Mutex<HashMap<String, Connection>>,
    next_connection_idx: AtomicUsize,
}

struct Connection {
    idx: usize,
    file_name: Option<String>,
    file_body: Box<dyn Stream<Item = Result<Bytes, Error>> + Send + Unpin>,
    on_completed: oneshot::Sender<()>,
}

async fn handle(state: Arc<State>, request: Request<Body>) -> Result<Response<Body>, Error> {
    if let Err(response) = validate_request(&state, &request) {
        return Ok(response);
    }

    println!("Handling: {} {}", request.method(), request.uri().path());

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/") => {
            return handle_index(state);
        }

        (&Method::POST, "/") => {
            return handle_send(state, request).await;
        }

        (&Method::GET, id) => {
            if let Some(id) = id.strip_prefix('/') {
                return handle_recv(state, id.to_string()).await;
            }
        }

        _ => {}
    }

    Ok(err_not_found(None))
}

fn validate_request(state: &State, request: &Request<Body>) -> Result<(), Response<Body>> {
    if request.uri().path().len() as u64 >= state.args.max_uri_length {
        return Err(err_payload_too_large());
    }

    if request.uri().query().map_or(0, |query| query.len()) as u64 >= state.args.max_uri_length {
        return Err(err_payload_too_large());
    }

    Ok(())
}

fn handle_index(state: Arc<State>) -> Result<Response<Body>, Error> {
    let body = state.args.motto.clone().map(Body::from).unwrap_or_default();

    Ok(Response::new(body))
}

async fn handle_send(state: Arc<State>, request: Request<Body>) -> Result<Response<Body>, Error> {
    let file_name = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "name")
            .map(|(_, value)| value.to_string())
    });

    let file_body = Box::new(request.into_body());

    // ---

    let mut connections = state.connections.lock().await;

    if connections.len() >= state.args.max_active_connections {
        println!("[-] connection rejected (too many active connections)");

        return Ok(err_server_overloaded());
    }

    let id = {
        let mut tries = 0;

        loop {
            tries += 1;

            if tries >= 64 {
                println!("[-] connection rejected (failed to generate name)");

                return Ok(err_server_overloaded());
            }

            let id = names::Generator::default().next().unwrap();

            if !connections.contains_key(&id) {
                break id;
            }
        }
    };

    let idx = state.next_connection_idx.fetch_add(1, Ordering::Relaxed);
    let (on_completed_tx, on_completed_rx) = oneshot::channel();

    let connection = Connection {
        idx,
        file_name,
        file_body,
        on_completed: on_completed_tx,
    };

    println!(
        "[{}/{}] connection created; active connections: {}",
        id,
        idx,
        1 + connections.len()
    );

    connections.insert(id.clone(), connection);

    drop(connections);

    // ---

    let response = if let Some(remote) = &state.args.remote {
        format!("{}/{}", remote, id)
    } else {
        id.clone()
    };

    let response = futures::stream::iter(Some(Ok::<_, Error>(format!("{}\r\n", response))))
        .chain(on_completed_rx.into_stream().map(|_| Ok(String::default())));

    // ---

    tokio::spawn(async move {
        time::sleep(state.args.initial_timeout).await;

        let mut connections = state.connections.lock().await;

        let is_stale = connections
            .get(&id)
            .map_or(false, |connection| connection.idx == idx);

        if is_stale {
            connections.remove(&id);

            println!(
                "[{}/{}] connection reaped; active connections: {}",
                id,
                idx,
                connections.len()
            );
        }
    });

    Ok(Response::new(Body::wrap_stream(response)))
}

async fn handle_recv(state: Arc<State>, id: String) -> Result<Response<Body>, Error> {
    let Some(connnection) = state.connections.lock().await.remove(&id) else {
        return Ok(err_not_found("no such connection found\r\n"));
    };

    let Connection {
        idx,
        file_name,
        mut file_body,
        on_completed,
    } = connnection;

    println!("[{}/{}] connection fused", id, idx);

    let stream = stream! {
        let mut transferred_bytes = 0;

        let reason = loop {
            let timeout = time::sleep(state.args.chunk_timeout);

            pin!(timeout);

            select! {
                chunk = file_body.next() => {
                    if let Some(chunk) = chunk {
                        if let Ok(chunk) = &chunk {
                            transferred_bytes += chunk.len() as u64;

                            if transferred_bytes >= state.args.max_transfer_size {
                                break "reached transfer size limit";
                            }
                        }

                        yield chunk;
                    } else {
                        break "transfer completed";
                    }
                }

                _ = &mut timeout => {
                    break "reached chunk time limit";
                }
            }
        };

        println!(
            "[{}/{}] connection closed ({}); active connections: {}",
            id,
            idx,
            reason,
            state.connections.lock().await.len(),
        );

        _ = on_completed.send(());

    };

    let mut response = Response::builder();

    if let Some(file_name) = file_name {
        response = response.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file_name),
        );
    }

    let response = response.body(Body::wrap_stream(stream)).unwrap();

    Ok(response)
}

fn err_not_found(body: impl Into<Option<&'static str>>) -> Response<Body> {
    let body = body.into().map(Body::from).unwrap_or_default();

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(body)
        .unwrap()
}

fn err_payload_too_large() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .body(Body::default())
        .unwrap()
}

fn err_server_overloaded() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from("server overloaded, please try again later"))
        .unwrap()
}

fn parse_duration(arg: &str) -> Result<Duration, humantime::DurationError> {
    arg.parse::<humantime::Duration>().map(Into::into)
}

fn parse_storage(arg: &str) -> Result<u64, String> {
    arg.parse::<ByteUnit>()
        .map(|u| u.as_u64())
        .map_err(|err| err.to_string())
}
