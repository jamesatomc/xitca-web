use std::{
    error,
    future::Future,
    io,
    net::{SocketAddr, TcpListener},
    pin::Pin,
    task::{Context, Poll},
};

use xitca_http::{
    body::ResponseBody,
    h1,
    http::{Request, Response},
    HttpServiceBuilder,
};
use xitca_io::net::TcpStream;
use xitca_server::{net::FromStream, Builder, ServerFuture, ServerHandle};
use xitca_service::ServiceFactory;

pub type Error = Box<dyn error::Error + Send + Sync>;

/// A general test server for any given service type that accept the connection from
/// xitca-server
pub fn test_server<F, T, Req>(factory: F) -> Result<TestServerHandle, Error>
where
    F: Fn() -> T + Send + Clone + 'static,
    T: ServiceFactory<Req, Config = ()>,
    Req: FromStream + Send + 'static,
{
    let lst = TcpListener::bind("127.0.0.1:0")?;

    let addr = lst.local_addr()?;

    let handle = Builder::new()
        .worker_threads(1)
        .server_threads(1)
        .disable_signal()
        .listen::<_, _, Req>("test_server", lst, factory)?
        .build();

    Ok(TestServerHandle { addr, handle })
}

/// A specialized http/1 server on top of [test_server]
pub fn test_h1_server<F, I>(factory: F) -> Result<TestServerHandle, Error>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: ServiceFactory<Request<h1::RequestBody>, Response = Response<ResponseBody>, Config = (), InitError = ()>
        + 'static,
{
    test_server::<_, _, TcpStream>(move || {
        let f = factory();
        HttpServiceBuilder::h1(f)
    })
}

pub struct TestServerHandle {
    addr: SocketAddr,
    handle: ServerFuture,
}

impl TestServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn ip_port_string(&self) -> String {
        format!("{}:{}", self.addr.ip(), self.addr.port())
    }

    pub fn try_handle(&mut self) -> io::Result<ServerHandle> {
        self.handle.handle()
    }
}

impl Future for TestServerHandle {
    type Output = <ServerFuture as Future>::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().handle).poll(cx)
    }
}