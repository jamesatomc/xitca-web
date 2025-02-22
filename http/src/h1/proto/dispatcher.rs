use std::{
    future::{pending, poll_fn, Future},
    io,
    marker::PhantomData,
    ops::DerefMut,
    pin::Pin,
    time::Duration,
};

use futures_core::stream::Stream;
use tracing::trace;
use xitca_io::io::{AsyncIo, Interest, Ready};
use xitca_service::Service;
use xitca_unsafe_collection::{
    bytes::read_buf,
    futures::{Select as _, SelectOutput},
    pin,
};

use crate::{
    body::{BodySize, Once},
    bytes::Bytes,
    config::HttpServiceConfig,
    date::DateTime,
    h1::{
        body::{RequestBody, RequestBodySender},
        error::Error,
    },
    http::{response::Parts, Response},
    request::Request,
    response,
    util::{futures::Timeout, hint::unlikely, keep_alive::KeepAlive},
};

use super::{
    buf::{BufInterest, BufWrite, FlatBuf, ListBuf},
    codec::TransferCoding,
    context::{ConnectionType, Context},
    error::{Parse, ProtoError},
};

/// function to generic over different writer buffer types dispatcher.
pub(crate) async fn run<
    'a,
    St,
    S,
    ReqB,
    ResB,
    BE,
    D,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
