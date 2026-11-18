use std::io::{self, Cursor, Read, Write};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hyper_util::rt::TokioIo;
use rand::RngCore;
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use rustls_pki_types::ServerName;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_WS_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEndpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub host_header: String,
}

pub fn parse_endpoint_url(raw: &str) -> Result<ParsedEndpoint, String> {
    let trimmed = raw.trim();
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("Invalid URL '{raw}': missing scheme"))?;
    let scheme_lower = scheme.to_ascii_lowercase();
    let (tls, default_port) = match scheme_lower.as_str() {
        "http" | "ws" => (false, 80u16),
        "https" | "wss" => (true, 443u16),
        _ => return Err(format!("Unsupported URL scheme '{scheme}'")),
    };

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("Invalid URL '{raw}': empty host"));
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (ipv6, rem) = after_bracket
            .split_once(']')
            .ok_or_else(|| format!("Invalid IPv6 host in '{raw}'"))?;
        let port = if let Some(p) = rem.strip_prefix(':') {
            p.parse::<u16>()
                .map_err(|_| format!("Invalid port in '{raw}'"))?
        } else if rem.is_empty() {
            default_port
        } else {
            return Err(format!("Invalid authority in '{raw}'"));
        };
        (ipv6.to_string(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        let port = p
            .parse::<u16>()
            .map_err(|_| format!("Invalid port in '{raw}'"))?;
        (h.to_string(), port)
    } else {
        (authority.to_string(), default_port)
    };

    if host.is_empty() {
        return Err(format!("Invalid URL '{raw}': empty host"));
    }

    Ok(ParsedEndpoint {
        tls,
        host,
        port,
        path: path.to_string(),
        host_header: authority.to_string(),
    })
}

fn tls_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let cfg = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(cfg)
        })
        .clone()
}

pub struct RustlsClientStream {
    tcp: TcpStream,
    conn: ClientConnection,
    out_buf: Vec<u8>,
    out_pos: usize,
    peer_eof: bool,
}

