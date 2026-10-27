use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
struct Call {
    method: String,
    target: String,
    length: usize,
    conditional: bool,
}
fn server(count: usize) -> (S3, Arc<Mutex<Vec<Call>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let saved = calls.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut input = BufReader::new(&mut socket);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap().to_string();
            let target = parts.next().unwrap().to_string();
            let mut length = 0;
            let mut conditional = false;
            loop {
                line.clear();
                input.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("if-none-match") && value.trim() == "*" {
                        conditional = true;
                    }
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut remaining = length;
            let mut block = [0u8; 65536];
            while remaining > 0 {
                let size = remaining.min(block.len());
                input.read_exact(&mut block[..size]).unwrap();
                remaining -= size;
            }
            saved.lock().unwrap().push(Call { method: method.clone(), target: target.clone(), length, conditional });
            let body = if method == "POST" && target.contains("uploads=") {
                "<InitiateMultipartUploadResult><UploadId>test-upload</UploadId></InitiateMultipartUploadResult>"
            } else if method == "POST" {
                "<CompleteMultipartUploadResult><ETag>test-etag</ETag></CompleteMultipartUploadResult>"
            } else {
                ""
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"part-etag\"\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let s3 = S3::new(
        "bucket",
        "us-east-1",
        Credentials { key_id: "test".into(), secret: "test-secret".into(), token: None },
        Some(&endpoint),
    )
    .unwrap();
    (s3, calls, worker)
}
#[test]
fn stream_uses_eight_mib_parts_and_conditional_completion() {
    let (s3, calls, worker) = server(4);
    let size = (8 << 20) + 7;
    assert!(s3.put_stream_new("files/test.age", &mut std::io::repeat(7).take(size), size, size).unwrap());
    worker.join().unwrap();
    let calls = calls.lock().unwrap();
    let parts: Vec<_> = calls.iter().filter(|c| c.method == "PUT").collect();
    assert_eq!(parts.iter().map(|c| c.length).collect::<Vec<_>>(), [8 << 20, 7]);
    assert!(calls.last().unwrap().target.contains("uploadId=test-upload"));
    assert!(calls.last().unwrap().conditional, "completion must never replace an existing object");
}
#[test]
fn overflow_aborts_and_declared_oversize_never_starts_upload() {
    let (s3, calls, worker) = server(2);
    assert!(s3.put_stream_new("files/test.age", &mut std::io::repeat(1).take(17), 16, 16).is_err());
    worker.join().unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.last().unwrap().method, "DELETE");
    assert!(!calls.iter().any(|c| c.method == "PUT"));
    assert!(s3.put_stream_new("files/test.age", &mut std::io::empty(), 17, 16).is_err());
}
#[test]
fn disconnect_after_a_part_aborts_without_completing() {
    struct Disconnected(u64);
    impl Read for Disconnected {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "test disconnect"));
            }
            let n = output.len().min(self.0 as usize);
            output[..n].fill(8);
            self.0 -= n as u64;
            Ok(n)
        }
    }
    let (s3, calls, worker) = server(3);
    assert!(s3.put_stream_new("files/test.age", &mut Disconnected(8 << 20), 16 << 20, 16 << 20).is_err());
    worker.join().unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls[1].method, "PUT");
    assert_eq!(calls[2].method, "DELETE");
    assert!(!calls.iter().any(|c| c.method == "POST" && c.target.contains("uploadId=")));
}
