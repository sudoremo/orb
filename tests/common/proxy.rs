//! A TCP proxy that misbehaves on command, for putting between the client and
//! the container.
//!
//! The container is a correct server; the failures that matter in the field
//! are the network's, and the network between two local sockets never
//! fails. So the test stands this in its place and tells it when to drop a
//! connection, or to keep it open and forward nothing — which is what a NAT
//! that has forgotten the flow, or a machine that has slept, looks like to
//! the client.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// What to do to a connection once it has carried enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Close both sockets: the client reads EOF.
    Drop,
    /// Keep both sockets open and forward nothing more, ever: the client
    /// waits for an answer that is never coming.
    Freeze,
}

struct Planned {
    fault: Fault,
    after_server_bytes: u64,
}

struct Connection {
    id: u64,
    frozen: AtomicBool,
    server_bytes: AtomicU64,
    token: CancellationToken,
}

struct Shared {
    upstream: String,
    planned: Mutex<Option<Planned>>,
    connections: Mutex<Vec<Arc<Connection>>>,
    accepted: AtomicU64,
    /// Close new connections at once instead of forwarding them: the server
    /// is unreachable.
    refusing: AtomicBool,
    shutdown: CancellationToken,
}

pub struct FaultProxy {
    pub port: u16,
    shared: Arc<Shared>,
}

impl FaultProxy {
    /// Listen on a free local port and forward to `upstream`.
    pub async fn start(upstream_host: &str, upstream_port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let shared = Arc::new(Shared {
            upstream: format!("{upstream_host}:{upstream_port}"),
            planned: Mutex::new(None),
            connections: Mutex::new(Vec::new()),
            accepted: AtomicU64::new(0),
            refusing: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
        });
        let accept_loop = shared.clone();
        tokio::spawn(async move {
            loop {
                let (client, _) = tokio::select! {
                    _ = accept_loop.shutdown.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(pair) => pair,
                        Err(_) => return,
                    },
                };
                let shared = accept_loop.clone();
                tokio::spawn(async move { serve(client, shared).await });
            }
        });
        Self { port, shared }
    }

    /// The next connection to receive this many bytes from the server
    /// suffers `fault`. One-shot: the connection opened to recover from it
    /// is left alone.
    pub async fn plan(&self, fault: Fault, after_server_bytes: u64) {
        *self.shared.planned.lock().await = Some(Planned {
            fault,
            after_server_bytes,
        });
    }

    /// Stop forwarding on every connection open right now, keeping the
    /// sockets open. Connections opened afterwards are unaffected.
    pub async fn freeze_all(&self) {
        for connection in self.shared.connections.lock().await.iter() {
            connection.frozen.store(true, Ordering::SeqCst);
        }
    }

    /// Close every connection open right now. Connections opened afterwards
    /// are unaffected.
    pub async fn drop_all(&self) {
        for connection in self.shared.connections.lock().await.drain(..) {
            connection.token.cancel();
        }
    }

    /// Close every new connection at once, as an unreachable server would
    /// (well, as one whose port answers would; a silent one is `freeze_all`).
    pub async fn refuse_new(&self) {
        self.shared.refusing.store(true, Ordering::SeqCst);
    }

    /// Forward new connections again.
    pub async fn accept_new(&self) {
        self.shared.refusing.store(false, Ordering::SeqCst);
    }

    /// How many connections the client has opened through the proxy.
    pub fn connections_accepted(&self) -> u64 {
        self.shared.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.shared.shutdown.cancel();
    }
}

async fn serve(client: TcpStream, shared: Arc<Shared>) {
    if shared.refusing.load(Ordering::SeqCst) {
        drop(client);
        return;
    }
    let Ok(server) = TcpStream::connect(&shared.upstream).await else {
        return;
    };
    let connection = Arc::new(Connection {
        id: shared.accepted.fetch_add(1, Ordering::SeqCst) + 1,
        frozen: AtomicBool::new(false),
        server_bytes: AtomicU64::new(0),
        token: shared.shutdown.child_token(),
    });
    shared.connections.lock().await.push(connection.clone());

    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let to_server = tokio::spawn(pump(
        client_read,
        server_write,
        connection.clone(),
        shared.clone(),
        false,
    ));
    let to_client = tokio::spawn(pump(
        server_read,
        client_write,
        connection.clone(),
        shared.clone(),
        true,
    ));
    let _ = tokio::join!(to_server, to_client);
    shared
        .connections
        .lock()
        .await
        .retain(|c| c.id != connection.id);
}

/// Copy one direction until it ends, the connection is cancelled, or a
/// fault says otherwise. `from_server` is the direction the faults count.
async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    connection: Arc<Connection>,
    shared: Arc<Shared>,
    from_server: bool,
) {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = tokio::select! {
            _ = connection.token.cancelled() => return,
            read = from.read(&mut buffer) => read,
        };
        let n = match read {
            Ok(0) | Err(_) => {
                let _ = to.shutdown().await;
                return;
            }
            Ok(n) => n,
        };

        if from_server {
            let total = connection
                .server_bytes
                .fetch_add(n as u64, Ordering::SeqCst)
                + n as u64;
            let due = {
                let mut planned = shared.planned.lock().await;
                match planned.as_ref() {
                    Some(p) if total >= p.after_server_bytes => planned.take().map(|p| p.fault),
                    _ => None,
                }
            };
            match due {
                Some(Fault::Drop) => {
                    connection.token.cancel();
                    return;
                }
                Some(Fault::Freeze) => connection.frozen.store(true, Ordering::SeqCst),
                None => {}
            }
        }

        if connection.frozen.load(Ordering::SeqCst) {
            // Hold the socket open and say nothing, until the proxy goes away.
            connection.token.cancelled().await;
            return;
        }
        if to.write_all(&buffer[..n]).await.is_err() {
            return;
        }
    }
}
