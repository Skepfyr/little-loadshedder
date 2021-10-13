use std::{
    convert::Infallible,
    net::SocketAddr,
    task::{Context, Poll},
    time::Duration,
};

use futures::{future::BoxFuture, FutureExt};
use hyper::{Body, Request, Response, Server};
use tokio::task::spawn_blocking;
use tower::{make::Shared, Service};
use underload::LoadShed;

#[tokio::main]
async fn main() {
    // We'll bind to 127.0.0.1:3000
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

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
                for _ in 0..10000 {
                    rand += rand::random::<f64>();
                }
                rand
            })
            .await
            .unwrap();
            Ok(Response::new(
                format!("Hello, World - rand is {}", rand).into(),
            ))
        }
        .boxed()
    }
}
