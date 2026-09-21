//! 优先级预渲染调度。
//!
//! # 两个必须解决的问题
//!
//! **① 快速翻页时的队头阻塞。**
//! 老师连按空格翻 10 页时，队列里会堆积 10 个渲染任务。
//! 若按 FIFO 执行，画面会「追赶」式地逐页闪过，且当前页要排队等前 9 页 ——
//! 这正是 WPS 翻页卡顿的根因之一。
//!
//! 解法是**代际（generation）取消**：每次导航都递增代际号，
//! 工作线程在开始处理任务前先检查代际，过期任务直接丢弃。
//! 于是「连翻 10 页」时前 9 页的任务会被跳过，只渲染最终停留的那页。
//!
//! **② 预渲染要抢占缩略图批处理。**
//! 缩略图生成是长尾任务（100 页要几百毫秒），
//! 但它绝不能挡住老师翻页。因此用优先级队列而非多个队列，
//! 让「当前页」永远排在「缩略图」之前。
//!
//! # 为什么不用 rayon 的全局线程池
//!
//! rayon 擅长「数据并行」而非「带优先级与取消的任务调度」。
//! 它的工作窃取队列无法表达「这个任务过期了别做了」。
//! 因此这里自建一个轻量线程池：`Mutex<BinaryHeap>` + `Condvar`，
//! 只有几十行，却精确满足需求。

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrd};
use std::sync::{Arc, Condvar, Mutex};

/// 任务优先级。数值越大越紧急。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// 远端预取（如跳到第 50 页后预热第 51 页）。
    Prefetch = 0,
    /// 缩略图批量生成。
    Thumbnail = 1,
    /// 相邻页预热（上一页/下一页）。
    Adjacent = 2,
    /// 当前页——最高优先级，永远排在前面。
    Current = 3,
}

/// 一个渲染任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderTask {
    pub page: usize,
    /// 缩放档位（见 `cache::scale_bucket`）。
    pub scale_bucket: u32,
    pub priority: Priority,
    /// 提交时的代际号。
    pub generation: u64,
}

impl Ord for RenderTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // 先比优先级；同级时页号小的先做（更接近当前位置的直觉）
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.page.cmp(&self.page))
    }
}

impl PartialOrd for RenderTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 调度器内部状态。
struct Inner {
    queue: BinaryHeap<RenderTask>,
    /// 当前代际号；小于它的任务一律作废。
    generation: u64,
    /// 已提交但未完成的任务数（含正在执行的）。
    pending: usize,
    /// 收到停机信号。
    shutdown: bool,
}

/// 带优先级与代际取消的任务调度器。
pub struct Scheduler {
    inner: Mutex<Inner>,
    signal: Condvar,
    /// 已丢弃的过期任务数（诊断用）。
    dropped: AtomicU64,
    /// 已完成的任务数（诊断用）。
    completed: AtomicU64,
}

impl Scheduler {
    pub fn new() -> Scheduler {
        Scheduler {
            inner: Mutex::new(Inner {
                queue: BinaryHeap::new(),
                generation: 0,
                pending: 0,
                shutdown: false,
            }),
            signal: Condvar::new(),
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        }
    }

    /// 开始新一代。
    ///
    /// 调用后，队列中所有旧代际的任务都会在出队时被丢弃。
    /// 返回新的代际号。
    pub fn begin_generation(&self) -> u64 {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        inner.generation += 1;
        let dropped = inner.queue.len();
        // 直接清空队列：既然全部作废，没必要让工作线程逐个出队再判断
        inner.queue.clear();
        inner.pending = inner.pending.saturating_sub(dropped);
        self.dropped
            .fetch_add(dropped as u64, AtomicOrd::Relaxed);
        self.signal.notify_all();
        inner.generation
    }

