use std::future::Future;

use bytes::Bytes;
use futures_core::Stream;
use http::{Request, Response};
use xitca_server::net::AsyncReadWrite;
use xitca_service::ServiceFactory;

use crate::body::ResponseBody;
use crate::builder::HttpServiceBuilder;
use crate::error::{BodyError, HttpServiceError};

use super::body::RequestBody;
use super::service::H1Service;

/// Http/1 Builder type.
/// Take in generic types of ServiceFactory for http and tls.
pub type H1ServiceBuilder<
    F,
    FE,
    FU,
    FA,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
> = HttpServiceBuilder<F, RequestBody, FE, FU, FA, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>;

impl<F, FE, FU, FA, const HEADER_LIMIT: usize, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    HttpServiceBuilder<F, RequestBody, FE, FU, FA, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
{
    #[cfg(feature = "openssl")]
    pub fn openssl(
        self,
        acceptor: crate::tls::openssl::TlsAcceptor,
    ) -> H1ServiceBuilder<
        F,
        FE,
        FU,
        crate::tls::openssl::TlsAcceptorService,
        HEADER_LIMIT,
        READ_BUF_LIMIT,
        WRITE_BUF_LIMIT,
    > {
        H1ServiceBuilder {
            factory: self.factory,
            expect: self.expect,
            upgrade: self.upgrade,
            tls_factory: crate::tls::openssl::TlsAcceptorService::new(acceptor),
            config: self.config,
            _body: std::marker::PhantomData,
        }
    }

    #[cfg(feature = "rustls")]
    pub fn rustls(
        self,
        config: crate::tls::rustls::RustlsConfig,
    ) -> H1ServiceBuilder<
        F,
        FE,
        FU,
        crate::tls::rustls::TlsAcceptorService,
        HEADER_LIMIT,
        READ_BUF_LIMIT,
        WRITE_BUF_LIMIT,
    > {
        H1ServiceBuilder {
            factory: self.factory,
            expect: self.expect,
            upgrade: self.upgrade,
            tls_factory: crate::tls::rustls::TlsAcceptorService::new(config),
            config: self.config,
            _body: std::marker::PhantomData,
        }
    }

    #[cfg(feature = "native-tls")]
    pub fn native_tls(
        self,
        acceptor: crate::tls::native_tls::TlsAcceptor,
    ) -> H1ServiceBuilder<
        F,
        FE,
        FU,
        crate::tls::native_tls::TlsAcceptorService,
        HEADER_LIMIT,
        READ_BUF_LIMIT,
        WRITE_BUF_LIMIT,
    > {
        H1ServiceBuilder {
            factory: self.factory,
            expect: self.expect,
            upgrade: self.upgrade,
            tls_factory: crate::tls::native_tls::TlsAcceptorService::new(acceptor),
            config: self.config,
            _body: std::marker::PhantomData,
        }
    }
}

impl<
        St,
        F,
        ResB,
        E,
        FE,
        FU,
        FA,
        TlsSt,
        const HEADER_LIMIT: usize,
        const READ_BUF_LIMIT: usize,
        const WRITE_BUF_LIMIT: usize,
    > ServiceFactory<St> for H1ServiceBuilder<F, FE, FU, FA, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    F: ServiceFactory<Request<RequestBody>, Response = Response<ResponseBody<ResB>>>,
    F::Service: 'static,
    F::InitError: From<FE::InitError> + From<FU::InitError> + From<FA::InitError>,

    // TODO: use a meaningful config.
    FE: ServiceFactory<Request<RequestBody>, Response = Request<RequestBody>, Config = ()>,
    FE::Service: 'static,

    // TODO: use a meaningful config.
    FU: ServiceFactory<Request<RequestBody>, Response = (), Config = ()>,
    FU::Service: 'static,

    FA: ServiceFactory<St, Response = TlsSt, Config = ()>,
    FA::Service: 'static,

    HttpServiceError<F::Error>: From<FU::Error> + From<FA::Error>,
    F::Error: From<FE::Error>,

    ResB: Stream<Item = Result<Bytes, E>> + 'static,
    E: 'static,
    BodyError: From<E>,

    St: AsyncReadWrite,
    TlsSt: AsyncReadWrite,
{
    type Response = ();
    type Error = HttpServiceError<F::Error>;
    type Config = F::Config;
    type Service =
        H1Service<F::Service, FE::Service, FU::Service, FA::Service, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>;
    type InitError = F::InitError;
    type Future = impl Future<Output = Result<Self::Service, Self::InitError>>;

    fn new_service(&self, cfg: Self::Config) -> Self::Future {
        let expect = self.expect.new_service(());
        let upgrade = self.upgrade.as_ref().map(|upgrade| upgrade.new_service(()));
        let service = self.factory.new_service(cfg);
        let tls_acceptor = self.tls_factory.new_service(());
        let config = self.config;

        async move {
            let expect = expect.await?;
            let upgrade = match upgrade {
                Some(upgrade) => Some(upgrade.await?),
                None => None,
            };
            let service = service.await?;
            let tls_acceptor = tls_acceptor.await?;

            Ok(H1Service::new(config, service, expect, upgrade, tls_acceptor))
        }
    }
}