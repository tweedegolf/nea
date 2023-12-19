use std::{
    future::Future,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use tokio::net::{TcpListener, TcpStream};

use crate::{error::Result, request::RequestBuf, response::Response};

const HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const PORT: u16 = 8000;

const ADDRESS: SocketAddr = SocketAddr::new(HOST, PORT);

pub async fn serve<F, Fut>(handler: F) -> Result<()>
where
    F: Fn(RequestBuf) -> Fut,
    Fut: Future<Output = Response>,
{
    let listener = TcpListener::bind(ADDRESS).await?;

    worker(&listener, handler).await?;

    Ok(())
}

async fn worker<F, Fut>(listener: &TcpListener, mut handler: F) -> Result<()>
where
    F: FnMut(RequestBuf) -> Fut,
    Fut: Future<Output = Response>,
{
    while let Ok((stream, _addr)) = listener.accept().await {
        // Use a buffer initialized with zeroes
        let mut buf = [0; 1024];

        // Fill buffer with request
        read_request(&stream, &mut buf).await?;

        // Parse request
        let request = std::str::from_utf8(&buf)?;
        let request = RequestBuf::new(request.to_owned());

        // Handle request
        let response = handler(request).await;

        // Return response
        write_response(&stream, response.to_string().as_bytes()).await?;
    }

    Ok(())
}

async fn read_request(stream: &TcpStream, buf: &mut [u8]) -> Result<()> {
    let mut bytes_read = 0;

    loop {
        stream.readable().await?;

        match stream.try_read(&mut buf[bytes_read..]) {
            Ok(0) => return Ok(()),
            Ok(n) => bytes_read += n,
            // FIXME: This returns Ok because curl doesn't close the connection after writing,
            //  resulting in an infinite loop. The benchmark requests will probably finish
            //  before blocking anyway, so it's not a big problem for now.
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

async fn write_response(stream: &TcpStream, response: &[u8]) -> Result<()> {
    let mut bytes_written = 0;

    loop {
        stream.writable().await?;

        match stream.try_write(response) {
            Ok(n) => {
                bytes_written += n;
                if bytes_written == response.len() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
