use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use tower::Service;

/// Load Shed Services current state of the world
#[derive(Debug, Clone)]
struct LoadShedConf {
    target: Duration,
    /// Seconds
    moving_average: Arc<Mutex<f64>>,
    /// In the range (0, 1)
    /// .25 means new values account for 25% of the moving average
    ewma_param: f64,
    /// Max inflight requests
    inflight: Arc<Mutex<usize>>,
}


#[derive(Debug, Clone)]
pub struct LoadShed<Inner> {
    conf: LoadShedConf,
    inner: Inner,
}

impl<Inner> LoadShed<Inner> {
    pub fn new(inner: Inner, ewma_param: f64, target: Duration) -> Self {
        Self {
            inner,
            conf: LoadShedConf {
                target,
                moving_average: Arc::new(Mutex::new(target.as_secs_f64())),
                ewma_param,
                inflight: Arc::new(Mutex::new(1))
            }
        }
    }
}

impl<Request, Inner> Service<Request> for LoadShed<Inner>
where
    Inner: Service<Request>,
    Inner::Response: std::fmt::Debug,
    Inner::Error: std::fmt::Debug,
{
    type Response = Inner::Response;
    type Error = Inner::Error;
    type Future = LoadShedFut<Inner::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        LoadShedFut {
            inner: self.inner.call(req),
            conf: self.conf.clone(),
            start: Instant::now()
        }
    }
}

#[pin_project::pin_project]
pub struct LoadShedFut<Inner> {
    #[pin]
    inner: Inner,
    start: Instant,
    conf: LoadShedConf,
}

impl<Inner> Future for LoadShedFut<Inner>
where
    Inner: Future,
    Inner::Output: std::fmt::Debug,
{
    type Output = Inner::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = match this.inner.poll(cx) {
            Poll::Ready(result) => result,
            Poll::Pending => return Poll::Pending,
        };

        let elapsed: f64 = this.start.elapsed().as_secs_f64();
        println!("Elapsed time for request is {}s", elapsed);
        let moving_average = {
            let mut moving_average = this.conf.moving_average.lock().unwrap();
            *moving_average =
                (*moving_average * (1.0 - this.conf.ewma_param)) + (this.conf.ewma_param * elapsed);
            *moving_average
        };
        println!("New average: {}", moving_average);

        Poll::Ready(result)
    }
}
