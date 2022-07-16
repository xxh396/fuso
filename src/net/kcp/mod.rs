mod third_party;

pub use third_party::KcpErr;

mod stream;
pub use stream::*;

mod builder;
pub use builder::*;

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    future::Future,
    hash::{Hash, Hasher},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use async_mutex::Mutex;

use crate::{
    ext::AsyncWriteExt, guard::buffer::Buffer, select::Select, time, Accepter, AsyncRead,
    AsyncWrite, Kind, UdpReceiverExt, UdpSocket,
};

type BoxedFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

type Manager<C> = Arc<Mutex<HashMap<u64, HashMap<u32, Session<C>>>>>;
type Clean<T> = Arc<std::sync::Mutex<Vec<BoxedFuture<crate::Result<T>>>>>;
type Callback = Option<Box<dyn Fn() + Sync + Send + 'static>>;

pub fn now_mills() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32
}

pub enum State<C> {
    Open((u64, u32, Session<C>)),
    Close(Session<C>),
}

#[derive(Debug, Clone)]
pub struct Increment(Arc<Mutex<u32>>);

pub struct Session<C> {
    pub(crate) conv: u32,
    pub(crate) target: Option<SocketAddr>,
    pub(crate) kcore: KcpCore<C>,
}

pub struct KcpListener<C> {
    pub(crate) core: C,
    pub(crate) manager: Manager<C>,
    pub(crate) futures: Vec<BoxedFuture<crate::Result<State<C>>>>,
    pub(crate) close_callbacks: Clean<State<C>>,
}

pub struct KcpConnector<C> {
    core: C,
    increment: Increment,
    sessions: Arc<Mutex<HashMap<u32, Session<C>>>>,
    close_callbacks: Arc<std::sync::Mutex<Vec<BoxedFuture<()>>>>,
}

impl<C> Session<C>
where
    C: UdpSocket + Send + Unpin + 'static,
{
    pub async fn input(&mut self, data: Vec<u8>) -> crate::Result<()> {
        let mut kcp = self.kcore.kcp.lock().await;
        let mut next_input = &data[..];

        while next_input.len() > third_party::KCP_OVERHEAD {
            let n = kcp.1.input(next_input)?;
            next_input = &data[n..];
        }

        drop(kcp);

        self.kcore.try_wake_read();

        Ok(())
    }
}

impl<C> KcpStream<C>
where
    C: UdpSocket + Unpin + Send + Sync + 'static,
{
    fn kcp_update(mut kcore: KcpCore<C>) -> BoxedFuture<crate::Result<()>> {
        Box::pin(async move {
            loop {
                let next = kcore.kcp.as_ref().lock().await.1.check(now_mills());

                log::trace!("next update {}ms, ", next);

                time::sleep(Duration::from_millis(next as u64)).await;

                let (wake_read, wake_write, wake_close, is_dead) = {
                    let mut kcp = kcore.kcp.as_ref().lock().await;
                    let kcp = &mut kcp.1;
                    if let Err(e) = kcp.update(now_mills()).await {
                        log::warn!("update err {}", e);
                        break Err(e);
                    }

                    let wake_read = kcp.peeksize().is_ok();

                    let wake_write = kcp.wait_snd() > 0;

                    (
                        wake_read,
                        wake_write,
                        kcp.wait_snd() == 0,
                        kcp.is_dead_link(),
                    )
                };

                if wake_read {
                    kcore.try_wake_read();
                }

                if wake_write {
                    kcore.try_wake_write();
                }

                if wake_close {
                    kcore.try_wake_close();
                }

                if is_dead {
                    kcore.try_wake_close();
                    break Err(KcpErr::IoError(std::io::ErrorKind::TimedOut.into()).into());
                }
            }
        })
    }

    pub fn new_server(conv: u32, core: KcpCore<C>, close_callback: Callback) -> Self {
        Self {
            conv,
            kcore: core.clone(),
            lazy_kcp_update: std::sync::Mutex::new(Some(Self::kcp_update(core))),
            close_callback,
        }
    }

    pub fn new_client(
        conv: u32,
        kcp_guard: BoxedFuture<crate::Result<()>>,
        core: KcpCore<C>,
        close_callback: Callback,
    ) -> Self {
        let kcp_update = Select::select(kcp_guard, Self::kcp_update(core.clone()));
        Self {
            conv,
            kcore: core,
            close_callback,
            lazy_kcp_update: std::sync::Mutex::new(Some(Box::pin(kcp_update))),
        }
    }
}

