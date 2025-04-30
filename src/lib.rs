#![allow(unused_variables, unused_imports, non_snake_case, dead_code)]

use std::collections::HashMap;
use std::sync::{LazyLock, Arc, atomic::{AtomicUsize, Ordering}};
use std::io;
use std::future::pending;
use std::any::Any;
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, broadcast};
use tokio::signal;
use bytes::{BytesMut, Buf};
use dashmap::DashMap;
use tokio::time::{sleep, Duration};
use async_trait::async_trait;
use tracing::{trace, error, info, warn, debug};
use tokio_kcp::KcpStream;

use gen_macro::{IActorLocationRpcHandler, IActorLocationRpcRequest, IActorLocationRpcResponse, 
    IActorLocationMessageRequest, IActorLocationMessageHandler, Singleton};

mod errors;
mod struct_macro;
mod event;
mod my_future;
mod protocol_parser;
mod kcp_wrapper;

use crate::errors::my_errors::RetResult;
use crate::errors::my_errors::MyError;
use protocol_parser::ProtocolParser;
//////////////////////////////////////////////////////////////////////////////////////////////
type TcpListener = tokio::net::TcpListener;  

fn start_runtime(work_thread: usize) -> Arc<tokio::runtime::Runtime> {

    // let mut core_ids = core_affinity::get_core_ids().unwrap();
    // let core_ids = core_ids.split_off(1);

    // let thread_id = Arc::new(std::sync::Mutex::new(0 as usize));

    
    Arc::new(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(work_thread)
        // .on_thread_start(move || {
        //     // 将每个工作线程绑定到不同的物理核心
        //     let thread_id = Arc::clone(&thread_id);
        //     let mut guard = thread_id.lock().unwrap();
        //     let core = core_ids[ *guard % core_ids.len()];
        //     info!("thread_id:{} 已绑定到线程:{:?}", *guard, core);
        //     core_affinity::set_for_current(core);
        //     *guard = *guard + 1;
        // })
        .enable_all()
        .build()
        .unwrap())
}

pub enum NetworkMessage {
    Request {
        client_id: usize,
        data: Arc::<Box<dyn IMessage>>,
    },
    Response {
        client_id: usize,
        data: Arc::<Box<dyn IMessage>>,
    },
    Message {
        data: Arc::<Box<dyn IMessage>>,
    },
    //Shutdown,
}

#[async_trait]
pub trait Server {
    async fn run(&self, work_thead_num: usize, queue_len: usize) -> io::Result<()>;
}

struct ClientConnection {
    addr: std::net::SocketAddr,
    writer: tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
}

pub struct TCPServer {
    clients: Arc<DashMap<usize, ClientConnection>>,
    addr: String,
    client_id: Arc<AtomicUsize>,
}