>(
    io: &'a mut St,
    timer: Pin<&'a mut KeepAlive>,
    config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
    service: &'a S,
    date: &'a D,
) -> Result<(), Error<S::Error, BE>>
where
    S: Service<Request<ReqB>, Response = Response<ResB>>,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    St: AsyncIo,
    D: DateTime,
{
    let is_vectored = config.vectored_write && io.is_vectored_write();

    let res = if is_vectored {
        let write_buf = ListBuf::<_, WRITE_BUF_LIMIT>::default();
        Dispatcher::new(io, timer, config, service, date, write_buf).run().await
    } else {
        let write_buf = FlatBuf::<WRITE_BUF_LIMIT>::default();
        Dispatcher::new(io, timer, config, service, date, write_buf).run().await
    };

    match res {
        Ok(_) | Err(Error::Closed) => Ok(()),
        Err(Error::KeepAliveExpire) => {
            trace!(target: "h1_dispatcher", "Connection keep-alive expired. Shutting down");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Http/1 dispatcher
struct Dispatcher<
    'a,
    St,
    S,
    ReqB,
    W,
    D,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
> {
    io: BufferedIo<'a, St, W, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
    timer: Pin<&'a mut KeepAlive>,
    ka_dur: Duration,
    ctx: Context<'a, D, HEADER_LIMIT>,
    service: &'a S,
    _phantom: PhantomData<ReqB>,
}

struct BufferedIo<'a, St, W, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> {
    io: &'a mut St,
    read_buf: FlatBuf<READ_BUF_LIMIT>,
    write_buf: W,
}

impl<'a, St, W, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize>
    BufferedIo<'a, St, W, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    St: AsyncIo,
    W: BufWrite,
{
    fn new(io: &'a mut St, write_buf: W) -> Self {
        Self {
            io,
            read_buf: FlatBuf::new(),
            write_buf,
        }
    }

    // read until blocked/read backpressure and advance read_buf.
    fn try_read(&mut self) -> io::Result<()> {
        loop {
            match read_buf(self.io, &mut *self.read_buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {
                    if self.read_buf.backpressure() {
                        trace!(target: "h1_dispatcher", "Read buffer limit reached(Current length: {} bytes). Entering backpressure(No log event for recovery).", self.read_buf.len());
                        return Ok(());
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn try_write(&mut self) -> io::Result<()> {
        self.write_buf.flush(self.io)
    }

    async fn read(&mut self) -> io::Result<()> {
        self.io.ready(Interest::READABLE).await?;
        self.try_read()
    }

    async fn write(&mut self) -> io::Result<()> {
        self.io.ready(Interest::WRITABLE).await?;
        self.try_write()
    }

    // drain write buffer and flush the io.
    async fn drain_write(&mut self) -> io::Result<()> {
        while self.write_buf.want_write() {
            self.write().await?;
        }
        Ok(())
    }

    // A specialized write check that always pending when write buffer is empty.
    // This is a hack for `Select`.
    async fn write_or_pending(&mut self) -> io::Result<()> {
        if !self.write_buf.want_write() {
            pending().await
        } else {
            self.write().await
        }
    }

    // Check readable and writable state of IO and ready state of request payload sender.
    // Remove readable state if request payload is not ready(Read ahead backpressure).
    async fn ready<D, const HEADER_LIMIT: usize>(
        &mut self,
        handle: &mut RequestBodyHandle,
        ctx: &mut Context<'_, D, HEADER_LIMIT>,
    ) -> io::Result<Ready> {
        if !self.write_buf.want_write() {
            handle.ready(ctx).await?;
            self.io.ready(Interest::READABLE).await
        } else {
            // for readable event the RequestBodyHandle must always be checked before polling
            // it's Interest.
            match handle.ready(ctx).select(self.io.ready(Interest::WRITABLE)).await {
                SelectOutput::A(res) => {
                    res?;
                    self.io.ready(Interest::READABLE | Interest::WRITABLE).await
                }
                SelectOutput::B(res) => res,
            }
        }
    }

    #[cold]
    #[inline(never)]
    async fn shutdown(&mut self) -> io::Result<()> {
        poll_fn(|cx| Pin::new(&mut *self.io).poll_shutdown(cx)).await
    }
}

impl<
        'a,
        St,
        S,
        ReqB,
        ResB,
        BE,
        W,
        D,
        const HEADER_LIMIT: usize,
        const READ_BUF_LIMIT: usize,
        const WRITE_BUF_LIMIT: usize,
    > Dispatcher<'a, St, S, ReqB, W, D, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Service<Request<ReqB>, Response = Response<ResB>>,
    ReqB: From<RequestBody>,
    ResB: Stream<Item = Result<Bytes, BE>>,
    St: AsyncIo,
    W: BufWrite,
    D: DateTime,
{
    fn new(
        io: &'a mut St,
        timer: Pin<&'a mut KeepAlive>,
        config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
        service: &'a S,
        date: &'a D,
        write_buf: W,
    ) -> Self {
        Self {
            io: BufferedIo::new(io, write_buf),
            timer,
            ka_dur: config.keep_alive_timeout,
            ctx: Context::new(date),
            service,
            _phantom: PhantomData,
        }
    }

    async fn run(mut self) -> Result<(), Error<S::Error, BE>> {
        loop {
            match self.ctx.ctype() {
                ConnectionType::Init => {}
                ConnectionType::KeepAlive => self.update_timer(),
                ConnectionType::Close => {
                    unlikely();
                    return self.io.shutdown().await.map_err(Into::into);
                }
            }

            self.io.read().timeout(self.timer.as_mut()).await??;

            'req: while let Some(res) = self.decode_head() {
                match res {
                    Ok((req, mut body_handle)) => {
                        let (parts, res_body) = self.request_handler(req, &mut body_handle).await?.into_parts();

                        let size = BodySize::from_stream(&res_body);
                        let encoder = &mut self.encode_head(parts, size)?;

                        self.response_handler(res_body, encoder, body_handle).await?;

                        if self.ctx.is_connection_closed() {
                            break 'req;
                        }
                    }
                    Err(ProtoError::Parse(Parse::HeaderTooLarge)) => {
                        self.request_error(response::header_too_large)?;
                        break 'req;
                    }
                    Err(ProtoError::Parse(_)) => {
                        self.request_error(response::bad_request)?;
                        break 'req;
                    }
                    // TODO: handle error that are meant to be a response.
                    Err(e) => return Err(e.into()),
                };
            }

            // TODO: add timeout for drain write?
            self.io.drain_write().await?;
        }
    }

    // update timer deadline according to keep alive duration.
    fn update_timer(&mut self) {
        let now = self.ctx.date.now() + self.ka_dur;
        self.timer.as_mut().update(now);
    }

    fn decode_head(&mut self) -> Option<Result<DecodedHead<ReqB>, ProtoError>> {
        match self.ctx.decode_head::<READ_BUF_LIMIT>(self.io.read_buf.deref_mut()) {
            Ok(Some((req, decoder))) => {
                let (body_handle, body) = RequestBodyHandle::new_pair(decoder);

                let req = req.map_body(move |_| body);

                Some(Ok((req, body_handle)))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }

    fn encode_head(&mut self, parts: Parts, size: BodySize) -> Result<TransferCoding, Error<S::Error, BE>> {
        self.ctx
            .encode_head(parts, size, &mut self.io.write_buf)
            .map_err(Into::into)
    }

    async fn request_handler(
        &mut self,
        req: Request<ReqB>,
        body_handle: &mut Option<RequestBodyHandle>,
    ) -> Result<S::Response, Error<S::Error, BE>> {
        match self
            .service
            .call(req)
            .select(self.request_body_handler(body_handle))
            .await
        {
            SelectOutput::A(res) => res.map_err(Error::Service),
            SelectOutput::B(res) => res,
        }
    }

    async fn request_body_handler(
        &mut self,
        body_handle: &mut Option<RequestBodyHandle>,
    ) -> Result<S::Response, Error<S::Error, BE>> {
        // decode request body and read more if needed.
        if let Some(handle) = body_handle {
            self._request_body_handler(handle).await?;
        }

        *body_handle = None;

        // pending and do nothing when RequestBodyHandle is gone.
        pending().await
    }

    async fn _request_body_handler(&mut self, handle: &mut RequestBodyHandle) -> io::Result<()> {
        if self.ctx.is_expect_header() {
            // Wait for body polled for expect header request.
            match handle.wait_for_poll().await {
                Ok(_) => {
                    // encode continue
                    self.ctx.encode_continue(&mut self.io.write_buf);
                    // use drain write to make sure continue is sent to client.
                    self.io.drain_write().await?;
                }
                // If body is already dropped then ignore sending 100 continue.
                Err(_) => return Ok(()),
            }
        }

        // Check the readiness of RequestBodyHandle
        // so read ahead does not buffer too much data.
        while handle.ready(&mut self.ctx).await.is_ok() {
            match handle.decode(&mut self.io.read_buf)? {
                DecodeState::Continue => self.io.read().await?,
                DecodeState::Eof => break,
            }
        }

        Ok(())
    }

    async fn response_handler(
        &mut self,
        body: ResB,
        encoder: &mut TransferCoding,
        mut body_handle: Option<RequestBodyHandle>,
    ) -> Result<(), Error<S::Error, BE>> {
        pin!(body);
        loop {
            match self
                .try_poll_body(body.as_mut())
                .select(self.response_body_handler(&mut body_handle))
                .await
            {
                SelectOutput::A(Some(res)) => {
                    let bytes = res.map_err(Error::Body)?;
                    encoder.encode(bytes, &mut self.io.write_buf);
                }
                SelectOutput::A(None) => {
                    if let Some(handle) = body_handle.take() {
                        // Request body is partial consumed.
                        // Close connection in case there are bytes remain in socket.
                        // Treat upgrade decoder as special case because it does not have
                        // an eof state.
                        if !handle.sender.is_eof() && !handle.decoder.is_upgrade() {
                            self.ctx.set_ctype(ConnectionType::Close);
                        }
                    }

                    encoder.encode_eof(&mut self.io.write_buf);
                    return Ok(());
                }
                SelectOutput::B(res) => res?,
            }
        }
    }

    fn try_poll_body<'b>(&self, mut body: Pin<&'b mut ResB>) -> impl Future<Output = Option<Result<Bytes, BE>>> + 'b {
        let write_backpressure = self.io.write_buf.backpressure();
        async move {
            if write_backpressure {
                pending().await
            } else {
                poll_fn(|cx| body.as_mut().poll_next(cx)).await
            }
        }
    }

    async fn response_body_handler(
        &mut self,
        body_handle: &mut Option<RequestBodyHandle>,
    ) -> Result<(), Error<S::Error, BE>> {
        match body_handle.as_mut() {
            Some(handle) => match handle.decode(&mut self.io.read_buf)? {
                DecodeState::Continue => {
                    debug_assert!(!self.io.read_buf.backpressure(), "Read buffer overflown. Please report");
                    match self.io.ready(handle, &mut self.ctx).await {
                        Ok(ready) => {
                            if ready.is_readable() {
                                self.io.try_read()?
                            }
                            if ready.is_writable() {
                                self.io.try_write()?;
                            }
                        }
                        // TODO: potential special handling error case of RequestBodySender::poll_ready ?
                        // This output would mix IO error and RequestBodySender::poll_ready error.
                        // For now the effect of not handling them differently is not clear.
                        Err(e) => {
                            handle.sender.feed_error(e.into());
                            *body_handle = None;
                        }
                    }
                }
                DecodeState::Eof => *body_handle = None,
            },
            None => self.io.write_or_pending().await?,
        }

        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn request_error<F>(&mut self, func: F) -> Result<(), Error<S::Error, BE>>
    where
        F: FnOnce() -> Response<Once<Bytes>>,
    {
        self.ctx.set_ctype(ConnectionType::Close);

        let (parts, body) = func().into_parts();

        let size = BodySize::from_stream(&body);

        self.encode_head(parts, size).map(|_| ())
    }
}

type DecodedHead<ReqB> = (Request<ReqB>, Option<RequestBodyHandle>);

struct RequestBodyHandle {
    decoder: TransferCoding,
    sender: RequestBodySender,
}

enum DecodeState {
    // TransferDecoding can continue for more data.
    Continue,
    // TransferDecoding is ended with eof.
    Eof,
}

impl RequestBodyHandle {
    fn new_pair<ReqB>(decoder: TransferCoding) -> (Option<Self>, ReqB)
    where
        ReqB: From<RequestBody>,
    {
        if decoder.is_eof() {
            let body = RequestBody::empty();
            (None, body.into())
        } else {
            let (sender, body) = RequestBody::create(false);
            let body_handle = RequestBodyHandle { decoder, sender };
            (Some(body_handle), body.into())
        }
    }

    fn decode<const READ_BUF_LIMIT: usize>(
        &mut self,
        read_buf: &mut FlatBuf<READ_BUF_LIMIT>,
    ) -> io::Result<DecodeState> {
        while let Some(bytes) = self.decoder.decode(&mut *read_buf)? {
            if bytes.is_empty() {
                self.sender.feed_eof();
                return Ok(DecodeState::Eof);
            }
            self.sender.feed_data(bytes);
        }

        Ok(DecodeState::Continue)
    }

    async fn ready<D, const HEADER_LIMIT: usize>(&mut self, ctx: &mut Context<'_, D, HEADER_LIMIT>) -> io::Result<()> {
        self.sender.ready().await.map_err(|e| {
            // RequestBodySender's only error case is when service future drop the request
            // body half way. In this case notify Context to close connection afterwards.
            //
            // Service future is trusted to produce a meaningful response after it drops
            // the request body.
            ctx.set_ctype(ConnectionType::Close);
            e
        })
    }

    async fn wait_for_poll(&mut self) -> io::Result<()> {
        // The error case is the same condition as Self::ready method.
        //
        // Remote should not have sent any body until 100 continue is received
        // which means it's safe to keep the connection open(possibly) on error path.
        self.sender.wait_for_poll().await
    }
}
