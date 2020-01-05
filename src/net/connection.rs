use crate::error::RatsioError;
use crate::ops::Op;
use futures::{
    future::{self, Either},
    prelude::*,
    task::Poll,
};
use parking_lot::RwLock;
use std::{
    net::{SocketAddr, ToSocketAddrs},
    str::FromStr,
    sync::Arc,
};
use super::connection_inner::NatsConnectionInner;
use super::ReconnectHandler;
use url::Url;

/// State of the raw connection
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NatsConnectionState {
    Connected,
    Reconnecting,
    Disconnected,
}

/// Represents a connection to a NATS server. Implements `Sink` and `Stream`
#[derive(Debug)]
pub struct NatsConnection {
    /// indicates if the connection is made over TLS
    pub(crate) is_tls: bool,
    /// Inner dual `Stream`/`Sink` of the TCP connection
    pub(crate) inner: Arc<RwLock<(Url, NatsConnectionInner)>>,
    /// Current state of the connection, and connect version.
    /// Version only increments on a successful reconnect.
    pub(crate) state: Arc<RwLock<(NatsConnectionState, u64)>>,

    /// Reconnect trigger
    pub(crate) reconnect_handler: ReconnectHandler,

    pub(crate) init_hosts: Vec<String>,
    pub(crate) reconnect_hosts: RwLock<Vec<String>>,
    pub(crate) reconnect_timeout: u64,
}

pub struct NatsConnSinkStream {
    /// Inner dual `Stream`/`Sink` of the TCP connection
    pub(crate) inner: Arc<RwLock<(Url, NatsConnectionInner)>>,
    /// Current state of the connection, and connect version.
    /// Version only increments on a successful reconnect.
    pub(crate) state: Arc<RwLock<(NatsConnectionState, u64)>>,

    /// Reconnect trigger
    pub(crate) reconnect_trigger: Box<dyn Fn() -> () + Sync + Send>,
}


impl NatsConnection {
    /// Connect to a raw TCP socket
    fn connect(addr: SocketAddr) -> impl Future<Output=Result<NatsConnectionInner, RatsioError>> {
        NatsConnectionInner::connect_tcp(addr).map(move |result| {
            debug!(target: "ratsio", "Got a socket successfully.");
            result.map(|socket| socket.into())
        })
    }

    /// Connect to a TLS over TCP socket. Upgrade is performed automatically
    fn connect_tls(host: String, addr: SocketAddr) -> impl Future<Output=Result<NatsConnectionInner, RatsioError>> {
        NatsConnectionInner::connect_tcp(addr)
            .and_then(move |socket| {
                debug!(target: "ratsio", "Got a socket successfully, upgrading to TLS");
                NatsConnectionInner::upgrade_tcp_to_tls(host, socket)
            })
            .map(move |result| {
                result.map(|socket| socket.into())
            })
    }

    /// Tries to reconnect once to the server; Only used internally. Blocks polling during reconnecting
    /// by forcing the object to return `Async::NotReady`/`AsyncSink::NotReady`
    pub(crate) fn trigger_reconnect(conn: Arc<Self>) {
        trace!(target: "ratsio", "Trigger reconnection ");
        let connect_version = conn.state.read().1;
        {
            let mut state_guard = conn.state.write();
            if state_guard.0 == NatsConnectionState::Reconnecting {
                // Another thread is busy reconnecting...
                trace!(target: "ratsio", "Already reconnection, nothing to do");
                return;
            } else if state_guard.0 == NatsConnectionState::Connected && state_guard.1 > connect_version {
                // Another thread has already reconnected ...
                trace!(target: "ratsio", "Another thread has reconnected, nothing to do");
                return;
            } else {
                let current_version = state_guard.1;
                *state_guard = (NatsConnectionState::Disconnected, current_version);
            }
        }
        NatsConnection::reconnect(conn);
    }

    fn reconnect(conn: Arc<Self>) {
        trace!(target: "ratsio", "Reconnecting");
        {
            let mut state_guard = conn.state.write();
            if state_guard.0 == NatsConnectionState::Disconnected {
                *state_guard = (NatsConnectionState::Reconnecting, state_guard.1);
            } else {
                return;
            }
        }

        let cluster_addrs: Vec<_> = NatsConnection::parse_uris(&conn.reconnect_hosts.read());
        trace!(target: "ratsio", "Retrying {:?}", &*conn.reconnect_hosts.read());

        tokio::spawn(NatsConnection::get_conn_inner(cluster_addrs, conn.is_tls)
            .then(move |inner_result| {
                let connect_version = (*conn.state.read()).1;
                let retry_conn = conn.clone();
                match inner_result {
                    Ok(new_inner) => {
                        *conn.inner.write() = new_inner;
                        *conn.state.write() = (NatsConnectionState::Connected, connect_version + 1);
                        let _ = conn.reconnect_handler.unbounded_send(conn.clone());
                        debug!(target: "ratsio", "Got a connection");
                        Either::Left(future::ok(()))
                    }
                    Err(err) => {
                        error!(target: "ratsio", "Error reconnecting :: {:?}", err);
                        *retry_conn.state.write() = (NatsConnectionState::Disconnected, connect_version);
                        //Rescedule another attempt
                        let task = tokio::time::delay_for(std::time::Duration::from_millis(retry_conn.reconnect_timeout))
                            .then(|_| async move  {
                                NatsConnection::trigger_reconnect(retry_conn);
                                Ok(())
                            });
                        Either::Right(task)
                    }
                }
            }));
    }

