//! 取消通道与终止信号处置（规格 §5.4）。
//!
//! Ctrl-C 在 raw 模式下是按键事件，由阅读器经取消通道通知翻译；SIGTERM/SIGHUP
//! 处理器线程收到信号后恢复终端，再以默认处置重新触发自身（退出码 128+n）。Ctrl-Z 是 raw
//! 模式下的普通按键，阅读器不绑定、按忽略处理。
//!
//! SIGPIPE 不用处理：Rust 启动时已把它设为忽略，写失败按 EPIPE 处理。

use std::io;
use std::thread::{self, JoinHandle};

use tokio::sync::watch;

use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::LeaveAlternateScreen;
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::{Handle, Signals};

/// Ctrl-C 取消通道：阅读器持触发端，翻译持观察端。
pub fn cancel_pair() -> (CancelTrigger, CancelWatch) {
    let (sender, mut receiver) = watch::channel(false);
    // 初始值记为已见：`cancelled` 只在置位后返回。
    receiver.borrow_and_update();
    (CancelTrigger { sender }, CancelWatch { receiver })
}

/// 阅读器侧：按下翻译期间的 Ctrl-C 时触发。
#[derive(Clone)]
pub struct CancelTrigger {
    sender: watch::Sender<bool>,
}

impl CancelTrigger {
    pub fn trigger(&self) {
        let _ = self.sender.send(true);
    }
}

/// 翻译侧句柄；每个并发请求持一份克隆。
#[derive(Clone)]
pub struct CancelWatch {
    receiver: watch::Receiver<bool>,
}

impl CancelWatch {
    /// 取消是否已触发。
    pub fn triggered(&self) -> bool {
        *self.receiver.borrow()
    }

    /// 等待触发；触发端销毁后永不返回。
    pub async fn cancelled(&mut self) {
        if self.receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub struct Termination {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

/// 安装 SIGTERM/SIGHUP 的管道处理器（§5.4）。
///
/// 实际终端恢复由普通线程执行；收到信号后恢复默认处置并重新触发自身。
pub fn install_termination() -> io::Result<Termination> {
    let mut signals = Signals::new([SIGTERM, SIGHUP])?;
    let handle = signals.handle();
    let thread = thread::spawn(move || {
        for signal in signals.forever() {
            restore_terminal();
            // SAFETY: `SIG_DFL` 是合法处置值；重发只影响本进程。
            unsafe {
                libc::signal(signal as libc::c_int, libc::SIG_DFL);
                libc::raise(signal as libc::c_int);
            }
        }
    });
    Ok(Termination {
        handle,
        thread: Some(thread),
    })
}

impl Termination {
    /// 摘除处理器并恢复默认处置。
    pub fn uninstall(mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        for signal in [SIGTERM, SIGHUP] {
            // SAFETY: `SIG_DFL` 是合法处置值。
            unsafe {
                libc::signal(signal as libc::c_int, libc::SIG_DFL);
            }
        }
    }
    for signal in [SIGTERM, SIGHUP] {
        // SAFETY: `SIG_DFL` 是合法处置值；重置后信号恢复操作系统默认行为。
        unsafe {
            libc::signal(signal as libc::c_int, libc::SIG_DFL);
        }
    }
}

/// 普通信号处理线程中的终端恢复序列，与阅读器进入的序列一一对应。
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 初始不触发_置位后取消() {
        let (trigger, mut watch) = cancel_pair();
        assert!(!watch.triggered());
        // 未置位时 `cancelled` 不返回。
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), watch.cancelled()).await;
        assert!(!watch.triggered());
        trigger.trigger();
        watch.cancelled().await;
        assert!(watch.triggered());
    }

    #[tokio::test]
    async fn 克隆共享触发_触发端销毁后永不返回() {
        let (trigger, watch) = cancel_pair();
        let mut clone = watch.clone();
        trigger.trigger();
        clone.cancelled().await;
        assert!(clone.triggered());

        let (trigger, mut watch) = cancel_pair();
        drop(trigger);
        let polled =
            tokio::time::timeout(std::time::Duration::from_millis(50), watch.cancelled()).await;
        assert!(polled.is_err());
    }
}
