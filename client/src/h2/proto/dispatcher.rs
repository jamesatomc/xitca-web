use std::{cmp, future::poll_fn};

use ::h2::{client, Reason};
use futures_core::stream::Stream;
use xitca_http::{
    date::DateTime,
    http::{
        self,
        const_header_name::PROTOCOL,
        header::{HeaderValue, CONNECTION, CONTENT_LENGTH, DATE, TRANSFER_ENCODING, UPGRADE},
        method::Method,
        version::Version,
    },
};
use xitca_io::io::{AsyncRead, AsyncWrite};

use crate::{
    body::{BodyError, BodySize, ResponseBody},
    bytes::Bytes,
    date::DateTimeHandle,
    h2::{body::ResponseBody as H2ResponseBody, Connection, Error},
};

pub(crate) async fn send<B, E>(
    stream: &mut Connection,
    date: DateTimeHandle<'_>,
    mut req: http::Request<B>,
) -> Result<http::Response<ResponseBody<'static>>, Error>
where
    B: Stream<Item = Result<Bytes, E>>,
    BodyError: From<E>,
{
    *req.version_mut() = Version::HTTP_2;

    let (parts, body) = req.into_parts();
    let mut req = http::Request::from_parts(parts, ());

    // Content length and is body is in eof state.
    let is_eof = match BodySize::from_stream(&body) {
        BodySize::None => {
            req.headers_mut().remove(CONTENT_LENGTH);
            true
        }
        BodySize::Stream => {
            req.headers_mut().remove(CONTENT_LENGTH);
            false
        }
        BodySize::Sized(len) if len == 0 => {
            req.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
            true
        }
        BodySize::Sized(len) => {
            let mut buf = itoa::Buffer::new();
            req.headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from_str(buf.format(len)).unwrap());
            false
        }
    };

    // TODO: consider skipping other headers according to:
    //       https://tools.ietf.org/html/rfc7540#section-8.1.2.2
    // omit HTTP/1.x only headers
    req.headers_mut().remove(CONNECTION);
    req.headers_mut().remove(TRANSFER_ENCODING);
    req.headers_mut().remove(UPGRADE);

    if !req.headers().contains_key(DATE) {
        let date = date.with_date(HeaderValue::from_bytes).unwrap();
        req.headers_mut().append(DATE, date);
    }

    let mut end_of_stream = is_eof;

    // websocket specific check.
    if let Some(v) = req.headers_mut().remove(PROTOCOL) {
        if req.method() != Method::CONNECT && !is_eof {
            return Err(::h2::Error::from(Reason::PROTOCOL_ERROR).into());
        }

        if let Ok(v) = v.to_str() {
            req.extensions_mut().insert(::h2::ext::Protocol::from(v));
            end_of_stream = false;
        }
    }

    let is_head_method = *req.method() == Method::HEAD;

    let (fut, mut stream) = stream.send_request(req, end_of_stream)?;

    if !is_eof {
        tokio::pin!(body);

        while let Some(res) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
            let mut chunk = res.map_err(BodyError::from)?;

            while !chunk.is_empty() {
                let len = chunk.len();

                stream.reserve_capacity(len);

                let cap = poll_fn(|cx| stream.poll_capacity(cx))
                    .await
                    .expect("No capacity left. http2 request is dropped")?;

                // Split chuck to writeable size and send to client.
                let bytes = chunk.split_to(cmp::min(cap, len));

                stream.send_data(bytes, false)?;
            }
        }

        stream.send_data(Bytes::new(), true)?;
    }

    let res = fut.await?;

    let res = if is_head_method {
        res.map(|_| ResponseBody::Eof)
    } else {
        res.map(|body| ResponseBody::H2(H2ResponseBody::new(stream, body.into())))
    };

    Ok(res)
}

pub(crate) async fn handshake<S>(stream: S) -> Result<Connection, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (conn, task) = client::Builder::new().enable_push(false).handshake(stream).await?;

    tokio::spawn(async {
        task.await.expect("http2 connection failed");
    });

    Ok(conn)
}
