//! Client for Nova's Echo control channel.
//!
//! One newline-delimited JSON object per line over mutual TLS — the client half
//! of `nova-server/src/echo/rpc.rs`. Both ends share
//! [`nova_core::envelope`], so the framing is defined once rather than agreed
//! twice.
//!
//! ## Two transports, one protocol
//!
//! - [`ControlChannel::connect_wan`] runs over the **punched UDP path**: TLS on
//!   top of [`nova_core::rudp`]'s reliable byte stream, sharing the socket that
//!   carries media. This is the path that needs no port forwarding and no
//!   trusted relay, and it is the one a real session uses.
//! - [`ControlChannel::connect_lan`] is the original TCP connection to port
//!   48011, kept for local use and debugging. The host now refuses that port
//!   from non-private addresses, so it cannot be reached from the internet
//!   even by accident.
//!
//! Above the stream the two are identical, which is the point: the command
//! surface, the authentication, and the trust store do not change with the
//! route.
//!
//! ## Why the host is authenticated by pin rather than by name
//!
//! Nova's certificate is self-signed and its identity *is* its fingerprint —
//! the same value the relay directory is keyed on and the same one paired into
//! `nova_paired.json`. There is no CA to consult and no hostname worth
//! verifying (the address came from a hole punch, not from DNS). Pinning the
//! fingerprint the caller already had to know is therefore stronger than name
//! validation would be, not a weaker substitute for it.

use std::net::SocketAddr;
use std::sync::Arc;

use nova_core::envelope::{decode_line, encode_line, read_line_bounded, InboundResponse, OutboundRequest};
use nova_core::identity::Identity;
use nova_core::rudp::{drive, RudpStream};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest response line accepted — the same bound the host applies to
/// requests, for the same reason.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Erased so one channel type serves both transports; the protocol above the
/// stream does not care which is underneath.
type Stream = Box<dyn Duplex + Unpin + Send>;

/// Marker for "a stream we can run TLS-framed NDJSON over".
pub trait Duplex: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> Duplex for T {}

pub struct ControlChannel {
    write: tokio::io::WriteHalf<Stream>,
    read: BufReader<tokio::io::ReadHalf<Stream>>,
    next_id: u64,
}

impl ControlChannel {
    /// Connect over the punched UDP path.
    ///
    /// `socket` must be the socket the hole punch opened — the NAT mapping
    /// belongs to it, and control has to leave from the same one or it needs a
    /// second mapping to punch and keep alive. `inbound` is fed by the caller's
    /// demultiplexer with the control datagrams it sees (see
    /// [`nova_core::demux`]); this module never reads the socket itself,
    /// because media is reading it too.
    pub async fn connect_wan(
        socket: Arc<tokio::net::UdpSocket>,
        peer: SocketAddr,
        inbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        identity: &Identity,
        host_pin: [u8; 32],
    ) -> Result<Self, String> {
        let (stream, plumbing) = RudpStream::new();

        // The driver moves bytes; it owns the reliability, not the socket.
        // `try_send_to` rather than an await: this closure is synchronous, and
        // a control datagram that hits a momentarily full send buffer is
        // exactly what retransmission is for.
        let sender = socket.clone();
        tokio::spawn(drive(
            nova_core::rudp::ControlChannel::new(),
            inbound,
            plumbing,
            move |d| {
                let _ = sender.try_send_to(d, peer);
            },
        ));

        Self::handshake(Box::new(stream), identity, host_pin, &format!("{peer} (WAN tunnel)")).await
    }

    /// Connect over TCP to the host's LAN control port.
    pub async fn connect_lan(
        addr: SocketAddr,
        identity: &Identity,
        host_pin: [u8; 32],
    ) -> Result<Self, String> {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect {addr}: {e}"))?;
        // Nagle would coalesce small commands on a channel whose whole point is
        // being responsive.
        let _ = tcp.set_nodelay(true);
        Self::handshake(Box::new(tcp), identity, host_pin, &addr.to_string()).await
    }

