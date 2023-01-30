//! A load-shedding middleware based on [Little's law].
//!
//! This provides middleware for shedding load to maintain a target average
//! latency, see the documentation on the [`LoadShed`] service for more detail.
//!
//! The `metrics` feature uses the [metrics] crate to provide insight into the
//! current queue sizes and measured latency.
//!
//! [Little's law]: https://en.wikipedia.org/wiki/Little%27s_law
//! [metrics]: https://docs.rs/metrics/latest/metrics

#![doc(html_root_url = "https://docs.rs/little-loadshedder/0.1.0")]
#![warn(missing_debug_implementations, missing_docs, non_ascii_idents)]
#![forbid(unsafe_code)]

use std::{
    cmp::Ordering,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[cfg(feature = "metrics")]
use metrics::{decrement_gauge, gauge, histogram, increment_counter, increment_gauge};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tower::{Layer, Service, ServiceExt};

/// Load Shed service's current state of the world
#[derive(Debug, Clone)]
struct LoadShedConf {
    /// The target average latency in seconds.
    target: f64,
    /// The exponentially weighted moving average parameter.
    /// Must be in the range (0, 1), `0.25` means new value accounts for 25% of
    /// the moving average.
    ewma_param: f64,
    /// Semaphore controlling the waiting queue of requests.
    available_queue: Arc<Semaphore>,
    /// Semaphore controlling concurrency to the inner service.
    available_concurrency: Arc<Semaphore>,
    /// Stats about the latency that change with each completed request.
    stats: Arc<Mutex<ConfStats>>,
}

#[derive(Debug)]
struct ConfStats {
    /// The current average latency in seconds.
    average_latency: f64,
    /// The average of the latency measured when
    /// `available_concurrent.available_permits() == 0`.
    average_latency_at_capacity: f64,
    /// The number of available permits in the queue semaphore
    /// (the current capacity of the queue).
    queue_capacity: usize,
    /// The number of permits in the available_concurrency semaphore.
    concurrency: usize,
    /// The value of `self.concurrency` before it was last changed.
    previous_concurrency: usize,
    /// The time that the concurrency was last adjusted, to rate limit changing it.
    last_changed: Instant,
    /// Average throughput when at the previous concurrency value.
    previous_throughput: f64,
}

// size of system [req] = target latency [s] * throughput [r/s]
// size of queue [req] = size of system [req] - concurrency [req]
// throughput [req/s] = concurrency [req] / average latency of service [s]
// => (size of queue [req] + concurrency[req]) = target latency [s] * concurrency[req] / latency [s]
// => size of queue [req] = concurrency [req] * (target latency [s] / latency [s] - 1)
//
// Control the concurrency:
// increase concurrency but not beyond target latency
//
// Control queue length:
// queue capacity = concurrency * ((target latency / average latency of service) - 1)

impl LoadShedConf {
    fn new(ewma_param: f64, target: f64) -> Self {
        #[cfg(feature = "metrics")]
        {
            gauge!("loadshedder.capacity", 1.0, "component" => "service");
            gauge!("loadshedder.capacity", 1.0, "component" => "queue");
            gauge!("loadshedder.size", 0.0, "component" => "service");
            gauge!("loadshedder.size", 0.0, "component" => "queue");
            gauge!("loadshedder.average_latency", target);
        }
        Self {
            target,
            ewma_param,
            available_concurrency: Arc::new(Semaphore::new(1)),
            available_queue: Arc::new(Semaphore::new(1)),
            stats: Arc::new(Mutex::new(ConfStats {
                average_latency: target,
                average_latency_at_capacity: target,
                queue_capacity: 1,
                concurrency: 1,
                previous_concurrency: 0,
                last_changed: Instant::now(),
                previous_throughput: 0.0,
            })),
        }
    }

    /// Add ourselves to the queue and wait until we've made it through and have
    /// obtained a permit to send the request.
    async fn start(&self) -> Result<Permit, ()> {
        {
            // Work inside a block so we drop the stats lock asap.
            let mut stats = self.stats.lock().unwrap();
            let desired_queue_capacity = usize::max(
                1, // The queue must always be at least 1 request long.
                // Use average latency at (concurrency) capacity so that this doesn't
                // grow too large while the system is under-utilised.
                (stats.concurrency as f64
                    * ((self.target / stats.average_latency_at_capacity) - 1.0))
                    .floor() as usize,
            );
            #[cfg(feature = "metrics")]
            gauge!("loadshedder.capacity", desired_queue_capacity as f64, "component" => "queue");

            // Adjust the semaphore capacity by adding or acquiring many permits.
            // If acquiring permits fails we can return overload and let the next
            // request recompute the queue capacity.
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

        // Finally get our queue permit, if this fails then the queue is full
        // and we need to bail out.
        let queue_permit = match self.available_queue.clone().try_acquire_owned() {
            Ok(queue_permit) => Permit::new(queue_permit, "queue"),
            Err(TryAcquireError::NoPermits) => return Err(()),
            Err(TryAcquireError::Closed) => panic!("queue semaphore closed?"),
        };
        // We're in the queue now so wait until we get ourselves a concurrency permit.
        let concurrency_permit = self
            .available_concurrency
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        // Now we've got the permit required to send the request we can leave the queue.
        drop(queue_permit);
        Ok(Permit::new(concurrency_permit, "service"))
    }

    /// Register a completed call of the inner service, providing the latency to
    /// update the statistics.
    fn stop(&mut self, elapsed: Duration, concurrency_permit: Permit) {
        let elapsed = elapsed.as_secs_f64();
        #[cfg(feature = "metrics")]
        histogram!("loadshedder.latency", elapsed);

        // This function solely updates the stats (and is not async) so hold the
        // lock for the entire function.
        let mut stats = self.stats.lock().expect("To be able to lock stats");

        let available_permits = self.available_concurrency.available_permits();
        // Have some leeway on what "at max concurrency" means as you might
        // otherwise never see this condition at large concurrency values.
        let at_max_concurrency = available_permits <= usize::max(1, stats.concurrency / 10);

        // Update the average latency using the EWMA algorithm.
        stats.average_latency =
            (stats.average_latency * (1.0 - self.ewma_param)) + (self.ewma_param * elapsed);
        #[cfg(feature = "metrics")]
        gauge!("loadshedder.average_latency", stats.moving_average);
        if at_max_concurrency {
            stats.average_latency_at_capacity = (stats.average_latency_at_capacity
                * (1.0 - self.ewma_param))
                + (self.ewma_param * elapsed);
        }

        // Only ever change max concurrency if we're at the limit as we need
        // measurements to have happened at the current limit.
        // Also, introduce a max rate of change that's somewhat magically
        // related to the latency and ewma parameter to prevent this from
        // changing too quickly.
        if stats.last_changed.elapsed().as_secs_f64()
            > (stats.average_latency / self.ewma_param) / 10.0
            && at_max_concurrency
        {
            // Plausibly should be using average latency at capacity here and
            // stats.concurrency but this appears to work. It might do weird
            // things if it's been running under capacity for a while then spikes.
            let current_concurrency = stats.concurrency - available_permits;
            let throughput = current_concurrency as f64 / stats.average_latency;
            // Was the throughput better or worse than it was previously.
            let negative_gradient = (throughput > stats.previous_throughput)
                ^ (current_concurrency > stats.previous_concurrency);
            if negative_gradient || (stats.average_latency > self.target) {
                // Don't reduce concurrency below 1 or everything stops.
                if stats.concurrency > 1 {
                    // negative gradient so decrease concurrency
                    concurrency_permit.forget();
                    stats.concurrency -= 1;
                    #[cfg(feature = "metrics")]
                    gauge!("loadshedder.capacity", stats.concurrency as f64, "component" => "service");

                    // Adjust the average latency assuming that the change in
                    // concurrency doesn't affect the service latency, which is
                    // closer to the truth than the latency not changing.
                    let latency_factor =
                        stats.concurrency as f64 / (stats.concurrency as f64 + 1.0);
                    stats.average_latency *= latency_factor;
                    stats.average_latency_at_capacity *= latency_factor;
                }
            } else {
                self.available_concurrency.add_permits(1);
                stats.concurrency += 1;
                #[cfg(feature = "metrics")]
                gauge!("loadshedder.capacity", stats.concurrency as f64, "component" => "service");

                // Adjust the average latency assuming that the change in
                // concurrency doesn't affect the service latency, which is
                // closer to the truth than the latency not changing.
                let latency_factor = stats.concurrency as f64 / (stats.concurrency as f64 - 1.0);
                stats.average_latency *= latency_factor;
                stats.average_latency_at_capacity *= latency_factor;
            }

            stats.previous_throughput = throughput;
            stats.previous_concurrency = current_concurrency;
            stats.last_changed = Instant::now()
        }
    }
}

/// A permit for something, this is used for updating metrics.
#[derive(Debug)]
struct Permit {
    /// The permit, this is only optional to enable the forget function.
    permit: Option<OwnedSemaphorePermit>,
    /// The name of the component this permit is for, used as a metric label.
    #[allow(unused)]
    component: &'static str,
}

impl Permit {
    /// Create a new permit for the given component.
    fn new(permit: OwnedSemaphorePermit, component: &'static str) -> Self {
        #[cfg(feature = "metrics")]
        increment_gauge!("loadshedder.size", 1.0, "component" => component);
        Self {
            permit: Some(permit),
            component,
        }
    }

    /// Forget the permit, essentially reducing the size of the semaphore by one.
    /// Note this does still decrement the size metric.
    fn forget(mut self) {
        self.permit.take().unwrap().forget()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        #[cfg(feature = "metrics")]
        decrement_gauge!("loadshedder.size", 1.0, "component" => self.component);
    }
}

/// A [`Service`] that attempts to hold the average latency at a given target.
///
/// It does this by placing a queue in front of the service and rejecting
/// requests when that queue is full (this means requests are either immediately
/// rejected or will be processed by the inner service). It calculates the size
/// of that queue using [Little's Law] which states that the average number of
/// items in a system is equal to the average throughput multiplied by the
/// average latency.
///
/// This service therefore measures the average latency and sets the queue size
/// such that when the queue is full a request will on average take the target
/// latency time to be responded to. Note that if the queue is not full the
/// latency will be below the target.
///
/// This service also optimises the number concurrent requests to the service.
/// This will usually be the same as the queue size, unless the target latency
/// (and hence the queue) is very large, or the underlying service can cope with
/// very few concurrent requests.
///
/// This service is reactive, if the underlying service degrades then the queues
/// will shorten, if it improves they will lengthen. The queue lengths will be
/// underestimates at startup and will only increase while the service is near
/// its concurrency limit. Be wary of using the queue length as a measure of
/// system capacity unless the queues have been at or above the concurrency for
/// a while.
///
/// [Little's law]: https://en.wikipedia.org/wiki/Little%27s_law
#[derive(Debug, Clone)]
pub struct LoadShed<Inner> {
    conf: LoadShedConf,
    inner: Inner,
}

impl<Inner> LoadShed<Inner> {
    /// Wrap a service with this middleware, using the given target average
    /// latency and computing the current average latency using an exponentially
    /// weighted moving average with the given parameter.
    pub fn new(inner: Inner, ewma_param: f64, target: Duration) -> Self {
        Self {
            inner,
            conf: LoadShedConf::new(ewma_param, target.as_secs_f64()),
        }
    }

    /// The current average latency of requests through the inner service,
    /// that is ignoring the queue this service adds.
    pub fn average_latency(&self) -> Duration {
        Duration::from_secs_f64(self.conf.stats.lock().unwrap().average_latency)
    }

    /// The current maximum concurrency of requests to the inner service.
    pub fn concurrency(&self) -> usize {
        self.conf.stats.lock().unwrap().concurrency
    }

    /// The current maximum capacity of this service (including the queue).
    pub fn queue_capacity(&self) -> usize {
        let stats = self.conf.stats.lock().unwrap();
        stats.concurrency + stats.queue_capacity
    }

    /// The current number of requests that have been accepted by this service.
    pub fn queue_len(&self) -> usize {
        let stats = self.conf.stats.lock().unwrap();
        let current_concurrency =
            stats.concurrency - self.conf.available_concurrency.available_permits();
        let current_queue = stats.queue_capacity - self.conf.available_queue.available_permits();
        current_concurrency + current_queue
    }
}

/// Either an error from the wrapped service or message that the request was shed
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LoadShedError<T> {
    /// An error generated by the service this middleware is wrapping.
    #[error("Inner service error")]
    Inner(#[from] T),
    ///
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

    /// Always ready because there's a queue between this service and the inner one.
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // We're fine to use the clone because inner hasn't been polled to
        // readiness yet.
        let inner = self.inner.clone();
        let mut conf = self.conf.clone();
        Box::pin(async move {
            let permit = match conf.start().await {
                Ok(permit) => {
                    #[cfg(feature = "metrics")]
                    increment_counter!("loadshedder.request", "status" => "accepted");
                    permit
                }
                Err(()) => {
                    #[cfg(feature = "metrics")]
                    increment_counter!("loadshedder.request", "status" => "rejected");
                    return Err(LoadShedError::Overload);
                }
            };
            let start = Instant::now();
            // The elapsed time includes waiting for readiness which should help
            // us stay under any upstream concurrency limiters.
            let response = inner.oneshot(req).await;
            conf.stop(start.elapsed(), permit);
            Ok(response?)
        })
    }
}

/// A [`Layer`] to wrap services in a [`LoadShed`] middleware.
///
/// See [`LoadShed`] for details of the load shedding algorithm.
#[derive(Debug, Clone)]
pub struct LoadShedLayer {
    ewma_param: f64,
    target: Duration,
}

impl LoadShedLayer {
    /// Create a new layer with the given target average latency and
    /// computing the current average latency using an exponentially weighted
    /// moving average with the given parameter.
    pub fn new(ewma_param: f64, target: Duration) -> Self {
        Self { ewma_param, target }
    }
}

impl<Inner> Layer<Inner> for LoadShedLayer {
    type Service = LoadShed<Inner>;

    fn layer(&self, inner: Inner) -> Self::Service {
        LoadShed::new(inner, self.ewma_param, self.target)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn html_root_url_version() {
        version_sync::assert_html_root_url_updated!("src/lib.rs");
    }
}
