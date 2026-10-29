//! The PROXY protocol v2 header a trusted pass-through proxy sends before the TLS handshake, which
//! names the client the proxy accepted. The proxy never decrypts, so the header is the only way
//! the node learns the client's address.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, bail, ensure};
use ppp::v2::{Addresses, Command, Header, PROTOCOL_PREFIX, Protocol};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::debug;

use crate::limits::ThrottledAlert;

/// The fixed part of a v2 header: its signature, then version, command, family, and length.
const MINIMUM_LENGTH: usize = 16;
/// Largest header read, TLVs included. The specification has a receiver accept at least 536 bytes.
const MAX_HEADER: usize = 1024;

/// The client address a trusted proxy's connection names, with an IPv4-mapped address as its IPv4
/// one; exactly the header's bytes are read, so the TLS handshake follows unread. `None` closes the
/// connection: a LOCAL header is the proxy's own check and is only logged, while any other failure
/// raises `refused`.
pub(super) async fn client_behind(
    conn: &mut (impl AsyncRead + Unpin),
    proxy: SocketAddr,
    refused: &ThrottledAlert,
) -> Option<SocketAddr> {
    match read_client(conn).await {
        Ok(None) => {
            debug!(%proxy, "PROXY protocol LOCAL header; closing");
            None
        }
        Ok(Some(client)) => {
            debug!(%proxy, %client, "client address from the PROXY protocol header");
            Some(client)
        }
        Err(e) => {
            refused.raise(&format!("{proxy}: {e}"));
            None
        }
    }
}

/// Reads one header and returns its source address, or `None` for a LOCAL header.
/// Error cases:
/// - the stream ends or fails before the header does, or the header is not valid v2, is over
///   `MAX_HEADER`, or gives a source that is not a TCP IPv4 or IPv6 address
async fn read_client(conn: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Option<SocketAddr>> {
    let mut buf = [0u8; MAX_HEADER];
    conn.read_exact(&mut buf[..MINIMUM_LENGTH]).await.context("reading the header")?;
    // Checked before the length is believed, so only a signed header gets its bytes read.
    ensure!(buf[..PROTOCOL_PREFIX.len()] == *PROTOCOL_PREFIX, "not a PROXY protocol v2 header");
    let total = MINIMUM_LENGTH + usize::from(u16::from_be_bytes([buf[14], buf[15]]));
    ensure!(total <= MAX_HEADER, "a header of {total} bytes, over the {MAX_HEADER} read");
    conn.read_exact(&mut buf[MINIMUM_LENGTH..total]).await.context("reading the header")?;
    let header = Header::try_from(&buf[..total]).context("parsing the header")?;
    if header.command == Command::Local {
        return Ok(None);
    }
    ensure!(header.protocol == Protocol::Stream, "a header for a transport other than TCP");
    match header.addresses {
        Addresses::IPv4(v4) => Ok(Some(SocketAddr::new(IpAddr::V4(v4.source_address), v4.source_port))),
        Addresses::IPv6(v6) => Ok(Some(SocketAddr::new(IpAddr::V6(v6.source_address).to_canonical(), v6.source_port))),
        Addresses::Unspecified | Addresses::Unix(_) => bail!("a header that names no IPv4 or IPv6 client"),
    }
}
