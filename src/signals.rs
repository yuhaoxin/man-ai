//! 两阶段 SIGINT（规格 §5.6）。
//!
//! 翻译期间（内层分页器尚未启动）把 SIGINT 记到标志上；启动分页器前恢复默认
//! 处置，于是第二次 Ctrl-C 按默认行为结束进程。
//!
//! SIGPIPE 不用处理：Rust 启动时已把它设为忽略，写失败按 EPIPE 处理。
//! SIGTERM / SIGHUP 保持默认。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use signal_hook::SigId;

/// 已安装的 SIGINT 处理器；`restore` 后恢复默认处置，可重复调用。
pub struct Interrupt {
    flag: Arc<AtomicBool>,
    id: Option<SigId>,
}

impl Interrupt {
    /// 安装处理器；注册失败时退化为从不触发的标志（默认处置仍然生效）。
    pub fn install() -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        let id = signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone()).ok();
        Self { flag, id }
    }

    /// 翻译任务用这个句柄观察第一次 Ctrl-C。
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// 恢复 SIGINT 默认处置（启动分页器前调用）。
    ///
    /// `signal_hook::low_level::unregister` 只摘掉动作、不还原处理器（其文档明确
    /// 说明），所以还要把 `SIG_DFL` 装回去，否则之后的 Ctrl-C 会被忽略。
    pub fn restore(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        signal_hook::low_level::unregister(id);
        default_sigint();
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.restore();
    }
}

fn default_sigint() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    // SAFETY: SIG_DFL 是合法的处置值，只影响本进程的 SIGINT。
    unsafe {
        signal(signal_hook::consts::SIGINT, 0);
    }
}
