use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message as WsMessage, WebSocket};
use yrs::sync::{Awareness, Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Origin, ReadTxn, Subscription, Transact, Update};

use super::document;
use crate::app::output::log_global;
use crate::system::LockRecover;

const READ_SLICE: Duration = Duration::from_millis(40);
const PING_INTERVAL: Duration = Duration::from_secs(20);

pub(crate) struct Room {
    awareness: Arc<Mutex<Awareness>>,
    peers: Mutex<Vec<PeerHandle>>,
    next_peer: AtomicU64,
    stopping: AtomicBool,
    synced_upstream: AtomicBool,
    subscriptions: Mutex<Vec<Subscription>>,
}

struct PeerHandle {
    id: u64,
    origin: Origin,
    sender: mpsc::Sender<Vec<u8>>,
    upstream: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Local,
    Upstream,
}

impl Room {
    pub(crate) fn new(awareness: Arc<Mutex<Awareness>>) -> Result<Arc<Room>> {
        let room = Arc::new(Room {
            awareness: awareness.clone(),
            peers: Mutex::new(Vec::new()),
            next_peer: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
            synced_upstream: AtomicBool::new(false),
            subscriptions: Mutex::new(Vec::new()),
        });
        let mut subscriptions = Vec::new();
        {
            let guard = awareness.lock_recover();
            let for_updates = Arc::downgrade(&room);
            subscriptions.push(
                guard
                    .doc()
                    .observe_update_v1(move |txn, event| {
                        if let Some(room) = for_updates.upgrade() {
                            let message = Message::Sync(SyncMessage::Update(event.update.clone()));
                            room.broadcast(encode(&message), txn.origin());
                        }
                    })
                    .map_err(|error| {
                        anyhow::anyhow!("Could not observe the shared document: {error}")
                    })?,
            );
            let for_awareness = Arc::downgrade(&room);
            subscriptions.push(guard.on_update(move |awareness, event, origin| {
                let Some(room) = for_awareness.upgrade() else {
                    return;
                };
                let changed = event.summary().all_changes();
                if changed.is_empty() {
                    return;
                }
                if let Ok(update) = awareness.update_with_clients(changed) {
                    room.broadcast(encode(&Message::Awareness(update)), origin);
                }
            }));
        }
        *room.subscriptions.lock_recover() = subscriptions;
        Ok(room)
    }

    pub(crate) fn synced_upstream(&self) -> bool {
        self.synced_upstream.load(Ordering::Acquire)
    }

    pub(crate) fn awareness(&self) -> &Arc<Mutex<Awareness>> {
        &self.awareness
    }

    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.peers.lock_recover().clear();
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub(crate) fn peer_count(&self, link: Link) -> usize {
        self.peers
            .lock_recover()
            .iter()
            .filter(|peer| peer.upstream == (link == Link::Upstream))
            .count()
    }

    fn broadcast(&self, bytes: Vec<u8>, origin: Option<&Origin>) {
        let mut peers = self.peers.lock_recover();
        peers.retain(|peer| {
            if origin == Some(&peer.origin) {
                return true;
            }
            peer.sender.send(bytes.clone()).is_ok()
        });
    }

    fn register(&self, upstream: bool) -> (PeerHandle, mpsc::Receiver<Vec<u8>>) {
        let id = self.next_peer.fetch_add(1, Ordering::AcqRel);
        let (sender, receiver) = mpsc::channel();
        let handle = PeerHandle {
            id,
            origin: Origin::from(format!("peer:{id}").as_str()),
            sender,
            upstream,
        };
        (handle, receiver)
    }

    fn attach(&self, handle: PeerHandle) -> (u64, Origin) {
        let id = handle.id;
        let origin = handle.origin.clone();
        self.peers.lock_recover().push(handle);
        (id, origin)
    }

    fn detach(&self, id: u64) {
        self.peers.lock_recover().retain(|peer| peer.id != id);
        let awareness = self.awareness.lock_recover();
        let stale = awareness
            .iter()
            .filter(|(client, state)| *client != awareness.client_id() && state.data.is_none())
            .map(|(client, _)| client)
            .collect::<Vec<_>>();
        for client in stale {
            awareness.remove_state(client);
        }
    }

