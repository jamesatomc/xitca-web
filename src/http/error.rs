use std::{
    error::Error,
    fmt::{self, Debug, Display, Formatter},
    io,
};

use log::error;

/// HttpService layer error.
pub enum HttpServiceError {
    #[cfg(feature = "openssl")]
    Openssl(super::tls::openssl::OpensslError),
    Rustls(super::tls::rustls::RustlsError),
    ServiceReady,
    Body(BodyError),
    // Http/2 error happen in HttpService handle.
    H2(h2::Error),
}

impl Debug for HttpServiceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ServiceReady => write!(f, "Service is not ready"),
            Self::Body(ref e) => write!(f, "{:?}", e),
            Self::H2(ref e) => write!(f, "{:?}", e),
            #[cfg(feature = "openssl")]
            Self::Openssl(ref e) => write!(f, "{:?}", e),
            Self::Rustls(ref e) => write!(f, "{:?}", e),
        }
    }
}

impl HttpServiceError {
    pub fn log(self) {
        // TODO: add logging for different error types.
        error!("HttpService Error: {:?}", self);
    }
}

/// Request/Response body layer error.
pub enum BodyError {
    Std(Box<dyn Error>),
    Io(io::Error),
    // Http/2 error happens when handling body.
    H2(h2::Error),
}

impl Debug for BodyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Std(ref e) => write!(f, "{:?}", e),
            Self::Io(ref e) => write!(f, "{:?}", e),
            Self::H2(ref e) => write!(f, "{:?}", e),
        }
    }
}

impl Display for BodyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Std(ref e) => write!(f, "{:?}", e),
            Self::Io(ref e) => write!(f, "{:?}", e),
            Self::H2(ref e) => write!(f, "{:?}", e),
        }
    }
}

impl Error for BodyError {}

impl From<io::Error> for BodyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<Box<dyn Error>> for BodyError {
    fn from(e: Box<dyn Error>) -> Self {
        Self::Std(e)
    }
}

impl From<BodyError> for HttpServiceError {
    fn from(e: BodyError) -> Self {
        Self::Body(e)
    }
}
