use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project_lite::pin_project;

use super::keep_alive::KeepAlive;

#[cfg(any(feature = "http1", feature = "http2"))]
pub(crate) use poll_fn::poll_fn;

#[cfg(any(feature = "http1", feature = "http2"))]
mod poll_fn {
    use super::*;

    #[inline]
    pub(crate) fn poll_fn<T, F>(f: F) -> PollFn<F>
    where
        F: FnMut(&mut Context<'_>) -> Poll<T>,
    {
        PollFn { f }
    }

    pub(crate) struct PollFn<F> {
        f: F,
    }

    impl<F> Unpin for PollFn<F> {}

    impl<T, F> Future for PollFn<F>
    where
        F: FnMut(&mut Context<'_>) -> Poll<T>,
    {
        type Output = T;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
            (&mut self.f)(cx)
        }
    }
}

#[cfg(feature = "http1")]
/// An async function that never resolve to the output.
#[inline]
pub(crate) async fn never<T>() -> T {
    poll_fn(|_| Poll::Pending).await
}

#[cfg(feature = "http1")]
#[inline]
pub(crate) async fn select2<Fut1, Fut2>(fut1: Fut1, fut2: Fut2) -> Select2<Fut1::Output, Fut2::Output>
where
    Fut1: Future,
    Fut2: Future,
{
    pin_project! {
        struct _Select2<Fut1, Fut2> {
            #[pin]
            fut1: Fut1,
            #[pin]
            fut2: Fut2,
        }
    }

    impl<Fut1, Fut2> Future for _Select2<Fut1, Fut2>
    where
        Fut1: Future,
        Fut2: Future,
    {
        type Output = Select2<Fut1::Output, Fut2::Output>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();

            if let Poll::Ready(a) = this.fut1.poll(cx) {
                return Poll::Ready(Select2::A(a));
            }

            this.fut2.poll(cx).map(Select2::B)
        }
    }

    _Select2 { fut1, fut2 }.await
}

#[cfg(feature = "http1")]
pub(crate) enum Select2<A, B> {
    A(A),
    B(B),
}

pub(crate) trait Timeout: Sized {
    fn timeout(self, timer: Pin<&mut KeepAlive>) -> TimeoutFuture<'_, Self>;
}

impl<F> Timeout for F
where
    F: Future,
{
    fn timeout(self, timer: Pin<&mut KeepAlive>) -> TimeoutFuture<'_, Self> {
        TimeoutFuture { fut: self, timer }
    }
}

pin_project! {
    pub(crate) struct TimeoutFuture<'a, F> {
        #[pin]
        fut: F,
        timer: Pin<&'a mut KeepAlive>
    }
}

impl<F: Future> Future for TimeoutFuture<'_, F> {
    type Output = Result<F::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(res) = this.fut.poll(cx) {
            return Poll::Ready(Ok(res));
        }

        this.timer.as_mut().poll(cx).map(Err)
    }
}