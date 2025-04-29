use std::io::Error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::LocalTime;
use tokio::io::{self};

#[allow(unused_imports)]
use tokio::time::{sleep, Duration};

use clap::Parser;


mod errors;
mod struct_macro;
mod et_event;
mod entity;
mod utils;
mod net;

mod my_future;

use entity::root::Root;




lazy_static::lazy_static! {
    pub static ref WORK_THREAD_NUM: usize = std::thread::available_parallelism().unwrap().get();
    pub static ref QUEUE_LEN: usize = 100_000;
    pub static ref BENCHMARK_VALUE: i64 = 0;
}


#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {  

    if let Err(e) = set_process_priority(true) {
        eprintln!("无法设置进程优先级: {}", e);
    } else {
        println!("进程优先级已设置");
    }

    let args = Args::parse(); // 先解析命令行参数
    init_logging(&args.log);

    // 初始化事件系统 - 会自动注册所有使用#[derive(EventHandler)]的处理器
    let _system = et_event::event_system::EventSystem::instance();

    // system.publish_async(crate::et_event::MonsterMoveParam { monster_id: 1, x: 1.0, y: 2.0 }).await;
    // tokio::spawn(system.publish_async(crate::et_event::MonsterMoveParam { monster_id: 2, x: 3.0, y: 4.0 }));
    // tokio::spawn(system.publish_async(crate::et_event::MonsterDeadParam {x: 1.0, y: 2.0 }));
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // trace!("1");
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // trace!("2");
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // trace!("3");

    let root = Root::instance();
    root.run().await;
    
    // 启动网络服务器
    let server = TiangZ::KCPServer::new("0.0.0.0:3100", "0.0.0.0:3101").await;
    
    // 运行服务器
    tokio::select! {
        _ = server.run(*WORK_THREAD_NUM, *QUEUE_LEN) => {

        }
        _ = sleep(Duration::from_secs(7)) => {

        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn set_process_priority(high_priority: bool) -> Result<(), Error> {

    #[allow(unused_imports)]
    use windows::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, 
        REALTIME_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS
    };

    unsafe {
        let handle = GetCurrentProcess();
        let priority = if high_priority {
            // 根据需要选择合适的优先级
            //REALTIME_PRIORITY_CLASS // 实时优先级（危险，需管理员权限）
            HIGH_PRIORITY_CLASS // 高优先级
        } else {
            NORMAL_PRIORITY_CLASS
        };
        
        if SetPriorityClass(handle, priority).is_ok() {
            Ok(())
        } else {
            Err(Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
fn set_process_priority(high_priority: bool) -> Result<(), Error> {

    // #[allow(unused_imports)]
    // use nix::sys::resource::{getpriority, setpriority, Priority};
    // use nix::unistd::Pid;
    
    // // 获取当前进程ID
    // let pid = Pid::this();
    
    // // 获取当前优先级
    // let current = getpriority(Priority::Process, pid.as_raw() as u32)
    //     .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    // println!("当前进程优先级: {}", current);
    
    // // 设置为-20 (最高优先级)
    // match setpriority(Priority::Process, pid.as_raw() as u32, -20) {
    //     Ok(_) => {
    //         println!("进程优先级已设置为-20");
    //         Ok(())
    //     },
    //     Err(e) => {
    //         eprintln!("无法设置进程优先级，可能需要root权限: {}", e);
    //         Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
    //     }
    // }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(author, version, about)] // 可选：添加命令行帮助信息
struct Args {
    /// 设置日志级别 (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log: String,
}

fn init_logging(log_level: &str) {
    let filter = EnvFilter::try_from_default_env()  // 先尝试从 RUST_LOG 读取
        .or_else(|_| EnvFilter::try_new(log_level)) // 失败则用命令行参数
        .unwrap();

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(true) // false没有颜色，适合生产环境
        .with_timer(LocalTime::rfc_3339()) // 自动读取 RUST_LOG
        .init();
}

#[cfg(test)]
mod main_tests {
   
}