#[async_trait]
impl Server for TCPServer {
    async fn run(&self, work_thead_num: usize, queue_len: usize) -> io::Result<()>{

        let listener = TcpListener::bind(&self.addr).await?;

        info!("服务器启动成功，监听端口:{}", self.addr);

        let (shutdown_sender, _) = broadcast::channel(1);

        let rt = start_runtime(work_thead_num);

        let channels: Vec<(mpsc::Sender<NetworkMessage>, mpsc::Receiver<NetworkMessage>)> = (0..work_thead_num)
        .map(|_| mpsc::channel(queue_len))
        .collect();


        let senders: Vec<_> = channels.iter().map(|(tx, _)| tx.clone()).collect();
        let receivers: Vec<_> = channels.into_iter().map(|(_, rx)| rx).collect();

        let (request_sender, mut request_receiver) = mpsc::channel(queue_len * work_thead_num);

        //做工线程，从
        let _: Vec<_> = receivers.into_iter().enumerate().map(|(_, mut rx)| {
            let mut s_receiver = shutdown_sender.subscribe();
            let clients = self.clients.clone();
            rt.spawn(async move {
                loop {
                    tokio::select! {
                        result = rx.recv() => {
                            match result {
                                Some(NetworkMessage::Response { client_id, data }) => {
                                    if let Some(client_connection) = clients.get(&client_id) {
                                        if let Err(e) = client_connection.writer.lock().await.write_all(data.to_bytes().as_ref()).await {
                                            error!("write_all clientid:{}, 出错:{}", client_id, e);
                                            clients.remove(&client_id);
                                            break;
                                        }
                                    } else {
                                        info!("client_id:{} 不在clients中, 丢弃data:{}, 不写入socket中", client_id, data.to_json_string());
                                    }
                                }
                                _ => {
                                    info!("通道已关闭，退出");
                                    break; // 连接关闭
                                }
                            }
                        }
                        _ = s_receiver.recv() => {
                            info!("client_connection.writer 收到退出消息，退出");
                            break; // 退出循环
                        }
                    }
                }
            });
        
        }).collect();
        
        
        let clients = self.clients.clone();
        let client_id = self.client_id.clone();

        let rt1 = Arc::clone(&rt);
        
        let mut s_receiver = shutdown_sender.subscribe();
        rt.spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((socket, addr)) = result {
    
                            let client_id = client_id.fetch_add(1, Ordering::SeqCst);
                            info!("新连接: {:?}, {}，当前共有{}个客户端", addr, client_id, clients.len());

                            let (mut reader, writer) = socket.into_split();
                            
                            let client_connection = ClientConnection {
                                addr: addr,
                                writer: tokio::sync::Mutex::new(writer),
                            };
                            clients.insert(client_id, client_connection);
    
                            let request_sender = request_sender.clone();
    
                            let clientx = clients.clone();
    
                            rt1.spawn(async move {
                                
                                let mut b = ProtocolParser::new().with_max_frame_size(10*1024);
                                
                                loop {
                                    match b.read_frame(&mut reader).await {
                                        Ok(Some(message)) => {
                                            // 使用提取的函数处理消息
                                            if let Err(err) = process_frame_message(&message, client_id, &request_sender).await {
                                                match *err.downcast_ref::<MyError>().unwrap() {
                                                    // 对于关键错误，断开连接
                                                    MyError::SendRequestFailed() => break,
                                                    _ => continue,
                                                }
                                            }
                                        },
                                        Ok(None) => {
                                            info!("客户端正常关闭, client:{client_id}, 关闭socket: 协程退出");
                                            break;
                                        },
                                        Err(e) => {
                                            error!("客户端异常关闭, client:{client_id}, 关闭socket, Err:{e} 协程退出");
                                            break;
                                        }
                                    }
                                }
    
                                trace!("read frame client:{client_id} 协程退出");
                                clientx.remove(&client_id);
                                return;
                            });
                        } else {
                            error!("accept 失败，退出");
                            return;
                        }
                    }
                    _ = s_receiver.recv() => {
                        info!("listener.accept received shutdown signal, exit");
                        return; // 退出
                    }
                }
            }
        });


        'outer: loop {
            
            tokio::select! {
                result = request_receiver.recv() => {
                    match result {
                        Some(NetworkMessage::Request { client_id, data }) => {
                            process_message_msgpack(data, client_id, std::option::Option::Some(&senders[client_id%(work_thead_num)])).await;
                        }
                        Some(NetworkMessage::Message { data }) => {
                            process_message_msgpack(data, 0, std::option::Option::None).await;
                        }
                        _ => {
                            error!("request_receiver 通道已关闭");
                            break; // 连接关闭
                        }
                    }
                }
            
                _result = signal::ctrl_c() => {
                    info!("CTRL_C 被按下，服务器终止！");
                    shutdown_sender.send(()).unwrap();
                    break 'outer;
                }

                _result = handle_signal() => {
                    info!("收到kill ，服务器终止！");
                    shutdown_sender.send(()).unwrap();
                    break 'outer;
                }
            }
        }

        shutdown_sender.send(()).unwrap();

        sleep(Duration::from_secs(2)).await;

        //终止创建的运行时
        match Arc::try_unwrap(rt) {
            Ok(rt) => rt.shutdown_background(),
            Err(_) => eprintln!("Failed to shutdown: Runtime is still shared"),
        }

        // loop {
        //     let rt = rt.clone();
        //     match Arc::try_unwrap(rt) {
        //         Ok(rt) => {rt.shutdown_background(); break;}
        //         Err(_) => continue,
        //     }
        // }

        Ok(())
        //ETTask::complete().await; 
    }

}

