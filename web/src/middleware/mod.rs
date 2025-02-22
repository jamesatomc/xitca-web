#[cfg(any(feature = "compress-br", feature = "compress-gz", feature = "compress-de"))]
pub mod compress;
#[cfg(any(feature = "compress-br", feature = "compress-gz", feature = "compress-de"))]
pub mod decompress;

#[cfg(feature = "tower-http-compat")]
pub mod tower_http_compat;

pub use xitca_http::util::middleware::Extension;

pub use xitca_service::middleware::UncheckedReady;

#[cfg(test)]
mod test {
    use xitca_http::{body::RequestBody, request::Request};
    use xitca_unsafe_collection::futures::NowOrPanic;

    use crate::{
        dev::service::{BuildService, Service},
        handler::{extension::ExtensionRef, handler_service},
        test::collect_string_body,
        App,
    };

    use super::*;

    #[test]
    fn extension() {
        async fn root(ExtensionRef(ext): ExtensionRef<'_, String>) -> String {
            ext.to_string()
        }

        let body = App::new()
            .at("/", handler_service(root))
            .enclosed(Extension::new("hello".to_string()))
            .enclosed(UncheckedReady)
            .finish()
            .build(())
            .now_or_panic()
            .unwrap()
            .call(Request::<RequestBody>::default())
            .now_or_panic()
            .unwrap()
            .into_body();

        let string = collect_string_body(body).now_or_panic().unwrap();
        assert_eq!(string, "hello");
    }
}
