use std::future::Future;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::{io, net};

use crate::config::Config;
use crate::reactor::{Reactor, TcpStream};
use crate::{BucketIndex, Executor, QueueIndex};

pub struct Nea<H> {
    handler: H,
}

impl<H, Fut> Nea<H>
where
    H: Copy + Fn(BucketIndex, TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = io::Result<()>> + Send,
{
    pub fn new(handler: H) -> Self {
        Nea { handler }
    }

    pub fn run(self) -> io::Result<()> {
        log::init();

        let config = Config::load();

        let executor = Executor::get_or_init(config.bucket_count, config.io_resources);
        let reactor = Reactor::get_or_init(config.bucket_count, config.io_resources).unwrap();

        let _handle1 = executor.spawn_worker().unwrap();
        let _handle2 = executor.spawn_worker().unwrap();

        let addr = format!("{}:{}", config.host, config.port);
        let listener = net::TcpListener::bind(&addr)?;
        let id = std::thread::current().id();
        println!("listening on http://{addr} on thread {id:?}");

        crate::IS_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);

        // accept connections and process them serially
        for (i, stream) in listener.incoming().enumerate() {
            let stream = stream.unwrap();
            stream.set_nonblocking(true).unwrap();
            let fd = stream.as_raw_fd();

            match executor.try_claim() {
                None => {
                    // no space in the queue
                    log::warn!("main: no space in the queue");
                    stream.shutdown(net::Shutdown::Both).unwrap();
                }
                Some(bucket_index) => {
                    executor.execute(bucket_index, async move {
                        log::info!(
                            "new connection {i} (index = {}, fd = {fd})",
                            bucket_index.index
                        );

                        let queue_index =
                            QueueIndex::from_bucket_index(config.io_resources, bucket_index);
                        let tcp_stream = reactor.register(queue_index, stream).unwrap();

                        match (self.handler)(bucket_index, tcp_stream).await {
                            Ok(()) => {}
                            Err(e) => match e.kind() {
                                ErrorKind::NotConnected => {}
                                _ => Err(e).unwrap(),
                            },
                        }
                    });
                }
            };

            log::info!("main spawned future for connection {i}");
        }

        Ok(())
    }
}
