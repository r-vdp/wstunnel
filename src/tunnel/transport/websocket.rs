use crate::tunnel::transport::{headers_from_file, TunnelRead, TunnelWrite, MAX_PACKET_LENGTH};
use crate::tunnel::{tunnel_to_jwt_token, RemoteAddr, JWT_HEADER_PREFIX};
use crate::WsClientConfig;
use anyhow::{anyhow, Context};
use bytes::{Bytes, BytesMut};
use fastwebsockets::{Frame, OpCode, Payload, WebSocketRead, WebSocketWrite};
use futures_util::lock::Mutex;
use http_body_util::Empty;
use hyper::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE};
use hyper::header::{CONNECTION, HOST, SEC_WEBSOCKET_KEY};
use hyper::http::response::Parts;
use hyper::upgrade::Upgraded;
use hyper::Request;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use std::io;
use std::io::ErrorKind;
use std::ops::DerefMut;
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tracing::{debug, trace};
use uuid::Uuid;

#[derive(Debug)]
pub struct PingState {
    ping_seq: u8,
    pong_seq: u8,
    max_diff: u8,
}

impl PingState {
    pub const fn new() -> Self {
        Self {
            ping_seq: 0,
            pong_seq: 0,
            // TODO: make this configurable
            max_diff: 3,
        }
    }

    fn is_ok(&self) -> bool {
        self.ping_seq - self.pong_seq <= self.max_diff
    }

    fn ping_inc(&mut self) {
        match self.ping_seq.checked_add(1) {
            Some(ping) => self.ping_seq = ping,
            // We reached the end of the range, so we will just start over from zero.
            None => self.reset(),
        }
    }

    fn set_pong_seq(&mut self, seq: u8) {
        if seq > self.pong_seq && seq <= self.ping_seq {
            self.pong_seq = seq;
        }

        // Try to reset once we reached half the range, since we will potentially
        // miss some pongs if we reach the actual end of the range where we need
        // to forcefully reset.
        if self.ping_seq == self.pong_seq && self.ping_seq > u8::MAX / 2 {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.ping_seq = 0;
        self.pong_seq = 0;
    }
}

pub struct WebsocketTunnelWrite {
    inner: Arc<Mutex<WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>>>,
    buf: BytesMut,
    ping_state: Arc<Mutex<PingState>>,
}

impl WebsocketTunnelWrite {
    pub fn new(
        ws: Arc<Mutex<WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>>>,
        ping_state: Arc<Mutex<PingState>>,
    ) -> Self {
        Self {
            inner: ws,
            buf: BytesMut::with_capacity(MAX_PACKET_LENGTH),
            ping_state,
        }
    }
}

impl TunnelWrite for WebsocketTunnelWrite {
    fn buf_mut(&mut self) -> &mut BytesMut {
        &mut self.buf
    }

    async fn write(&mut self) -> Result<(), io::Error> {
        let read_len = self.buf.len();
        let buf = &mut self.buf;

        let ret = self
            .inner
            .lock()
            .await
            .write_frame(Frame::binary(Payload::BorrowedMut(&mut buf[..read_len])))
            .await;

        if let Err(err) = ret {
            return Err(io::Error::new(ErrorKind::ConnectionAborted, err));
        }

        // If the buffer has been completely filled with previous read, Grows it !
        // For the buffer to not be a bottleneck when the TCP window scale.
        // We clamp it to 32Mb to avoid unbounded growth and as websocket max frame size is 64Mb by default
        // For udp, the buffer will never grow.
        const _32_MB: usize = 32 * 1024 * 1024;
        buf.clear();
        if buf.capacity() == read_len && buf.capacity() < _32_MB {
            let new_size = buf.capacity() + (buf.capacity() / 4); // grow buffer by 1.25 %
            buf.reserve(new_size);
            trace!(
                "Buffer {} Mb {} {} {}",
                buf.capacity() as f64 / 1024.0 / 1024.0,
                new_size,
                buf.len(),
                buf.capacity()
            )
        }

        Ok(())
    }

