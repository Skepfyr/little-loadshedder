use std::{
    cmp::Ordering,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

// size of system [req] = target latency [s] * throughput [r/s]
// size of queue [req] = size of system [req] - concurrency [req]
// throughput [req/s] = concurrency [req] / average latency of service [s]

// Control the concurrency:
// - increase concurrency but not beyond target latency
// Control queue length:
// - queue capacity = concurrency * ((target latency / average latency of service) - 1)

// Possible extension: You could hit maximum throughput before target latency on system
// and should not increase concurrency beyond that point.

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tower::Service;

/// Load Shed Services current state of the world
#[derive(Debug, Clone)]
struct LoadShedConf {
    target: f64,
    /// In the range (0, 1)
    /// .25 means new values account for 25% of the moving average
    ewma_param: f64,

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
    /// Concurrency: number of permits in the available_concurrency semaphore
    concurrency: f64,
    /// Controls how often concurrency is updated
    last_decrement: Instant,
    /// current capacity of queue
    queue_capacity: usize,
}

impl LoadShedConf {
    fn new(ewma_param: f64, target: f64) -> Self {
        Self {
            target,
            ewma_param,
            available_concurrency: Arc::new(Semaphore::new(1)),
            available_queue: Arc::new(Semaphore::new(1)),
            stats: Arc::new(Mutex::new(ConfStats {
                moving_average: target,
                concurrency: 1.0,
                last_decrement: Instant::now(),
                queue_capacity: 1,
            })),
        }
    }

    async fn start(&self) -> Result<OwnedSemaphorePermit, ()> {
        {
            let mut stats = self.stats.lock().unwrap();
            let desired_queue_capacity = usize::max(
                1,
                (stats.concurrency * ((self.target / stats.moving_average) - 1.0)).floor() as usize,
            );
            match desired_queue_capacity.cmp(&stats.queue_capacity) {
                Ordering::Less => {
                    match self
                        .available_queue
                        .try_acquire_many((stats.queue_capacity - desired_queue_capacity) as u32)
                    {
                        Ok(permits) => permits.forget(),
                        Err(TryAcquireError::NoPermits) => return Err(()),
                        Err(TryAcquireError::Closed) => panic!(),
                    }
                }
                Ordering::Equal => {}
                Ordering::Greater => self
                    .available_queue
                    .add_permits(desired_queue_capacity - stats.queue_capacity),
            }
            stats.queue_capacity = desired_queue_capacity;
        }

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

    fn stop(&mut self, elapsed: Duration, concurrency_permit: OwnedSemaphorePermit) {
        let elapsed = elapsed.as_secs_f64();
        let mut stats = self.stats.lock().expect("To be able to lock stats");
        stats.moving_average =
            (stats.moving_average * (1.0 - self.ewma_param)) + (self.ewma_param * elapsed);
        // println!("mAvg = {}, concurrency = {}, target = {}", stats.moving_average, stats.concurrency)
        if self.available_concurrency.available_permits() == 0 && stats.moving_average < self.target
        {
            self.available_concurrency.add_permits(1);
            stats.concurrency += 1.0;
            println!(
                "Concurrency maxed & latency < target, adding permit {}",
                stats.concurrency
            );
        } else if stats.moving_average > self.target
            && stats.last_decrement.elapsed().as_secs_f64() > self.target
        {
            concurrency_permit.forget();
            stats.concurrency -= 1.0;
            stats.last_decrement = Instant::now();
            println!("Concurrency -- {}", stats.concurrency);
        }
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

type BoxFuture<Output> = Pin<Box<dyn Future<Output = Output> + Send>>;

impl<Request, Inner> Service<Request> for LoadShed<Inner>
where
    Request: Send + 'static,
    Inner: Service<Request> + Clone + Send + 'static,
    Inner::Future: Send,
{
    type Response = Inner::Response;
    type Error = LoadShedError<Inner::Error>;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(LoadShedError::Inner)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let mut inner = self.inner.clone();
        let mut conf = self.conf.clone();
        Box::pin(async move {
            let permit = match conf.start().await {
                Ok(permit) => permit,
                Err(_) => return Err(LoadShedError::QueueFull),
            };
            let start = Instant::now();
            let response = inner.call(req).await;
            conf.stop(start.elapsed(), permit);
            Ok(response?)
        })
    }
}
