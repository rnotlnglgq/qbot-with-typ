use clap::Parser;
use std::io::Write;
use std::sync::Arc;
use tracing::info;

mod bot;
mod qq_media;
mod worker;

/// QQ 机器人 - 支持 Typst 渲染（通过 typst worker 子进程池）
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 额外字体目录（传递给 worker）
    #[arg(short, long)]
    font_dir: Option<String>,

    /// 本地 Typst 包目录（语义同 typst CLI，传递给 worker）
    #[arg(long, short='l')]
    package_path: Option<String>,

    /// Typst 包缓存目录（语义同 typst CLI，传递给 worker）
    #[arg(long, short='p')]
    package_cache_path: Option<String>,

    /// Worker 进程池大小
    #[arg(long, default_value_t = 2)]
    pool_size: usize,

    /// 单次渲染超时（秒）
    #[arg(long, default_value_t = 30)]
    render_timeout: u64,

    /// Worker 启动（字体加载）超时（秒）
    #[arg(long, default_value_t = 120)]
    spawn_timeout: u64,

    /// Worker 内存上限（MiB）
    #[arg(long, default_value_t = 512)]
    memory_limit: u64,

    /// Worker 进程 CPU 累计时间安全网（秒）
    #[arg(long, default_value_t = 3600)]
    cpu_lifetime: u64,

    /// 禁用 worker 的 RLIMIT_FSIZE 文件大小限制（默认限制为 0 字节）
    #[arg(long)]
    no_fsize_lim: bool,

    /// 启用 debug 级别日志，追踪完整消息处理流程
    #[arg(long)]
    debug: bool,

    /// 使用 QQ 开放平台沙箱 API（测试环境）
    #[arg(long)]
    qq_sandbox: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let filter = if args.debug {
        "botrs=debug,qbot_with_typ=debug".to_string()
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "botrs=info,qbot_with_typ=info".into())
    };
    let parent_indent = " ".repeat(worker::log_prefix_width(args.pool_size));
    tracing_subscriber::fmt()
        .with_env_filter(&filter)
        .with_writer(move || PaddedStderr::new(parent_indent.clone()))
        .init();
    info!("日志级别过滤器：{}", filter);

    let mut extra_args = vec!["worker".to_string(), "--meter".to_string()];
    extra_args.push("--max-pages".to_string());
    extra_args.push("10".to_string());
    if let Some(ref font_dir) = args.font_dir {
        extra_args.push("--font-path".to_string());
        extra_args.push(font_dir.clone());
    }
    if let Some(ref p) = args.package_path {
        extra_args.push("--package-path".to_string());
        extra_args.push(p.clone());
    }
    if let Some(ref p) = args.package_cache_path {
        extra_args.push("--package-cache-path".to_string());
        extra_args.push(p.clone());
    }

    let config = worker::PoolConfig {
        pool_size: args.pool_size,
        render_timeout: std::time::Duration::from_secs(args.render_timeout),
        spawn_timeout: std::time::Duration::from_secs(args.spawn_timeout),
        memory_limit_bytes: args.memory_limit * 1024 * 1024,
        cpu_lifetime_secs: args.cpu_lifetime,
        fsize_limit_bytes: if args.no_fsize_lim { None } else { Some(0) },
        extra_args,
        ..Default::default()
    };

    info!(
        pool_size = config.pool_size,
        "正在启动 typst worker 进程池..."
    );
    let pool = worker::WorkerPool::new(config).await?;
    let pool = Arc::new(pool);

    bot::run_bot(pool, args.qq_sandbox).await?;

    Ok(())
}

/// Writer 包装 stderr，在每个换行后插入固定缩进，让父进程的 tracing 输出
/// 与 worker 的 `<slot_id> ` 前缀列对齐。
struct PaddedStderr {
    pad: String,
    at_line_start: bool,
}

impl PaddedStderr {
    fn new(pad: String) -> Self {
        Self { pad, at_line_start: true }
    }
}

impl Write for PaddedStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut out = std::io::stderr().lock();
        for &b in buf {
            if self.at_line_start {
                out.write_all(self.pad.as_bytes())?;
                self.at_line_start = false;
            }
            out.write_all(&[b])?;
            if b == b'\n' {
                self.at_line_start = true;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().lock().flush()
    }
}
