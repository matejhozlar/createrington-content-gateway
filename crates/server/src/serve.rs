use std::{future::IntoFuture, time::Duration};

use axum::Router;
use tokio::{net::TcpListener, sync::oneshot, task::JoinError};

#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    Drained,
    GraceExpired,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("server stopped: {0}")]
    Io(#[from] std::io::Error),
    #[error("server task failed: {0}")]
    Task(#[from] JoinError),
}

pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> Result<Stopped, ServeError> {
    let (stop, stopped) = oneshot::channel::<()>();
    let mut server = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                stopped.await.ok();
            })
            .into_future(),
    );

    tokio::select! {
        result = &mut server => {
            result??;
            return Ok(Stopped::Drained);
        }
        () = shutdown => {}
    }

    stop.send(()).ok();
    match tokio::time::timeout(grace, &mut server).await {
        Ok(result) => {
            result??;
            Ok(Stopped::Drained)
        }
        Err(_) => {
            server.abort();
            Ok(Stopped::GraceExpired)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use axum::routing::get;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use super::*;

    const GRACE: Duration = Duration::from_millis(300);

    async fn start(
        router: Router,
    ) -> (
        std::net::SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<Stopped, ServeError>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(
            listener,
            router,
            async {
                signal.await.ok();
            },
            GRACE,
        ));
        (address, shutdown, task)
    }

    async fn send(address: std::net::SocketAddr, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes())
            .await
            .unwrap();
        stream
    }

    #[tokio::test]
    async fn stops_at_once_when_nothing_is_in_flight() {
        let (address, shutdown, task) =
            start(Router::new().route("/", get(|| async { "ok" }))).await;
        let mut stream = send(address, "/").await;
        let mut answer = [0u8; 12];
        stream.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"HTTP/1.1 200");
        drop(stream);

        let started = Instant::now();
        shutdown.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), Stopped::Drained);
        assert!(started.elapsed() < GRACE);
    }

    #[tokio::test]
    async fn gives_up_on_a_request_that_never_finishes() {
        let hanging = Router::new().route("/hang", get(std::future::pending::<&'static str>));
        let (address, shutdown, task) = start(hanging).await;
        let _stream = send(address, "/hang").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        shutdown.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap(), Stopped::GraceExpired);
        assert!(started.elapsed() >= GRACE);
        assert!(started.elapsed() < GRACE * 3);
    }
}
