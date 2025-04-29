#![allow(unused_imports)]

use std::sync::atomic::{AtomicI32, Ordering};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use bytes::{BytesMut, BufMut};
use serde::Serialize;

use tokio::io::AsyncReadExt;
use tokio_kcp::{KcpConfig, KcpStream};


const CONCURRENT_REQUESTS: usize = 10;
const TOTAL_REQUESTS: usize = 1000;

fn parse_null_terminated(bytes: &[u8]) -> &str {
    // 找到第一个 `0` 的位置，截取之前的部分
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap() // 安全：已知有效 UTF-8
}

#[tokio::test(flavor = "multi_thread")]
async fn load_test_kcp() {
    let start = Instant::now();
    let mut handles = vec![];
    
    let rpc_id = Arc::new(AtomicI32::new(0));
    for _ in 0..CONCURRENT_REQUESTS {


        //let rpc_id = rpc_id.clone();

        handles.push(tokio::spawn(async move {

            let config = tokio_kcp::KcpConfig {
                mtu: 470,
                nodelay: tokio_kcp::KcpNoDelayConfig{
                    nodelay: true,
                    interval: 10,
                    resend: 2,
                    nc: false,
                },

                wnd_size: (32, 32),
                session_expire: std::time::Duration::from_secs(90),
                flush_write: true,
                flush_acks_input: false,
                stream: false,
                allow_recv_empty_packet: false,
            };

            let server_addr = "127.0.0.1:3100".parse::<SocketAddr>().unwrap();
            let mut writer = KcpStream::connect(&config, server_addr).await.unwrap();

            writer.write_all(b"Sync|").await.unwrap();

            let mut buffer = [0u8; 128];
            writer.read(&mut buffer).await.unwrap();
            let prefix = b"SyncAck|";
            println!("收到SyncAck|消息，正在解析{}", std::str::from_utf8(&buffer).unwrap());
            if buffer.starts_with(prefix) {

                let client_id_string = parse_null_terminated(&buffer[prefix.len()..]);
                println!("解析SyncAck|消息成功,client_id:{:?},准备链接3101", client_id_string.as_bytes());

                let client_id = client_id_string.parse::<usize>().unwrap();

                println!("解析SyncAck|消息成功,client_id{},准备链接3101", client_id);

                let server_addr = "127.0.0.1:3101".parse::<SocketAddr>().unwrap();
                let mut reader = KcpStream::connect(&config, server_addr).await.unwrap();

                let msg = format!("Ack|{}", client_id);
                println!("正在准备发送Ack|: {}", msg);
                reader.write_all(msg.as_bytes()).await.unwrap();

                tokio::time::sleep(Duration::from_secs(1)).await;

                println!("发送Ack|: 成功，准备发送C2M_PingRequest");
                let mut ping_msg = Vec::new();
                TiangZ::C2M_PingRequest{rpc_id:1, _t: "C2M_PingRequest".to_string()}
                    .serialize(&mut rmp_serde::Serializer::new(&mut ping_msg).with_struct_map()).unwrap();

                let mut buf = BytesMut::with_capacity(2 + ping_msg.len());
                buf.put_u16(ping_msg.len() as u16);
                buf.put_slice(&ping_msg);

                //不停的写

                for _ in 0..TOTAL_REQUESTS/CONCURRENT_REQUESTS {
                    writer.write_all(&buf).await.unwrap();
                    let mut buffer = vec![0; 128];
                    reader.read(&mut buffer).await.unwrap();                 
                }
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