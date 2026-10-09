use super::*;
use anyhow::{bail, Context};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc as channel, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const REQUEST_WAIT: Duration = SEND_INPUT_TIMEOUT.saturating_mul(4);
const TEST_SANDBOX_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

#[derive(Clone, Copy)]
enum Reply {
    Success,
    Error,
}

struct InputRequest {
    payload: Vec<u8>,
    reply: Option<oneshot::Sender<Reply>>,
}

impl InputRequest {
    fn respond(&mut self, reply: Reply) {
        assert!(self.reply.take().unwrap().send(reply).is_ok());
    }
}

struct RpcFixture {
    address: String,
    requests: channel::UnboundedReceiver<InputRequest>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl RpcFixture {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = format!("http://{}", listener.local_addr()?);
        let (requests_tx, requests) = channel::unbounded_channel();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted?;
                        connections.spawn(receive_input(socket, requests_tx.clone()));
                    }
                    finished = connections.join_next(), if !connections.is_empty() => {
                        finished.unwrap()??;
                    }
                }
            }
            connections.shutdown().await;
            Ok(())
        });
        Ok(Self {
            address,
            requests,
            stop: Some(stop),
            task: Some(task),
        })
    }

    async fn next(&mut self) -> Option<InputRequest> {
        tokio::time::timeout(REQUEST_WAIT, self.requests.recv())
            .await
            .ok()
            .flatten()
    }

    async fn expect_input(&mut self, expected: &[u8], phase: &str) -> Result<InputRequest> {
        let request = tokio::time::timeout(REQUEST_WAIT, self.requests.recv())
            .await
            .with_context(|| format!("timed out waiting for {phase} payload {expected:?}"))?
            .with_context(|| {
                format!("request channel closed while waiting for {phase} payload {expected:?}")
            })?;
        assert_eq!(request.payload, expected, "unexpected {phase} payload");
        Ok(request)
    }

    async fn finish(mut self) -> Result<()> {
        let _ = self.stop.take().unwrap().send(());
        tokio::time::timeout(REQUEST_WAIT, self.task.as_mut().unwrap()).await???;
        Ok(())
    }
}

impl Drop for RpcFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<SendInputRequest> {
    let mut bytes = Vec::new();
    loop {
        if socket.read_buf(&mut bytes).await? == 0 {
            bail!("input request closed before its body arrived");
        }
        if bytes.len() > 16 * 1024 {
            bail!("input request exceeds fixture limit");
        }
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end])?;
        if headers.lines().next() != Some("POST /process.Process/SendInput HTTP/1.1") {
            bail!("unexpected input RPC");
        }
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .context("input request is missing content length")?
            .1
            .trim()
            .parse::<usize>()?;
        if length > 16 * 1024 - end - 4 {
            bail!("input request body exceeds fixture limit");
        }
        if bytes.len() >= end + 4 + length {
            return Ok(SendInputRequest::decode(&bytes[end + 4..end + 4 + length])?);
        }
    }
}

async fn receive_input(
    mut socket: TcpStream,
    requests: channel::UnboundedSender<InputRequest>,
) -> Result<()> {
    let request = tokio::time::timeout(REQUEST_WAIT, read_request(&mut socket)).await??;
    if request.process.and_then(|process| process.selector)
        != Some(process_selector::Selector::Pid(42))
    {
        bail!("unexpected process selector");
    }
    let Some(process_input::Input::Pty(payload)) = request.input.and_then(|input| input.input)
    else {
        bail!("expected PTY input");
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    requests.send(InputRequest {
        payload,
        reply: Some(reply_tx),
    })?;
    let Ok(reply) = reply_rx.await else {
        return Ok(());
    };
    let response = match reply {
        Reply::Success => b"HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
        Reply::Error => b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
    };
    socket.write_all(response).await?;
    Ok(())
}

struct InputSession {
    state: SessionState,
    task: Option<JoinHandle<SessionEnd>>,
}

impl InputSession {
    async fn finish(mut self) -> Result<()> {
        self.state.stop();
        let end = tokio::time::timeout(REQUEST_WAIT, self.task.as_mut().unwrap()).await??;
        assert!(matches!(end, SessionEnd::Clean));
        Ok(())
    }
}

impl Drop for InputSession {
    fn drop(&mut self) {
        self.state.stop();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn check_recovery_input(
    first_payload: &[u8],
    queued_payloads: [&[u8]; 2],
    first_reply: Option<Reply>,
) -> Result<()> {
    let mut fixture = RpcFixture::start().await?;
    let transport = Arc::new(Transport::new(&fixture.address, TEST_SANDBOX_ID, None)?);
    let selector = ProcessSelector {
        selector: Some(process_selector::Selector::Pid(42)),
    };
    let state = SessionState::new();
    state.recovery_mode.store(true, Ordering::Relaxed);
    let (stdin_tx, stdin_rx) = mpsc::unbounded();
    let (activity_tx, _activity_rx) = mpsc::unbounded();
    let task = tokio::spawn(maintain_stream_input(
        transport,
        selector,
        stdin_rx,
        state.clone(),
        activity_tx,
    ));
    let session = InputSession {
        state,
        task: Some(task),
    };

    stdin_tx.unbounded_send(first_payload.to_vec())?;
    let mut first = fixture
        .expect_input(first_payload, "first recovery input")
        .await?;
    let mut received = vec![first.payload.clone()];
    for payload in queued_payloads {
        stdin_tx.unbounded_send(payload.to_vec())?;
    }
    if let Some(reply) = first_reply {
        first.respond(reply);
    }

    for (index, payload) in queued_payloads.into_iter().enumerate() {
        let mut request = fixture
            .expect_input(payload, &format!("queued recovery input {}", index + 1))
            .await?;
        received.push(request.payload.clone());
        request.respond(Reply::Success);
    }
    stdin_tx.unbounded_send(b"fresh\n".to_vec())?;
    let mut fresh = fixture
        .expect_input(b"fresh\n", "fresh recovery input")
        .await?;
    received.push(fresh.payload.clone());
    fresh.respond(Reply::Success);
    drop(first);
    session.finish().await?;
    let extra = fixture.next().await.map(|request| request.payload);
    fixture.finish().await?;
    assert!(extra.is_none(), "unexpected extra input request: {extra:?}");
    assert_eq!(
        received,
        vec![
            first_payload.to_vec(),
            queued_payloads[0].to_vec(),
            queued_payloads[1].to_vec(),
            b"fresh\n".to_vec()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn recovery_keeps_later_input_after_success() -> Result<()> {
    check_recovery_input(&[0x03], [b"second\n", b"third\n"], Some(Reply::Success)).await
}

#[tokio::test]
async fn recovery_keeps_later_input_after_rpc_error() -> Result<()> {
    check_recovery_input(&[0x03], [b"second\n", b"third\n"], Some(Reply::Error)).await
}

#[tokio::test]
async fn recovery_keeps_later_input_after_timeout() -> Result<()> {
    check_recovery_input(&[0x03], [b"second\n", b"third\n"], None).await
}

#[tokio::test]
async fn recovery_keeps_ctrl_c_queued_during_another_input() -> Result<()> {
    check_recovery_input(
        b"first\n",
        [&[0x03], b"after-interrupt\n"],
        Some(Reply::Success),
    )
    .await
}
