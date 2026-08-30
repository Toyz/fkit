//! A minimal RFC 6455 WebSocket implementation (server and client).
//!
//! Hand-rolled to keep fkit at two dependencies. WebSockets are a good fit for
//! sync because the protocol is a *conversation* — "what do you have?", "send me
//! these", "now I need these too" — which maps badly onto request/response HTTP
//! and perfectly onto a persistent bidirectional connection.
//!
//! # The handshake
//!
//! A WebSocket starts life as an HTTP GET with `Upgrade: websocket`. The server
//! proves it understood by echoing back
//! `base64(sha1(client_key + MAGIC))`. This is not security — the magic string
//! is a public constant — it exists so that a caching proxy or a plain HTTP
//! server can never accidentally complete the handshake.
//!
//! # Framing
//!
//! ```text
//!   byte 0:  FIN(1) RSV(3) OPCODE(4)
//!   byte 1:  MASK(1) LEN(7)          len 126 => next 2 bytes are the length
//!                                    len 127 => next 8 bytes are the length
//!   [4-byte masking key, if MASK]
//!   payload (XORed with the key, if MASK)
//! ```
//!
//! Client-to-server frames MUST be masked; server-to-client frames MUST NOT be.
//! The mask is not encryption — it exists to stop malicious JS from crafting
//! bytes that a confused intermediary would read as an HTTP request.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;

// RFC 6455 section 1.3. Note the last group is `C5AB0DC85B11` — it is easy to
// transpose that leading `C` to the end and produce a constant that is
// self-consistent (two fkit peers will happily agree with each other) yet
// rejects, and is rejected by, every real WebSocket implementation. The test
// vector below is the guard against exactly that.
const MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Refuse absurd frames rather than trying to allocate them.
pub const MAX_FRAME: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_u8(b: u8) -> Result<Opcode> {
        Ok(match b {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            other => bail!("reserved websocket opcode 0x{other:X}"),
        })
    }
    fn to_u8(self) -> u8 {
        match self {
            Opcode::Continuation => 0x0,
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xA,
        }
    }
    fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// The byte pipe under the framing.
///
/// A plain socket and a TLS session differ only in how bytes get on and off the
/// wire, and every frame operation below already goes through `Read`/`Write`.
/// An enum keeps `WebSocket` a concrete type — the server accepts one, the CLI
/// dials one, and neither has to name a stream parameter.
enum Transport {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Transport::Tls(s) => s.flush(),
        }
    }
}

pub struct WebSocket {
    stream: Transport,
    /// Frames we send must be masked if we are the client.
    masking: bool,
    mask_counter: u64,
    pub closed: bool,
}

impl WebSocket {
    /// Complete the server side of the handshake on an accepted TCP connection.
    ///
    /// Returns the requested path alongside the socket, so the server can route
    /// `/my-repo` to the right repository.
    pub fn accept(mut stream: TcpStream) -> Result<(WebSocket, String)> {
        // The connecting side already does this; this side never did.
        //
        // Without it every reply is held by Nagle's algorithm waiting to be
        // coalesced with whatever comes next -- and since the protocol is
        // strictly request and reply, nothing comes next until this reply
        // arrives. The result is a stall per round trip, which on a transfer
        // made of thousands of them is nearly all of the wall clock. It is not
        // fatal, which is why it went unnoticed: everything worked, slowly.
        let _ = stream.set_nodelay(true);

        let request = read_http_head(&mut stream)?;
        let mut lines = request.lines();
        let start = lines.next().unwrap_or_default();
        let mut parts = start.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or("/").to_string();

        if method != "GET" {
            let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n");
            bail!("expected GET, got {method}");
        }

        let mut key = None;
        let mut upgrade_ok = false;
        for line in lines {
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "sec-websocket-key" => key = Some(v.to_string()),
                "upgrade" if v.eq_ignore_ascii_case("websocket") => upgrade_ok = true,
                _ => {}
            }
        }

        if !upgrade_ok {
            let body = "fkit websocket endpoint — connect with a fkit client\n";
            let _ = write!(
                stream,
                "HTTP/1.1 426 Upgrade Required\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            bail!("not a websocket upgrade request");
        }
        let key = key.context("missing Sec-WebSocket-Key header")?;

        let accept = base64_encode(&sha1(format!("{key}{MAGIC}").as_bytes()));
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )?;
        stream.flush()?;

