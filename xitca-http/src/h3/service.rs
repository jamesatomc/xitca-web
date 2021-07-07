use std::{
    fmt,
    future::Future,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Stream;
use http::{Request, Response};
use xitca_server::net::UdpStream;
use xitca_service::Service;

use super::proto::Dispatcher;
use crate::body::ResponseBody;
use crate::error::{BodyError, HttpServiceError};
use crate::flow::HttpFlow;

use super::body::RequestBody;

pub struct H3Service<S> {
    flow: HttpFlow<S, (), ()>,
}

impl<S> H3Service<S> {
    /// Construct new Http3Service.
    /// No upgrade/expect services allowed in Http/3.
    pub fn new(service: S) -> Self {
        Self {
            flow: HttpFlow::new(service, (), None),
        }
    }
}

impl<S, B, E> Service<UdpStream> for H3Service<S>
where
    S: Service<Request<RequestBody>, Response = Response<ResponseBody<B>>> + 'static,
    S::Error: fmt::Debug,

    B: Stream<Item = Result<Bytes, E>> + 'static,
    E: 'static,
    BodyError: From<E>,
{
    type Response = ();
    type Error = HttpServiceError<S::Error>;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.flow
            .service
            .poll_ready(cx)
            .map_err(|_| HttpServiceError::ServiceReady)
    }

    fn call(&self, stream: UdpStream) -> Self::Future<'_> {
        async move {
            let dispatcher = Dispatcher::new(stream, &self.flow);

            dispatcher.run().await?;

            Ok(())
        }
    }
}