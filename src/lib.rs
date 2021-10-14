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

use metrics::{decrement_gauge, gauge, histogram, increment_counter, increment_gauge};
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
    concurrency: usize,
    /// When the concurrency was last adjusted, to rate limit changing it
    last_changed: Instant,
    /// current capacity of queue
    queue_capacity: usize,
    /// Exponential weighted average of latency ONLY when
    /// available_concurrent.available_permits() == 0
    average_latency_at_capacity: f64,

    /// Throughput @ last concurrency value
    previous_throughput: f64,
    previous_concurrency: usize,
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
                concurrency: 1,
                last_changed: Instant::now(),
                queue_capacity: 1,
                average_latency_at_capacity: target,
                previous_throughput: 0.0,
                previous_concurrency: 0,
            })),
        }
    }

    async fn start(&self) -> Result<Permit, ()> {
        {
            let mut stats = self.stats.lock().unwrap();
            let desired_queue_capacity = usize::max(
                1,
                (stats.concurrency as f64
                    * ((self.target / stats.average_latency_at_capacity) - 1.0))
                    .floor() as usize,
            );
            gauge!("underload.capacity", desired_queue_capacity as f64, "component" => "queue");
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

        let queue_permit = match self.available_queue.clone().try_acquire_owned() {
            Ok(queue_permit) => Permit::new(queue_permit, "queue"),
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
        Ok(Permit::new(concurrency_permit, "service"))
    }

    fn stop(&mut self, elapsed: Duration, concurrency_permit: Permit) {
        let elapsed = elapsed.as_secs_f64();
        histogram!("underload.latency", elapsed);
        let mut stats = self.stats.lock().expect("To be able to lock stats");
        stats.moving_average =
            (stats.moving_average * (1.0 - self.ewma_param)) + (self.ewma_param * elapsed);
        gauge!("underload.average_latency", stats.moving_average);
        let available_permits = self.available_concurrency.available_permits();

        if stats.last_changed.elapsed().as_secs_f64()
            > (stats.moving_average / self.ewma_param) / 10.0
            && available_permits == 0
        {
            let current_concurrency = stats.concurrency - available_permits;
            let throughput = current_concurrency as f64 / stats.moving_average;
            let negative_gradient = (throughput > stats.previous_throughput)
                ^ (current_concurrency > stats.previous_concurrency);
            if negative_gradient || (stats.moving_average > self.target) {
                if stats.concurrency >= 2 {
                    // negative gradient so decrease concurrency
                    concurrency_permit.forget();
                    stats.concurrency -= 1;
                    gauge!("underload.capacity", stats.concurrency as f64, "component" => "service");

                    let latency_factor =
                        stats.concurrency as f64 / (stats.concurrency as f64 + 1.0);
                    stats.moving_average *= latency_factor;
                    stats.average_latency_at_capacity *= latency_factor;
                }
            } else {
                self.available_concurrency.add_permits(1);
                stats.concurrency += 1;
                gauge!("underload.capacity", stats.concurrency as f64, "component" => "service");

                let latency_factor = stats.concurrency as f64 / (stats.concurrency as f64 - 1.0);
                stats.moving_average *= latency_factor;
                stats.average_latency_at_capacity *= latency_factor;
            }

            stats.previous_throughput = throughput;
            stats.previous_concurrency = current_concurrency;
            stats.last_changed = Instant::now()
        }
        if available_permits <= usize::max(1, stats.concurrency / 10) {
            stats.average_latency_at_capacity = (stats.average_latency_at_capacity
                * (1.0 - self.ewma_param))
                + (self.ewma_param * elapsed);
        }
    }
}

#[derive(Debug)]
struct Permit {
    permit: Option<OwnedSemaphorePermit>,
    component: &'static str,
}

impl Permit {
    fn new(permit: OwnedSemaphorePermit, component: &'static str) -> Self {
        increment_gauge!("underload.size", 1.0, "component" => component);
        Self {
            permit: Some(permit),
            component,
        }
    }

    fn forget(mut self) {
        self.permit.take().unwrap().forget()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        decrement_gauge!("underload.size", 1.0, "component" => self.component);
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
                Ok(permit) => {
                    increment_counter!("underload.request", "status" => "accepted");
                    permit
                }
                Err(_) => {
                    increment_counter!("underload.request", "status" => "rejected");
                    return Err(LoadShedError::QueueFull);
                }
            };
            let start = Instant::now();
            let response = inner.call(req).await;
            conf.stop(start.elapsed(), permit);
            Ok(response?)
        })
    }
}
