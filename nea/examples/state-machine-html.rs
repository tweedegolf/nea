use std::io::Cursor;

fn main() -> std::io::Result<()> {
    nea::run_request_handler(handler)
}

#[derive(Debug)]
enum Cmd {
    Read,
    Write(Vec<u8>),
    Done,
}

#[derive(Debug)]
enum Msg {
    Nothing,
    Read(Vec<u8>),
}

#[derive(Debug)]
enum Model {
    Initial,
    Writing,
}

fn roc_handler(model: Model, msg: Msg) -> (Model, Cmd) {
    match (model, msg) {
        (Model::Initial, Msg::Nothing) => {
            // let's read something
            (Model::Initial, Cmd::Read)
        }
        (Model::Initial, Msg::Read(buf)) => {
            //
            drop(buf);

            use std::io::Write;
            let input = "foobar";
            let mut buffer = [0u8; 1024];
            let mut response = Cursor::new(&mut buffer[..]);
            let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
            let _ = response.write_all(b"Content-Type: text/html\r\n");
            let _ = response.write_all(b"\r\n");
            let _ = response.write_fmt(format_args!("<html>{input}</html>"));
            let n = response.position() as usize;

            let buf = response.get_ref()[..n].to_vec();

            (Model::Writing, Cmd::Write(buf))
        }
        (Model::Writing, _) => (Model::Writing, Cmd::Done),
    }
}

async fn handler(
    _bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut msg = Msg::Nothing;
    let mut model = Model::Initial;

    loop {
        let (new_model, cmd) = roc_handler(model, msg);
        model = new_model;

        match cmd {
            Cmd::Read => {
                let mut buf = vec![0u8; 1024];
                let n = tcp_stream.read(&mut buf).await?;

                buf.truncate(n);
                msg = Msg::Read(buf);
            }
            Cmd::Write(buf) => {
                let mut remaining = buf.len();
                while remaining > 0 {
                    remaining -= tcp_stream.write(&buf).await?;
                }

                msg = Msg::Nothing;
            }
            Cmd::Done => break,
        }
    }

    Ok(())
}
