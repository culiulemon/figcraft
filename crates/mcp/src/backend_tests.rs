//! The attached backend reads the app's replies with a bound (#1113).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;

use super::{Attached, Backend};

/// A fake app that takes one connection per reply, reads one request on it and answers with the
/// reply. It keeps the connections open until [`finish`] (so an unterminated reply never reaches
/// EOF), then returns how many requests it read.
fn fake_app(replies: Vec<Vec<u8>>) -> (u16, mpsc::Sender<()>, std::thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done, wait) = mpsc::channel::<()>();
    let app = std::thread::spawn(move || {
        let mut open = Vec::new();
        for reply in &replies {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            out.write_all(reply).unwrap();
            out.flush().unwrap();
            open.push(out);
        }
        wait.recv_timeout(Duration::from_secs(30)).ok();
        open.len()
    });
    (port, done, app)
}

/// Let the fake app close its connections, and check it read `requests` requests.
fn finish(done: mpsc::Sender<()>, app: std::thread::JoinHandle<usize>, requests: usize) {
    drop(done);
    assert_eq!(app.join().unwrap(), requests);
}

fn ok_reply(text: &str) -> Vec<u8> {
    format!("{}\n", json!({"id": 1, "ok": true, "result": text})).into_bytes()
}

#[test]
fn an_oversized_reply_is_refused_and_the_next_call_reconnects() {
    let (port, done, fake) = fake_app(vec![vec![b'x'; 200], ok_reply("fine")]);
    let mut app = Attached::port(port).unwrap();
    app.max_reply = 64;
    let err = app.call("engine.commands", json!({})).unwrap_err();
    assert!(err.contains("reply larger than"), "{err}");
    assert!(err.contains("may have completed"), "{err}");
    assert!(app.conn.is_none());
    assert_eq!(app.call("engine.commands", json!({})).unwrap(), json!("fine"));
    finish(done, fake, 2);
}

#[test]
fn a_reply_at_the_limit_is_read() {
    let reply = ok_reply("at the limit");
    let (port, done, fake) = fake_app(vec![reply.clone()]);
    let mut app = Attached::port(port).unwrap();
    app.max_reply = reply.len() - 1; // the newline doesn't count
    assert_eq!(app.call("engine.commands", json!({})).unwrap(), json!("at the limit"));
    finish(done, fake, 1);
}

#[test]
fn a_reply_that_is_not_utf8_is_an_error() {
    let (port, done, fake) = fake_app(vec![b"{\"ok\": true, \"result\": \"\xff\"}\n".to_vec()]);
    let mut app = Attached::port(port).unwrap();
    let err = app.call("engine.commands", json!({})).unwrap_err();
    assert!(err.contains("failed"), "{err}");
    assert!(app.conn.is_none());
    finish(done, fake, 1);
}