    fn handle_message(&self, origin: &Origin, message: Message) -> Result<Option<Message>> {
        match message {
            Message::Sync(SyncMessage::SyncStep1(state_vector)) => {
                let awareness = self.awareness.lock_recover();
                let update = awareness
                    .doc()
                    .transact()
                    .encode_state_as_update_v1(&state_vector);
                Ok(Some(Message::Sync(SyncMessage::SyncStep2(update))))
            }
            Message::Sync(SyncMessage::SyncStep2(update))
            | Message::Sync(SyncMessage::Update(update)) => {
                let update = Update::decode_v1(&update).context("Malformed document update")?;
                let awareness = self.awareness.lock_recover();
                let mut txn = awareness.doc().transact_mut_with(origin.clone());
                txn.apply_update(update)
                    .context("Document update could not be applied")?;
                Ok(None)
            }
            Message::Auth(Some(reason)) => bail!("The room rejected this peer: {reason}"),
            Message::Auth(None) => Ok(None),
            Message::AwarenessQuery => {
                let awareness = self.awareness.lock_recover();
                Ok(Some(Message::Awareness(awareness.update()?)))
            }
            Message::Awareness(update) => {
                let awareness = self.awareness.lock_recover();
                awareness.apply_update_with(update, origin.clone())?;
                Ok(None)
            }
            Message::Custom(_, _) => Ok(None),
        }
    }

    fn greeting(&self) -> Result<Vec<u8>> {
        let awareness = self.awareness.lock_recover();
        let state_vector = awareness.doc().transact().state_vector();
        let mut encoder = EncoderV1::new();
        Message::Sync(SyncMessage::SyncStep1(state_vector)).encode(&mut encoder);
        Message::Awareness(awareness.update()?).encode(&mut encoder);
        Ok(encoder.to_vec())
    }

    pub(crate) fn serve_socket<S>(
        self: &Arc<Self>,
        mut socket: WebSocket<S>,
        link: Link,
    ) -> Result<()>
    where
        S: std::io::Read + std::io::Write,
    {
        let (handle, outgoing) = self.register(link == Link::Upstream);
        let (id, origin) = self.attach(handle);
        let result = self.pump(&mut socket, &origin, &outgoing, link);
        self.detach(id);
        let _ = socket.close(None);
        result
    }

    fn pump<S>(
        &self,
        socket: &mut WebSocket<S>,
        origin: &Origin,
        outgoing: &mpsc::Receiver<Vec<u8>>,
        link: Link,
    ) -> Result<()>
    where
        S: std::io::Read + std::io::Write,
    {
        socket.send(WsMessage::Binary(self.greeting()?.into()))?;
        let mut last_ping = Instant::now();
        loop {
            if self.stopping() {
                return Ok(());
            }
            match socket.read() {
                Ok(WsMessage::Binary(data)) => {
                    let responses = self.handle_bytes(origin, &data, link)?;
                    for response in responses {
                        socket.send(WsMessage::Binary(response.into()))?;
                    }
                }
                Ok(WsMessage::Close(_)) => return Ok(()),
                Ok(WsMessage::Ping(payload)) => socket.send(WsMessage::Pong(payload))?,
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            while let Ok(bytes) = outgoing.try_recv() {
                socket.send(WsMessage::Binary(bytes.into()))?;
            }
            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                socket.send(WsMessage::Ping(Vec::new().into()))?;
            }
        }
    }

    fn handle_bytes(&self, origin: &Origin, data: &[u8], link: Link) -> Result<Vec<Vec<u8>>> {
        let mut decoder = DecoderV1::new(yrs::encoding::read::Cursor::new(data));
        let reader = MessageReader::new(&mut decoder);
        let mut responses = Vec::new();
        for message in reader {
            let message = message.context("Malformed room message")?;
            let completes_sync = matches!(message, Message::Sync(SyncMessage::SyncStep2(_)));
            if let Some(response) = self.handle_message(origin, message)? {
                responses.push(encode(&response));
            }
            if completes_sync && link == Link::Upstream {
                self.synced_upstream.store(true, Ordering::Release);
            }
        }
        Ok(responses)
    }
}