    async fn handshake(
        transport: Stream,
        identity: &Identity,
        host_pin: [u8; 32],
        label: &str,
    ) -> Result<Self, String> {
        let tls = nova_core::identity::client_config_pinned(identity, host_pin)?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));

        // A name is required by the TLS API but carries no authority here: the
        // pinned verifier decides, and it looks only at the certificate. Any
        // syntactically valid name works, so use one that reads honestly in a
        // packet capture.
        let name = rustls_pki_types::ServerName::try_from("nova.host")
            .map_err(|e| format!("server name: {e}"))?;
        let stream = connector
            .connect(name, transport)
            .await
            .map_err(|e| format!("TLS handshake with {label}: {e} — is this fingerprint really this host?"))?;

        let (read, write) = tokio::io::split(Box::new(stream) as Stream);
        Ok(Self { write, read: BufReader::new(read), next_id: 1 })
    }

    /// Send a command and wait for its reply.
    ///
    /// Strictly sequential: one request in flight at a time, so responses need
    /// no correlation beyond the id check below. A control channel at human
    /// cadence has nothing to gain from pipelining, and plenty to lose in
    /// complexity.
    pub async fn call(&mut self, command: &str, params: Map<String, Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let line = encode_line(&OutboundRequest::call(id, command, params))?;
        self.write.write_all(&line).await.map_err(|e| format!("send {command}: {e}"))?;
        self.write.flush().await.map_err(|e| format!("flush {command}: {e}"))?;

        // Notifications are answered only when they FAIL, and such a response
        // carries no id. It can land here, between a numbered call and its
        // reply, so those lines are skipped rather than mistaken for desync —
        // treating one as a mismatch would tear down a healthy channel because
        // a mouse packet was rejected.
        let response = loop {
            let text = read_line_bounded(&mut self.read, MAX_LINE_BYTES)
                .await
                .map_err(|e| format!("read reply to {command}: {e}"))?
                .ok_or_else(|| format!("host closed the connection during {command}"))?;
            let response: InboundResponse = decode_line(text.as_bytes())?;
            if response.id.is_none() {
                if !response.ok {
                    // stderr rather than a logging crate: this is the only line
                    // in the crate that would need one, and the host logs the
                    // same rejection with more context than the client has.
                    let why = response
                        .error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unspecified".into());
                    eprintln!("⚠️  host rejected a notification: {why}");
                }
                continue;
            }
            break response;
        };

        // A mismatched id means the stream desynced, which is worse than any
        // single failed command — the next reply would be read as this one's.
        if response.id != Some(id) {
            return Err(format!(
                "reply id mismatch on {command} (expected {id}, got {:?}) — closing",
                response.id
            ));
        }
        if !response.ok {
            let err = response.error.map(|e| e.to_string()).unwrap_or_else(|| "unspecified".into());
            return Err(err);
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Build a channel over an arbitrary stream, for tests.
    #[cfg(test)]
    fn over(stream: Stream) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self { write, read: BufReader::new(read), next_id: 1 }
    }

    /// Send a command without waiting for a reply.
    ///
    /// This is the input path's reason for existing. [`call`](Self::call) is
    /// strictly one-in-flight, so a command per pointer event serialises the
    /// whole input stream behind a network round trip each — which put the
    /// host's cursor seconds behind the user's hand, still coasting after they
    /// stopped moving (live 2026-08-15).
    ///
    /// Failures come back asynchronously as an id-less response, which
    /// [`call`](Self::call) logs and skips. That is the trade: immediate input,
    /// errors reported out of band rather than returned here.
    pub async fn notify(&mut self, command: &str, params: Map<String, Value>) -> Result<(), String> {
        let line = encode_line(&OutboundRequest::notification(command, params))?;
        self.write.write_all(&line).await.map_err(|e| format!("send {command}: {e}"))?;
        self.write.flush().await.map_err(|e| format!("flush {command}: {e}"))
    }
}

/// Extract a required string field from a result object.
pub fn field_str<'a>(result: &'a Value, key: &str) -> Result<&'a str, String> {
    result
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("host reply is missing \"{key}\""))
}

/// Extract a required integer field.
pub fn field_u64(result: &Value, key: &str) -> Result<u64, String> {
    result
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("host reply is missing \"{key}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    /// Notifications are answered only when they fail, and such a response
    /// carries no id. It can land between a numbered call and its reply.
    ///
    /// Before notifications existed, `call` treated ANY id it did not expect as
    /// a fatal desync — so a single rejected input packet would have torn down
    /// a perfectly healthy control channel mid-session. This is the regression
    /// that guards against reintroducing that.
    #[tokio::test]
    async fn a_rejected_notification_does_not_look_like_desync() {
        let (client, mut host) = tokio::io::duplex(4096);
        let mut ctl = ControlChannel::over(Box::new(client));

        tokio::spawn(async move {
            // Read the request so the client's write completes.
            let mut buf = [0u8; 512];
            let _ = tokio::io::AsyncReadExt::read(&mut host, &mut buf).await;

            // Two id-less rejections arrive first — input the host refused —
            // then the actual reply to the numbered call.
            host.write_all(
                b"{\"ok\":false,\"error\":{\"code\":\"not_the_owner\",\"message\":\"no\"}}\n\
                  {\"ok\":false,\"error\":{\"code\":\"bad_request\",\"message\":\"nope\"}}\n\
                  {\"id\":1,\"ok\":true,\"result\":{\"session_id\":42}}\n",
            )
            .await
            .unwrap();
            host.flush().await.unwrap();
        });

        let result = ctl
            .call("stop_session", Map::new())
            .await
            .expect("the call must survive rejections aimed at notifications");
        assert_eq!(result.get("session_id").and_then(Value::as_u64), Some(42));
    }

    /// A genuinely mismatched *numbered* reply is still fatal: the next reply
    /// would otherwise be read as this one's.
    #[tokio::test]
    async fn a_mismatched_numbered_reply_is_still_treated_as_desync() {
        let (client, mut host) = tokio::io::duplex(4096);
        let mut ctl = ControlChannel::over(Box::new(client));

        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let _ = tokio::io::AsyncReadExt::read(&mut host, &mut buf).await;
            host.write_all(b"{\"id\":99,\"ok\":true,\"result\":{}}\n").await.unwrap();
            host.flush().await.unwrap();
        });

        let err = ctl.call("stop_session", Map::new()).await.unwrap_err();
        assert!(err.contains("mismatch"), "expected a desync error, got: {err}");
    }
}
