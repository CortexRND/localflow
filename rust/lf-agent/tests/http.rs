use lf_agent::{ChatClient, OllamaClient, MAX_RESPONSE_BYTES};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

fn serve(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request);
        let _ = socket.write_all(&response);
    });
    (format!("http://{addr}"), handle)
}
#[test]
fn bounded_http_and_redirect_error_paths() {
    let client = OllamaClient::new().unwrap();
    let mut responses = vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"models\":[]}".to_vec(),
        b"HTTP/1.1 302 Found\r\nLocation: http://remote.invalid/\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\noops".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{}".to_vec(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes(),
    ];
    let mut unbounded = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
    unbounded.extend(vec![b' '; MAX_RESPONSE_BYTES + 1]);
    responses.push(unbounded);
    for (i, response) in responses.into_iter().enumerate() {
        let (url, handle) = serve(response);
        let result = client.tags(&url);
        handle.join().unwrap();
        assert_eq!(result.is_ok(), i == 0, "response {i}: {result:?}");
    }
    assert!(client.tags("http://remote.invalid").is_err());
}