    /// 提交一个任务。
    pub fn submit(&self, task: RenderTask) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // 过期任务直接拒绝，不进入队列
        if task.generation < inner.generation {
            self.dropped.fetch_add(1, AtomicOrd::Relaxed);
            return;
        }
        inner.queue.push(task);
        inner.pending += 1;
        self.signal.notify_one();
    }

    /// 批量提交（如同一批缩略图）。
    pub fn submit_all(&self, tasks: impl IntoIterator<Item = RenderTask>) {
        for t in tasks {
            self.submit(t);
        }
    }

    /// 取下一个有效任务；**队列空时会等待**新任务到来。
    ///
    /// 工作线程靠这个「空转即挂起」的行为待命 ——
    /// 若队列一空就返回 `None`，线程会在启动瞬间集体退出。
    /// 需要「立刻问一句有没有活」的场景请用 [`Scheduler::try_next_task`]。
    ///
    /// `stop` 为真时立即返回 `None`，用于让工作线程响应停机。
    pub fn next_task(&self, stop: &AtomicBool) -> Option<RenderTask> {
        loop {
            if let Some(task) = self.try_next_task(stop) {
                return Some(task);
            }

            let inner = self.inner.lock().ok()?;
            if inner.shutdown || stop.load(AtomicOrd::Relaxed) {
                return None;
            }
            // 期间若已有新任务入队，直接回到上面的取任务逻辑
            if !inner.queue.is_empty() {
                drop(inner);
                continue;
            }
            // 挂起等待：提交任务与推进代际都会 `notify`
            drop(self.signal.wait(inner).ok()?);
        }
    }

    /// 尝试取一个有效任务，**不阻塞**：队列空或已停机时立刻返回 `None`。
    ///
    /// 与 [`Scheduler::next_task`] 共用同一套「代际过期即丢弃」的判定。
    pub fn try_next_task(&self, stop: &AtomicBool) -> Option<RenderTask> {
        let mut inner = self.inner.lock().ok()?;

        if inner.shutdown || stop.load(AtomicOrd::Relaxed) {
            return None;
        }
        loop {
            match inner.queue.pop() {
                Some(task) => {
                    // 出队后再校验一次代际：任务可能在排队期间过期
                    if task.generation < inner.generation {
                        self.dropped.fetch_add(1, AtomicOrd::Relaxed);
                        continue;
                    }
                    return Some(task);
                }
                None => return None,
            }
        }
    }

    /// 标记一个任务完成。
    pub fn complete(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.pending = inner.pending.saturating_sub(1);
        }
        self.completed.fetch_add(1, AtomicOrd::Relaxed);
    }

    /// 队列中待处理的任务数。
    pub fn pending(&self) -> usize {
        self.inner.lock().map(|i| i.pending).unwrap_or(0)
    }

    /// 队列是否为空。
    pub fn is_idle(&self) -> bool {
        self.pending() == 0
    }

    /// 当前代际号。
    pub fn generation(&self) -> u64 {
        self.inner.lock().map(|i| i.generation).unwrap_or(0)
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(AtomicOrd::Relaxed)
    }

    pub fn completed_count(&self) -> u64 {
        self.completed.load(AtomicOrd::Relaxed)
    }

    /// 停止所有工作线程。
    pub fn shutdown(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shutdown = true;
            inner.queue.clear();
        }
        self.signal.notify_all();
    }

    /// 阻塞等待队列清空（供基准测试与同步场景使用）。
    pub fn wait_idle(&self, stop: &AtomicBool, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.is_idle() || stop.load(AtomicOrd::Relaxed) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.is_idle()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Scheduler::new()
    }
}

/// 渲染线程池。
///
/// 线程数由性能档位决定：老机器双核只开 2 个（避免与 WebView 抢 CPU），
/// 高配机器可以开到 `min(cores, 4)` —— 再多的收益递减，
/// 因为单页渲染内部的图片解码已经会用满内存带宽。
pub struct WorkerPool {
    scheduler: Arc<Scheduler>,
    stop: Arc<AtomicBool>,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl WorkerPool {
    /// 启动线程池。
    ///
    /// `job` 是任务处理函数，返回 `true` 表示处理成功。
    /// 它会被多个线程并发调用，因此必须自己处理线程安全
    /// （实际使用中由 `Pipeline` 传入闭包，内部共享渲染器与缓存）。
    pub fn start<F>(scheduler: Arc<Scheduler>, threads: usize, job: F) -> WorkerPool
    where
        F: Fn(RenderTask) -> bool + Send + Sync + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let job = Arc::new(job);
        let mut handles = Vec::with_capacity(threads);

        for i in 0..threads.max(1) {
            let scheduler = Arc::clone(&scheduler);
            let stop = Arc::clone(&stop);
            let job = Arc::clone(&job);

            let handle = std::thread::Builder::new()
                .name(format!("ppt-render-{i}"))
                .spawn(move || {
                    while let Some(task) = scheduler.next_task(&stop) {
                        let _ = job(task);
                        scheduler.complete();
                    }
                });

            match handle {
                Ok(h) => handles.push(h),
                Err(e) => {
                    // 线程创建失败（资源不足）时降级为更少线程，
                    // 而不是让整个应用启动失败
                    log::warn!("渲染线程 {i} 创建失败：{e}");
                }
            }
        }

        WorkerPool {
            scheduler,
            stop,
            handles: Mutex::new(handles),
        }
    }