impl<C> KcpCore<C>
where
    C: UdpSocket + Send + Unpin + 'static,
{
    fn try_wake_read(&mut self) {
        if let Some(waker) = self.read_waker.take() {
            waker.wake();
        }
    }

    fn try_wake_write(&mut self) {
        if let Some(waker) = self.write_waker.take() {
            waker.wake()
        }
    }

    fn try_wake_close(&mut self) {
        if let Some(waker) = self.close_waker.take() {
            waker.wake();
        }
    }

    async fn close(&self) -> crate::Result<()> {
        self.kcp.lock().await.1.close()
    }
}

impl<C> Session<C>
where
    C: UdpSocket + Unpin + Send + Sync + 'static,
{
    pub fn new(conv: u32, target: Option<SocketAddr>, output: C) -> Self {
        let output = KOutput {
            output,
            target: target.clone(),
        };

        let mut kcp = third_party::Kcp::new(conv, output);

        kcp.set_wndsize(512, 521);
        kcp.set_nodelay(true, 20, 2, true);
        kcp.set_maximum_resend_times(10);

        let kcore = KcpCore {
            kcp: { Arc::new(Mutex::new((Buffer::new(), kcp))) },
            write_waker: None,
            read_waker: None,
            close_waker: None,
        };

        Self {
            conv,
            target,
            kcore,
        }
    }

    pub async fn close(&self) -> crate::Result<()> {
        self.kcore.close().await
    }

    pub fn new_client(
        &self,
        fut: BoxedFuture<crate::Result<()>>,
        close_callback: Callback,
    ) -> KcpStream<C> {
        KcpStream::new_client(self.conv, fut, self.kcore.clone(), close_callback)
    }

    pub fn new_server(&self, close_callback: Callback) -> KcpStream<C> {
        KcpStream::new_server(self.conv, self.kcore.clone(), close_callback)
    }
}

impl<C> KcpListener<C>
where
    C: UdpSocket + Send + Sync + Clone + Unpin + 'static,
{
    pub fn bind(core: C) -> crate::Result<Self> {
        let manager: Manager<C> = Default::default();

        let core_fut = Box::pin(Self::run_core(core.clone(), manager.clone()));

        Ok(Self {
            core,
            manager,
            futures: vec![core_fut],
            close_callbacks: Default::default(),
        })
    }

    async fn run_core(core: C, manager: Manager<C>) -> crate::Result<State<C>> {
        loop {
            let mut data = Vec::with_capacity(1500);

            unsafe {
                data.set_len(1500);
            }

            let (n, addr) = core.recv_from(&mut data).await?;
            data.truncate(n);

            let id = {
                let mut hasher = DefaultHasher::new();
                addr.hash(&mut hasher);
                hasher.finish()
            };

            if n < third_party::KCP_OVERHEAD {
                log::warn!("received an invalid kcp packet");
                continue;
            }

            let conv = third_party::get_conv(&data);

            let mut manager = manager.lock().await;

            let manager = manager.entry(id).or_default();

            let new_kcp = !manager.contains_key(&conv);

            let session = manager
                .entry(conv)
                .or_insert_with(|| Session::new(conv, Some(addr), core.clone()));

            if let Err(e) = session.input(data).await {
                log::warn!("{}", e);
                manager.remove(&conv);
            } else if new_kcp {
                break Ok(State::Open((id, conv, session.clone())));
            }
        }
    }

    fn make_close_fn(&self, id: u64, conv: u32) -> Callback {
        let manager = self.manager.clone();
        let kcp_close = self.close_callbacks.clone();
        let close_callback = {
            let manager = manager.clone();
            move || match kcp_close.lock() {
                Ok(mut kcp) => {
                    let manager = manager.clone();
                    let close_fut = async move {
                        if let Some(sessions) = manager.lock().await.get_mut(&id) {
                            match sessions.remove(&conv) {
                                Some(session) => {
                                    let _ = session.close().await;
                                    Ok(State::Close(session))
                                }
                                None => Err(KcpErr::Lock.into()),
                            }
                        } else {
                            Err(KcpErr::Lock.into())
                        }
                    };
                    kcp.push(Box::pin(close_fut));
                }
                Err(e) => {
                    log::warn!("{}", e);
                }
            }
        };

        Some(Box::new(close_callback))
    }
}

