//! Event-driven TCP over the overlay packet interface. No OS routes or TUN device.

use crate::{Error, Stream};
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::tcp,
    time::Instant,
    wire::{HardwareAddress, IpAddress, IpCidr},
};
use std::{
    collections::VecDeque,
    net::Ipv6Addr,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU16, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::{Mutex, Notify, mpsc, oneshot},
};
use yggdrasil::ipv6rwc::ReadWriteCloser;

struct PacketDevice {
    incoming: VecDeque<Vec<u8>>,
    outgoing: VecDeque<Vec<u8>>,
}
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);
impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut bytes = vec![0; len];
        let result = f(&mut bytes);
        self.0.push_back(bytes);
        result
    }
}
impl Device for PacketDevice {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        if self.outgoing.len() >= 256 {
            return None;
        }
        self.incoming
            .pop_front()
            .map(|v| (Rx(v), Tx(&mut self.outgoing)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (self.outgoing.len() < 256).then_some(Tx(&mut self.outgoing))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = 1280;
        caps.max_burst_size = Some(64);
        caps
    }
}

enum Command {
    Connect(Ipv6Addr, u16, oneshot::Sender<Result<Stream, Error>>),
}
struct Entry {
    handle: smoltcp::iface::SocketHandle,
    bridge: Option<DuplexStream>,
}
struct NotifyWake(Arc<Notify>);
impl Wake for NotifyWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}

pub struct Stack {
    commands: mpsc::Sender<Command>,
    incoming: Mutex<mpsc::Receiver<Stream>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Stack {
    pub fn new(rwc: Arc<ReadWriteCloser>, address: Ipv6Addr, listen: Option<u16>) -> Self {
        let (commands, rx) = mpsc::channel(64);
        let (accepted, incoming) = mpsc::channel(32);
        let (packets, packet_rx) = mpsc::channel(256);
        let reader = rwc.clone();
        let read_task = tokio::spawn(async move {
            let mut buffer = vec![0; 65536];
            while let Ok(packet) = reader.read(&mut buffer).await {
                if packets.send(packet.to_vec()).await.is_err() {
                    break;
                }
            }
        });
        let reactor = tokio::spawn(run(rwc, address, listen, rx, accepted, packet_rx));
        Self {
            commands,
            incoming: Mutex::new(incoming),
            tasks: vec![read_task, reactor],
        }
    }
    ///
    /// # Errors
    /// Returns an error if the TCP stack has stopped or cannot open the connection.
    pub async fn connect(&self, address: Ipv6Addr, port: u16) -> Result<Stream, Error> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Connect(address, port, tx))
            .await
            .map_err(|_| Error::Protocol("overlay stopped".into()))?;
        rx.await
            .map_err(|_| Error::Protocol("overlay stopped".into()))?
    }
    pub async fn accept(&self) -> Option<Stream> {
        self.incoming.lock().await.recv().await
    }
}
impl Drop for Stack {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn socket() -> tcp::Socket<'static> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 65536]),
        tcp::SocketBuffer::new(vec![0; 65536]),
    );
    socket.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
    socket.set_nagle_enabled(false);
    socket
}

fn ephemeral_port() -> u16 {
    // Each saved server keeps its IPv6 address across reconnects. Reusing 49153
    // conflicts with an established or TIME_WAIT socket in the remote stack.
    // Share a rotating range across node restarts and randomize the process seed.
    static NEXT: OnceLock<AtomicU16> = OnceLock::new();
    NEXT.get_or_init(|| AtomicU16::new(49152 + rand::random::<u16>() % 16384))
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |port| {
            Some(if port == u16::MAX { 49152 } else { port + 1 })
        })
        .expect("port update always returns a value")
}

