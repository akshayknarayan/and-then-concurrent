# and-then-concurrent

Use on `impl Stream`s via the [`TryStreamAndThenExt`] trait.

Why is this necessary? Consider the example below. We have a `Stream` from `try_unfold`, but
this stream is splitting some larger stream into sub-streams, with each sub-stream
represented by a channel. If we simply called `and_then`, that function's implementation,
as an optimization, only keeps *one* "pending" future in its state. This means that it
cannot poll the backing stream, because that might produce another future which it has no
space for. So, it *must* run the pending future to completion before polling the stream
again.

Unfortunately, in this case, the backing stream has to be polled for our future to resolve!
So using `and_then` will deadlock. Instead, this crate makes a tradeoff: it will hold a
list of pending futures in a `FuturesUnordered`, so it is safe to
poll the backing stream. This means that if the resulting futures don't resolve, we could
have a large list of futures.

```rust
let c = futures_util::stream::try_unfold(
    (
        0,
        HashMap::<usize, mpsc::UnboundedSender<(usize, usize)>>::default(),
    ),
    move |(mut i, mut map)| async move {
        loop {
            sleep(Duration::from_millis(10)).await;
            let (substream, message) = (i % 3, i);
            i += 1;
            if i > 25 {
                return Ok(None);
            }

            let mut new = None;
            if map
                .entry(substream)
                .or_insert_with(|| {
                    let (sub_s, sub_r) = mpsc::unbounded_channel();
                    new = Some(sub_r);
                    sub_s
                })
                .send((substream, message))
                .is_err()
            {
                map.remove(&substream);
            }

            if let Some(new_sub_r) = new {
                return Ok::<_, String>(Some((new_sub_r, (i, map))));
            }
        }
    },
)
// .and_then(...) would deadlock!
.and_then_concurrent(|mut res| async move {
    loop {
        let (stream, val): (usize, usize) = match res.recv().await {
            None => return Ok(()),
            Some(s) => s,
        };
        println!("got {:?} on stream {:?}", val, stream);
    }
})
.try_collect::<Vec<_>>();
c.await.unwrap();
```