    async fn ping(&mut self) -> Result<(), io::Error> {
        let mut ping_state = self.ping_state.lock().await;
        debug!("{:?}", *ping_state);
        if !ping_state.is_ok() {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "No pong received"));
        }
        ping_state.ping_inc();
        debug!("Sending ping({})", ping_state.ping_seq);
        if let Err(err) = self
            .inner
            .lock()
            .await
            .write_frame(Frame::new(
                true,
                OpCode::Ping,
                None,
                Payload::BorrowedMut(&mut [ping_state.ping_seq]),
            ))
            .await
        {
            return Err(io::Error::new(ErrorKind::BrokenPipe, err));
        }

        Ok(())
    }

    async fn close(&mut self) -> Result<(), io::Error> {
        if let Err(err) = self.inner.lock().await.write_frame(Frame::close(1000, &[])).await {
            return Err(io::Error::new(ErrorKind::BrokenPipe, err));
        }

        Ok(())
    }
}

pub struct WebsocketTunnelRead {
    ws_rx: WebSocketRead<ReadHalf<TokioIo<Upgraded>>>,
    ws_tx: Arc<Mutex<WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>>>,
    ping_state: Arc<Mutex<PingState>>,
}

impl WebsocketTunnelRead {
    pub const fn new(
        ws_rx: WebSocketRead<ReadHalf<TokioIo<Upgraded>>>,
        ws_tx: Arc<Mutex<WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>>>,
        ping_state: Arc<Mutex<PingState>>,
    ) -> Self {
        Self {
            ws_rx,
            ws_tx,
            ping_state,
        }
    }
}

impl TunnelRead for WebsocketTunnelRead {
    async fn copy(&mut self, mut writer: impl AsyncWrite + Unpin + Send) -> Result<(), io::Error> {
        loop {
            let msg = match self
                .ws_rx
                .read_frame(&mut |frame| async { self.ws_tx.clone().lock().await.write_frame(frame).await })
                .await
            {
                Ok(msg) => msg,
                Err(err) => return Err(io::Error::new(ErrorKind::ConnectionAborted, err)),
            };

            trace!("receive ws frame {:?} {:?}", msg.opcode, msg.payload);
            match msg.opcode {
                OpCode::Continuation | OpCode::Text | OpCode::Binary => {
                    return match writer.write_all(msg.payload.as_ref()).await {
                        Ok(_) => Ok(()),
                        Err(err) => Err(io::Error::new(ErrorKind::ConnectionAborted, err)),
                    }
                }
                OpCode::Close => return Err(io::Error::new(ErrorKind::NotConnected, "websocket close")),
                // Pings get handled internally, see the closure that we pass to read_frame above
                OpCode::Ping => continue,
                OpCode::Pong => {
                    let seq = msg.payload[0];
                    debug!("Received pong({})", seq);
                    let mut ping_state = self.ping_state.lock().await;
                    ping_state.set_pong_seq(seq);
                    debug!("{:?}", *ping_state);
                }
            };
        }
    }
}