async fn run(
    rwc: Arc<ReadWriteCloser>,
    address: Ipv6Addr,
    listen: Option<u16>,
    mut commands: mpsc::Receiver<Command>,
    accepted: mpsc::Sender<Stream>,
    mut packets: mpsc::Receiver<Vec<u8>>,
) {
    let start = std::time::Instant::now();
    let timestamp =
        || Instant::from_millis(i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX));
    let mut device = PacketDevice {
        incoming: VecDeque::new(),
        outgoing: VecDeque::new(),
    };
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = rand::random();
    let mut interface = Interface::new(config, &mut device, timestamp());
    interface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv6(address), 7));
    });
    let mut sockets = SocketSet::new(Vec::new());
    let mut entries: Vec<Entry> = Vec::new();
    let notify = Arc::new(Notify::new());
    let waker = Waker::from(Arc::new(NotifyWake(notify.clone())));
    loop {
        if let Some(port) = listen
            && entries.len() < 64
            && !entries
                .iter()
                .any(|e| sockets.get::<tcp::Socket>(e.handle).state() == tcp::State::Listen)
        {
            let mut listener = socket();
            if listener.listen(port).is_ok() {
                entries.push(Entry {
                    handle: sockets.add(listener),
                    bridge: None,
                });
            }
        }
        interface.poll(timestamp(), &mut device, &mut sockets);
        poll_connections(&mut entries, &mut sockets, &accepted, &waker);
        entries.retain(|entry| {
            if sockets.get::<tcp::Socket>(entry.handle).state() == tcp::State::Closed {
                sockets.remove(entry.handle);
                false
            } else {
                true
            }
        });
        interface.poll(timestamp(), &mut device, &mut sockets);
        while let Some(packet) = device.outgoing.pop_front() {
            if rwc.write(&packet).await.is_err() {
                return;
            }
        }
        let delay =
            interface
                .poll_delay(timestamp(), &sockets)
                .map_or(Duration::from_secs(30), |d| {
                    Duration::from_millis(d.total_millis())
                        .max(Duration::from_millis(1))
                        .min(Duration::from_secs(30))
                });
        tokio::select! {
            command=commands.recv()=>match command {
                Some(Command::Connect(remote,port,result))=>{
                    if entries.len()>=64 {let _=result.send(Err(Error::Protocol("too many overlay connections".into())));continue;}
                    let mut connection=socket();
                    let local_port = loop {
                        let candidate = ephemeral_port();
                        if listen != Some(candidate) && !sockets.iter().any(|(_, socket)| {
                            let smoltcp::socket::Socket::Tcp(socket) = socket;
                            socket.local_endpoint().is_some_and(|endpoint| endpoint.port == candidate)
                        }) { break candidate; }
                    };
                    match connection.connect(interface.context(),(IpAddress::Ipv6(remote),port),local_port){
                        Ok(())=>{let(app,bridge)=tokio::io::duplex(65536);entries.push(Entry{handle:sockets.add(connection),bridge:Some(bridge)});let _=result.send(Ok(Box::new(app)));},
                        Err(error)=>{let _=result.send(Err(Error::Protocol(error.to_string())));}
                    }
                },None=>return
            },
            packet=packets.recv()=>match packet {Some(packet)=>device.incoming.push_back(packet),None=>return},
            ()=notify.notified()=>{},
            ()=tokio::time::sleep(delay)=>{}
        }
    }
}

fn poll_connections(
    entries: &mut [Entry],
    sockets: &mut SocketSet<'_>,
    accepted: &mpsc::Sender<Stream>,
    waker: &Waker,
) {
    let mut cx = Context::from_waker(waker);
    for entry in entries {
        let socket = sockets.get_mut::<tcp::Socket>(entry.handle);
        if entry.bridge.is_none() && socket.state() == tcp::State::Established {
            let (app, bridge) = tokio::io::duplex(65536);
            if accepted.try_send(Box::new(app)).is_ok() {
                entry.bridge = Some(bridge);
            } else {
                socket.abort();
            }
        }
        let Some(bridge) = &mut entry.bridge else {
            continue;
        };
        if socket.can_send() {
            let mut closed = false;
            let _ = socket.send(|bytes| {
                let mut read = ReadBuf::new(bytes);
                match Pin::new(&mut *bridge).poll_read(&mut cx, &mut read) {
                    Poll::Ready(Ok(())) => {
                        closed = read.filled().is_empty();
                        (read.filled().len(), ())
                    }
                    Poll::Ready(Err(_)) => {
                        closed = true;
                        (0, ())
                    }
                    Poll::Pending => (0, ()),
                }
            });
            if closed {
                socket.close();
            }
        }
        if socket.can_recv() {
            let mut failed = false;
            let _ = socket.recv(
                |bytes| match Pin::new(&mut *bridge).poll_write(&mut cx, bytes) {
                    Poll::Ready(Ok(n)) => (n, ()),
                    Poll::Ready(Err(_)) => {
                        failed = true;
                        (0, ())
                    }
                    Poll::Pending => (0, ()),
                },
            );
            if failed {
                socket.abort();
            }
        }
        if !socket.may_recv()
            && !socket.can_recv()
            && !matches!(
                socket.state(),
                tcp::State::SynSent | tcp::State::SynReceived
            )
        {
            let _ = Pin::new(&mut *bridge).poll_shutdown(&mut cx);
        }
    }
}
