use std::{io, marker::PhantomData, pin::Pin, time::Duration};

use actix_server_alt::net::AsyncReadWrite;
use actix_service_alt::Service;
use bytes::{Buf, Bytes};
use futures_core::stream::Stream;
use http::{response::Parts, Request, Response};
use tokio::{io::Interest, pin, select};
use tracing::trace;

use crate::body::ResponseBody;
use crate::config::HttpServiceConfig;
use crate::error::BodyError;
use crate::flow::HttpFlowInner;
use crate::h1::{
    body::{RequestBody, RequestBodySender},
    error::Error,
};
use crate::response::{self, ResponseError};
use crate::util::{
    date::Date,
    futures::{never, poll_fn},
    keep_alive::KeepAlive,
};

use super::buf::{ReadBuf, WriteBuf};
use super::context::{ConnectionType, Context};
use super::decode::{RequestBodyItem, TransferDecoding};
use super::encode::TransferEncoding;
use super::error::{Parse, ProtoError};

/// Http/1 dispatcher
pub(crate) struct Dispatcher<
    'a,
    St,
    S,
    ReqB,
    X,
    U,
    const HEADER_LIMIT: usize,
    const READ_BUF_LIMIT: usize,
    const WRITE_BUF_LIMIT: usize,
> {
    io: Io<'a, St, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
    timer: Pin<&'a mut KeepAlive>,
    ka_dur: Duration,
    ctx: Context<'a, HEADER_LIMIT>,
    flow: &'a HttpFlowInner<S, X, U>,
    _phantom: PhantomData<ReqB>,
}

struct Io<'a, St, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> {
    io: &'a mut St,
    read_buf: ReadBuf<READ_BUF_LIMIT>,
    write_buf: WriteBuf<WRITE_BUF_LIMIT>,
}

