use std::{net::ToSocketAddrs, pin::Pin, sync::Arc};

use futures::Future;

use crate::{
    client::Route,
    penetrate::SocksUdpForwardMock,
    udp::{Datagram, VirtualUdpSocket},
    Addr, Address, FusoExecutor, FusoStream, InnerAddr, InvalidAddr, Provider, Socket,
    SocketErr, SocketKind, ToBoxStream, WrappedProvider, SmolUdpSocket, NetSocket,
};

type BoxedFuture<T> = Pin<Box<dyn Future<Output = crate::Result<T>> + Send + 'static>>;

pub struct FusoPenetrateConnector {
    udp_server: Arc<Datagram<Arc<SmolUdpSocket>, FusoExecutor>>,
}

pub struct FusoUdpForwardProvider(Arc<Datagram<Arc<SmolUdpSocket>, FusoExecutor>>);

impl FusoPenetrateConnector {
    pub async fn new() -> crate::Result<Self> {
        Ok(Self {
            udp_server: Arc::new(Datagram::new(
                Arc::new(SmolUdpSocket::bind("0.0.0.0:0").await?),
                FusoExecutor,
            )?),
        })
    }
}

impl Provider<Socket> for FusoPenetrateConnector {
    type Output = BoxedFuture<Route<FusoStream>>;

    fn call(&self, socket: Socket) -> Self::Output {
        let udp_server = self.udp_server.clone();
        Box::pin(async move {
            match socket.kind() {
                SocketKind::Tcp => Ok(Route::Forward(
                    smol::net::TcpStream::connect(socket.as_string())
                        .await?
                        .into_boxed_stream(),
                )),
                SocketKind::Ufd => Ok(Route::Provider(WrappedProvider::wrap(SocksUdpForwardMock(
                    WrappedProvider::wrap(FusoUdpForwardProvider(udp_server)),
                )))),
                _ => Err(SocketErr::NotSupport(socket).into()),
            }
        })
    }
}

impl Provider<Addr> for FusoUdpForwardProvider {
    type Output = BoxedFuture<(std::net::SocketAddr, VirtualUdpSocket<Arc<SmolUdpSocket>>)>;
    fn call(&self, addr: Addr) -> Self::Output {
        let udp_server = self.0.clone();
        Box::pin(async move {
            log::debug!("try connect to udp {}", addr);

            let addr = addr
                .as_string()
                .to_socket_addrs()?
                .next()
                .ok_or(InvalidAddr::Domain(addr.as_string()))?;

            let udp = udp_server.connect(addr).await?;
            match udp.local_addr()? {
                Address::One(socket) => match socket.into_addr().into_inner() {
                    InnerAddr::Socket(addr) => return Ok((addr, udp)),
                    _ => {}
                },
                _ => {}
            }
            unsafe { std::hint::unreachable_unchecked() }
        })
    }
}