impl TCPServer {
    pub async fn new(addr: &str) -> Self {
        TCPServer {
            clients: Arc::new(DashMap::new()),
            addr: addr.to_string(),
            client_id: Arc::new(AtomicUsize::new(0)),
        }
    }

}


async fn handle_signal() {
    pending::<()>().await;
}

//////////////////////////////////////////////////////////////////////////////////////////////

// struct KcpClientConnection {
//     client_id: usize, 
//     addr: std::net::SocketAddr,
//     writer: Option<tokio::sync::Mutex<tokio_kcp::KcpStream>>,
//     reader: Arc<tokio::sync::Mutex<tokio_kcp::KcpStream>>,
//     runtime: std::sync::Arc<tokio::runtime::Runtime>,
//     request_sender: mpsc::Sender<NetworkMessage>,
//     clients: Arc<DashMap<usize, KcpClientConnection>>,
// }
struct KcpClientConnection {
    addr: std::net::SocketAddr,
    writer: tokio::sync::Mutex<tokio::io::WriteHalf<KcpStream>>,
}

pub struct KCPServer {
    clients: Arc<DashMap<usize, KcpClientConnection>>,
    addr: String,
    client_ids: Arc<AtomicUsize>,
}

#[async_trait]
impl Server for KCPServer {
    async fn run(&self, work_thead_num: usize, queue_len: usize) -> io::Result<()> {
        
        let config = tokio_kcp::KcpConfig {
            nodelay: tokio_kcp::KcpNoDelayConfig::fastest(),

            mtu: 1400,
            wnd_size: (1024, 1024),
            session_expire: std::time::Duration::from_secs(30),
            flush_write: true,
            flush_acks_input: false,
            stream: false,
            allow_recv_empty_packet: true,
        };

        let mut listener = tokio_kcp::KcpListener::bind(
            config, self.addr.clone()).await.unwrap();
        
        info!("KCP服务器启动成功,  监听read端口:{}", self.addr);

        let (shutdown_sender, _) = broadcast::channel(1);

        let rt = start_runtime(work_thead_num);

        let channels: Vec<(mpsc::Sender<NetworkMessage>, mpsc::Receiver<NetworkMessage>)> = (0..work_thead_num)
        .map(|_| mpsc::channel(queue_len))
        .collect();


        let senders: Vec<_> = channels.iter().map(|(tx, _)| tx.clone()).collect();
        let receivers: Vec<_> = channels.into_iter().map(|(_, rx)| rx).collect();

        let (request_sender, mut request_receiver) = mpsc::channel(queue_len * work_thead_num);

        //做工线程，从
        let _: Vec<_> = receivers.into_iter().enumerate().map(|(_, mut rx)| {
            let mut s_receiver = shutdown_sender.subscribe();
            let clients = self.clients.clone();
            rt.spawn(async move {
                loop {
                    tokio::select! {
                        result = rx.recv() => {
                            match result {
                                Some(NetworkMessage::Response { client_id, data }) => {
                                    if let Some(client_connection) = clients.get(&client_id) {
                                        if let Err(e) = client_connection.writer.lock().await.write_all(data.to_bytes().as_ref()).await {
                                            error!("write_all clientid:{}, 出错:{}", client_id, e);
                                            clients.remove(&client_id);
                                            break;
                                        }
                                    } else {
                                        info!("client_id:{} 不在clients中, 丢弃data:{}, 不写入socket中", client_id, data.to_json_string());
                                    }
                                }
                                _ => {
                                    info!("通道已关闭，退出");
                                    break; // 连接关闭
                                }
                            }
                        }
                        _ = s_receiver.recv() => {
                            info!("client_connection.writer 收到退出消息，退出");
                            break; // 退出循环
                        }
                    }
                }
            });
        
        }).collect();
        
        
        let clients = self.clients.clone();
        let client_id = self.client_ids.clone();
      
        let mut s_receiver = shutdown_sender.subscribe();
        let rt1 = rt.clone();
        rt.spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((socket, addr)) = result {
    
                            let client_id = client_id.fetch_add(1, Ordering::SeqCst);
                            info!("新连接: {:?}, {}，当前共有{}个客户端", addr, client_id, clients.len());

                            let (mut read_half, write_half) = tokio::io::split(socket);
                            
                            let client_connection = KcpClientConnection{
                                addr: addr,
                                writer: tokio::sync::Mutex::new(write_half),
                            };

                            clients.insert(client_id, client_connection);

                            let request_sender = request_sender.clone();
    
                            let clientx = clients.clone();

                            rt1.spawn(async move {
                                
                                let mut b = ProtocolParser::new().with_max_frame_size(10*1024);
                                
                                loop {
                                    match b.read_frame_from_kcp(&mut read_half).await {
                                        Ok(Some(message)) => {
                                            // 使用提取的函数处理消息
                                            if let Err(err) = process_frame_message(&message, client_id, &request_sender).await {
                                                match *err.downcast_ref::<MyError>().unwrap() {
                                                    // 对于关键错误，断开连接
                                                    MyError::SendRequestFailed() => break,
                                                    _ => continue,
                                                }
                                            }
                                        },
                                        Ok(None) => {
                                            info!("客户端正常关闭, client:{client_id}, 关闭socket: 协程退出");
                                            break;
                                        },
                                        Err(e) => {
                                            error!("客户端异常关闭, client:{client_id}, 关闭socket, Err:{e} 协程退出");
                                            break;
                                        }
                                    }
                                }
    
                                trace!("read frame client:{client_id} 协程退出");
                                clientx.remove(&client_id);
                                return;
                            });
                        } else {
                            error!("accept 失败，退出");
                            return;
                        }
                    }

