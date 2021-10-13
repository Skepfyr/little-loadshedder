use std::{
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use pin_project::{pin_project, pinned_drop};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit, TryAcquireError};
use tower::Service;

/// Load Shed Services current state of the world
#[derive(Debug, Clone)]
struct LoadShedConf {
    target: f64,
    /// In the range (0, 1)
    /// .25 means new values account for 25% of the moving average
    ewma_param: f64,

    /// Semaphore permits are added here when we want to remove capacity from the system
    /// When workers finish jobs they try_acquire() this semaphore.
    /// If they are successful they do not return their token to available_queue pool
    /// If they are not they return the token to the pool.moving_average
    capacity_to_be_removed: Arc<Semaphore>,

    /// Semaphore controlling concurrency to the inner service.
    available_concurrency: Arc<Semaphore>,
    /// Queue Space
    available_queue: Arc<Semaphore>,

    stats: Arc<Mutex<ConfStats>>,
}

#[derive(Debug)]
struct ConfStats {
    /// Seconds
    moving_average: f64,
    /// Capacity of the entire system, which is queue length + concurrency.
    capacity: usize,
    last_decrement: Instant,
}

impl LoadShedConf {
    fn new(ewma_param: f64, target: f64) -> Self {
        Self {
            target,
            ewma_param,
            capacity_to_be_removed: Arc::new(Semaphore::new(0)),
            available_concurrency: Arc::new(Semaphore::new(1)),
            available_queue: Arc::new(Semaphore::new(1)),
            stats: Arc::new(Mutex::new(ConfStats {
                moving_average: target,
                capacity: 1,
                last_decrement: Instant::now(),
            })),
        }
    }

    async fn start(&self) -> Result<OwnedSemaphorePermit, ()> {
        let queue_permit = match self.available_queue.try_acquire() {
            Ok(queue_permit) => queue_permit,
            Err(TryAcquireError::NoPermits) => return Err(()),
            Err(TryAcquireError::Closed) => panic!("queue semaphore closed?"),
        };
        let concurrency_permit = self
            .available_concurrency
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        drop(queue_permit);
        Ok(concurrency_permit)
    }

    fn stop(&mut self, elapsed: Duration) {
        let elapsed = elapsed.as_secs_f64();

        let stats = self.stats.lock().expect("To be able to lock stats");
        stats.moving_average =
            (stats.moving_average * (1.0 - self.ewma_param)) + (self.ewma_param * elapsed);

        if sta.capacity == self.inflight && self.moving_average < self.target {
            self.capacity += 1;
            println!("Increasing capacity by 1 to {}", self.capacity);
            println!("Average time for request is {}s", self.moving_average);
            self.available_concurrency;
            self.available_queue;
        } else if self.moving_average > self.target
            && self.last_decrement.elapsed().as_secs_f64() > self.target
        {
            self.capacity -= 1;
            self.last_decrement = Instant::now();
            self.capacity_to_be_removed.add_permits(1);
            println!("Decreasing cap by 1");
        }
        self.inflight -= 1;
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
            conf: LoadShedConf::new(ewma_param, target.as_secs_f64()),
        }
    }
}

/// Either an error from the wrapped service or message that the request was shed
#[derive(Error, Debug)]
pub enum LoadShedError<T> {
    #[error("Inner service error")]
    Inner(#[from] T),
    #[error("Load shed due to full queue")]
    QueueFull,
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
        if let Ok(permit) = self.conf.start() {
            LoadShedFut(LoadShedFutInner::Future {
                inner: self.inner.call(req),
                conf: self.conf.clone(),
                start: Instant::now(),
                permit: Some(permit),
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
#[pin_project(project = LoadShedFutProj)]
enum LoadShedFutInner<Inner> {
    Future {
        #[pin]
        inner: Inner,
        start: Instant,
        conf: LoadShedConf,
        permit: Option<OwnedSemaphorePermit>,
    },
    Shed,
}

impl<Inner, Output, Error> Future for LoadShedFut<Inner>
where
    Inner: Future<Output = Result<Output, Error>>,
{
    type Output = Result<Output, LoadShedError<Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (inner, start, conf, permit) = match self.project().0.project() {
            LoadShedFutProj::Future {
                inner,
                start,
                conf,
                permit,
            } => (inner, start, conf, permit),
            LoadShedFutProj::Shed => return Poll::Ready(Err(LoadShedError::QueueFull)),
        };
        let result = match inner.poll(cx) {
            Poll::Ready(result) => result,
            Poll::Pending => return Poll::Pending,
        };
        conf.stop(start.elapsed(), permit.unwrap());
        Poll::Ready(Ok(result?))
    }
}