impl<St, const READ_BUF_LIMIT: usize, const WRITE_BUF_LIMIT: usize> Io<'_, St, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    St: AsyncReadWrite,
{
    /// read until blocked/read backpressure and advance readbuf.
    fn try_read(&mut self) -> Result<(), Error> {
        let read_buf = &mut self.read_buf;
        read_buf.advance(false);

        loop {
            match self.io.try_read_buf(read_buf.buf_mut()) {
                Ok(0) => return Err(Error::Closed),
                Ok(_) => {
                    read_buf.advance(true);

                    if read_buf.backpressure() {
                        trace!(target: "h1_event", "Read buffer limit reached(Current length: {} bytes). Entering backpressure(No log event for recovery).", read_buf.len());
                        return Ok(());
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Return true when write is blocked and need wait.
    /// Return false when write is finished.(Did not blocked)
    fn try_write(&mut self) -> Result<bool, Error> {
        match self.write_buf {
            WriteBuf::List(ref mut list) => {
                let queue = list.list_mut();
                while queue.remaining() > 0 {
                    let mut iovs = [io::IoSlice::new(&[]); 64];
                    let len = queue.chunks_vectored(&mut iovs);
                    match self.io.try_write_vectored(&iovs[..len]) {
                        Ok(0) => return Err(Error::Closed),
                        Ok(n) => queue.advance(n),
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Ok(true);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            WriteBuf::Flat(ref mut buf) => {
                let mut written = 0;
                let len = buf.len();

                while written < len {
                    match self.io.try_write(&buf[written..]) {
                        Ok(0) => return Err(Error::Closed),
                        Ok(n) => written += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            buf.advance(written);
                            return Ok(true);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }

                buf.clear();
            }
        }

        Ok(false)
    }

    /// Block task and read.
    #[inline(always)]
    async fn read(&mut self) -> Result<(), Error> {
        let _ = self.io.ready(Interest::READABLE).await?;
        self.try_read()
    }

    /// drain write buffer and flush the io.
    async fn drain_write(&mut self) -> Result<(), Error> {
        while self.try_write()? {
            let _ = self.io.ready(Interest::WRITABLE).await?;
        }
        poll_fn(|cx| Pin::new(&mut *self.io).poll_flush(cx)).await?;
        Ok(())
    }

    /// A specialized readable check that always pending when read buffer is full.
    /// This is a hack for tokio::select macro.
    #[inline(always)]
    async fn readable(&self) -> Result<(), Error> {
        if self.read_buf.backpressure() {
            never().await
        } else {
            let _ = self.io.ready(Interest::READABLE).await?;
            Ok(())
        }
    }

    /// A specialized writable check that always pending when write buffer is empty.
    /// This is a hack for tokio::select macro.
    #[inline(always)]
    async fn writable(&self) -> Result<(), Error> {
        if self.write_buf.empty() {
            never().await
        } else {
            self.io.ready(Interest::WRITABLE).await?;
            Ok(())
        }
    }

    /// Return Ok when read is done or body handle is dropped by service call.
    async fn handle_request_body<const HEADER_LIMIT: usize>(
        &mut self,
        handle: &mut RequestBodyHandle,
        ctx: &mut Context<'_, HEADER_LIMIT>,
    ) -> Result<(), Error> {
        while poll_fn(|cx| handle.sender.poll_ready(cx)).await.is_ok() {
            self.readable().await?;
            self.try_read()?;

            let buf = &mut self.read_buf;

            if buf.advanced() {
                while let Some(item) = handle.decoder.decode(buf.buf_mut())? {
                    match item {
                        RequestBodyItem::Chunk(bytes) => handle.sender.feed_data(bytes),
                        RequestBodyItem::Eof => {
                            handle.sender.feed_eof();
                            return Ok(());
                        }
                    }
                }
            }
        }

        // When service call dropped payload there is no tell how many bytes
        // still remain readable in the connection.
        // close the connection would be a safe bet than draining it.
        ctx.set_force_close();

        Ok(())
    }

    /// A ready check version of `Io::handle_request_body`.
    /// This is to avoid borrow self as mut to co-op well with `tokio::select` macro.
    async fn request_body_ready<const HEADER_LIMIT: usize>(
        &self,
        body_handle: &mut Option<RequestBodyHandle>,
        ctx: &mut Context<'_, HEADER_LIMIT>,
    ) -> Result<RequestBodyHandle, Error> {
        if let Some(handle) = body_handle.as_mut() {
            match poll_fn(|cx| handle.sender.poll_ready(cx)).await {
                Ok(_) => {
                    self.readable().await?;
                    // TODO: This is an unwrap happen with every successful check. get rid of it.
                    return Ok(body_handle.take().unwrap());
                }
                Err(_) => {
                    // When service call dropped payload there is no tell how many bytes
                    // still remain readable in the connection.
                    // close the connection would be a safe bet than draining it.
                    ctx.set_force_close();
                    *body_handle = None;
                }
            }
        }

        // A future that would never resolve. This is a hack to work with tokio::select macro.
        never().await
    }
}

impl<
        'a,
        St,
        S,
        ReqB,
        ResB,
        E,
        X,
        U,
        const HEADER_LIMIT: usize,
        const READ_BUF_LIMIT: usize,
        const WRITE_BUF_LIMIT: usize,
    > Dispatcher<'a, St, S, ReqB, X, U, HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>
where
    S: Service<Request<ReqB>, Response = Response<ResponseBody<ResB>>> + 'static,
    S::Error: ResponseError<S::Response>,

    X: Service<Request<ReqB>, Response = Request<ReqB>> + 'static,
    X::Error: ResponseError<S::Response>,

    ReqB: From<RequestBody>,

    ResB: Stream<Item = Result<Bytes, E>>,
    BodyError: From<E>,

    St: AsyncReadWrite,
{
    pub(crate) fn new(
        io: &'a mut St,
        timer: Pin<&'a mut KeepAlive>,
        config: HttpServiceConfig<HEADER_LIMIT, READ_BUF_LIMIT, WRITE_BUF_LIMIT>,
        flow: &'a HttpFlowInner<S, X, U>,
        date: &'a Date,
    ) -> Self {
        let is_vectored = if config.http1_pipeline {
            false
        } else {
            io.is_write_vectored()
        };

        let io = Io {
            io,
            read_buf: ReadBuf::new(),
            write_buf: WriteBuf::new(is_vectored),
        };

        Self {
            io,
            timer,
            ka_dur: config.keep_alive_timeout,
            ctx: Context::new(date),
            flow,
            _phantom: PhantomData,
        }
    }

    pub(crate) async fn run(mut self) -> Result<(), Error> {
        loop {
            'req: while let Some(res) = self.decode_head() {
                match res {
                    Ok((req, mut body_handle)) => {
                        // have new request. update timer deadline.
                        let now = self.ctx.date.borrow().now() + self.ka_dur;
                        self.timer.as_mut().update(now);

                        let (parts, res_body) = self
                            .request_handler(req, &mut body_handle)
                            .await?
                            .unwrap_or_else(|ref mut e| ResponseError::response_error(e))
                            .into_parts();

                        self.encode_head(parts, &res_body)?;

                        let encoder = &mut res_body.encoder(self.ctx.ctype());

                        self.response_handler(res_body, encoder, body_handle).await?;
                    }
                    Err(ProtoError::Parse(Parse::HeaderTooLarge)) => {
                        // Header is too large to be parsed.
                        // Close the connection after sending error response as it's pointless
                        // to read the remaining bytes inside connection.
                        self.ctx.set_force_close();

                        let (parts, res_body) = response::header_too_large().into_parts();

                        self.encode_head(parts, &res_body)?;

                        break 'req;
                    }
                    // TODO: handle error that are meant to be a response.
                    Err(e) => return Err(e.into()),
                };
            }

            self.io.drain_write().await?;

            match self.ctx.ctype() {
                ConnectionType::Init => {
                    if self.ctx.is_force_close() {
                        trace!(target: "h1_event", "Connection error. Shutting down");
                        return Ok(());
                    } else {
                        // use timer to detect slow connection.
                        select! {
                            biased;
                            res = self.io.read() => res?,
                            _ = self.timer.as_mut() => {
                                trace!(target: "h1_event", "Slow Connection detected. Shutting down");
                                return Ok(())
                            }
                        }
                    }
                }
                ConnectionType::KeepAlive => {
                    if self.ctx.is_force_close() {
                        trace!(target: "h1_event", "Connection is keep-alive but meet a force close condition. Shutting down");
                        return Ok(());
                    } else {
                        select! {
                            biased;
                            res = self.io.read() => res?,
                            _ = self.timer.as_mut() => {
                                trace!(target: "h1_event", "Connection keep-alive timeout. Shutting down");
                                return Ok(());
                            }
                        }
                    }
                }
                ConnectionType::Upgrade | ConnectionType::Close => {
                    trace!(target: "h1_event", "Connection not keep-alive. Shutting down");
                    return Ok(());
                }
            }
        }
    }

    fn decode_head(&mut self) -> Option<Result<DecodedHead<ReqB>, ProtoError>> {
        // Do not try when nothing new read.
        if self.io.read_buf.advanced() {
            let buf = self.io.read_buf.buf_mut();

            match self.ctx.decode_head::<READ_BUF_LIMIT>(buf) {
                Ok(Some((req, decoder))) => {
                    let (body_handle, body) = RequestBodyHandle::new_pair(decoder);

                    let (parts, _) = req.into_parts();
                    let req = Request::from_parts(parts, body);

                    return Some(Ok((req, body_handle)));
                }
                Err(e) => return Some(Err(e)),
                _ => {}
            }
        }

        None
    }

    fn encode_head(&mut self, parts: Parts, body: &ResponseBody<ResB>) -> Result<(), Error> {
        let size = body.size();
        self.ctx.encode_head(parts, size, &mut self.io.write_buf)?;
        Ok(())
    }

    async fn request_handler(
        &mut self,
        mut req: Request<ReqB>,
        body_handle: &mut Option<RequestBodyHandle>,
    ) -> Result<Result<S::Response, S::Error>, Error> {
        if self.ctx.is_expect_header() {
            match self.flow.expect.call(req).await {
                Ok(expect_res) => {
                    // encode continue
                    self.ctx.encode_continue(&mut self.io.write_buf);

                    // use drain write to make sure continue is sent to client.
                    // the world can wait until it happens.
                    self.io.drain_write().await?;

                    req = expect_res;
                }
                Err(ref mut e) => return Ok(Ok(ResponseError::response_error(e))),
            }
        };

        let fut = self.flow.service.call(req);

        pin!(fut);

        if let Some(handle) = body_handle {
            select! {
                biased;
                res = fut.as_mut() => return Ok(res),
                res = self.io.handle_request_body(handle, &mut self.ctx) => {
                    // request body read is done or dropped. remove body_handle.
                    res?;
                    *body_handle = None;
                }
            }
        }

        let res = fut.await;

        Ok(res)
    }

    async fn response_handler(
        &mut self,
        body: ResponseBody<ResB>,
        encoder: &mut TransferEncoding,
        mut body_handle: Option<RequestBodyHandle>,
    ) -> Result<(), Error> {
        pin!(body);

        'res: loop {
            if self.io.write_buf.backpressure() {
                trace!(target: "h1_event", "Write buffer limit reached. Enter backpressure.");
                self.io.drain_write().await?;
                trace!(target: "h1_event", "Write buffer empty. Recover from backpressure.");
            } else {
                select! {
                    biased;
                    res = body.as_mut().next() => match res {
                        Some(bytes) => {
                            let bytes = bytes?;
                            encoder.encode(bytes, &mut self.io.write_buf)?;
                        },
                        None => {
                            encoder.encode_eof(&mut self.io.write_buf)?;
                            return Ok(())
                        }
                    },
                    res = self.io.writable() => {
                        res?;
                        let _ = self.io.try_write()?;
                        // TODO: add flush?
                    },
                    res = self.io.request_body_ready(&mut body_handle, &mut self.ctx) => {
                        let mut handle = res?;
                        self.io.try_read()?;

                        let buf = &mut self.io.read_buf;

                        if buf.advanced() {
                            while let Some(item) = handle.decoder.decode(buf.buf_mut())? {
                                match item {
                                    RequestBodyItem::Chunk(bytes) => handle.sender.feed_data(bytes),
                                    RequestBodyItem::Eof => {
                                        handle.sender.feed_eof();
                                        continue 'res;
                                    }
                                }
                            }
                        }

                        body_handle = Some(handle);
                    }
                }
            }
        }
    }
}

type DecodedHead<ReqB> = (Request<ReqB>, Option<RequestBodyHandle>);

struct RequestBodyHandle {
    decoder: TransferDecoding,
    sender: RequestBodySender,
}

impl RequestBodyHandle {
    fn new_pair<ReqB>(decoder: TransferDecoding) -> (Option<Self>, ReqB)
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
}