                    _ = s_receiver.recv() => {
                        info!("listener.accept received shutdown signal, exit");
                        return; // 退出
                    }
                }
            }
        });


        'outer: loop {
            
            tokio::select! {
                result = request_receiver.recv() => {
                    match result {
                        Some(NetworkMessage::Request { client_id, data }) => {
                            process_message_msgpack(data, client_id, std::option::Option::Some(&senders[client_id%(work_thead_num)])).await;
                        }
                        Some(NetworkMessage::Message { data }) => {
                            process_message_msgpack(data, 0, std::option::Option::None).await;
                        }
                        _ => {
                            error!("request_receiver 通道已关闭");
                            break; // 连接关闭
                        }
                    }
                }
            
                _result = signal::ctrl_c() => {
                    info!("CTRL_C 被按下，服务器终止！");
                    shutdown_sender.send(()).unwrap();
                    break 'outer;
                }

                _result = handle_signal() => {
                    info!("收到kill ，服务器终止！");
                    shutdown_sender.send(()).unwrap();
                    break 'outer;
                }
            }
        }

        shutdown_sender.send(()).unwrap();

        sleep(Duration::from_secs(2)).await;

        //终止创建的运行时
        match Arc::try_unwrap(rt) {
            Ok(rt) => rt.shutdown_background(),
            Err(_) => eprintln!("Failed to shutdown: Runtime is still shared"),
        }

        // loop {
        //     let rt = rt.clone();
        //     match Arc::try_unwrap(rt) {
        //         Ok(rt) => {rt.shutdown_background(); break;}
        //         Err(_) => continue,
        //     }
        // }

        Ok(())
        //ETTask::complete().await; 
    }

}

impl KCPServer {

    pub async fn new(addr: &str) -> Self {
        KCPServer {
            clients: Arc::new(DashMap::new()),
            addr: addr.to_string(),
            client_ids: Arc::new(AtomicUsize::new(0)),
        }
    }
    
}


//////////////////////////////////////////////////////////////////////////////////////////////
pub trait Singleton {
    fn instance() -> &'static Self;
}

