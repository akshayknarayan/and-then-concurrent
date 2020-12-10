//! Use on `impl Stream`s via the [`TryStreamAndThenExt`] trait.
//!
//! Why is this necessary? Consider the example below. We have a `Stream` from `try_unfold`, but
//! this stream is splitting some larger stream into sub-streams, with each sub-stream
//! represented by a channel. If we simply called `and_then`, that function's implementation,
//! as an optimization, only keeps *one* "pending" future in its state. This means that it
//! cannot poll the backing stream, because that might produce another future which it has no
//! space for. So, it *must* run the pending future to completion before polling the stream
//! again.
//!
//! Unfortunately, in this case, the backing stream has to be polled for our future to resolve!
//! So using `and_then` will deadlock. Instead, this crate makes a tradeoff: it will hold a
//! list of pending futures in a `FuturesUnordered`, so it is safe to
//! poll the backing stream. This means that if the resulting futures don't resolve, we could
//! have a large list of futures.
//!
//! ```rust
//! # use and_then_concurrent::TryStreamAndThenExt;
//! # use futures_util::stream::TryStreamExt;
//! # use std::collections::HashMap;
//! # use std::time::Duration;
//! # use tokio::{sync::mpsc, time::sleep};
//! # #[tokio::main]
//! # async fn main() {
//! let c = futures_util::stream::try_unfold(
//!     (
//!         0,
//!         HashMap::<usize, mpsc::UnboundedSender<(usize, usize)>>::default(),
//!     ),
//!     move |(mut i, mut map)| async move {
//!         loop {
//!             sleep(Duration::from_millis(10)).await;
//!             let (substream, message) = (i % 3, i);
//!             i += 1;
//!             if i > 25 {
//!                 return Ok(None);
//!             }
//!
//!             let mut new = None;
//!             if map
//!                 .entry(substream)
//!                 .or_insert_with(|| {
//!                     let (sub_s, sub_r) = mpsc::unbounded_channel();
//!                     new = Some(sub_r);
//!                     sub_s
//!                 })
//!                 .send((substream, message))
//!                 .is_err()
//!             {
//!                 map.remove(&substream);
//!             }
//!
//!             if let Some(new_sub_r) = new {
//!                 return Ok::<_, String>(Some((new_sub_r, (i, map))));
//!             }
//!         }
//!     },
//! )
//! // .and_then(...) would deadlock!
//! .and_then_concurrent(|mut res| async move {
//!     loop {
//!         let (stream, val): (usize, usize) = match res.recv().await {
//!             None => return Ok(()),
//!             Some(s) => s,
//!         };
//!         println!("got {:?} on stream {:?}", val, stream);
//!     }
//! })
//! .try_collect::<Vec<_>>();
//! c.await.unwrap();
//! # }
//! ```

use futures_util::{
    future::TryFuture,
    stream::{FuturesUnordered, Stream, TryStream},
};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Extension to [`futures_util::stream::TryStreamExt`]
pub trait TryStreamAndThenExt: TryStream {
    /// Chain a computation when a stream value is ready, passing `Ok` values to the closure `f`.
    ///
    /// This function is similar to [`futures_util::stream::TryStreamExt::and_then`], but the
    /// stream is polled concurrently with the futures returned by `f`. An unbounded number of
    /// futures corresponding to past stream values is kept via `FuturesUnordered`.
    ///
    /// See [crate-level docs](`crate`) for an explanation and usage example.
    fn and_then_concurrent<Fut, F>(self, f: F) -> AndThenConcurrent<Self, Fut, F>
    where
        Self: Sized,
        Fut: TryFuture<Error = Self::Error>,
        F: FnMut(Self::Ok) -> Fut;
}

impl<S: TryStream> TryStreamAndThenExt for S {
    fn and_then_concurrent<Fut, F>(self, f: F) -> AndThenConcurrent<Self, Fut, F>
    where
        Self: Sized,
        Fut: TryFuture<Error = Self::Error>,
        F: FnMut(Self::Ok) -> Fut,
    {
        AndThenConcurrent {
            stream: self,
            futs: FuturesUnordered::new(),
            fun: f,
        }
    }
}

/// Stream type for [`TryStreamAndThenExt::and_then_concurrent`].
#[pin_project(project = AndThenConcurrentProj)]
pub struct AndThenConcurrent<St, Fut: TryFuture, F> {
    #[pin]
    stream: St,
    #[pin]
    futs: FuturesUnordered<Fut>,
    fun: F,
}

impl<St, Fut, F, T> Stream for AndThenConcurrent<St, Fut, F>
where
    St: TryStream,
    Fut: Future<Output = Result<T, St::Error>>,
    F: FnMut(St::Ok) -> Fut,
{
    type Item = Result<T, St::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let AndThenConcurrentProj {
            mut stream,
            mut futs,
            fun,
        } = self.project();

        match stream.as_mut().try_poll_next(cx) {
            Poll::Ready(Some(Ok(n))) => {
                futs.push(fun(n));
            }
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
            Poll::Pending => {
                if futs.is_empty() {
                    return Poll::Pending;
                }
            }
            _ => (),
        }

        let x = futs.as_mut().poll_next(cx);
        if let Poll::Pending = x {
            // check stream once more
            match stream.as_mut().try_poll_next(cx) {
                Poll::Ready(Some(Ok(n))) => {
                    futs.push(fun(n));
                }
                _ => (),
            }
        }
        x
    }
}
