use std::{convert::Infallible, future::Future};

use http_encoding::{encoder, Coder, ContentEncoding};

use crate::{
    dev::service::{ready::ReadyService, BuildService, Service},
    request::WebRequest,
    response::WebResponse,
    stream::WebStream,
};

/// A compress middleware look into [WebRequest]'s `Accept-Encoding` header and
/// apply according compression to [WebResponse]'s body according to enabled compress feature.
/// `compress-x` feature must be enabled for this middleware to function correctly.
#[derive(Clone)]
pub struct Compress;

impl<S> BuildService<S> for Compress {
    type Service = CompressService<S>;
    type Error = Infallible;
    type Future = impl Future<Output = Result<Self::Service, Self::Error>>;

    fn build(&self, service: S) -> Self::Future {
        async { Ok(CompressService { service }) }
    }
}

pub struct CompressService<S> {
    service: S,
}

impl<'r, S, C, ReqB, ResB, Err> Service<WebRequest<'r, C, ReqB>> for CompressService<S>
where
    C: 'static,
    ReqB: 'static,
    S: for<'rs> Service<WebRequest<'rs, C, ReqB>, Response = WebResponse<ResB>, Error = Err>,
    ResB: WebStream,
{
    type Response = WebResponse<Coder<ResB>>;
    type Error = Err;
    type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> where Self: 'f;

    fn call(&self, req: WebRequest<'r, C, ReqB>) -> Self::Future<'_> {
        async move {
            let encoding = ContentEncoding::from_headers(req.req().headers());
            let res = self.service.call(req).await?;
            Ok(encoder(res, encoding))
        }
    }
}

impl<'r, S, C, ReqB, ResB, Err, Rdy> ReadyService<WebRequest<'r, C, ReqB>> for CompressService<S>
where
    C: 'static,
    ReqB: 'static,
    S: for<'rs> ReadyService<WebRequest<'rs, C, ReqB>, Response = WebResponse<ResB>, Error = Err, Ready = Rdy>,
    ResB: WebStream,
{
    type Ready = Rdy;
    type ReadyFuture<'f> = impl Future<Output = Self::Ready> where Self: 'f;

    #[inline]
    fn ready(&self) -> Self::ReadyFuture<'_> {
        async move { self.service.ready().await }
    }
}