#[derive(Singleton)]
pub struct Root{}
//////////////////////////////////////////////////////////////////////////////////////////////


pub enum Layer2Type {
    ActorMessage,
    ActorRequest,
    ActorResponse,
}

pub trait GetLayer2Type {
    fn get_layer2_type() -> &'static Layer2Type;
}

pub enum Layer3Type {
    ActorLocationMessage,
    ActorLocationRequest,
    ActorLocationResponse,
}

pub trait GetLayer3Type {
    fn get_layer3_type() -> &'static Layer3Type;
}

pub trait IMessage : Any + Send + Sync {
    fn with_serde_json_value(&self, message: serde_json::Value) -> RetResult<Box<dyn IMessage>>;
    fn as_any(&self) -> &dyn Any;
    fn get_type_name(&self) -> &str;
    fn to_bytes(&self) -> Vec<u8>;
    fn to_json_string(&self) -> String;
}

pub trait IRequest: IMessage{
    fn get_rpc_id(&self) -> i32;
}

pub trait IResponse: IMessage{
    fn get_rpc_id(&self) -> i32;
    fn get_error(&self) -> i32;
    fn get_message(&self) -> String;
}
///////////////////////////////////////////////////////////////////////////////////////////////

pub trait IActorMessage: IMessage{
}

pub trait IActorRequest: IRequest{
}

pub trait IActorResponse: IResponse{
}
///////////////////////////////////////////////////////////////////////////////////////////////

pub trait IActorLocationMessage: IActorMessage {

}

pub trait IActorLocationRequest: IActorRequest {

}

pub trait IActorLocationResponse: IActorResponse {

}

#[async_trait]
pub trait IMActorHandler : Send + Sync {
    async fn handle_message(&self, message: Arc<Box<dyn IMessage>>, client_id: usize, sender: std::option::Option<&mpsc::Sender<NetworkMessage>>);
}

//带应答的ActorLocationRpc消息
#[async_trait]
pub trait AMActorLocationRpcHandler<T1: IActorLocationRequest, T2: IActorLocationResponse> {
    async fn handler(&self, request: &T1, response: &mut T2);
}

//带应答的ActorRpc消息
#[async_trait]
pub trait AMActorRpcHandler<T1: IActorRequest, T2: IActorResponse> {
    async fn handler(&self, request: &T1, response: &mut T2);
}

//不带应答的ActorLocation消息
#[async_trait]
pub trait AMActorLocationHandler<T: IActorLocationMessage> {
    async fn handler(&self, message: &T);
}

//不带应答的Actor消息
#[async_trait]
pub trait AMActorHandler<T: IActorMessage> {
    async fn handler(&self, message: &T);
}

///////////////////////////////////////////////////////////////////////////////////////////////
struct KeyedHandler {
    key: &'static str,
    handler: &'static LazyLock<Box<dyn IMActorHandler>>,
}

inventory::collect!(KeyedHandler);

lazy_static::lazy_static! {
    static ref HANDLER_MAP: HashMap<&'static str, &'static Box<dyn IMActorHandler>> = {
        let mut m = HashMap::new();
        for handler in inventory::iter::<KeyedHandler> {
            m.insert(handler.key, &**handler.handler);
        }
        m
    };
}

struct MessageParaser {
    key: &'static str,
    paraser: &'static LazyLock<Box<dyn IMessage>>,
}

inventory::collect!(MessageParaser);

lazy_static::lazy_static! {
    static ref MESSAGE_PARASER: HashMap<&'static str, &'static Box<dyn IMessage>> = {
        let mut m = HashMap::new();
        for handler in inventory::iter::<MessageParaser> {
            m.insert(handler.key, &**handler.paraser);
        }
        m
    };
}
///////////////////////////////////////////////////////////////////////////////////////////////

create_actorLocationMessageRequest! {
    pub struct C2M_MoveToMessage {
        #[serde(default)]  // 如果字段不存在，使用默认值（None）
        pub player_id: i64 
    }
}

