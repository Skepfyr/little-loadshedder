use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use pin_project::{pin_project, pinned_drop};
use thiserror::Error;
use tower::Service;

/// Load Shed Services current state of the world
#[derive(Debug)]
struct LoadShedConf {
    target: f64,
    /// Seconds
    moving_average: f64,
    /// In the range (0, 1)
    /// .25 means new values account for 25% of the moving average
    ewma_param: f64,
    capacity: usize,
    inflight: usize,
    last_decrement: Instant,
}

impl LoadShedConf {
    fn new(ewma_param: f64, target: f64) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            target,
            moving_average: target,
            ewma_param,
            capacity: 1,
            inflight: 0,
            last_decrement: Instant::now(),
        }))
    }

    fn start(&mut self) -> bool {
        if self.inflight >= self.capacity {
            return false;
        }
        self.inflight += 1;
        true
    }

    fn stop(&mut self, elapsed: Duration) {
        let elapsed = elapsed.as_secs_f64();
        self.moving_average =
            (self.moving_average * (1.0 - self.ewma_param)) + (self.ewma_param * elapsed);
        if self.capacity == self.inflight && self.moving_average < self.target {
            self.capacity += 1;
            println!("Increasing capacity by 1 to {}", self.capacity);
            println!("Average time for request is {}s", self.moving_average);
        } else if self.moving_average > self.target
            && self.last_decrement.elapsed().as_secs_f64() > self.target
        {
            self.capacity -= 1;
            self.last_decrement = Instant::now();
            println!("Decreasing cap by 1");
        }
        self.inflight -= 1;
    }
}

#[derive(Debug, Clone)]
pub struct LoadShed<Inner> {
    conf: Arc<Mutex<LoadShedConf>>,
    inner: Inner,
}

impl<Inner> LoadShed<Inner> {
    pub fn new(inner: Inner, ewma_param: f64, target: Duration) -> Self {
        Self {
            inner,
            conf: LoadShedConf::new(ewma_param, target.as_secs_f64()),
        }
    }
}

/// Either an error from the wrapped service or message that the request was shed
#[derive(Error, Debug)]
pub enum LoadShedError<T> {
    #[error("Inner service error")]
    Inner(#[from] T),
    #[error("Load shed due to overload")]
    Overload,
}

impl<Request, Inner: Service<Request>> Service<Request> for LoadShed<Inner> {
    type Response = Inner::Response;
    type Error = LoadShedError<Inner::Error>;
    type Future = LoadShedFut<Inner::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(LoadShedError::Inner)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if self.conf.lock().unwrap().start() {
            LoadShedFut(LoadShedFutInner::Future {
                inner: self.inner.call(req),
                conf: Arc::clone(&self.conf),
                start: Instant::now(),
                stopped: false,
            })
        } else {
            LoadShedFut(LoadShedFutInner::Shed)
        }
    }
}

#[pin_project]
pub struct LoadShedFut<Inner>(#[pin] LoadShedFutInner<Inner>);

/// Calling .project() on a LoadShedResult yields a LoadShedFutProj
/// type which has pinned fields
#[pin_project(PinnedDrop, project = LoadShedFutProj)]
enum LoadShedFutInner<Inner> {
    Future {
        #[pin]
        inner: Inner,
        start: Instant,
        conf: Arc<Mutex<LoadShedConf>>,
        stopped: bool,
    },
    Shed,
}

impl<Inner, Output, Error> Future for LoadShedFut<Inner>
where
    Inner: Future<Output = Result<Output, Error>>,
{
    type Output = Result<Output, LoadShedError<Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (inner, start, conf, stopped) = match self.project().0.project() {
            LoadShedFutProj::Future {
                inner,
                start,
                conf,
                stopped,
            } => (inner, start, conf, stopped),
            LoadShedFutProj::Shed => return Poll::Ready(Err(LoadShedError::Overload)),
        };
        let result = match inner.poll(cx) {
            Poll::Ready(result) => result,
            Poll::Pending => return Poll::Pending,
        };
        *stopped = true;
        conf.lock().unwrap().stop(start.elapsed());
        Poll::Ready(Ok(result?))
    }
}

#[pinned_drop]
impl<Inner> PinnedDrop for LoadShedFutInner<Inner> {
    fn drop(self: Pin<&mut Self>) {
        match self.project() {
            LoadShedFutProj::Future {
                start,
                conf,
                stopped,
                ..
            } => {
                if !*stopped {
                    conf.lock().unwrap().stop(start.elapsed());
                }
            }
            LoadShedFutProj::Shed => {}
        }
    }
}
