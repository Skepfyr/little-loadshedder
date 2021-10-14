use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{future::BoxFuture, FutureExt};
use hyper::{service::service_fn, Body, Request, Response, Server};
use metrics_exporter_prometheus::PrometheusBuilder;
use tower::{make::Shared, Service};
use underload::{LoadShed, LoadShedError};

#[tokio::main]
async fn main() {
    // We'll bind to 127.0.0.1:8080
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    PrometheusBuilder::new()
        .set_buckets(&[0.0, 0.05, 0.1, 0.15, 0.20, 0.25, 0.30])
        .install()
        .unwrap();

    let service = LinearService::default();
    let mut service = LoadShed::new(service, 0.01, Duration::from_millis(2000));
    let service = service_fn(move |req| {
        let resp = service.call(req);
        async move {
            match resp.await {
                Ok(resp) => Ok(resp),
                Err(LoadShedError::Inner(inner)) => match inner {},
                Err(LoadShedError::Overload) => Response::builder().status(503).body(Body::empty()),
            }
        }
    });

    let server = Server::bind(&addr).serve(Shared::new(service));

    // Run this server for... forever!
    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

#[derive(Debug, Default, Clone)]
struct LinearService {
    inflight: Arc<AtomicU64>,
    average: Arc<Mutex<f64>>,
}

impl Service<Request<Body>> for LinearService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        let inflight = self.inflight.clone();
        let average = self.average.clone();
        async move {
            let count = inflight.fetch_add(1, Ordering::AcqRel) + 1;
            let sleep = {
                let mut average = average.lock().unwrap();
                *average = *average * 0.5 + count as f64 * 0.5;
                Duration::from_secs_f64((100.0 + *average * *average) / 1000.0)
            };
            tokio::time::sleep(sleep).await;
            inflight.fetch_sub(1, Ordering::AcqRel);
            Ok(Response::new(format!("Hello, World {}", count).into()))
        }
        .boxed()
    }
}