#[allow(non_camel_case_types)]
#[derive(IActorLocationMessageHandler, Default)]
#[RequestType(C2M_MoveToMessage)]
pub struct C2M_MoveToMessageHandler;

impl C2M_MoveToMessageHandler  {
    async fn run(&self, _request: &C2M_MoveToMessage)  {
        //trace!("C2M_MoveToMessage");
    }
}
///////////////////////////////////////////////////////////////////////////////////////////////

create_actorLocationRpcRequest! {
    pub struct C2M_GetPlayerInfoRequest {
        #[serde(default)]  // 如果字段不存在，使用默认值（None）
        pub player_id: i64 
    }
}

create_actorLocationRpcResponse! {
    pub struct M2C_GetPlayerInfoResponse {
    }
}

#[allow(non_camel_case_types)]
#[derive(IActorLocationRpcHandler, Default)]
#[ResponseType(M2C_GetPlayerInfoResponse)]
#[RequestType(C2M_GetPlayerInfoRequest)]
pub struct C2M_GetPlayerInfoHandler;

impl C2M_GetPlayerInfoHandler  {
    async fn run(&self, _request: &C2M_GetPlayerInfoRequest, _response: &mut M2C_GetPlayerInfoResponse)  {
        trace!("正在处理C2M_GetPlayerInfoRequest, rpc_id:{}", _request.get_rpc_id());
        //if request.player_id == 0 {
            //error!("C2M_GetPlayerInfoRequest的请求参数player_id为0，请求不合法");
        //    response.error = 1;
        //    return;
        //}
        //response.error = 0;
    }
}
//////////////////////////////以下这两部分，应该是用工具生成的代码，然后编译时在处理宏////////////////////////////////////////
create_actorLocationRpcRequest! {
    pub struct C2M_PingRequest {
    }
}

create_actorLocationRpcResponse! {
    pub struct C2M_PingResponse {
        count: i64,
    }
}
////////////////////////////以下这段，是开发者自己增加的，在于生成一个处理请求和返回的Handler//////////////
#[allow(non_camel_case_types)]
#[derive(IActorLocationRpcHandler, Default)]
#[ResponseType(C2M_PingResponse)]
#[RequestType(C2M_PingRequest)]
pub struct C2M_PingHandler;

impl C2M_PingHandler  {
    async fn run(&self, _request: &C2M_PingRequest, _response: &mut C2M_PingResponse)  {
        _response.count += 1;
        trace!("正在处理C2M_PingRequest, rpc_id:{}", _request.get_rpc_id());
    }
}
///////////////////////////////////////////////////////////////////////////////////////////////////
pub async fn  process_message_msgpack(data: Arc<Box<dyn IMessage>>, 
    client_id: usize, 
    sender: std::option::Option<&mpsc::Sender<NetworkMessage>>){
    let type_name = data.get_type_name();
        if let Some(handler) = get_handler(type_name) {
        handler.handle_message(data, client_id, sender).await;
    } else {
        error!("消息：{} 没有对应的Handler ", type_name);
    }
}

pub fn get_handler(key: &str) -> Option<&'static dyn IMActorHandler> {
    HANDLER_MAP.get(key).map(|v| &**v).map(|v| &**v)
}

pub fn get_paraser(key: &str) -> Option<&'static dyn IMessage> {
    MESSAGE_PARASER.get(key).map(|v| &**v).map(|v| &**v)
}