    /// 已启动的线程数。
    pub fn thread_count(&self) -> usize {
        self.handles.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// 停机并等待所有线程退出。
    pub fn shutdown(&self) {
        self.stop.store(true, AtomicOrd::Relaxed);
        self.scheduler.shutdown();
        if let Ok(mut handles) = self.handles.lock() {
            for h in handles.drain(..) {
                let _ = h.join();
            }
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 根据 CPU 核心数选择渲染线程数。
///
/// 上限 4 是经验值：单页渲染内部（图片解码、路径光栅化）
/// 已经会用到内存带宽，线程再多收益递减，
/// 反而会与 WebView2 的合成线程抢 CPU 导致界面卡顿。
pub fn recommended_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    match cores {
        0 | 1 => 1,
        2 | 3 => 2,
        _ => 4.min(cores - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn task(page: usize, priority: Priority, generation: u64) -> RenderTask {
        RenderTask {
            page,
            scale_bucket: 100,
            priority,
            generation,
        }
    }

    #[test]
    fn higher_priority_dequeues_first() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(5, Priority::Thumbnail, gen));
        s.submit(task(1, Priority::Current, gen));
        s.submit(task(3, Priority::Adjacent, gen));

        let stop = AtomicBool::new(false);
        let first = s.next_task(&stop).unwrap();
        assert_eq!(first.priority, Priority::Current, "当前页应最先出队");
        let second = s.next_task(&stop).unwrap();
        assert_eq!(second.priority, Priority::Adjacent);
        let third = s.next_task(&stop).unwrap();
        assert_eq!(third.priority, Priority::Thumbnail);
    }

    #[test]
    fn same_priority_prefers_smaller_page() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(9, Priority::Adjacent, gen));
        s.submit(task(2, Priority::Adjacent, gen));

        let stop = AtomicBool::new(false);
        assert_eq!(s.next_task(&stop).unwrap().page, 2);
        assert_eq!(s.next_task(&stop).unwrap().page, 9);
    }

    #[test]
    fn new_generation_drops_queued_tasks() {
        let s = Scheduler::new();
        let gen1 = s.begin_generation();
        for p in 0..10 {
            s.submit(task(p, Priority::Current, gen1));
        }
        assert_eq!(s.pending(), 10);

        // 老师又翻了一页：旧任务应全部作废
        let gen2 = s.begin_generation();
        assert!(gen2 > gen1);
        assert_eq!(s.pending(), 0, "旧代际的任务应被清空");
        assert_eq!(s.dropped_count(), 10);

        let stop = AtomicBool::new(false);
        // 用不阻塞的 `try_next_task`：`next_task` 在队列空时会挂起等待
        assert!(s.try_next_task(&stop).is_none(), "队列应为空");
    }

    #[test]
    fn stale_task_is_rejected_on_submit() {
        let s = Scheduler::new();
        let gen1 = s.begin_generation();
        let gen2 = s.begin_generation();

        s.submit(task(0, Priority::Current, gen1));
        assert_eq!(s.pending(), 0, "过期任务不应入队");
        assert_eq!(s.dropped_count(), 1);

        s.submit(task(0, Priority::Current, gen2));
        assert_eq!(s.pending(), 1, "当代任务应正常入队");
    }

    #[test]
    fn task_expiring_while_queued_is_skipped() {
        let s = Scheduler::new();
        let gen1 = s.begin_generation();
        s.submit(task(1, Priority::Thumbnail, gen1));
        s.submit(task(2, Priority::Current, gen1));

        // 模拟：任务已入队，但出队前代际又推进了
        let stop = AtomicBool::new(false);
        // 先取出最高优先级的那个
        let t = s.next_task(&stop).unwrap();
        assert_eq!(t.page, 2);
        s.complete();

        // 此时推进代际，剩余任务作废
        s.begin_generation();
        assert!(s.try_next_task(&stop).is_none(), "过期任务不应再被取出");
    }

    #[test]
    fn next_task_waits_for_work_instead_of_returning_none() {
        let s = Arc::new(Scheduler::new());
        let gen = s.begin_generation();
        let stop = Arc::new(AtomicBool::new(false));

        let waiter = {
            let s = Arc::clone(&s);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || s.next_task(&stop).map(|t| t.page))
        };

        // 队列此刻为空：工作线程必须**挂起等待**。
        // 如果这里改成「队列空就返回 None」，线程池会在启动瞬间集体退出，
        // 表现为「打开课件后永远是白页」—— 所以用这条断言把它钉住
        std::thread::sleep(Duration::from_millis(60));
        assert!(!waiter.is_finished(), "队列空时 next_task 应挂起等待");

        s.submit(task(7, Priority::Current, gen));
        assert_eq!(waiter.join().expect("线程应正常结束"), Some(7));
    }

    #[test]
    fn next_task_returns_none_when_stopped() {
        let s = Scheduler::new();
        let stop = AtomicBool::new(true);
        assert!(s.next_task(&stop).is_none());
    }