impl RustlsClientStream {
    async fn connect(host: &str, tcp: TcpStream) -> Result<Self, String> {
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|_| format!("Invalid TLS server name '{host}'"))?;
        let conn = ClientConnection::new(tls_client_config(), server_name)
            .map_err(|e| format!("TLS init error: {e}"))?;
        let mut stream = Self {
            tcp,
            conn,
            out_buf: Vec::new(),
            out_pos: 0,
            peer_eof: false,
        };
        stream.complete_handshake().await?;
        Ok(stream)
    }

    async fn complete_handshake(&mut self) -> Result<(), String> {
        let mut read_buf = [0u8; 8192];
        loop {
            while self.conn.wants_write() {
                let mut tmp = Vec::new();
                self.conn
                    .write_tls(&mut tmp)
                    .map_err(|e| format!("TLS write error: {e}"))?;
                if !tmp.is_empty() {
                    self.tcp
                        .write_all(&tmp)
                        .await
                        .map_err(|e| format!("TLS TCP write failed: {e}"))?;
                }
            }
            if !self.conn.is_handshaking() {
                return Ok(());
            }
            let n = self
                .tcp
                .read(&mut read_buf)
                .await
                .map_err(|e| format!("TLS handshake read failed: {e}"))?;
            if n == 0 {
                return Err("TLS handshake closed unexpectedly by peer".to_string());
            }
            let mut cursor = Cursor::new(&read_buf[..n]);
            self.conn
                .read_tls(&mut cursor)
                .map_err(|e| format!("TLS read decode error: {e}"))?;
            self.conn
                .process_new_packets()
                .map_err(|e| format!("TLS certificate/handshake error: {e}"))?;
        }
    }

    fn flush_out(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.conn.wants_write() {
            if let Err(e) = self.conn.write_tls(&mut self.out_buf) {
                return Poll::Ready(Err(e));
            }
        }
        while self.out_pos < self.out_buf.len() {
            match Pin::new(&mut self.tcp).poll_write(cx, &self.out_buf[self.out_pos..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "TLS socket closed while flushing",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    self.out_pos += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.out_buf.clear();
        self.out_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for RustlsClientStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let unfilled = buf.initialize_unfilled();
            match self.conn.reader().read(unfilled) {
                Ok(n) if n > 0 => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(_) => {
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if self.peer_eof {
                        return Poll::Ready(Ok(()));
                    }
                    let _ = self.flush_out(cx);
                    let mut tmp = [0u8; 8192];
                    let mut rbuf = ReadBuf::new(&mut tmp);
                    match Pin::new(&mut self.tcp).poll_read(cx, &mut rbuf) {
                        Poll::Ready(Ok(())) => {
                            let filled = rbuf.filled();
                            if filled.is_empty() {
                                self.peer_eof = true;
                                continue;
                            }
                            let mut cursor = Cursor::new(filled);
                            if let Err(err) = self.conn.read_tls(&mut cursor) {
                                return Poll::Ready(Err(err));
                            }
                            if let Err(err) = self.conn.process_new_packets() {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    err.to_string(),
                                )));
                            }
                        }
                        Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for RustlsClientStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.out_pos < self.out_buf.len() {
            match self.flush_out(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = self.conn.writer().write(data)?;
        match self.flush_out(cx) {
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            _ => Poll::Ready(Ok(n)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.flush_out(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.tcp).poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.conn.send_close_notify();
        match self.flush_out(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.tcp).poll_shutdown(cx),
            other => other,
        }
    }
}

pub enum TransportStream {
    Plain(TcpStream),
    Tls(Box<RustlsClientStream>),
    Upgraded(TokioIo<hyper::upgrade::Upgraded>),
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
            Self::Upgraded(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, data),
            Self::Tls(s) => Pin::new(&mut **s).poll_write(cx, data),
            Self::Upgraded(s) => Pin::new(s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
            Self::Upgraded(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
            Self::Upgraded(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

async fn connect_transport(endpoint: &ParsedEndpoint) -> Result<TransportStream, String> {
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("Failed connecting to {addr}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if endpoint.tls {
        let tls = RustlsClientStream::connect(&endpoint.host, tcp).await?;
        Ok(TransportStream::Tls(Box::new(tls)))
    } else {
        Ok(TransportStream::Plain(tcp))
    }
}

pub async fn http_post_json(
    url: &str,
    bearer_token: Option<&str>,
    body_json: &str,
) -> Result<(u16, String), String> {
    let endpoint = parse_endpoint_url(url)?;
    let mut stream = connect_transport(&endpoint).await?;

    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        endpoint.path,
        endpoint.host_header,
        body_json.len()
    );
    if let Some(tok) = bearer_token.filter(|s| !s.trim().is_empty()) {
        req.push_str(&format!("Authorization: Bearer {}\r\n", tok.trim()));
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("HTTP write header failed: {e}"))?;
    stream
        .write_all(body_json.as_bytes())
        .await
        .map_err(|e| format!("HTTP write body failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("HTTP flush failed: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|e| format!("HTTP read failed: {e}"))?;

    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let split_pos = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Malformed HTTP response (missing header terminator)".to_string())?;
    let header_bytes = &raw[..split_pos];
    let body_bytes = &raw[split_pos + 4..];

    let header_str = String::from_utf8_lossy(header_bytes);
    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| "Empty HTTP status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("Invalid HTTP status line '{status_line}'"))?;

    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            let name_l = name.trim().to_ascii_lowercase();
            let val_l = val.trim().to_ascii_lowercase();
            if name_l == "transfer-encoding" && val_l.contains("chunked") {
                is_chunked = true;
            } else if name_l == "content-length" {
                content_length = val.trim().parse::<usize>().ok();
            }
        }
    }

    let decoded = if is_chunked {
        decode_chunked_body(body_bytes)?
    } else if let Some(len) = content_length {
        body_bytes[..body_bytes.len().min(len)].to_vec()
    } else {
        body_bytes.to_vec()
    };

    Ok((status, String::from_utf8_lossy(&decoded).to_string()))
}

fn decode_chunked_body(mut bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let Some(pos) = bytes.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let len_line = std::str::from_utf8(&bytes[..pos])
            .map_err(|_| "Invalid chunked UTF-8".to_string())?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(len_line, 16)
            .map_err(|_| format!("Invalid chunk length '{len_line}'"))?;
        bytes = &bytes[pos + 2..];
        if size == 0 {
            break;
        }
        if bytes.len() < size {
            out.extend_from_slice(bytes);
            break;
        }
        out.extend_from_slice(&bytes[..size]);
        bytes = &bytes[size..];
        if bytes.starts_with(b"\r\n") {
            bytes = &bytes[2..];
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

pub struct WsStream {
    stream: TransportStream,
    is_server: bool,
}

pub struct WsReader {
    reader: ReadHalf<TransportStream>,
}

pub struct WsWriter {
    writer: WriteHalf<TransportStream>,
    is_server: bool,
}

impl WsStream {
    pub fn new(stream: TransportStream, is_server: bool) -> Self {
        Self { stream, is_server }
    }

    pub fn into_split(self) -> (WsReader, WsWriter) {
        let (reader, writer) = tokio::io::split(self.stream);
        (
            WsReader { reader },
            WsWriter {
                writer,
                is_server: self.is_server,
            },
        )
    }

    #[cfg(test)]
    pub async fn send_text(&mut self, text: &str) -> Result<(), String> {
        write_ws_frame(&mut self.stream, !self.is_server, 0x1, text.as_bytes()).await
    }

    #[cfg(test)]
    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), String> {
        write_ws_frame(&mut self.stream, !self.is_server, 0xA, payload).await
    }

    #[cfg(test)]
    pub async fn send_close(&mut self) -> Result<(), String> {
        write_ws_frame(&mut self.stream, !self.is_server, 0x8, &[]).await
    }

    #[cfg(test)]
    pub async fn recv(&mut self) -> Result<WsMessage, String> {
        read_ws_message(&mut self.stream).await
    }
}

impl WsReader {
    pub async fn recv(&mut self) -> Result<WsMessage, String> {
        read_ws_message(&mut self.reader).await
    }
}

impl WsWriter {
    pub async fn send_text(&mut self, text: &str) -> Result<(), String> {
        write_ws_frame(&mut self.writer, !self.is_server, 0x1, text.as_bytes()).await
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), String> {
        write_ws_frame(&mut self.writer, !self.is_server, 0xA, payload).await
    }

    #[cfg(test)]
    pub async fn send_close(&mut self) -> Result<(), String> {
        write_ws_frame(&mut self.writer, !self.is_server, 0x8, &[]).await
    }
}

pub fn compute_ws_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(WS_GUID.as_bytes());
    BASE64.encode(hasher.finalize())
}

pub async fn connect_ws(ws_url: &str, bearer_token: Option<&str>) -> Result<WsStream, String> {
    let endpoint = parse_endpoint_url(ws_url)?;
    let mut stream = connect_transport(&endpoint).await?;

    let mut key_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let key_b64 = BASE64.encode(key_bytes);
    let expected_accept = compute_ws_accept(&key_b64);

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        endpoint.path, endpoint.host_header, key_b64
    );
    if let Some(tok) = bearer_token.filter(|s| !s.trim().is_empty()) {
        req.push_str(&format!("Authorization: Bearer {}\r\n", tok.trim()));
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("WS handshake write failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("WS handshake flush failed: {e}"))?;

    let mut hdr = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if hdr.len() >= 16384 {
            return Err("WS handshake response header too large".to_string());
        }
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| format!("WS handshake read failed: {e}"))?;
        if n == 0 {
            return Err("WS handshake closed before 101 response".to_string());
        }
        hdr.push(byte[0]);
        if hdr.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let hdr_str = String::from_utf8_lossy(&hdr);
    let mut lines = hdr_str.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if status_code != 101 {
        return Err(format!(
            "WebSocket upgrade rejected (HTTP {status_code}): {status_line}"
        ));
    }

    let mut accept_ok = false;
    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("sec-websocket-accept")
                && val.trim() == expected_accept
            {
                accept_ok = true;
            }
        }
    }
    if !accept_ok {
        return Err("WebSocket handshake failed: invalid Sec-WebSocket-Accept".to_string());
    }

    Ok(WsStream::new(stream, false))
}

pub fn accept_ws_upgrade(
    req: Request,
) -> Result<(Response, oneshot::Receiver<WsStream>), StatusCode> {
    let has_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !has_upgrade {
        return Err(StatusCode::UPGRADE_REQUIRED);
    }

    let ws_key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let accept_val = compute_ws_accept(&ws_key);
    let upgrade = hyper::upgrade::on(req);
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let stream = TransportStream::Upgraded(TokioIo::new(upgraded));
            let _ = tx.send(WsStream::new(stream, true));
        }
    });

    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = resp.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(val) = HeaderValue::from_str(&accept_val) {
        headers.insert("sec-websocket-accept", val);
    }
    Ok((resp, rx))
}

pub fn http_base_to_ws_url(http_url: &str, ws_path: &str) -> Result<String, String> {
    let trimmed = http_url.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("Invalid core URL '{http_url}'"))?;
    let ws_scheme = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err(format!("Unsupported URL scheme '{scheme}'")),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let path = if ws_path.starts_with('/') {
        ws_path.to_string()
    } else {
        format!("/{ws_path}")
    };
    Ok(format!("{ws_scheme}://{authority}{path}"))
}

pub fn normalize_core_http_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Ok(trimmed.to_string())
    } else if !trimmed.contains("://") {
        Ok(format!("http://{trimmed}"))
    } else {
        Err(format!("Invalid Core URL '{raw}'"))
    }
}

