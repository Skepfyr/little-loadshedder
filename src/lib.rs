use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use thiserror::Error;

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
    /// Subtract one when starting work, add one when completing
    free_capacity: Arc<Mutex<usize>>,
}

impl LoadShedConf {
    fn start(&self) -> bool {
        let mut free_capacity = self
            .free_capacity
            .lock()
            .expect("To be able to lock inflight in conf");
        if *free_capacity > 0 {
            *free_capacity -= 1;
            return true;
        }
        false
    }

    fn stop(&self) {
        *self.free_capacity.lock().unwrap() += 1;
    }
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
                free_capacity: Arc::new(
                    // Checked subtractions are hard on atomics
                    // This emulates a Semaphore, so we could pull in a proper one
                    #[allow(clippy::mutex_atomic)]
                    Mutex::new(1),
                ),
            },
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
        if self.conf.start() {
            LoadShedFut(LoadShedFutInner::Future {
                inner: self.inner.call(req),
                conf: self.conf.clone(),
                start: Instant::now(),
            })
        } else {
            LoadShedFut(LoadShedFutInner::Shed)
        }
    }
}

#[pin_project::pin_project]
pub struct LoadShedFut<Inner>(#[pin] LoadShedFutInner<Inner>);

/// Calling .project() on a LoadShedResult yields a LoadShedFutProj
/// type which has pinned fields
#[pin_project::pin_project(project = LoadShedFutProj)]
enum LoadShedFutInner<Inner> {
    Future {
        #[pin]
        inner: Inner,
        start: Instant,
        conf: LoadShedConf,
    },
    Shed,
}

impl<Inner, Output, Error> Future for LoadShedFut<Inner>
where
    Inner: Future<Output = Result<Output, Error>>,
{
    type Output = Result<Output, LoadShedError<Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (inner, start, conf) = match self.project().0.project() {
            LoadShedFutProj::Future { inner, start, conf } => (inner, start, conf),
            LoadShedFutProj::Shed => return Poll::Ready(Err(LoadShedError::Overload)),
        };
        let result = match inner.poll(cx) {
            Poll::Ready(result) => result,
            Poll::Pending => return Poll::Pending,
        };

        let elapsed: f64 = start.elapsed().as_secs_f64();
        println!("Elapsed time for request is {}s", elapsed);
        let moving_average = {
            let mut moving_average = conf.moving_average.lock().unwrap();
            *moving_average =
                (*moving_average * (1.0 - conf.ewma_param)) + (conf.ewma_param * elapsed);
            *moving_average
        };
        conf.stop();
        println!("New average: {}", moving_average);
        Poll::Ready(Ok(result?))
    }
}
