use http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use httparse::{Error, EMPTY_HEADER};

use super::error::MultipartError;

// TODO: remove mime.
/// Extract boundary info from headers.
pub(super) fn boundary<E>(headers: &HeaderMap) -> Result<String, MultipartError<E>> {
    headers
        .get(&CONTENT_TYPE)
        .ok_or(MultipartError::NoContentType)?
        .to_str()
        .ok()
        .and_then(|content_type| content_type.parse::<mime::Mime>().ok())
        .ok_or(MultipartError::ParseContentType)?
        .get_param(mime::BOUNDARY)
        .map(|boundary| boundary.as_str().to_string())
        .ok_or(MultipartError::Boundary)
}

pub(super) fn parse_headers<E>(slice: &[u8]) -> Result<HeaderMap, MultipartError<E>> {
    let mut hdrs = [EMPTY_HEADER; 16];
    match httparse::parse_headers(slice, &mut hdrs)? {
        httparse::Status::Complete((_, hdrs)) => {
            let mut headers = HeaderMap::with_capacity(hdrs.len());

            for h in hdrs {
                let name = HeaderName::try_from(h.name).map_err(|_| Error::HeaderName)?;
                let value = HeaderValue::try_from(h.value).map_err(|_| Error::HeaderValue)?;
                headers.append(name, value);
            }

            Ok(headers)
        }
        httparse::Status::Partial => Err(Error::TooManyHeaders.into()),
    }
}

pub(super) fn check_headers<E>(headers: &HeaderMap) -> Result<(), MultipartError<E>> {
    // According to RFC 7578 §4.2, a Content-Disposition header must always be present and
    // set to "form-data".

    match headers.get(&CONTENT_DISPOSITION) {
        Some(_) => {}
        None => return Err(MultipartError::NoContentDisposition),
    };

    let ct = headers
        .get(&CONTENT_TYPE)
        .and_then(|ct| ct.to_str().ok())
        .and_then(|ct| ct.parse().ok())
        .unwrap_or(mime::APPLICATION_OCTET_STREAM);

    // nested multipart stream is not supported
    if ct.type_() == mime::MULTIPART {
        return Err(MultipartError::Nested);
    }

    Ok(())
}

pub(super) fn content_length_opt<E>(headers: &HeaderMap) -> Result<Option<u64>, MultipartError<E>> {
    match headers.get(&CONTENT_LENGTH) {
        Some(len) => {
            let len = len
                .to_str()
                .map_err(|_| Error::HeaderValue)?
                .parse()
                .map_err(|_| Error::HeaderValue)?;
            Ok(Some(len))
        }
        None => Ok(None),
    }
}