use std::{
    convert::Infallible,
    net::SocketAddr,
    task::{Context, Poll},
    time::Duration,
};

use futures::{future::BoxFuture, FutureExt};
use hyper::{Body, Request, Response, Server};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::task::spawn_blocking;
use tower::{make::Shared, Service};
use underload::LoadShed;

#[tokio::main]
async fn main() {
    // We'll bind to 127.0.0.1:8080
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));

    PrometheusBuilder::new()
        .set_buckets(&[0.0, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0])
        .install()
        .unwrap();

    let service = LinearService::default();
    let service = LoadShed::new(service, 0.05, Duration::from_millis(2000));

    let server = Server::bind(&addr).serve(Shared::new(service));

    // Run this server for... forever!
    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

#[derive(Debug, Default, Clone)]
struct LinearService;

impl Service<Request<Body>> for LinearService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        async move {
            let rand = spawn_blocking(|| {
                let mut rand: f64 = 0.0;
                for _ in 0..100000 {
                    rand += rand::random::<f64>();
                }
                rand
            })
            .await
            .unwrap();
            //tokio::time::sleep(Duration::from_millis(1000)).await;
            Ok(Response::new(format!("Hello, World {}", rand).into()))
        }
        .boxed()
    }
}