        Ok((
            WebSocket {
                stream: Transport::Plain(stream),
                masking: false,
                mask_counter: 0,
                closed: false,
            },
            path,
        ))
    }

    /// Client side: connect to `ws://host:port/path` or `wss://host/path`.
    ///
    /// The default port follows the scheme the way a browser's does — 7420 for
    /// plain (fkitd's port), 443 for TLS — so a `wss://` URL with no port is
    /// the ordinary case rather than something to remember.
    pub fn connect(url: &str) -> Result<WebSocket> {
        let (rest, secure) = match url.strip_prefix("wss://") {
            Some(rest) => (rest, true),
            None => (
                url.strip_prefix("ws://")
                    .context("a remote must be a ws:// or wss:// URL")?,
                false,
            ),
        };
        #[cfg(not(feature = "tls"))]
        if secure {
            bail!("this build has no TLS support — rebuild with the 'tls' feature for wss://");
        }

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.to_string();
        // The Host header carries the authority as written; a default port must
        // not appear in it, or a virtual host will not match.
        let host = authority.clone();
        let default_port = if secure { 443 } else { 7420 };
        let addr = if authority.contains(':') {
            authority.clone()
        } else {
            format!("{authority}:{default_port}")
        };
        // The name to validate the certificate against, without any port.
        let server_name = authority.split(':').next().unwrap_or(&authority).to_string();

        let tcp = TcpStream::connect(&addr)
            .with_context(|| format!("connecting to {addr}"))?;
        tcp.set_nodelay(true)?;

        let mut stream = if secure {
            #[cfg(feature = "tls")]
            {
                tls_connect(tcp, &server_name)?
            }
            #[cfg(not(feature = "tls"))]
            {
                unreachable!("guarded above")
            }
        } else {
            Transport::Plain(tcp)
        };

        // The key only needs to be unpredictable enough to defeat caches, not
        // cryptographically random.
        let nonce = {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let mixed = t ^ ((std::process::id() as u64) << 32);
            base64_encode(&mixed.to_le_bytes().repeat(2))
        };

        write!(
            stream,
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {nonce}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        )?;
        stream.flush()?;

        let response = read_http_head(&mut stream)?;
        let status = response.lines().next().unwrap_or_default();
        if !status.contains("101") {
            bail!("server refused the websocket upgrade: {status}");
        }

        let expected = base64_encode(&sha1(format!("{nonce}{MAGIC}").as_bytes()));
        let got = response
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim().eq_ignore_ascii_case("sec-websocket-accept")).then(|| v.trim().to_string())
            })
            .context("server omitted Sec-WebSocket-Accept")?;
        if got != expected {
            bail!("bad Sec-WebSocket-Accept — this is not a websocket server");
        }

        Ok(WebSocket { stream, masking: true, mask_counter: 0, closed: false })
    }

    /// Send one binary message.
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.send_frame(Opcode::Binary, payload)
    }

    fn send_frame(&mut self, op: Opcode, payload: &[u8]) -> Result<()> {
        let mut header = Vec::with_capacity(14);
        header.push(0x80 | op.to_u8()); // FIN set: we never fragment on send

        let len = payload.len();
        let mask_bit = if self.masking { 0x80 } else { 0x00 };
        if len < 126 {
            header.push(mask_bit | len as u8);
        } else if len <= u16::MAX as usize {
            header.push(mask_bit | 126);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(mask_bit | 127);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }

        // Header and payload go out together.
        //
        // Written separately they are two segments on the wire, and with Nagle
        // in play the second waits on an acknowledgement of the first. Even
        // without it, it is two syscalls per frame for no reason. Joining them
        // costs one copy of the payload, which is already being copied when
        // masked.
        if self.masking {
            self.mask_counter = self.mask_counter.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (self.mask_counter >> 16) as u32;
            let key = key.to_be_bytes();
            header.extend_from_slice(&key);
            header.reserve(payload.len());
            for (i, b) in payload.iter().enumerate() {
                header.push(b ^ key[i % 4]);
            }
            self.stream.write_all(&header)?;
        } else {
            header.reserve(payload.len());
            header.extend_from_slice(payload);
            self.stream.write_all(&header)?;
        }
        self.stream.flush()?;
        Ok(())
    }

    /// Receive one complete message, transparently handling fragmentation and
    /// answering ping/pong. Returns `None` once the peer closes.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut message: Vec<u8> = Vec::new();
        let mut started = false;

        loop {
            let (fin, op, payload) = match self.read_frame()? {
                Some(f) => f,
                None => return Ok(None),
            };

            if op.is_control() {
                match op {
                    Opcode::Close => {
                        self.closed = true;
                        let _ = self.send_frame(Opcode::Close, &[]);
                        return Ok(None);
                    }
                    Opcode::Ping => {
                        self.send_frame(Opcode::Pong, &payload)?;
                        continue;
                    }
                    Opcode::Pong => continue,
                    _ => unreachable!(),
                }
            }

            match (started, op) {
                (false, Opcode::Continuation) => bail!("continuation frame with nothing to continue"),
                (true, Opcode::Binary | Opcode::Text) => bail!("new data frame inside a fragmented message"),
                _ => {}
            }
            started = true;
            message.extend_from_slice(&payload);

            if message.len() as u64 > MAX_FRAME {
                bail!("message exceeds {MAX_FRAME} bytes");
            }
            if fin {
                return Ok(Some(message));
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<(bool, Opcode, Vec<u8>)>> {
        let mut head = [0u8; 2];
        match self.stream.read_exact(&mut head) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                self.closed = true;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }

        let fin = head[0] & 0x80 != 0;
        if head[0] & 0x70 != 0 {
            bail!("reserved websocket bits set (no extensions negotiated)");
        }
        let op = Opcode::from_u8(head[0] & 0x0F)?;
        let masked = head[1] & 0x80 != 0;
        let len = match head[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.stream.read_exact(&mut b)?;
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0u8; 8];
                self.stream.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };

        if op.is_control() && (len > 125 || !fin) {
            bail!("control frames must be short and unfragmented");
        }
        if len > MAX_FRAME {
            bail!("frame of {len} bytes exceeds the {MAX_FRAME} byte limit");
        }

        let mut key = [0u8; 4];
        if masked {
            self.stream.read_exact(&mut key)?;
        }
        let mut payload = vec![0u8; len as usize];
        self.stream.read_exact(&mut payload)?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i % 4];
            }
        }
        Ok(Some((fin, op, payload)))
    }

    pub fn close(&mut self) {
        if !self.closed {
            let _ = self.send_frame(Opcode::Close, &[]);
            self.closed = true;
        }
    }
}