pub(crate) fn encode(message: &Message) -> Vec<u8> {
    let mut encoder = EncoderV1::new();
    message.encode(&mut encoder);
    encoder.to_vec()
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Listener {
    pub(crate) port: u16,
}

pub(crate) fn listen(room: Arc<Room>, token: String) -> Result<Listener> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("Could not open the collaboration port")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    thread::Builder::new()
        .name(format!("renium-collab-listen-{port}"))
        .spawn(move || {
            while !room.stopping() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let room = room.clone();
                        let token = token.clone();
                        thread::Builder::new()
                            .name("renium-collab-peer".to_string())
                            .spawn(move || {
                                if let Err(error) = accept_peer(&room, stream, &token) {
                                    log_global(
                                        4,
                                        format_args!("[renium] collab peer ended: {error:#}"),
                                    );
                                }
                            })
                            .ok();
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(200)),
                }
            }
        })
        .context("Could not start the collaboration listener")?;
    Ok(Listener { port })
}

#[allow(clippy::result_large_err)]
fn accept_peer(room: &Arc<Room>, stream: TcpStream, token: &str) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let expected = token.to_string();
    let socket = tungstenite::accept_hdr(
        stream,
        |request: &tungstenite::handshake::server::Request, response| {
            let presented = request
                .uri()
                .query()
                .and_then(|query| query_value(query, "token"))
                .unwrap_or_default();
            if presented == expected {
                Ok(response)
            } else {
                let response = tungstenite::handshake::server::ErrorResponse::new(Some(
                    "invalid token".to_string(),
                ));
                Err(response)
            }
        },
    )
    .map_err(|error| anyhow::anyhow!("Collaboration handshake failed: {error}"))?;
    socket.get_ref().set_read_timeout(Some(READ_SLICE))?;
    room.serve_socket(socket, Link::Local)
}

pub(crate) fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

pub(crate) struct UpstreamStatus {
    pub(crate) connected: AtomicBool,
    pub(crate) error: Mutex<Option<String>>,
}

impl Default for UpstreamStatus {
    fn default() -> Self {
        Self {
            connected: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }
}

pub(crate) fn connect_upstream(
    room: Arc<Room>,
    url: String,
    status: Arc<UpstreamStatus>,
) -> Result<()> {
    thread::Builder::new()
        .name("renium-collab-upstream".to_string())
        .spawn(move || {
            let mut delay = Duration::from_millis(500);
            while !room.stopping() {
                match open_upstream(&url) {
                    Ok(socket) => {
                        status.connected.store(true, Ordering::Release);
                        *status.error.lock_recover() = None;
                        delay = Duration::from_millis(500);
                        let result = room.serve_socket(socket, Link::Upstream);
                        status.connected.store(false, Ordering::Release);
                        if let Err(error) = result {
                            *status.error.lock_recover() = Some(format!("{error:#}"));
                        }
                    }
                    Err(error) => {
                        *status.error.lock_recover() = Some(format!("{error:#}"));
                    }
                }
                if room.stopping() {
                    break;
                }
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        })
        .context("Could not start the collaboration connection")?;
    Ok(())
}

fn open_upstream(url: &str) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let (socket, _) =
        tungstenite::connect(url).with_context(|| format!("Could not reach {url}"))?;
    let stream = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => Some(stream),
        MaybeTlsStream::Rustls(stream) => Some(stream.get_ref()),
        _ => None,
    };
    if let Some(stream) = stream {
        stream.set_read_timeout(Some(READ_SLICE))?;
    }
    Ok(socket)
}

pub(crate) fn document_is_empty(awareness: &Arc<Mutex<Awareness>>) -> bool {
    let awareness = awareness.lock_recover();
    let doc = awareness.doc();
    let files = document::files_map(doc);
    let meta = document::meta_map(doc);
    let txn = doc.transact();
    document::entry_count(&txn, &files) == 0
        && document::meta_value(&txn, &meta, "project").is_none()
}
