use std::net::SocketAddr;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::{UPSTREAM_CONNECT_TIMEOUT, is_local, set_keepalive};

const ADDRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
use crate::logging::create_logger;

pub(super) enum Refusal {
    Local,
    Unreachable,
}

pub(super) async fn connect(host: &str, port: u16) -> Result<TcpStream, Refusal> {
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, async {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| Refusal::Unreachable)?
            .collect();
        if addresses.iter().any(|address| is_local(address.ip())) {
            return Err(Refusal::Local);
        }
        for address in addresses {
            if let Ok(Ok(stream)) =
                tokio::time::timeout(ADDRESS_CONNECT_TIMEOUT, TcpStream::connect(address)).await
            {
                set_keepalive(&stream);
                return Ok(stream);
            }
        }
        Err(Refusal::Unreachable)
    })
    .await
    .unwrap_or(Err(Refusal::Unreachable))
}

pub(super) async fn splice<C>(mut client: C, mut upstream: TcpStream)
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let Ok((sent, received)) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await
    else {
        return;
    };
    create_logger("intercept").debug(
        "tunnel closed",
        Some(serde_json::Map::from_iter([
            ("method".to_string(), json!("CONNECT")),
            ("bytesUp".to_string(), json!(sent)),
            ("bytesDown".to_string(), json!(received)),
        ])),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tunnel_copies_bytes_both_ways_until_a_side_closes() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let mut received = [0u8; 4];
            socket.read_exact(&mut received).await.unwrap();
            socket.write_all(b"pong").await.unwrap();
            received
        });
        let upstream = TcpStream::connect(address).await.unwrap();
        let (mut client, proxy_side) = tokio::io::duplex(64);
        let tunnel = tokio::spawn(splice(proxy_side, upstream));

        client.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        assert_eq!(&echo.await.unwrap(), b"ping");
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), tunnel)
            .await
            .unwrap()
            .unwrap();
    }
}