async fn write_ws_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    masked: bool,
    opcode: u8,
    payload: &[u8],
) -> Result<(), String> {
    let mut header_buf = Vec::with_capacity(14);
    header_buf.push(0x80 | (opcode & 0x0F));

    let mask_bit = if masked { 0x80u8 } else { 0x00u8 };
    let len = payload.len();
    if len <= 125 {
        header_buf.push(mask_bit | (len as u8));
    } else if len <= 65535 {
        header_buf.push(mask_bit | 126);
        header_buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header_buf.push(mask_bit | 127);
        header_buf.extend_from_slice(&(len as u64).to_be_bytes());
    }

    if masked {
        let mut mask = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut mask);
        header_buf.extend_from_slice(&mask);
        writer
            .write_all(&header_buf)
            .await
            .map_err(|e| format!("WS write frame header failed: {e}"))?;
        let mut masked_payload = payload.to_vec();
        for (i, b) in masked_payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        writer
            .write_all(&masked_payload)
            .await
            .map_err(|e| format!("WS write payload failed: {e}"))?;
    } else {
        writer
            .write_all(&header_buf)
            .await
            .map_err(|e| format!("WS write frame header failed: {e}"))?;
        if !payload.is_empty() {
            writer
                .write_all(payload)
                .await
                .map_err(|e| format!("WS write payload failed: {e}"))?;
        }
    }
    writer
        .flush()
        .await
        .map_err(|e| format!("WS flush failed: {e}"))
}

