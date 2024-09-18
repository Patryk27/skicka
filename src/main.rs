use anyhow::Result;
use axum::body::{Body, BodyDataStream};
use axum::extract::{Path, Query, State as AxumState};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::{Error, Router};
use clap::Parser;
use futures::{stream, FutureExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::{task, time};
use tokio_stream::wrappers::ReceiverStream;
use ubyte::ByteUnit;

#[derive(Debug, Parser)]
struct Args {
    #[clap(long)]
    #[clap(default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// Remote URL at which this service is installed; optional.
    ///
    /// It's used to prefix links shown to users, so that they know the entire
    /// download link at once instead of being shown just the randomized code.
    #[clap(long)]
    remote: Option<String>,

    /// Motto, printed during `GET /`; optional.
    #[clap(long)]
    motto: Option<String>,

    /// Maximum time between someone calling `POST /` and the corresponding
    /// `GET /:id`.
    ///
    /// This timeout doesn't include the transmission time, it's just about the
    /// time between creating the upload and *starting* the download.
    #[clap(long)]
    #[clap(default_value = "5m")]
    #[arg(value_parser = Self::parse_duration)]
    intent_timeout: Duration,

    /// Maximum time between retrieving and sending consecutive chunks.
    #[clap(long)]
    #[clap(default_value = "2m")]
    #[arg(value_parser = Self::parse_duration)]
    chunk_timeout: Duration,

    #[clap(long)]
    #[clap(default_value = "8GB")]
    #[arg(value_parser = Self::parse_storage)]
    max_transfer_size: u64,

    #[clap(long)]
    #[clap(default_value = "1024")]
    max_connections: usize,
}

impl Args {
    fn parse_duration(arg: &str) -> Result<Duration, humantime::DurationError> {
        arg.parse::<humantime::Duration>().map(Into::into)
    }

    fn parse_storage(arg: &str) -> Result<u64, String> {
        arg.parse::<ByteUnit>()
            .map(|u| u.as_u64())
            .map_err(|err| err.to_string())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!(r#"   _____ _    _      _         "#);
    println!(r#"  / ____| |  (_)    | |        "#);
    println!(r#" | (___ | | ___  ___| | ____ _ "#);
    println!(r#"  \___ \| |/ / |/ __| |/ / _` |"#);
    println!(r#"  ____) |   <| | (__|   < (_| |"#);
    println!(r#" |_____/|_|\_\_|\___|_|\_\__,_|"#);
    println!();
    println!("listening at {}", &args.listen);
    println!();

    let listener = TcpListener::bind(&args.listen).await?;

    let state = Arc::new(State {
        args,
        conns: Default::default(),
        next_conn_idx: Default::default(),
    });

    let app = Router::new()
        .route("/", get(handle_index).put(handle_send).post(handle_send))
        .route("/:id", get(handle_recv))
        .with_state(state);

    axum::serve(listener, app).await?;

    Ok(())
}

struct State {
    args: Args,
    conns: Mutex<HashMap<String, Conn>>,
    next_conn_idx: AtomicUsize,
}

struct Conn {
    idx: usize,
    name: Option<String>,
    body: BodyDataStream,
    on_completed: oneshot::Sender<()>,
}

async fn handle_index(state: AxumState<Arc<State>>) -> String {
    state.args.motto.clone().unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct SendQuery {
    name: Option<String>,
}

async fn handle_send(
    state: AxumState<Arc<State>>,
    query: Query<SendQuery>,
    body: Body,
) -> Result<Response, Response> {
    let mut conns = state.conns.lock().await;

    if conns.len() >= state.args.max_connections {
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

            if !conns.contains_key(&id) {
                break id;
            }
        }
    };

    let idx = state.next_conn_idx.fetch_add(1, Ordering::Relaxed);
    let (on_completed_tx, on_completed_rx) = oneshot::channel();

    conns.insert(
        id.clone(),
        Conn {
            idx,
            name: query.0.name,
            body: body.into_data_stream(),
            on_completed: on_completed_tx,
        },
    );

    println!(
        "[{idx}:{id}] connection created; active connections: {}",
        conns.len(),
    );

    let response = Body::from_stream({
        let response = if let Some(remote) = &state.args.remote {
            format!("{}/{}", remote, id)
        } else {
            id.clone()
        };

        stream::once(async move { Ok::<_, Error>(format!("{}\r\n", response)) })
            .chain(on_completed_rx.into_stream().map(|_| Ok(String::default())))
    });

    task::spawn({
        let state = state.clone();

        async move {
            time::sleep(state.args.intent_timeout).await;

            let mut conns = state.conns.lock().await;

            if let Some(conn) = conns.get(&id) {
                if conn.idx == idx {
                    conns.remove(&id);

                    println!(
                        "[{idx}:{id}] connection reaped; active connections: {}",
                        conns.len()
                    );
                }
            }
        }
    });

    Ok(Response::new(response))
}

async fn handle_recv(
    state: AxumState<Arc<State>>,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let Some(conn) = state.conns.lock().await.remove(&id) else {
        return Ok(err_not_found("no such connection found\r\n"));
    };

    let Conn {
        idx,
        name,
        mut body,
        on_completed,
    } = conn;

    println!("[{idx}:{id}] connection fused");

    let (stream_tx, stream_rx) = mpsc::channel(1);

    task::spawn(async move {
        let mut size = 0;

        let reason = loop {
            let chunk = time::timeout(state.args.chunk_timeout, body.next()).await;

            match chunk {
                Ok(Some(chunk)) => {
                    if let Ok(chunk) = &chunk {
                        size += chunk.len() as u64;

                        if size >= state.args.max_transfer_size {
                            break "reached transfer size limit";
                        }
                    }

                    match time::timeout(state.args.chunk_timeout, stream_tx.send(chunk)).await {
                        Ok(Ok(_)) => {
                            continue;
                        }

                        Ok(Err(_)) => {
                            break "transfer abandoned";
                        }

                        Err(_) => {
                            break "timed-out sending the current chunk";
                        }
                    }
                }

                Ok(None) => {
                    break "transfer completed";
                }

                Err(_) => {
                    break "timed-out retrieving the next chunk";
                }
            }
        };

        _ = on_completed.send(());

        println!(
            "[{idx}:{id}] connection closed after {}: {reason}; \
             active connections: {}",
            ByteUnit::Byte(size),
            state.conns.lock().await.len(),
        );
    });

    let mut response = Response::builder();

    if let Some(file_name) = name {
        response = response.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file_name),
        );
    }

    let response = response
        .body(Body::from_stream(ReceiverStream::new(stream_rx)))
        .unwrap();

    Ok(response)
}

fn err_not_found(body: impl Into<Option<&'static str>>) -> Response<Body> {
    let body = body.into().map(Body::from).unwrap_or_default();

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(body)
        .unwrap()
}

fn err_server_overloaded() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from("server overloaded, please try again later"))
        .unwrap()
}
