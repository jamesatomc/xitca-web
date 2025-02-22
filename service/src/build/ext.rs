use crate::{
    async_closure::AsyncClosure,
    pipeline::{marker, PipelineT},
};

#[cfg(feature = "alloc")]
use crate::{
    build::boxed::Boxed,
    object::{DefaultObjectConstructor, ObjectConstructor},
};

use super::BuildService;

/// Extend trait for [BuildService]
///
/// Provide methods for mutation of it's associated types.
pub trait BuildServiceExt<Arg>: BuildService<Arg> {
    /// Mutate `<<Self::Service as Service<Req>>::Future as Future>::Output` type with given
    /// closure.
    fn map<F, Res, ResMap>(self, mapper: F) -> PipelineT<Self, F, marker::Map>
    where
        F: Fn(Res) -> ResMap + Clone,
        Self: Sized,
    {
        PipelineT::new(self, mapper)
    }

    /// Mutate `<Self::Service as Service<Req>>::Error` type with given closure.
    fn map_err<F, Err, ErrMap>(self, err: F) -> PipelineT<Self, F, marker::MapErr>
    where
        F: Fn(Err) -> ErrMap + Clone,
        Self: Sized,
    {
        PipelineT::new(self, err)
    }

    /// Chain another service factory who's service takes `Self`'s `Service::Response` output as
    /// `Service::Request`.
    fn and_then<F>(self, factory: F) -> PipelineT<Self, F, marker::AndThen>
    where
        F: BuildService<Arg>,
        Self: Sized,
    {
        PipelineT::new(self, factory)
    }

    /// Enclose Self with given `T as BuildService<<Self as BuildService<_>>::Service>>`.
    /// In other word T would take Self's Service type it's generic argument of `BuildService<_>`.
    fn enclosed<T>(self, build: T) -> PipelineT<Self, T, marker::Enclosed>
    where
        T: BuildService<Self::Service> + Clone,
        Self: BuildService<Arg> + Sized,
    {
        PipelineT::new(self, build)
    }

    /// Function version of [Self::enclosed] method.
    fn enclosed_fn<T, Req, Req2>(self, func: T) -> PipelineT<Self, T, marker::EnclosedFn<Req2>>
    where
        T: for<'s> AsyncClosure<(&'s Self::Service, Req)> + Clone,
        Self: BuildService<Arg> + Sized,
    {
        PipelineT::new(self, func)
    }

    #[cfg(feature = "alloc")]
    /// Box `<Self as BuildService<_>>::Future` to reduce it's stack size.
    ///
    /// *. This combinator does not box `Self` or `Self::Service`.
    fn boxed_future(self) -> Boxed<Self>
    where
        Self: Sized,
    {
        Boxed::new(self)
    }

    #[cfg(feature = "alloc")]
    /// Box self and cast it to a trait object.
    ///
    /// This would erase `Self::Service` type and it's GAT nature.
    ///
    /// See [crate::object::DefaultObjectConstructor] for detail.
    fn into_object<Req>(self) -> <DefaultObjectConstructor<Req, Arg> as ObjectConstructor<Self>>::Object
    where
        Self: Sized,
        DefaultObjectConstructor<Req, Arg>: ObjectConstructor<Self>,
    {
        DefaultObjectConstructor::into_object(self)
    }
}

impl<F, Arg> BuildServiceExt<Arg> for F where F: BuildService<Arg> {}

#[cfg(test)]
mod test {
    use super::*;

    use core::{convert::Infallible, future::Future};

    use xitca_unsafe_collection::futures::NowOrPanic;

    use crate::{fn_service, Service};

    #[derive(Clone)]
    struct DummyMiddleware;

    #[derive(Clone)]
    struct DummyMiddlewareService<S: Clone>(S);

    impl<S: Clone> BuildService<S> for DummyMiddleware {
        type Service = DummyMiddlewareService<S>;
        type Error = Infallible;
        type Future = impl Future<Output = Result<Self::Service, Self::Error>>;

        fn build(&self, service: S) -> Self::Future {
            async { Ok(DummyMiddlewareService(service)) }
        }
    }

    impl<S, Req> Service<Req> for DummyMiddlewareService<S>
    where
        S: Service<Req> + Clone,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future<'f> = impl Future<Output = Result<Self::Response, Self::Error>> where S: 'f;

        fn call(&self, req: Req) -> Self::Future<'_> {
            async move { self.0.call(req).await }
        }
    }

    async fn index(s: &'static str) -> Result<&'static str, ()> {
        Ok(s)
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn service_object() {
        let service = fn_service(index)
            .enclosed(DummyMiddleware)
            .into_object()
            .build(())
            .now_or_panic()
            .unwrap();

        let res = service.call("996").now_or_panic().unwrap();
        assert_eq!(res, "996");
    }

    #[test]
    fn map() {
        let service = fn_service(index).map(|_| "251").build(()).now_or_panic().unwrap();

        let err = service.call("996").now_or_panic().ok().unwrap();
        assert_eq!(err, "251");
    }

    #[test]
    fn map_err() {
        let service = fn_service(|_: &str| async { Err::<(), _>(()) })
            .map_err(|_| "251")
            .build(())
            .now_or_panic()
            .unwrap();

        let err = service.call("996").now_or_panic().err().unwrap();
        assert_eq!(err, "251");
    }

    #[test]
    fn enclosed_fn() {
        async fn enclosed<S>(service: &S, req: &'static str) -> Result<&'static str, ()>
        where
            S: Service<&'static str, Response = &'static str, Error = ()>,
        {
            let res = service.call(req).now_or_panic()?;
            assert_eq!(res, "996");
            Ok("251")
        }

        let res = fn_service(index)
            .enclosed_fn(enclosed)
            .build(())
            .now_or_panic()
            .unwrap()
            .call("996")
            .now_or_panic()
            .ok()
            .unwrap();

        assert_eq!(res, "251");
    }
}