pub async fn connect(
    request_id: Uuid,
    client_cfg: &WsClientConfig,
    dest_addr: &RemoteAddr,
) -> anyhow::Result<(WebsocketTunnelRead, WebsocketTunnelWrite, Parts)> {
    let mut pooled_cnx = match client_cfg.cnx_pool().get().await {
        Ok(cnx) => Ok(cnx),
        Err(err) => Err(anyhow!("failed to get a connection to the server from the pool: {err:?}")),
    }?;

    let mut req = Request::builder()
        .method("GET")
        .uri(format!("/{}/events", &client_cfg.http_upgrade_path_prefix))
        .header(HOST, &client_cfg.http_header_host)
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "upgrade")
        .header(SEC_WEBSOCKET_KEY, fastwebsockets::handshake::generate_key())
        .header(SEC_WEBSOCKET_VERSION, "13")
        .header(
            SEC_WEBSOCKET_PROTOCOL,
            format!("v1, {}{}", JWT_HEADER_PREFIX, tunnel_to_jwt_token(request_id, dest_addr)),
        )
        .version(hyper::Version::HTTP_11);

    let headers = req.headers_mut().unwrap();
    for (k, v) in &client_cfg.http_headers {
        let _ = headers.remove(k);
        headers.append(k, v.clone());
    }

    if let Some(auth) = &client_cfg.http_upgrade_credentials {
        let _ = headers.remove(AUTHORIZATION);
        headers.append(AUTHORIZATION, auth.clone());
    }

    if let Some(headers_file_path) = &client_cfg.http_headers_file {
        let (host, headers_file) = headers_from_file(headers_file_path);
        for (k, v) in headers_file {
            let _ = headers.remove(&k);
            headers.append(k, v);
        }
        if let Some((host, val)) = host {
            let _ = headers.remove(&host);
            headers.append(host, val);
        }
    }

    let req = req.body(Empty::<Bytes>::new()).with_context(|| {
        format!(
            "failed to build HTTP request to contact the server {:?}",
            client_cfg.remote_addr
        )
    })?;
    debug!("with HTTP upgrade request {:?}", req);
    let transport = pooled_cnx.deref_mut().take().unwrap();
    let (mut ws, response) = fastwebsockets::handshake::client(&TokioExecutor::new(), req, transport)
        .await
        .with_context(|| format!("failed to do websocket handshake with the server {:?}", client_cfg.remote_addr))?;

    ws.set_auto_apply_mask(client_cfg.websocket_mask_frame);

    let (ws_rx, ws_tx) = ws.split(tokio::io::split);
    let ws_tx = Arc::new(Mutex::new(ws_tx));
    let ping_state = Arc::new(Mutex::new(PingState::new()));

    Ok((
        WebsocketTunnelRead::new(ws_rx, ws_tx.clone(), ping_state.clone()),
        WebsocketTunnelWrite::new(ws_tx, ping_state),
        response.into_parts().0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping_state() {
        let mut ping_state = PingState::new();

        // An initial ping state has zeroes and is OK
        assert!(ping_state.is_ok());
        assert_eq!(ping_state.ping_seq, 0);
        assert_eq!(ping_state.pong_seq, 0);

        // Send 3 pings, the ping sequence increases, pong sequence doesn't
        for it in 1..=3 {
            ping_state.ping_inc();
            assert_eq!(ping_state.ping_seq, it);
            assert_eq!(ping_state.pong_seq, 0);
            assert!(ping_state.is_ok());
        }

        // After the fourth ping with no pong received, the ping state is not OK
        ping_state.ping_inc();
        assert_eq!(ping_state.ping_seq, 4);
        assert_eq!(ping_state.pong_seq, 0);
        assert!(!ping_state.is_ok());

        // We received two pongs, the pin state is OK again
        ping_state.set_pong_seq(1);
        assert!(ping_state.is_ok());
        ping_state.set_pong_seq(4);
        assert!(ping_state.is_ok());

        // Advance the ping state beyond the middle of the u8 range,
        // it won't wrap since we didn't receive pongs
        for _ in 5..=130 {
            ping_state.ping_inc();
        }
        assert_eq!(ping_state.ping_seq, 130);
        assert_eq!(ping_state.pong_seq, 4);
        assert!(!ping_state.is_ok());

        // As soon as we do receive a pong, we wrap the sequence numbers around
        ping_state.set_pong_seq(130);
        assert_eq!(ping_state.ping_seq, 0);
        assert_eq!(ping_state.pong_seq, 0);
        assert!(ping_state.is_ok());

        // If we receive pongs for every ping, we wrap at 128, half of the u8 range
        for it in 1..=128 {
            ping_state.ping_inc();
            ping_state.set_pong_seq(it)
        }
        assert_eq!(ping_state.ping_seq, 0);
        assert_eq!(ping_state.pong_seq, 0);
        assert!(ping_state.is_ok());
    }
}