    pub fn create_connection(reconnect_handler: ReconnectHandler, reconnect_timeout: u64,
                             cluster_uris: &[String], tls_required: bool) -> impl Future<Output=NatsConnection> {
        let cluster_addrs = NatsConnection::parse_uris(cluster_uris);
        let init_hosts = cluster_uris.to_vec();
        NatsConnection::get_conn_inner(cluster_addrs, tls_required)
            .map(move |inner| {
                NatsConnection {
                    is_tls: tls_required,
                    state: Arc::new(RwLock::new((NatsConnectionState::Connected, 0))),
                    inner: Arc::new(RwLock::new(inner)),
                    init_hosts: init_hosts.clone(),
                    reconnect_hosts: RwLock::new(init_hosts),
                    reconnect_handler,
                    reconnect_timeout,
                }
            })
    }

    pub fn parse_uris(cluster_uris: &[String]) -> Vec<(Url, SocketAddr)> {
        cluster_uris.iter().map(|cluster_uri| {
            let formatted_url = if cluster_uri.starts_with("nats://") {
                cluster_uri.clone()
            } else {
                format!("nats://{}", cluster_uri)
            };
            let node_url = Url::parse(&formatted_url);
            match node_url {
                Ok(node_url) =>
                    match node_url.host_str() {
                        Some(host) => {
                            let host_and_port = format!("{}:{}", &host, node_url.port().unwrap_or(4222));
                            match SocketAddr::from_str(&host_and_port) {
                                Ok(sock_addr) => {
                                    info!(" Resolved {} to {}", &host, &sock_addr);
                                    vec!((node_url.clone(), sock_addr))
                                }
                                Err(_) => {
                                    match host_and_port.to_socket_addrs() {
                                        Ok(ips_iter) => ips_iter.map(|x| {
                                            info!(" Resolved {} to {}", &host, &x);
                                            (node_url.clone(), x)
                                        }).collect::<Vec<_>>(),
                                        Err(err) => {
                                            error!("Unable resolve url => {} to ip address => {}", cluster_uri, err);
                                            Vec::new()
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            Vec::new()
                        }
                    }
                Err(err) => {
                    error!("Unable to parse url => {} => {}", cluster_uri, err);
                    Vec::new()
                }
            }
        }).flatten().collect()
    }


    fn get_conn_inner(cluster_addrs: Vec<(Url, SocketAddr)>, tls_required: bool)
                      -> impl Future<Output=Result<(Url, NatsConnectionInner), RatsioError>> {
        if cluster_addrs.is_empty() {
            warn!("No addresses to connect to.");
            return Either::Left(future::err(RatsioError::NoRouteToHostError));
        }
        fn get_conn_step(cluster_addrs: &[(Url, SocketAddr)], tls_required: bool)
                         -> impl Future<Output=(Url, NatsConnectionInner)> {
            if cluster_addrs.is_empty() {
                Either::Left(future::err::<(Url, NatsConnectionInner), RatsioError>(RatsioError::NoRouteToHostError))
            } else {
                Either::Right(future::ok(cluster_addrs[0].clone())
                    .and_then(move |(node_url, node_addr)| {
                        if tls_required {
                            match node_url.host_str() {
                                Some(host) => future::ok(Either::Right(NatsConnection::connect_tls(host.to_string(), node_addr)
                                    .map(move |con| (node_url.clone(), con)))),
                                None => future::err(RatsioError::NoRouteToHostError),
                            }
                        } else {
                            future::ok(Either::Left(NatsConnection::connect(node_addr)
                                .map(move |con| (node_url.clone(), con))))
                        }
                    })
                    .flatten())
            }
        }
        Either::Right(loop_fn(cluster_addrs,
                          move |cluster_addrs| {
                              let rem_addrs = Vec::from(&cluster_addrs[1..]).clone();
                              get_conn_step(&cluster_addrs[..], tls_required)
                                  .then(move |inner| {
                                      Ok(Loop::Break(inner))
                                  })
                                  .or_else(move |_err| {
                                      if rem_addrs.is_empty() {
                                          Err(RatsioError::NoRouteToHostError)
                                      } else {
                                          Ok(Loop::Continue(rem_addrs))
                                      }
                                  })
                          }))
    }
}

impl Sink<Op> for NatsConnSinkStream {
    type Error = RatsioError;

    fn start_send(&mut self, item: Op) -> Result<(), Self::Error> {
        if match self.state.try_read() {
            Some(state) => (*state).0 != NatsConnectionState::Connected,
            _ => true,
        } {
            return Ok(());
        }

        if let Some(mut inner) = self.inner.try_write() {
            match (*inner).1.start_send(item.clone()) {
                Err(RatsioError::ServerDisconnected(_)) => {
                    (*self.reconnect_trigger)();
                    Ok(())
                }
                poll_res => poll_res,
            }
        } else {
            Ok(())
        }
    }

    fn poll_flush(&mut self) -> Poll<Result<(), Self::Error>> {
        if match self.state.try_read() {
            Some(state) => (*state).0 != NatsConnectionState::Connected,
            _ => true,
        } {
            return Poll::Pending;
        }

        if let Some(mut inner) = self.inner.try_write() {
            match (*inner).1.poll_flush() {
                Err(RatsioError::ServerDisconnected(_)) => {
                    (*self.reconnect_trigger)();
                    Poll::Pending
                }
                poll_res => poll_res,
            }
        } else {
            Poll::Pending
        }
    }
}

impl Stream for NatsConnSinkStream {
    type Item = Op;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        if match self.state.try_read() {
            Some(state) => (*state).0 != NatsConnectionState::Connected,
            _ => true,
        } {
            return Poll::Pending;
        }

        if let Some(mut inner) = self.inner.try_write() {
            match (*inner).1.poll_next() {
                Err(RatsioError::ServerDisconnected(_)) => {
                    (*self.reconnect_trigger)();
                    Poll::Pending
                }
                poll_res => poll_res,
            }
        } else {
            Poll::Pending
        }
    }
}