async fn read_ws_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<WsMessage, String> {
    let mut first_opcode: Option<u8> = None;
    let mut assembled = Vec::new();

    loop {
        let mut hdr = [0u8; 2];
        reader
            .read_exact(&mut hdr)
            .await
            .map_err(|e| format!("WS read closed: {e}"))?;
        let fin = (hdr[0] & 0x80) != 0;
        let opcode = hdr[0] & 0x0F;
        let masked = (hdr[1] & 0x80) != 0;
        let len_code = hdr[1] & 0x7F;

        let len: usize = match len_code {
            0..=125 => len_code as usize,
            126 => {
                let mut b = [0u8; 2];
                reader
                    .read_exact(&mut b)
                    .await
                    .map_err(|e| format!("WS read ext16 failed: {e}"))?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                reader
                    .read_exact(&mut b)
                    .await
                    .map_err(|e| format!("WS read ext64 failed: {e}"))?;
                u64::from_be_bytes(b) as usize
            }
            _ => unreachable!(),
        };

        if len > MAX_WS_FRAME_BYTES || assembled.len().saturating_add(len) > MAX_WS_FRAME_BYTES {
            return Err("WebSocket frame exceeded 8 MiB limit".to_string());
        }

        let mask = if masked {
            let mut m = [0u8; 4];
            reader
                .read_exact(&mut m)
                .await
                .map_err(|e| format!("WS read mask failed: {e}"))?;
            Some(m)
        } else {
            None
        };

        let mut payload = vec![0u8; len];
        if len > 0 {
            reader
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("WS read payload failed: {e}"))?;
            if let Some(m) = mask {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m[i % 4];
                }
            }
        }

        match opcode {
            0x8 => return Ok(WsMessage::Close),
            0x9 => return Ok(WsMessage::Ping(payload)),
            0xA => return Ok(WsMessage::Pong(payload)),
            0x1 | 0x2 => {
                first_opcode = Some(opcode);
                assembled.extend_from_slice(&payload);
                if fin {
                    break;
                }
            }
            0x0 => {
                if first_opcode.is_none() {
                    return Err("Unexpected continuation frame".to_string());
                }
                assembled.extend_from_slice(&payload);
                if fin {
                    break;
                }
            }
            other => return Err(format!("Unsupported WS opcode {other}")),
        }
    }

    match first_opcode {
        Some(0x1) => {
            let text = String::from_utf8(assembled)
                .map_err(|_| "Invalid UTF-8 in WebSocket text frame".to_string())?;
            Ok(WsMessage::Text(text))
        }
        Some(0x2) => Ok(WsMessage::Binary(assembled)),
        _ => Err("Invalid fragmented WebSocket sequence".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tokio::net::TcpListener;

    #[test]
    fn parses_http_ws_and_ipv6_urls() {
        let p1 = parse_endpoint_url("https://core.example.com/api/cluster/enroll").unwrap();
        assert!(p1.tls);
        assert_eq!(p1.host, "core.example.com");
        assert_eq!(p1.port, 443);
        assert_eq!(p1.path, "/api/cluster/enroll");

        let p2 = parse_endpoint_url("ws://[::1]:8443/api/cluster/node-ws").unwrap();
        assert!(!p2.tls);
        assert_eq!(p2.host, "::1");
        assert_eq!(p2.port, 8443);
        assert_eq!(p2.path, "/api/cluster/node-ws");

        assert_eq!(
            http_base_to_ws_url("https://panel.local:8443/", "/api/cluster/node-ws").unwrap(),
            "wss://panel.local:8443/api/cluster/node-ws"
        );
    }

    #[test]
    fn computes_rfc6455_accept_vector() {
        // RFC 6455 Section 1.3 example key
        assert_eq!(
            compute_ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[tokio::test]
    async fn full_duplex_axum_ws_roundtrip() {
        let app = Router::new().route(
            "/ws",
            get(|req: Request| async move {
                match accept_ws_upgrade(req) {
                    Ok((resp, rx)) => {
                        tokio::spawn(async move {
                            if let Ok(mut ws) = rx.await {
                                while let Ok(msg) = ws.recv().await {
                                    match msg {
                                        WsMessage::Text(t) => {
                                            let _ = ws.send_text(&format!("echo:{t}")).await;
                                        }
                                        WsMessage::Ping(p) => {
                                            let _ = ws.send_pong(&p).await;
                                        }
                                        WsMessage::Close => {
                                            let _ = ws.send_close().await;
                                            break;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        });
                        resp
                    }
                    Err(code) => Response::builder()
                        .status(code)
                        .body(Body::empty())
                        .unwrap(),
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("ws://{addr}/ws");
        let client = connect_ws(&url, Some("test-bearer")).await.unwrap();
        let (mut reader, mut writer) = client.into_split();

        writer.send_text("hello-mesh").await.unwrap();
        assert_eq!(
            reader.recv().await.unwrap(),
            WsMessage::Text("echo:hello-mesh".to_string())
        );

        writer.send_close().await.unwrap();
        assert_eq!(reader.recv().await.unwrap(), WsMessage::Close);

        server.abort();
    }
}