/// Read HTTP headers up to the blank line, without consuming any body bytes.
/// Wrap a connected socket in a TLS session.
///
/// Roots are compiled in rather than read from the operating system: the
/// release binaries are static musl builds that may run in a container with no
/// certificate store at all, and "works on my machine, fails in the image" is
/// exactly the failure this avoids.
#[cfg(feature = "tls")]
fn tls_connect(tcp: TcpStream, server_name: &str) -> Result<Transport> {
    use rustls::pki_types::pem::PemObject;
    use std::sync::Arc;

    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };

    // A private deployment — an internal CA, a certificate minted for a name
    // that only exists on an overlay network — has no path to a public root.
    // FKIT_CA_BUNDLE adds those, which is the difference between "use TLS" and
    // "turn TLS off", and the reason there is no --insecure flag.
    if let Ok(path) = std::env::var("FKIT_CA_BUNDLE")
        && !path.is_empty()
    {
        let pem = std::fs::read(&path)
            .with_context(|| format!("reading FKIT_CA_BUNDLE {path}"))?;
        let mut added = 0usize;
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
            let cert = cert.with_context(|| format!("parsing a certificate in {path}"))?;
            roots.add(cert).with_context(|| format!("adding a certificate from {path}"))?;
            added += 1;
        }
        if added == 0 {
            bail!("FKIT_CA_BUNDLE {path} contains no certificates");
        }
    }
    // The provider is named rather than left to rustls' process-level default.
    // Another crate in the same binary (the hub's HTTP client) enables a
    // different provider, and with two compiled in rustls refuses to guess —
    // at runtime, by panicking, which is not a thing to discover in the field.
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("no usable TLS protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|_| anyhow::anyhow!("{server_name} is not a valid DNS name for TLS"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .with_context(|| format!("starting TLS with {server_name}"))?;

    Ok(Transport::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
}

fn read_http_head<S: Read>(stream: &mut S) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 16 * 1024 {
            bail!("HTTP headers too large");
        }
        let n = stream.read(&mut byte)?;
        if n == 0 {
            bail!("connection closed during handshake");
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

// ---- SHA-1 and base64, needed only for the handshake --------------------
//
// SHA-1 is cryptographically broken and MUST NOT be used for anything that
// matters. It appears here solely because RFC 6455 hard-codes it into the
// handshake, where its only job is to prove the server parsed the request.
// Everything fkit actually trusts is hashed with BLAKE3.

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex(sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// The published RFC 6455 section 1.3 example.
    ///
    /// This vector is interoperability itself: it is what tungstenite, every
    /// browser, and every other WebSocket stack compute. If this test fails,
    /// the MAGIC constant is wrong — do not "fix" the expected value.
    #[test]
    fn handshake_accept_matches_the_rfc_example() {
        let accept = |key: &str| base64_encode(&sha1(format!("{key}{MAGIC}").as_bytes()));
        assert_eq!(accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