impl<C> Accepter for KcpListener<C>
where
    C: UdpSocket + Send + Clone + Unpin + Sync + 'static,
{
    type Stream = KcpStream<C>;

    fn poll_accept(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<crate::Result<Self::Stream>> {
        let futures = std::mem::replace(&mut self.futures, Default::default());

        let mut futures = futures.into_iter().chain(std::mem::replace(
            &mut *self.close_callbacks.lock()?,
            Default::default(),
        ));

        while let Some(mut future) = futures.next() {
            match Pin::new(&mut future).poll(cx) {
                Poll::Ready(Err(e)) => {
                    log::warn!("{}", e)
                }
                Poll::Ready(Ok(State::Close(kcp))) => {
                    log::debug!("kcp closed conv={}", kcp.conv);
                    drop(kcp)
                }
                Poll::Ready(Ok(State::Open((id, conv, kcp)))) => {
                    let accept_fut = Self::run_core(self.core.clone(), self.manager.clone());

                    self.futures.push(Box::pin(accept_fut));

                    self.futures.extend(futures);

                    let close_fn = self.make_close_fn(id, conv);

                    return Poll::Ready(Ok(kcp.new_server(close_fn)));
                }
                Poll::Pending => {
                    self.futures.push(future);
                }
            }
        }

        Poll::Pending
    }
}

impl<C> AsyncWrite for KcpCore<C>
where
    C: UdpSocket + Unpin + 'static,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<crate::Result<usize>> {
        match self.kcp.try_lock_arc() {
            Some(mut kcp) => Poll::Ready(kcp.1.send(buf)),
            None => {
                let waker = cx.waker().clone();
                drop(std::mem::replace(&mut self.write_waker, Some(waker)));
                Poll::Pending
            }
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<crate::Result<()>> {
        match self.kcp.try_lock_arc() {
            Some(kcp) => {
                if kcp.1.wait_snd() > 0 {
                    let waker = cx.waker().clone();
                    drop(std::mem::replace(&mut self.close_waker, Some(waker)));
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            None => {
                let waker = cx.waker().clone();
                drop(std::mem::replace(&mut self.close_waker, Some(waker)));
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<crate::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl<C> AsyncRead for KcpCore<C>
where
    C: UdpSocket + Unpin + 'static,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut crate::ReadBuf<'_>,
    ) -> Poll<crate::Result<usize>> {
        match self.kcp.try_lock_arc() {
            None => {
                let waker = cx.waker().clone();
                drop(std::mem::replace(&mut self.read_waker, Some(waker)));
                Poll::Pending
            }
            Some(mut kcp) => {
                let unfilled = buf.initialize_unfilled();

                if !kcp.0.is_empty() {
                    let n = kcp.0.read_to_buffer(unfilled);
                    return Poll::Ready(Ok(n));
                }

                match kcp.1.recv(unfilled) {
                    Ok(n) => {
                        buf.advance(n);
                        return Poll::Ready(Ok(n));
                    }
                    Err(e) => match e.kind() {
                        Kind::Kcp(KcpErr::RecvQueueEmpty | KcpErr::ExpectingFragment) => {
                            let waker = cx.waker().clone();
                            drop(std::mem::replace(&mut self.read_waker, Some(waker)));
                            return Poll::Pending;
                        }
                        Kind::Kcp(KcpErr::UserBufTooSmall) => unsafe {
                            let size = kcp.1.peeksize().unwrap_unchecked();
                            let mut data = Vec::with_capacity(size);

                            data.set_len(size);

                            let n = kcp.1.recv(&mut data)?;
                            data.truncate(n);
                            let len = unfilled.len();

                            std::ptr::copy(data.as_ptr(), unfilled.as_mut_ptr(), len);

                            buf.advance(len);

                            kcp.0.push_back(&data[len..]);

                            return Poll::Ready(Ok(len));
                        },
                        _ => Poll::Ready(Err(e)),
                    },
                }
            }
        }
    }
}

impl<C> Clone for KcpCore<C> {
    fn clone(&self) -> Self {
        KcpCore {
            kcp: self.kcp.clone(),
            write_waker: self.write_waker.clone(),
            read_waker: self.read_waker.clone(),
            close_waker: self.close_waker.clone(),
        }
    }
}

impl<C> Clone for Session<C> {
    fn clone(&self) -> Self {
        Self {
            conv: self.conv,
            kcore: self.kcore.clone(),
            target: self.target.clone(),
        }
    }
}

impl Default for Increment {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(1)))
    }
}

impl<C> KcpConnector<C>
where
    C: UdpSocket + Unpin + Clone + Send + Sync + 'static,
{
    pub fn new(core: C) -> Self {
        Self {
            core: core,
            increment: Default::default(),
            sessions: Default::default(),
            close_callbacks: Default::default(),
        }
    }

    fn safe_connect(&self, _: u32) -> BoxedFuture<crate::Result<()>> {
        let core = self.core.clone();
        let sessions = self.sessions.clone();
        let clean_futures = self.close_callbacks.clone();

        let fut = async move {
            loop {
                let mut buf = Vec::with_capacity(1500);

                unsafe {
                    buf.set_len(1500);
                }

                let n = core.recv(&mut buf).await?;
                buf.truncate(n);

                if n < third_party::KCP_OVERHEAD {
                    log::warn!("bad kcp packet");
                    continue;
                }

                let conv = third_party::get_conv(&buf);

                let mut sessions = sessions.lock().await;
                if let Some(session) = sessions.get_mut(&conv) {
                    if let Err(e) = session.input(buf).await {
                        log::warn!("package error {}", e);
                        drop(session);
                        let mut session = unsafe { sessions.remove(&conv).unwrap_unchecked() };
                        let _ = session.kcore.flush().await;
                        let _ = session.kcore.close().await;
                    }
                }

                drop(sessions);

                let mut futures = match clean_futures.lock() {
                    Err(e) => return Err(e.into()),
                    Ok(mut futures) => std::mem::replace(&mut *futures, Default::default()),
                };

                tokio::spawn(async move {
                    while let Some(futures) = futures.pop() {
                        futures.await;
                    }
                });
            }
        };

        Box::pin(fut)
    }

    pub async fn connect(&self) -> crate::Result<KcpStream<C>> {
        let conv = {
            let sessions = self.sessions.lock().await;
            let conv = self.increment.current().await;
            let mut next_conv = conv;
            loop {
                log::trace!("current conv={}", next_conv);

                if !sessions.contains_key(&next_conv) {
                    break next_conv;
                }

                next_conv = self.increment.next().await;

                if next_conv == conv {
                    return Err(KcpErr::NoMoreConv.into());
                }
            }
        };

        let (close_callbacks, sessions) = (self.close_callbacks.clone(), self.sessions.clone());

        let session = Session::new(conv, None, self.core.clone());

        self.sessions.lock().await.insert(conv, session.clone());

        let fut = self.safe_connect(conv);

        let close_callback = move || match close_callbacks.lock() {
            Ok(mut close) => {
                let sessions = sessions.clone();
                let close_fut = async move {
                    if let Some(session) = sessions.lock().await.remove(&conv) {
                        drop(session.close().await);
                    }
                };

                close.push(Box::pin(close_fut));
            }
            Err(e) => {
                log::warn!("{}", e)
            }
        };

        Ok(session.new_client(Box::pin(fut), Some(Box::new(close_callback))))
    }
}

impl Increment {
    pub async fn current(&self) -> u32 {
        *self.0.lock().await
    }

    pub async fn next(&self) -> u32 {
        let mut inc = self.0.lock().await;
        let (_, overflow) = inc.overflowing_add(1);
        *inc = if overflow { 0 } else { *inc + 1 };
        *inc
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        ext::{AsyncReadExt, AsyncWriteExt},
        AccepterExt,
    };

    use super::{KcpConnector, KcpListener};

    fn init_logger() {
        env_logger::builder()
            .filter_module("fuso", log::LevelFilter::Debug)
            .init();
    }

    #[test]
    pub fn test_kcp_server() {
        init_logger();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let udp = tokio::net::UdpSocket::bind("0.0.0.0:7777").await.unwrap();
                let udp = Arc::new(udp);

                let mut kcp = KcpListener::bind(udp).unwrap();

                loop {
                    match kcp.accept().await {
                        Ok(mut kcp) => {
                            tokio::spawn(async move {
                                loop {
                                    let mut buf = Vec::new();
                                    buf.resize(1500, 0);
                                    let n = kcp.read(&mut buf).await.unwrap();
                                    buf.truncate(n);
                                    let _ = kcp.write_all(b"hello world").await;
                                    log::debug!("{:?}", String::from_utf8_lossy(&buf));
                                }
                            });
                        }
                        Err(_) => {}
                    }
                }
            });
    }

    #[test]
    pub fn test_kcp_client() {
        init_logger();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let udp = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
                udp.connect("127.0.0.1:7777").await.unwrap();

                let kcp = KcpConnector::new(Arc::new(udp));

                let mut kcp = kcp.connect().await.unwrap();

                loop {
                    let mut buf = String::new();
                    std::io::stdin().read_line(&mut buf).unwrap();
                    let _ = kcp.write_all(buf.as_bytes()).await;
                    let mut buf = Vec::new();
                    buf.resize(1500, 0);
                    let n = kcp.read(&mut buf).await.unwrap();
                    buf.truncate(n);
                    log::debug!("{:?}", buf)
                }
            });
    }
}