// 处理单个消息帧
async fn process_frame_message(
    message: &[u8],
    client_id: usize,
    request_sender: &mpsc::Sender<NetworkMessage>
) -> RetResult<()> {
    // 解析消息为serde_json::Value
    let v = match rmp_serde::from_slice::<Value>(message) {
        Ok(v) => v,
        Err(_) => {
            error!("client:{client_id}, 收到消息, 但解析失败, 不是一段合法的rmp_serde,msgpack数据, 丢弃data:{}", message.len());
            return Err(MyError::NotMessagePack().into());
        }
    };
    
    // 获取消息类型
    let message_type = match v["_t"].as_str() {
        Some(t) => t,
        None => {
            error!("client:{client_id}, 收到消息, 但是其中没有_t字段, 无法处理, 丢弃data:{}", message.len());
            return Err(MyError::MessageNoTField().into());
        }
    };
    
    // 获取解析器
    let paraser = match get_paraser(message_type) {
        Some(p) => p,
        None => {
            error!("client:{client_id}, 收到消息{}, 但是其中没有对应的handler, 无法处理, 丢弃data:{}", message_type, message.len());
            return Err(MyError::MessageHandlerNotFound(message_type.to_string()).into());
        }
    };
    
    // 解析具体消息对象
    let message_obj = match paraser.with_serde_json_value(v) {
        Ok(obj) => obj,
        Err(e) => {
            error!("with_serde_json_value 解析数据失败，client:{client_id}, Err:{}", e);
            return Err(MyError::MessageObjectConvertFailed().into());
        }
    };
    
    // 发送消息到处理队列
    if let Err(e) = request_sender.send(NetworkMessage::Request {
        client_id,
        data: Arc::new(message_obj),
    }).await {
        error!("request_sender.send Request 发送消息失败, {}", e);
        return Err(MyError::SendRequestFailed().into());
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use bytes::{BytesMut, BufMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::time::LocalTime;
    use serde::Serialize;

    use tracing::{trace, error, info, warn, debug};

    #[tokio::test]
    async fn test_kcp() -> std::io::Result<()> {   

        let filter = EnvFilter::try_from_default_env()  // 先尝试从 RUST_LOG 读取
        .or_else(|_| EnvFilter::try_new("info")) // 失败则用命令行参数
        .unwrap();

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(true) // false没有颜色，适合生产环境
            .with_timer(LocalTime::rfc_3339()) // 自动读取 RUST_LOG
            .init();

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


        let server_addr = "127.0.0.1:3100".parse::<std::net::SocketAddr>().unwrap();
        let mut listener = tokio_kcp::KcpListener::bind(config, server_addr).await.unwrap();

        info!("kcp read server start {}", server_addr);

        tokio::spawn(async move{
            loop {
                let (mut read_stream, peer_addr) = match listener.accept().await {
                    Ok(s) => s,
                    Err(err) => {
                        error!("accept failed, error: {}", err);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
    
                println!("accepted {} in {}", peer_addr, server_addr);

                tokio::spawn(async move {
                    let mut buffer = vec![0; 128];
                    while let Ok(n) = read_stream.read(&mut buffer).await {
                        println!("recv {:?}, len:{}", &buffer[..n], n);
                        if n == 0 {
                            break;
                        }
                    }
                    debug!("client {} closed", peer_addr);
                });
            }
        });


        let server_addr2 = "127.0.0.1:3101".parse::<std::net::SocketAddr>().unwrap();
        let mut listener2 = tokio_kcp::KcpListener::bind(config, server_addr2).await.unwrap();

        info!("kcp write server start {}", server_addr2);

        tokio::spawn(async move{
            loop {
                let (mut write_stream, peer_addr2) = match listener2.accept().await {
                    Ok(s) => s,
                    Err(err) => {
                        error!("accept failed, error: {}", err);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };


                println!("accepted {} in {}", peer_addr2, server_addr2);

                let mut buffer = vec![0; 128];
                write_stream.read(&mut buffer).await.unwrap();
                
                let mut ping_msg = Vec::new();
                crate::C2M_PingRequest{rpc_id:1, _t: "C2M_PingRequest".to_string()}
                    .serialize(&mut rmp_serde::Serializer::new(&mut ping_msg).with_struct_map()).unwrap();

                let mut buf = BytesMut::with_capacity(2 + ping_msg.len());
                buf.put_u16(ping_msg.len() as u16);
                buf.put_slice(&ping_msg);

                for _ in 0..10 {
                    write_stream.write_all(&buf).await.unwrap();
                    write_stream.flush().await.unwrap();
                    println!("write {:?}, len:{}", buf, buf.len());
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        });


        tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
        Ok(())
    }
}