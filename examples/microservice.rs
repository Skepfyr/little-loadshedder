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

use dialoguer::{theme::ColorfulTheme, Input};
use futures::{future::BoxFuture, FutureExt};
use hyper::{service::service_fn, Body, Request, Response, Server};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{
    select,
    sync::watch::{channel, Receiver},
    task::spawn_blocking,
};
use tower::{make::Shared, Service};
use underload::{LoadShed, LoadShedError};

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    PrometheusBuilder::new()
        .set_buckets(&[0.0, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0])
        .install()
        .unwrap();

    let (multiplier_tx, multiplier_rx) = channel(1.0);
    let service = LinearService::new(multiplier_rx);
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
    let user_input = spawn_blocking(move || loop {
        multiplier_tx
            .send(
                Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Multiplier:")
                    .interact_text()
                    .unwrap(),
            )
            .unwrap();
    });

    select! {
        Err(e) = server => eprintln!("server error: {}", e),
        _ = user_input => {}
    }
}

#[derive(Debug, Clone)]
struct LinearService {
    inflight: Arc<AtomicU64>,
    average: Arc<Mutex<f64>>,
    multiplier: Receiver<f64>,
}

impl LinearService {
    fn new(multiplier: Receiver<f64>) -> Self {
        Self {
            inflight: Arc::new(AtomicU64::new(0)),
            average: Arc::new(Mutex::new(0.0)),
            multiplier,
        }
    }
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
        let multiplier = *self.multiplier.borrow();
        async move {
            let count = inflight.fetch_add(1, Ordering::AcqRel) + 1;
            let sleep = {
                let mut average = average.lock().unwrap();
                *average = *average * 0.95 + count as f64 * 0.05;
                Duration::from_secs_f64(multiplier * (100.0 + *average * *average) / 1000.0)
            };
            tokio::time::sleep(sleep).await;
            inflight.fetch_sub(1, Ordering::AcqRel);
            Ok(Response::new(format!("Hello, World {}", count).into()))
        }
        .boxed()
    }
}
