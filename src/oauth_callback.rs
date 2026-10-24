use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use url::Url;

const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(180);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 8 * 1024;

pub(crate) struct OAuthCallback {
    stream: TcpStream,
    url: Url,
}

impl OAuthCallback {
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) async fn respond_html(&mut self, body: &str) -> Result<()> {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        self.stream.write_all(response.as_bytes()).await?;
        Ok(())
    }
}

pub(crate) async fn receive_callback(
    listener: &TcpListener,
    expected_path: &str,
) -> Result<OAuthCallback> {
    // Only answer requests addressed to this listener: during the window
    // anything else that hits the port is a probe, not the browser redirect.
    let expected_host = host_header(&listener.local_addr()?);
    timeout(AUTHORIZATION_TIMEOUT, async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let Ok(Ok((target, host))) =
                timeout(REQUEST_TIMEOUT, read_request_target(&mut stream)).await
            else {
                continue;
            };
            if host.as_deref() != Some(expected_host.as_str()) {
                continue;
            }
            let Ok(url) = Url::parse(&format!("http://{expected_host}{target}")) else {
                continue;
            };
            if url.path() == expected_path {
                return Ok(OAuthCallback { stream, url });
            }
        }
    })
    .await
    .context("OAuth callback timed out")?
}

/// The `Host` header value a browser sends for this listener's address.
fn host_header(address: &SocketAddr) -> String {
    match address.ip() {
        IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
        IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
    }
}

/// Reads the request target and the `Host` header. HTTP/1.0 may legally omit
/// `Host`, but a request whose origin cannot be verified is as good as a
/// forged one, so a missing header is rejected just like a mismatched one.
async fn read_request_target(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<(String, Option<String>)> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            bail!("OAuth callback connection closed before sending headers");
        }
        if request.len().saturating_add(read) > MAX_HEADER_BYTES {
            bail!("OAuth callback headers exceed {MAX_HEADER_BYTES} bytes");
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }

    let request = std::str::from_utf8(&request).context("OAuth callback headers are not UTF-8")?;
    let mut lines = request.lines();
    let mut parts = lines
        .next()
        .context("OAuth callback omitted the request line")?
        .split_whitespace();
    let method = parts.next();
    let target = parts.next();
    let version = parts.next();
    if method != Some("GET")
        || !matches!(version, Some("HTTP/1.0" | "HTTP/1.1"))
        || parts.next().is_some()
    {
        bail!("OAuth callback request line is invalid");
    }
    let Some(target) = target else {
        bail!("OAuth callback omitted the request target");
    };
    if !target.starts_with('/') {
        bail!("OAuth callback request target is invalid");
    }
    let host = lines.take_while(|line| !line.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("host")
            .then(|| value.trim().to_owned())
    });
    Ok((target.to_owned(), host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receives_fragmented_callback_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move { receive_callback(&listener, "/callback").await });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"GET /call").await.unwrap();
        client
            .write_all(
                format!(
                    "back?code=secret HTTP/1.1\r\nHost: {}\r\n\r\n",
                    host_header(&address)
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let callback = receiver.await.unwrap().unwrap();
        assert_eq!(callback.url().path(), "/callback");
        assert_eq!(callback.url().query(), Some("code=secret"));
    }

    #[tokio::test]
    async fn ignores_requests_with_foreign_host_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move { receive_callback(&listener, "/callback").await });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /callback?code=forged HTTP/1.1\r\nHost: evil.example\r\n\r\n")
            .await
            .unwrap();

        // The forged request is skipped, so a later redirect is still the
        // one that wins.
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                format!(
                    "GET /callback?code=real HTTP/1.1\r\nHost: {}\r\n\r\n",
                    host_header(&address)
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let callback = receiver.await.unwrap().unwrap();
        assert_eq!(callback.url().query(), Some("code=real"));
    }

    #[tokio::test]
    async fn reports_missing_host_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /callback?code=secret HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();

        let (target, host) = read_request_target(&mut stream).await.unwrap();
        assert_eq!(target, "/callback?code=secret");
        assert_eq!(host, None);
    }

    #[tokio::test]
    async fn rejects_oversized_callback_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(&vec![b'a'; MAX_HEADER_BYTES + 1])
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();

        assert!(read_request_target(&mut stream).await.is_err());
        client.await.unwrap();
    }
}