    #[test]
    fn next_task_returns_none_after_shutdown() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(0, Priority::Current, gen));
        s.shutdown();

        let stop = AtomicBool::new(false);
        assert!(s.next_task(&stop).is_none(), "停机后不应再出队");
    }

    #[test]
    fn pending_and_completed_counters_track_progress() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(0, Priority::Current, gen));
        s.submit(task(1, Priority::Current, gen));
        assert_eq!(s.pending(), 2);
        assert!(!s.is_idle());

        let stop = AtomicBool::new(false);
        let _ = s.next_task(&stop);
        s.complete();
        assert_eq!(s.pending(), 1);
        assert_eq!(s.completed_count(), 1);

        let _ = s.next_task(&stop);
        s.complete();
        assert!(s.is_idle());
        assert_eq!(s.completed_count(), 2);
    }

    #[test]
    fn wait_idle_returns_when_queue_drains() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(0, Priority::Current, gen));

        let stop = AtomicBool::new(false);
        assert!(
            !s.wait_idle(&stop, Duration::from_millis(20)),
            "队列非空时不应报告空闲"
        );

        let _ = s.next_task(&stop);
        s.complete();
        assert!(s.wait_idle(&stop, Duration::from_millis(200)));
    }

    #[test]
    fn worker_pool_processes_all_tasks() {
        let scheduler = Arc::new(Scheduler::new());
        let counter = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));

        let c = Arc::clone(&counter);
        let s = Arc::clone(&seen);
        let pool = WorkerPool::start(Arc::clone(&scheduler), 2, move |task| {
            s.lock().unwrap().push(task.page);
            c.fetch_add(1, AtomicOrd::Relaxed);
            true
        });

        assert!(pool.thread_count() >= 1);

        let gen = scheduler.begin_generation();
        for p in 0..20 {
            scheduler.submit(task(p, Priority::Current, gen));
        }

        let stop = AtomicBool::new(false);
        assert!(
            scheduler.wait_idle(&stop, Duration::from_secs(5)),
            "所有任务应被处理完"
        );
        assert_eq!(counter.load(AtomicOrd::Relaxed), 20);
        assert_eq!(seen.lock().unwrap().len(), 20);

        pool.shutdown();
    }

    #[test]
    fn worker_pool_skips_stale_tasks_on_rapid_navigation() {
        let scheduler = Arc::new(Scheduler::new());
        let rendered = Arc::new(AtomicU64::new(0));

        // 用栅栏把线程卡住，确保任务在队列里堆积
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = Arc::clone(&gate);
        let r = Arc::clone(&rendered);

        let pool = WorkerPool::start(Arc::clone(&scheduler), 1, move |_task| {
            let (lock, cv) = &*g;
            let mut go = lock.lock().unwrap();
            while !*go {
                go = cv.wait(go).unwrap();
            }
            r.fetch_add(1, AtomicOrd::Relaxed);
            true
        });

        // 模拟连续翻页：每次都推进代际
        for page in 0..10 {
            let gen = scheduler.begin_generation();
            scheduler.submit(task(page, Priority::Current, gen));
        }
        // 最终只应有最后一个任务有效
        let final_gen = scheduler.generation();
        scheduler.submit(task(10, Priority::Current, final_gen));

        // 放行工作线程
        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        let stop = AtomicBool::new(false);
        assert!(scheduler.wait_idle(&stop, Duration::from_secs(5)));

        // 关键性质：11 次提交中，绝大多数被代际取消丢弃。
        //
        // 不能断言「恰好执行 1 个」—— 第一次提交的任务在推进代际之前
        // 就已经被工作线程取走并卡在栅栏上，而「已在执行」的任务无法取消。
        // 这是调度器的固有边界：取消只对「还在队列里」的任务生效。
        let done = rendered.load(AtomicOrd::Relaxed);
        assert!(
            done <= 3,
            "快速翻页应只渲染极少数页，实际渲染了 {done} 页"
        );
        assert!(
            scheduler.dropped_count() >= 8,
            "大部分任务应被代际取消，实际丢弃 {}",
            scheduler.dropped_count()
        );

        pool.shutdown();
    }

    #[test]
    fn recommended_threads_is_bounded() {
        let n = recommended_threads();
        assert!((1..=4).contains(&n), "线程数应在 1..=4，实际 {n}");
    }

    #[test]
    fn scheduler_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Scheduler>();
    }

    #[test]
    fn completed_count_is_exposed_for_diagnostics() {
        let s = Scheduler::new();
        let gen = s.begin_generation();
        s.submit(task(0, Priority::Current, gen));
        let stop = AtomicBool::new(false);
        let _ = s.next_task(&stop);
        s.complete();
        assert_eq!(s.completed_count(), 1);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let scheduler = Arc::new(Scheduler::new());
        let pool = WorkerPool::start(Arc::clone(&scheduler), 1, |_| true);
        pool.shutdown();
        // 再次停机不应 panic（Drop 里还会调一次）
        pool.shutdown();
    }
}
