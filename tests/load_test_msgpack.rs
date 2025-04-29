use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use bytes::{BytesMut, BufMut};
use std::sync::atomic::AtomicI32;
use std::time::Instant;
use serde::Serialize;

use std::sync::{Arc, atomic::Ordering};

const CONCURRENT_REQUESTS: usize = 128;
const TOTAL_REQUESTS: usize = 1280000;

#[tokio::test(flavor = "multi_thread")]
async fn load_test_msgpack() {
    let start = Instant::now();
    let mut handles = vec![];
    
    let rpc_id = Arc::new(AtomicI32::new(0));
    for _ in 0..CONCURRENT_REQUESTS {


        let rpc_id = rpc_id.clone();

        handles.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect("127.0.0.1:8080").await.unwrap();

            let rpc_id = rpc_id.fetch_add(1, Ordering::SeqCst);

            let mut ping_msg = Vec::new();
            TiangZ::C2M_PingRequest{rpc_id, _t: "C2M_PingRequest".to_string()}
                .serialize(&mut rmp_serde::Serializer::new(&mut ping_msg).with_struct_map()).unwrap();

            let mut buf = BytesMut::with_capacity(2 + ping_msg.len());
            buf.put_u16(ping_msg.len() as u16);
            buf.put_slice(&ping_msg);

            for _ in 0..TOTAL_REQUESTS/CONCURRENT_REQUESTS {
                stream.write_all(&buf).await.unwrap();
                
                let mut response = vec![0; 128];
                let _ = stream.read(&mut response).await.unwrap();
            }
        }));
    }
    
    for handle in handles {
        handle.await.unwrap();
    }
    
    let duration = start.elapsed();
    println!(
        "Completed {} requests in {:?} ({:.2} req/s, rpc_id: {:?})",
        TOTAL_REQUESTS,
        duration,
        TOTAL_REQUESTS as f64 / duration.as_secs_f64(),
        rpc_id
    );
}